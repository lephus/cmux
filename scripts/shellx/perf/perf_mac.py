#!/usr/bin/env python3
"""ShellX macOS perf harness and 10 % regression gate.

Usage (see README.md next to this file for the exact methods):

    perf_mac.py --app <path.app> --cli <bundled-cli> --runs 5 --out <file.json>
                [--commit <sha>] [--keystroke-ms <float>] [--socket <path>]
                [--work-dir <dir>] [--purge] [--no-hitches]
    perf_mac.py hitches --app <path.app> --cli <bundled-cli> --out <file.json> [--runs 1]
    perf_mac.py compare <baseline.json> <candidate.json> --max-regression 0.10

Every metric is lower-is-better. `compare` exits 1 when any metric of the
candidate is worse than the baseline by more than `--max-regression`; a metric
that is null or missing on either side is skipped.

Safety: the harness only ever signals processes whose executable lives inside
the `--app` bundle it was given. It refuses the stable cmux bundle id
(`com.cmuxterm.app`) and anything under /Applications (the user's ShellX
Flutter app shares the bundle id `pro.shellx.desktop`), and it always passes an
explicit `--socket` to the CLI so implicit socket discovery can never reach a
cmux the user is running.
"""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import platform
import plistlib
import re
import shlex
import shutil
import signal
import statistics
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
from pathlib import Path
from typing import Any, Iterable

METRICS = (
    "cold_start_ms",
    "cat_100mb_ms",
    "idle_rss_mb_4panes",
    "idle_cpu_pct",
    "hitch_ms_per_s",
    "max_hitch_ms",
    "keystroke_ms",
)

REFUSED_BUNDLE_IDS = frozenset({"com.cmuxterm.app"})
REFUSED_PATH_PREFIXES = ("/Applications/",)

CAT_FILE_BYTES = 100 * 1024 * 1024
CAT_LINE_BYTES = 80
COLD_START_TIMEOUT_S = 120.0
SOCKET_TIMEOUT_S = 60.0
SPLIT_SETTLE_TIMEOUT_S = 10.0
CAT_TIMEOUT_S = 900.0
IDLE_WAIT_S = 60.0
CPU_INTERVAL_S = 5.0
CPU_WINDOW_S = 60.0
HITCH_RECORD_S = 30
HITCH_LOAD_DELAY_S = 3.0
HITCH_SIDEBAR_TOGGLES = 20
HITCH_TOGGLE_INTERVAL_S = 1.0
HITCH_ATTEMPTS = 3
HITCH_MIN_FREE_BYTES = 12 * 1024**3
KILL_GRACE_S = 5.0

# Environment of the terminal that runs the harness (often a cmux or ShellX
# pane) must not leak into the measured app or its CLI.
STRIPPED_ENV_PREFIXES = ("CMUX", "GHOSTTY", "SHELLX")
STRIPPED_ENV_KEYS = frozenset({"ZDOTDIR", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "TMUX", "TMUX_PANE"})


class HarnessError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# Comparison gate (pure, exercised on Linux CI)
# --------------------------------------------------------------------------


def _is_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def compare(base: dict, cand: dict, max_regression: float) -> list[str]:
    """Return the metrics where `cand` is worse than `base` by more than `max_regression`.

    All metrics are lower-is-better. Exactly `max_regression` worse passes. A
    metric that is null, missing or non-numeric on either side is skipped.
    """
    base_metrics = base.get("metrics") or {}
    cand_metrics = cand.get("metrics") or {}
    names = [name for name in METRICS if name in base_metrics or name in cand_metrics]
    names += sorted((set(base_metrics) | set(cand_metrics)) - set(METRICS))
    flagged: list[str] = []
    for name in names:
        b = base_metrics.get(name)
        c = cand_metrics.get(name)
        if not (_is_number(b) and _is_number(c)):
            continue
        allowed = abs(b) * max_regression
        # Tolerance keeps 1000 -> 1100 at 10 % from failing on float rounding.
        tolerance = 1e-9 * max(1.0, abs(b))
        if c - b > allowed + tolerance:
            flagged.append(name)
    return flagged


