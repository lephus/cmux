# ShellX macOS perf harness

`perf_mac.py` measures a macOS build of cmux / ShellX and gates a candidate
against a baseline (spec C10: at most 10 % worse on every metric). W0 records
the cmux baseline; W1's last task reruns the same commands on ShellX and runs
`compare`. Rerun with the same machine, the same Ghostty config, on AC power,
with no other heavy apps open, and with the same harness commit.

## Commands

```bash
# 1. Measure (5 runs, medians). The app must be a tagged build (reload.sh --tag).
python3 scripts/shellx/perf/perf_mac.py \
  --app "<DerivedData>/Build/Products/Release/cmux DEV perf-base.app" \
  --cli "<same app>/Contents/Resources/bin/cmux" \
  --runs 5 --commit d0b2cd7e --keystroke-ms <typometer median> \
  --out /tmp/perf-cmux-d0b2cd7e.json

# 2. Hitches only (re-record without redoing the rest; merges into --out)
python3 scripts/shellx/perf/perf_mac.py hitches --app <app> --cli <cli> --out <file.json> [--runs 1]

# 3. Gate: exit 1 if any metric is worse by more than 10 %
python3 scripts/shellx/perf/perf_mac.py compare baseline.json candidate.json --max-regression 0.10
```

Options: `--socket <path>` (default: the bundle's `Info.plist`
`LSEnvironment.CMUX_SOCKET_PATH`, which `reload.sh --tag` writes, e.g.
`/tmp/cmux-debug-<tag>.sock`), `--work-dir` (default
`~/Library/Caches/shellx-perf`), `--purge` (run `sudo -n /usr/sbin/purge`
before each cold-start sample; needs passwordless sudo for `purge`), and
`--no-hitches`. `--keystroke-ms` can be added to the JSON by hand later
(`metrics.keystroke_ms`) if Typometer is run after the measuring pass.

## Output

```json
{"app": "...", "bundle_id": "...", "commit": "...",
 "metrics": {"cold_start_ms": 0, "cat_100mb_ms": 0, "idle_rss_mb_4panes": 0,
             "idle_cpu_pct": 0, "hitch_ms_per_s": 0, "max_hitch_ms": 0,
             "keystroke_ms": null},
 "samples": {"<metric>": [per-run values], "runs": [per-run details]},
 "method": {...}, "machine": {"macos": "...", "model": "...", "cpu": "..."}}
```

Each metric is the median over the runs. Every metric is lower-is-better.
`compare` skips a metric that is null or missing on either side and passes a
metric exactly at the limit (1000 → 1100 passes at 10 %, 1101 fails).

## Safety

- Only processes whose executable path (`proc_pidpath`) is inside the `--app`
  bundle are ever signalled. A running `com.cmuxterm.app`, the user's
  `/Applications/ShellX.app` (a Flutter app that shares bundle id
  `pro.shellx.desktop`) or anything else outside that path is never touched.
- The harness refuses a bundle with id `com.cmuxterm.app`, any bundle under
  `/Applications/`, and running from a terminal that is itself inside the
  measured bundle.
- Every CLI call passes `--socket <path>` explicitly. Without it the CLI's
  implicit discovery falls back to the stable cmux socket and would drive the
  user's own cmux.
- The environment passed to `open` and to the CLI has every `CMUX*`,
  `GHOSTTY*`, `SHELLX*`, `ZDOTDIR`, `TERM_PROGRAM*` and `TMUX*` variable removed,
  so running the harness from inside a cmux pane cannot leak a surface id or
  socket into the measured app.

## Methods (one run = the steps below, in this order)

Shell setup: the work dir holds `zdotdir/.zshrc`, rewritten each time the harness starts:
`PROMPT='%# '`, `RPROMPT=''`, `TIMEFMT='real %*E'`, and a one-shot `precmd`
hook that writes `perl -MTime::HiRes=time -e 'printf "%.6f", time'` to
`first-prompt.ts` (via a temp file and `mv`) if that file does not exist yet.
Because `ZDOTDIR` points there, the user's own `~/.zshrc`/`~/.zshenv` are not
read (cmux's integration hands a user `ZDOTDIR` through as
`CMUX_ZSH_ZDOTDIR`). The login shell must be zsh.

1. **Cold start** (`cold_start_ms`)
   - SIGTERM every process inside the bundle, wait up to 5 s, SIGKILL the
     rest, wait up to 5 s; then assert none remains (the harness aborts
     otherwise). With `--purge`, `sudo -n purge` follows.
   - Delete `first-prompt.ts`, take `t0 = time.time()`, then
     `open -n --env ZDOTDIR=<work>/zdotdir --env CMUX_DISABLE_SESSION_RESTORE=1 <app>`
     (session restore is off so every sample opens one window with one
     terminal, whatever the previous run left behind).
   - Wait (≤ 120 s) for `first-prompt.ts`; `cold_start_ms = (marker − t0) × 1000`.
   - Then wait (≤ 60 s) until `cli --socket S ping` succeeds.
   - Without `--purge` the file-system page cache is warm from the previous
     run (and from the build); record whether `--purge` was used
     (`method.purge`) and compare like with like.
