#!/usr/bin/env python3
"""Give every tracked file an mtime derived from its git blob id.

A fresh CI checkout stamps every file with "now", so a DerivedData restored
from the cache sees every input as changed and recompiles everything. With a
content-derived mtime, an unchanged file keeps the exact mtime it had when the
cached DerivedData was built, and a changed file gets a different one, so
Xcode's incremental build only redoes what changed.

Used by .github/workflows/shellx-mac.yml. Walks the repo and every checked-out
submodule (recursively). Symlinks and gitlinks are skipped.
"""
import os
import subprocess
import sys
from pathlib import Path

BASE = 1_500_000_000  # 2017-07-14; any fixed past time works
SPAN = 1 << 28        # ~8.5 years of distinct seconds


def stamp(root: Path) -> int:
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-s", "-z"],
        check=True, capture_output=True,
    ).stdout
    count = 0
    for entry in out.split(b"\0"):
        if not entry:
            continue
        meta, _, name = entry.partition(b"\t")
        mode, sha, _stage = meta.split(b" ")
        path = root / os.fsdecode(name)
        if mode == b"160000":
            if (path / ".git").exists():
                count += stamp(path)
            continue
        if mode not in (b"100644", b"100755"):
            continue
        t = BASE + int(sha[:12], 16) % SPAN
        try:
            os.utime(path, (t, t), follow_symlinks=False)
            count += 1
        except FileNotFoundError:
            pass  # sparse or deleted in the worktree
    return count


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    print(f"normalize-mtimes: stamped {stamp(root)} files under {root}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