def compare_report(base: dict, cand: dict, max_regression: float) -> list[str]:
    base_metrics = base.get("metrics") or {}
    cand_metrics = cand.get("metrics") or {}
    flagged = set(compare(base, cand, max_regression))
    lines = []
    for name in METRICS:
        b = base_metrics.get(name)
        c = cand_metrics.get(name)
        if not (_is_number(b) and _is_number(c)):
            lines.append(f"  {name:<20} skipped (baseline={b!r}, candidate={c!r})")
            continue
        delta = ((c - b) / b * 100.0) if b else (0.0 if c == b else float("inf"))
        verdict = "REGRESSION" if name in flagged else "ok"
        lines.append(f"  {name:<20} {b:>12.3f} -> {c:>12.3f}  ({delta:+.1f} %)  {verdict}")
    return lines


# --------------------------------------------------------------------------
# Output parsers (pure, exercised on Linux CI)
# --------------------------------------------------------------------------

_BASH_REAL = re.compile(r"^real\s+(?:(\d+)m)?(\d+(?:\.\d+)?)s\s*$", re.MULTILINE)
_ZSH_REAL = re.compile(r"^real\s+((?:\d+:){0,2}\d+(?:\.\d+)?)\s*$", re.MULTILINE)


def parse_real_seconds(text: str) -> float | None:
    """Wall time from a `time` report: zsh `TIMEFMT='real %*E'` or bash `real 0m1.234s`."""
    match = _BASH_REAL.search(text)
    if match:
        return int(match.group(1) or 0) * 60 + float(match.group(2))
    match = _ZSH_REAL.search(text)
    if match:
        seconds = 0.0
        for part in match.group(1).split(":"):
            seconds = seconds * 60 + float(part)
        return seconds
    return None


def _resolver(root: ET.Element):
    by_id = {el.get("id"): el for el in root.iter() if el.get("id") is not None}

    def resolve(el: ET.Element | None) -> ET.Element | None:
        if el is not None and el.get("ref") is not None:
            return by_id.get(el.get("ref"))
        return el

    return resolve


def _row_pid(row: ET.Element, resolve) -> int | None:
    process = resolve(row.find("process"))
    if process is None:
        return None
    pid_el = resolve(process.find("pid"))
    if pid_el is not None and (pid_el.text or "").strip().isdigit():
        return int(pid_el.text.strip())
    match = re.search(r"\((\d+)\)\s*$", process.get("fmt") or "")
    return int(match.group(1)) if match else None


def hitch_durations_ms(xml_text: str, pids: Iterable[int] | None) -> list[float]:
    """Hitch durations (ms) from an `xctrace export` of the `hitches` table.

    Rows attributed to a process outside `pids` are dropped; rows without a
    process column are kept. Values are nanoseconds in the export.
    """
    root = ET.fromstring(xml_text)
    resolve = _resolver(root)
    wanted = set(pids) if pids is not None else None
    durations: list[float] = []
    for row in root.iter("row"):
        duration = resolve(row.find("duration"))
        if duration is None or not (duration.text or "").strip():
            continue
        if wanted is not None:
            pid = _row_pid(row, resolve)
            if pid is not None and pid not in wanted:
                continue
        durations.append(int(duration.text.strip()) / 1_000_000.0)
    return durations


def hitch_metrics(durations_ms: list[float], record_seconds: float) -> dict[str, float]:
    return {
        "hitch_ms_per_s": sum(durations_ms) / record_seconds,
        "max_hitch_ms": max(durations_ms) if durations_ms else 0.0,
    }


# --------------------------------------------------------------------------
# Bundle and process handling (macOS only)
# --------------------------------------------------------------------------


def clean_env(extra: dict[str, str] | None = None) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(STRIPPED_ENV_PREFIXES) and key not in STRIPPED_ENV_KEYS
    }
    if extra:
        env.update(extra)
    return env