2. **Idle with 4 panes** (`idle_rss_mb_4panes`, `idle_cpu_pct`)
   - `cli --socket S new-split right`, `new-split down`, `new-split down`
     (no `--focus`, so focus stays on the first terminal);
     `cli --socket S --json list-panes` must report ≥ 4 panes (polled for up to 10 s: `new-split` returns before the app applies the split).
   - Wait 60 s. RSS = sum of `ps -o pid=,rss= -p <pids>` over every process
     inside the bundle, in MiB (KiB / 1024). The per-executable split is kept
     in `samples.runs[].idle.rss_mb_by_process`.
   - CPU: every 5 s for 60 s (12 samples), sum `ps -o %cpu= -p <pids>` over the
     same processes; `idle_cpu_pct` = mean of the 12 sums. (`ps %cpu` is the
     kernel's decaying average, so this is a smoothed figure, identical on
     both sides.)
3. **`cat` 100 MiB** (`cat_100mb_ms`)
   - `<work>/cat-100mib.txt` is created once: 1 310 720 lines of exactly 80
     ASCII bytes (`%08d ` + a fixed 70-byte pangram/digit slice + `\n`) =
     104 857 600 bytes. It is regenerated only if its size differs. It is
     read once by the harness before the sample so the page cache is warm
     and the sample measures the terminal, not the disk.
   - The focused terminal (the first pane) gets, via
     `cli --socket S send -- '<command>\n'` (the CLI turns a literal `\n`
     into Return):
     `{ time cat <file>; } 2> <work>/cat.time; echo done > <work>/cat.done`
   - Wait (≤ 900 s) for `cat.done`, parse the `real` line of `cat.time`
     (`real 1.234` or `real 1:02.345` from zsh; bash `real 0m1.234s` is
     accepted too); `cat_100mb_ms = real × 1000`.
   - Deviation from the plan text: the plan wrote `time cat FILE >/dev/null`.
     With stdout on `/dev/null` the terminal never sees the bytes and the
     number would only measure the disk cache, so the output goes to the
     terminal (spec §2.2 (c): "thời gian `cat` một file 100 MB" in the app).
4. **Hitches** (`hitch_ms_per_s`, `max_hitch_ms`; skipped by `--no-hitches`)
   - `xcrun xctrace record --template "Animation Hitches" --all-processes --time-limit 30s --no-prompt --output <work>/hitches-<run>.trace`
     (all processes, not `--attach`, so it works on hardened-runtime release
     builds without `get-task-allow`). The trace can take several GB, so it is
     deleted after export together with the `instruments*.ktrace` /
     `xrtmp__*` scratch xctrace leaves in `getconf DARWIN_USER_TEMP_DIR` /
     `DARWIN_USER_CACHE_DIR` (~5 GB per recording; only files created during
     this recording), so only the numbers are kept; the run stops when less
     than 12 GB is free, and a failed `xctrace` (hang after the time limit →
     SIGINT, or exit -5 while saving) is retried up to 3 times.
   - 3 s after starting the recorder: `cli --socket S send -- 'seq 1 2000000\n'`,
     then `cli --socket S right-sidebar toggle` 20 times, 1 s apart (an even
     count, so the sidebar ends where it started).
   - Export: `xcrun xctrace export --input <trace> --toc` must list a table
     with schema `hitches`; then
     `xcrun xctrace export --input <trace> --xpath '/trace-toc/run[@number="1"]/data/table[@schema="hitches"]'`.
     Each row's `duration` (ns, `ref=` resolved) is kept when the row's
     `process` pid belongs to the bundle (rows without a process are kept).
   - `hitch_ms_per_s = Σ duration_ms / 30`, `max_hitch_ms = max duration_ms`
     (0 when there are none).
   - The plan says "toggles the sidebar". The left sidebar has no CLI or
     socket command in cmux (only the ⌘B shortcut and the titlebar button);
     the right sidebar does (`right-sidebar toggle`), so that is what is
     toggled, on both baseline and candidate.
5. Kill every process of the bundle (same procedure as step 1). The JSON is
   rewritten after every run, so an aborted session keeps the finished runs.

**Keystroke latency** (`keystroke_ms`) is measured by hand with Typometer:
200 samples, default font, an empty shell in a fresh window of the build under
test; enter the median with `--keystroke-ms`. Leave it out (null) and
`compare` skips it.

## CLI commands used

All taken from the bundled CLI source (`CLI/cmux.swift`); every call is
`<cli> --socket <S> ...`:

| Purpose | Command |
|---|---|
| socket readiness | `ping` |
| type into the focused terminal | `send -- '<text>\n'` (`\n` → Return) |
| split | `new-split <left\|right\|up\|down>` |
| count panes | `--json list-panes` → `{"panes": [...]}` |
| toggle a sidebar | `right-sidebar toggle` |