class Bundle:
    def __init__(self, app: str) -> None:
        root = Path(os.path.realpath(app))
        if root.suffix != ".app" or not (root / "Contents" / "Info.plist").is_file():
            raise HarnessError(f"--app is not an app bundle: {app}")
        with open(root / "Contents" / "Info.plist", "rb") as handle:
            self.info = plistlib.load(handle)
        self.root = root
        self.bundle_id = str(self.info.get("CFBundleIdentifier", ""))
        executable = self.info.get("CFBundleExecutable")
        self.main_executable = str(root / "Contents" / "MacOS" / executable) if executable else None
        if self.bundle_id in REFUSED_BUNDLE_IDS:
            raise HarnessError(f"refusing to drive {self.bundle_id}: that is the user's cmux; use a tagged build")
        if (str(root) + "/").startswith(REFUSED_PATH_PREFIXES):
            raise HarnessError(f"refusing to drive an app under /Applications: {root}")

    def contains(self, path: str) -> bool:
        return os.path.realpath(path).startswith(str(self.root) + "/")

    def socket_path(self, override: str | None) -> str:
        if override:
            return override
        value = (self.info.get("LSEnvironment") or {}).get("CMUX_SOCKET_PATH")
        if not value:
            raise HarnessError(
                "the bundle's Info.plist has no LSEnvironment CMUX_SOCKET_PATH (not a tagged build); "
                "pass --socket explicitly"
            )
        return str(value).strip()


_libc = None


def _libproc():
    global _libc
    if _libc is None:
        lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        lib.proc_listallpids.argtypes = [ctypes.c_void_p, ctypes.c_int]
        lib.proc_listallpids.restype = ctypes.c_int
        lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
        lib.proc_pidpath.restype = ctypes.c_int
        _libc = lib
    return _libc


def all_process_paths() -> dict[int, str]:
    """pid -> executable path (proc_pidpath), for every process we may inspect."""
    lib = _libproc()
    count = lib.proc_listallpids(None, 0)
    capacity = max(count, 1024) * 2
    buffer = (ctypes.c_int * capacity)()
    count = lib.proc_listallpids(buffer, ctypes.sizeof(buffer))
    paths: dict[int, str] = {}
    path_buffer = ctypes.create_string_buffer(4096)
    for pid in buffer[: max(count, 0)]:
        if pid <= 0:
            continue
        length = lib.proc_pidpath(pid, path_buffer, ctypes.sizeof(path_buffer))
        if length > 0:
            paths[pid] = path_buffer.value.decode("utf-8", "replace")
    return paths


def remove_path(path: Path) -> None:
    if path.is_dir():
        shutil.rmtree(path)
    elif path.exists():
        path.unlink()


def remove_xctrace_scratch(since: float) -> None:
    """xctrace leaves ~5 GB of instruments*.ktrace / xrtmp__* scratch per 30 s recording, even on success.

    Only entries created after `since` (this harness's recording) are removed.
    """
    candidates: list[Path] = []
    for name, pattern in (("DARWIN_USER_TEMP_DIR", "instruments*.ktrace"),
                          ("DARWIN_USER_CACHE_DIR", "com.apple.dt.InstrumentsCLI/path_manager/xrtmp__*")):
        base = subprocess.run(["getconf", name], capture_output=True, text=True).stdout.strip()
        if base:
            candidates.extend(Path(base).glob(pattern))
    for path in candidates:
        try:
            if path.stat().st_birthtime >= since:
                remove_path(path)
        except FileNotFoundError:
            pass


def bundle_processes(bundle: Bundle) -> dict[int, str]:
    me = os.getpid()
    return {pid: path for pid, path in all_process_paths().items() if pid != me and bundle.contains(path)}


def parent_pid(pid: int) -> int | None:
    out = subprocess.run(["ps", "-o", "ppid=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return int(out) if out.isdigit() else None


def refuse_if_running_inside(bundle: Bundle) -> None:
    """Killing the bundle must not kill the terminal this harness runs in."""
    paths = all_process_paths()
    pid: int | None = os.getpid()
    seen = set()
    while pid and pid > 1 and pid not in seen:
        seen.add(pid)
        if pid in paths and bundle.contains(paths[pid]):
            raise HarnessError("the harness is running inside the app it measures; run it from another terminal")
        pid = parent_pid(pid)


def kill_bundle(bundle: Bundle) -> None:
    """SIGTERM, then SIGKILL, every process inside the bundle; fail if any survives."""
    for sig, grace in ((signal.SIGTERM, KILL_GRACE_S), (signal.SIGKILL, KILL_GRACE_S)):
        procs = bundle_processes(bundle)
        if not procs:
            return
        for pid in procs:
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline and bundle_processes(bundle):
            time.sleep(0.1)
    survivors = bundle_processes(bundle)
    if survivors:
        listing = ", ".join(f"{pid}:{path}" for pid, path in sorted(survivors.items()))
        raise HarnessError(f"processes of the bundle survived SIGKILL: {listing}")


def assert_no_bundle_processes(bundle: Bundle) -> None:
    survivors = bundle_processes(bundle)
    if survivors:
        listing = ", ".join(f"{pid}:{path}" for pid, path in sorted(survivors.items()))
        raise HarnessError(f"bundle processes still alive before a cold-start sample: {listing}")


# --------------------------------------------------------------------------
# Measuring session
# --------------------------------------------------------------------------


def wait_for(predicate, timeout: float, what: str, interval: float = 0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    raise HarnessError(f"timed out after {timeout:.0f}s waiting for {what}")


ZSHRC_TEMPLATE = """\
# Written by scripts/shellx/perf/perf_mac.py. Minimal shell so every sample is identical.
PROMPT='%# '
RPROMPT=''
TIMEFMT='real %*E'
_shellx_perf_first_prompt() {{
  if [[ ! -e {mark} ]]; then
    perl -MTime::HiRes=time -e 'printf "%.6f", time' > {mark_tmp} && mv {mark_tmp} {mark}
  fi
  precmd_functions=(${{precmd_functions:#_shellx_perf_first_prompt}})
}}
precmd_functions+=(_shellx_perf_first_prompt)
"""


class Session:
    def __init__(self, args: argparse.Namespace) -> None:
        self.bundle = Bundle(args.app)
        self.cli = os.path.realpath(args.cli)
        if not os.access(self.cli, os.X_OK):
            raise HarnessError(f"--cli is not executable: {args.cli}")
        self.socket = self.bundle.socket_path(args.socket)
        self.work = Path(args.work_dir).expanduser().resolve()
        self.work.mkdir(parents=True, exist_ok=True)
        self.purge = bool(getattr(args, "purge", False))
        self.zdotdir = self.work / "zdotdir"
        self.mark = self.work / "first-prompt.ts"
        self.zdotdir.mkdir(exist_ok=True)
        (self.zdotdir / ".zshrc").write_text(
            ZSHRC_TEMPLATE.format(
                mark=shlex.quote(str(self.mark)),
                mark_tmp=shlex.quote(str(self.mark) + ".tmp"),
            )
        )
        refuse_if_running_inside(self.bundle)

    # -- CLI ---------------------------------------------------------------

    def run_cli(self, *argv: str, check: bool = True, timeout: float = 30.0) -> subprocess.CompletedProcess:
        proc = subprocess.run(
            [self.cli, "--socket", self.socket, *argv],
            capture_output=True,
            text=True,
            env=clean_env(),
            timeout=timeout,
        )
        if check and proc.returncode != 0:
            raise HarnessError(f"cli {' '.join(argv)} failed ({proc.returncode}): {proc.stderr.strip()}")
        return proc

    def send_line(self, command: str) -> None:
        # `cmux send` turns a literal backslash-n into Return (\r).
        self.run_cli("send", "--", command + "\\n")

    def wait_for_socket(self) -> None:
        def ping() -> bool:
            try:
                return self.run_cli("ping", check=False, timeout=5.0).returncode == 0
            except subprocess.TimeoutExpired:
                return False

        wait_for(ping, SOCKET_TIMEOUT_S, f"the app socket {self.socket}", interval=0.25)

    # -- cold start ----------------------------------------------------------

    def cold_start_ms(self) -> float:
        kill_bundle(self.bundle)
        assert_no_bundle_processes(self.bundle)
        if self.purge:
            result = subprocess.run(["sudo", "-n", "/usr/sbin/purge"], capture_output=True, text=True)
            if result.returncode != 0:
                raise HarnessError(f"--purge needs passwordless sudo for /usr/sbin/purge: {result.stderr.strip()}")
        for stale in (self.mark, Path(str(self.mark) + ".tmp")):
            stale.unlink(missing_ok=True)
        start = time.time()
        subprocess.run(
            [
                "open",
                "-n",
                "--env", f"ZDOTDIR={self.zdotdir}",
                "--env", "CMUX_DISABLE_SESSION_RESTORE=1",
                str(self.bundle.root),
            ],
            check=True,
            env=clean_env({"ZDOTDIR": str(self.zdotdir), "CMUX_DISABLE_SESSION_RESTORE": "1"}),
        )
        wait_for(self.mark.exists, COLD_START_TIMEOUT_S, "the first prompt (is the login shell zsh?)")
        stamp = float(self.mark.read_text().strip())
        return (stamp - start) * 1000.0

    # -- cat 100 MB ----------------------------------------------------------

    def cat_file(self) -> Path:
        path = self.work / "cat-100mib.txt"
        if not path.exists() or path.stat().st_size != CAT_FILE_BYTES:
            body = ("The quick brown fox jumps over the lazy dog 0123456789 " * 2)[: CAT_LINE_BYTES - 10]
            lines = CAT_FILE_BYTES // CAT_LINE_BYTES
            tmp = path.with_suffix(".tmp")
            with open(tmp, "w", encoding="ascii") as handle:
                chunk: list[str] = []
                for index in range(lines):
                    chunk.append(f"{index:08d} {body}\n")
                    if len(chunk) == 16384:
                        handle.write("".join(chunk))
                        chunk.clear()
                handle.write("".join(chunk))
            tmp.replace(path)
        return path

    def cat_100mb_ms(self) -> float:
        data = self.cat_file()
        with open(data, "rb") as handle:  # warm the page cache: measure the terminal, not the disk
            while handle.read(1 << 20):
                pass
        report = self.work / "cat.time"
        done = self.work / "cat.done"
        report.unlink(missing_ok=True)
        done.unlink(missing_ok=True)
        q = shlex.quote
        self.send_line(f"{{ time cat {q(str(data))}; }} 2> {q(str(report))}; echo done > {q(str(done))}")
        wait_for(done.exists, CAT_TIMEOUT_S, "cat 100 MiB to finish", interval=0.1)
        seconds = parse_real_seconds(report.read_text())
        if seconds is None:
            raise HarnessError(f"could not parse a `real` time from {report}: {report.read_text()!r}")
        return seconds * 1000.0

    # -- idle with 4 panes ------------------------------------------------

    def make_four_panes(self) -> int:
        for direction in ("right", "down", "down"):
            self.run_cli("new-split", direction)

        # `new-split` returns before the app applies the split, so `list-panes`
        # can lag behind by one pane for a moment: poll briefly.
        def count() -> int:
            payload = json.loads(self.run_cli("--json", "list-panes").stdout or "{}")
            return len(payload.get("panes") or [])

        deadline = time.monotonic() + SPLIT_SETTLE_TIMEOUT_S
        panes = count()
        while panes < 4 and time.monotonic() < deadline:
            time.sleep(0.1)
            panes = count()
        if panes < 4:
            raise HarnessError(f"expected 4 panes after 3 splits, the app reports {panes}")
        return panes

    def rss_mb(self) -> tuple[float, dict[str, float]]:
        procs = bundle_processes(self.bundle)
        if not procs:
            raise HarnessError("no bundle process alive to measure")
        out = subprocess.run(
            ["ps", "-o", "pid=,rss=", "-p", ",".join(str(pid) for pid in procs)],
            capture_output=True,
            text=True,
        ).stdout
        per_process: dict[str, float] = {}
        total_kb = 0
        for line in out.splitlines():
            parts = line.split()
            if len(parts) != 2 or not parts[0].isdigit() or not parts[1].isdigit():
                continue
            pid, kb = int(parts[0]), int(parts[1])
            total_kb += kb
            name = os.path.relpath(procs.get(pid, str(pid)), self.bundle.root)
            per_process[name] = per_process.get(name, 0.0) + kb / 1024.0
        return total_kb / 1024.0, per_process

    def cpu_pct_sample(self) -> float:
        procs = bundle_processes(self.bundle)
        if not procs:
            raise HarnessError("no bundle process alive to measure")
        out = subprocess.run(
            ["ps", "-o", "%cpu=", "-p", ",".join(str(pid) for pid in procs)],
            capture_output=True,
            text=True,
        ).stdout
        return sum(float(value) for value in out.split() if re.fullmatch(r"\d+(?:\.\d+)?", value))

    def idle(self) -> dict[str, Any]:
        panes = self.make_four_panes()
        time.sleep(IDLE_WAIT_S)
        rss, per_process = self.rss_mb()
        cpu_samples = []
        for _ in range(int(CPU_WINDOW_S / CPU_INTERVAL_S)):
            time.sleep(CPU_INTERVAL_S)
            cpu_samples.append(self.cpu_pct_sample())
        return {
            "panes": panes,
            "idle_rss_mb_4panes": rss,
            "rss_mb_by_process": per_process,
            "idle_cpu_pct": statistics.fmean(cpu_samples),
            "cpu_pct_samples": cpu_samples,
        }

    # -- hitches -------------------------------------------------------------

    def hitches(self, label: str) -> dict[str, Any]:
        # xctrace itself is flaky (hangs after --time-limit, or exits -5 while saving): retry the whole recording.
        for attempt in range(1, HITCH_ATTEMPTS + 1):
            try:
                return self._hitches_once(label)
            except (HarnessError, subprocess.SubprocessError) as error:
                if attempt == HITCH_ATTEMPTS:
                    raise
                print(f"  xctrace attempt {attempt}/{HITCH_ATTEMPTS} failed, retrying: {str(error)[-300:]}", flush=True)
        raise AssertionError("unreachable")

    def _hitches_once(self, label: str) -> dict[str, Any]:
        trace = self.work / f"hitches-{label}.trace"
        remove_path(trace)
        free = shutil.disk_usage(self.work).free
        if free < HITCH_MIN_FREE_BYTES:
            raise SystemExit(f"error: only {free / 1e9:.1f} GB free; an all-processes trace can take several GB")
        started = time.time()
        try:
            return self._record_hitches(trace)
        finally:
            # Traces of all processes are GBs each; keep only the numbers.
            remove_path(trace)
            remove_xctrace_scratch(started)

    def _record_hitches(self, trace: Path) -> dict[str, Any]:
        recorder = subprocess.Popen(
            [
                "xcrun", "xctrace", "record",
                "--template", "Animation Hitches",
                "--all-processes",
                "--time-limit", f"{HITCH_RECORD_S}s",
                "--no-prompt",
                "--output", str(trace),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=clean_env(),
        )
        try:
            time.sleep(HITCH_LOAD_DELAY_S)
            self.send_line("seq 1 2000000")
            for _ in range(HITCH_SIDEBAR_TOGGLES):
                self.run_cli("right-sidebar", "toggle")
                time.sleep(HITCH_TOGGLE_INTERVAL_S)
            try:
                output, _ = recorder.communicate(timeout=HITCH_RECORD_S + 120)
            except subprocess.TimeoutExpired:
                # xctrace sometimes hangs after --time-limit; SIGINT makes it stop and save the trace.
                recorder.send_signal(signal.SIGINT)
                output, _ = recorder.communicate(timeout=120)
        except BaseException:
            recorder.kill()
            raise
        if recorder.returncode != 0 or not trace.exists():
            raise HarnessError(f"xctrace record failed ({recorder.returncode}): {output.strip()[-2000:]}")
        schema = self.hitch_schema(trace)
        exported = subprocess.run(
            [
                "xcrun", "xctrace", "export",
                "--input", str(trace),
                "--xpath", f'/trace-toc/run[@number="1"]/data/table[@schema="{schema}"]',
            ],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        durations = hitch_durations_ms(exported, bundle_processes(self.bundle).keys())
        return {**hitch_metrics(durations, HITCH_RECORD_S), "hitch_count": len(durations)}

    @staticmethod
    def hitch_schema(trace: Path) -> str:
        toc = subprocess.run(
            ["xcrun", "xctrace", "export", "--input", str(trace), "--toc"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        schemas = [table.get("schema") or "" for table in ET.fromstring(toc).iter("table")]
        if "hitches" in schemas:
            return "hitches"
        raise HarnessError(f"no `hitches` table in {trace}; tables: {sorted(set(schemas))}")


# --------------------------------------------------------------------------
# Commands
# --------------------------------------------------------------------------


def machine_info() -> dict[str, str]:
    def sysctl(name: str) -> str:
        return subprocess.run(["sysctl", "-n", name], capture_output=True, text=True).stdout.strip()

    return {"macos": platform.mac_ver()[0], "model": sysctl("hw.model"), "cpu": sysctl("machdep.cpu.brand_string")}


def method_info(args: argparse.Namespace, hitches: bool) -> dict[str, Any]:
    return {
        "runs": args.runs,
        "purge": bool(getattr(args, "purge", False)),
        "idle_wait_s": IDLE_WAIT_S,
        "cpu_interval_s": CPU_INTERVAL_S,
        "cpu_window_s": CPU_WINDOW_S,
        "cat_file_bytes": CAT_FILE_BYTES,
        "hitches": hitches,
        "hitch_record_s": HITCH_RECORD_S,
        "hitch_sidebar_toggles": HITCH_SIDEBAR_TOGGLES,
    }


def median_or_none(values: list[float]) -> float | None:
    return statistics.median(values) if values else None


def write_json(path: str, payload: dict) -> None:
    Path(path).write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n")


def cmd_run(args: argparse.Namespace) -> int:
    session = Session(args)
    samples: dict[str, list] = {name: [] for name in METRICS if name != "keystroke_ms"}
    details: list[dict[str, Any]] = []
    try:
        for run in range(1, args.runs + 1):
            print(f"[run {run}/{args.runs}] cold start", flush=True)
            cold = session.cold_start_ms()
            session.wait_for_socket()
            print(f"[run {run}/{args.runs}] idle (4 panes, {IDLE_WAIT_S:.0f}s + {CPU_WINDOW_S:.0f}s)", flush=True)
            idle = session.idle()
            print(f"[run {run}/{args.runs}] cat 100 MiB", flush=True)
            cat_ms = session.cat_100mb_ms()
            hitch = None
            if not args.no_hitches:
                print(f"[run {run}/{args.runs}] hitches ({HITCH_RECORD_S}s)", flush=True)
                hitch = session.hitches(f"run{run}")
            kill_bundle(session.bundle)
            samples["cold_start_ms"].append(cold)
            samples["cat_100mb_ms"].append(cat_ms)
            samples["idle_rss_mb_4panes"].append(idle["idle_rss_mb_4panes"])
            samples["idle_cpu_pct"].append(idle["idle_cpu_pct"])
            if hitch:
                samples["hitch_ms_per_s"].append(hitch["hitch_ms_per_s"])
                samples["max_hitch_ms"].append(hitch["max_hitch_ms"])
            details.append({"run": run, "cold_start_ms": cold, "cat_100mb_ms": cat_ms, "idle": idle, "hitches": hitch})
            metrics = {name: median_or_none(values) for name, values in samples.items()}
            metrics["keystroke_ms"] = args.keystroke_ms
            write_json(args.out, {
                "app": str(session.bundle.root),
                "bundle_id": session.bundle.bundle_id,
                "commit": args.commit,
                "metrics": {name: metrics.get(name) for name in METRICS},
                "samples": {**samples, "runs": details},
                "method": method_info(args, not args.no_hitches),
                "machine": machine_info(),
            })
    finally:
        kill_bundle(session.bundle)
    print(f"wrote {args.out}")
    return 0


def cmd_hitches(args: argparse.Namespace) -> int:
    session = Session(args)
    out = Path(args.out)
    payload = json.loads(out.read_text()) if out.exists() else {
        "app": str(session.bundle.root),
        "bundle_id": session.bundle.bundle_id,
        "commit": args.commit,
        "metrics": {name: None for name in METRICS},
        "samples": {},
        "machine": machine_info(),
    }
    if payload.get("app") not in (None, str(session.bundle.root)):
        raise HarnessError(f"{out} was measured for {payload['app']}, not {session.bundle.root}")
    per_s: list[float] = []
    longest: list[float] = []
    runs = []
    try:
        for run in range(1, args.runs + 1):
            session.cold_start_ms()
            session.wait_for_socket()
            result = session.hitches(f"hitches-run{run}")
            kill_bundle(session.bundle)
            per_s.append(result["hitch_ms_per_s"])
            longest.append(result["max_hitch_ms"])
            runs.append(result)
    finally:
        kill_bundle(session.bundle)
    payload.setdefault("metrics", {})
    payload["metrics"]["hitch_ms_per_s"] = statistics.median(per_s)
    payload["metrics"]["max_hitch_ms"] = statistics.median(longest)
    payload.setdefault("samples", {})
    payload["samples"]["hitch_ms_per_s"] = per_s
    payload["samples"]["max_hitch_ms"] = longest
    payload["samples"]["hitch_runs"] = runs
    write_json(args.out, payload)
    print(f"wrote {args.out}")
    return 0


def cmd_compare(args: argparse.Namespace) -> int:
    base = json.loads(Path(args.baseline).read_text())
    cand = json.loads(Path(args.candidate).read_text())
    flagged = compare(base, cand, args.max_regression)
    print(f"baseline  {base.get('app')} @ {base.get('commit')}")
    print(f"candidate {cand.get('app')} @ {cand.get('commit')}")
    print(f"max regression {args.max_regression * 100:.1f} % (lower is better for every metric)")
    print("\n".join(compare_report(base, cand, args.max_regression)))
    if flagged:
        print(f"FAIL: regressed beyond the gate: {', '.join(flagged)}")
        return 1
    print("PASS")
    return 0


def _add_session_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--app", required=True, help="tagged build to measure (never /Applications or com.cmuxterm.app)")
    parser.add_argument("--cli", required=True, help="the CLI bundled in that app (Contents/Resources/bin/cmux)")
    parser.add_argument("--out", required=True, help="JSON result file")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--commit", default="unknown", help="source commit of the measured build")
    parser.add_argument("--socket", help="control socket; default: the bundle's LSEnvironment CMUX_SOCKET_PATH")
    parser.add_argument(
        "--work-dir",
        default="~/Library/Caches/shellx-perf",
        help="ZDOTDIR, markers, the 100 MiB file and traces",
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    if argv and argv[0] == "compare":
        parser = argparse.ArgumentParser(prog="perf_mac.py compare")
        parser.add_argument("baseline")
        parser.add_argument("candidate")
        parser.add_argument("--max-regression", type=float, default=0.10)
        args = parser.parse_args(argv[1:])
        args.func = cmd_compare
        return args
    if argv and argv[0] == "hitches":
        parser = argparse.ArgumentParser(prog="perf_mac.py hitches")
        _add_session_args(parser)
        parser.set_defaults(runs=1)
        args = parser.parse_args(argv[1:])
        args.func = cmd_hitches
        return args
    parser = argparse.ArgumentParser(prog="perf_mac.py", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    _add_session_args(parser)
    parser.add_argument("--keystroke-ms", type=float, default=None, help="Typometer median, entered by hand")
    parser.add_argument("--purge", action="store_true", help="sudo -n purge before each cold-start sample")
    parser.add_argument("--no-hitches", action="store_true", help="skip the xctrace hitch recording")
    args = parser.parse_args(argv)
    args.func = cmd_run
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if getattr(args, "runs", 1) < 1:
        print("--runs must be at least 1", file=sys.stderr)
        return 2
    if args.func is not cmd_compare and sys.platform != "darwin":
        print("measuring needs macOS", file=sys.stderr)
        return 2
    try:
        return args.func(args)
    except HarnessError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
