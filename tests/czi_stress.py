#!/usr/bin/env python3
"""Repeat isolated CZI PTY scenarios. No real Codex history or shell config is used."""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from tty_channel import exercise
from pty_navigation import navigation

DEFAULTS = {"quick": 10, "full": 50, "stress": 200}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=DEFAULTS, default="quick")
    parser.add_argument("--rounds", type=int)
    parser.add_argument("--seed", type=int, default=2026)
    parser.add_argument("--binary", type=Path, default=Path("target/release/czi"))
    parser.add_argument("--case", choices=("all", "terminal", "shell", "navigation"), default="all")
    args = parser.parse_args()
    rounds = args.rounds if args.rounds is not None else DEFAULTS[args.mode]
    if rounds < 1:
        parser.error("--rounds must be positive")
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"binary {binary} not found; run cargo build --release --locked")
    for name in ("bash", "fish", "stty", "python3"):
        if shutil.which(name) is None:
            parser.error(f"{name} is required; no PTY scenario was run")
    version = subprocess.run([str(binary), "--version"], text=True, capture_output=True, check=True).stdout.strip()
    print(f"CZI Stability Test Report\nVersion: {version}\nMode: {args.mode}  Rounds: {rounds}  Seed: {args.seed}  Case: {args.case}", flush=True)
    started = time.monotonic()
    passed = failed = timed_out = 0
    samples = []
    timings = []
    current = "setup"
    try:
        with tempfile.TemporaryDirectory(prefix="czi-stability-") as tmp:
            base = Path(tmp)
            def run(label, fn):
                nonlocal passed, failed, timed_out, current
                current = label
                start = time.monotonic()
                try:
                    result = fn()
                    if result:
                        samples.extend(result)
                    passed += 1
                    timings.append((label, time.monotonic() - start))
                    trend = ""
                    if result and len(result) > 1:
                        trend = f"; shell FD {result[0][0]}→{result[-1][0]}, RSS {result[0][1]}→{result[-1][1]} KiB"
                    print(f"{label}: PASS ({timings[-1][1]:.2f}s{trend})", flush=True)
                except Exception as exc:
                    failed += 1
                    if isinstance(exc, TimeoutError) or "timed out" in str(exc).lower():
                        timed_out += 1
                    raise

            if args.case in ("all", "terminal"):
                root = base / "terminal"
                root.mkdir()
                run("stdout pipe + redirected stdin", lambda: exercise(str(binary), root, redirect_input=True))
                run("query timeout and cleanup", lambda: exercise(str(binary), root, timeout_query=True))
                run("Esc and Ctrl-C cancellation", lambda: (exercise(str(binary), root, key=b"\x1b"), exercise(str(binary), root, key=b"\x03")) and None)
            if args.case in ("all", "shell"):
                for shell in ("fish", "bash"):
                    root = base / shell
                    root.mkdir()
                    run(f"{shell}: {rounds} Esc cancellations", lambda shell=shell, root=root: exercise(str(binary), root, shell=shell, key=b"\x1b", count=rounds))
                    run(f"{shell}: {rounds} selection and resume", lambda shell=shell, root=root: exercise(str(binary), root, shell=shell, count=rounds))
                    run(f"{shell}: {rounds} last resumes", lambda shell=shell, root=root: exercise(str(binary), root, shell=shell, mode="last", count=rounds))
                    run(f"{shell}: Ctrl-C cancel", lambda shell=shell, root=root: exercise(str(binary), root, shell=shell, key=b"\x03"))
                    run(f"{shell}: real SIGINT to fake Codex", lambda shell=shell, root=root: exercise(str(binary), root, shell=shell, exit_code="waitsig"))
                    if args.mode != "quick":
                        for code in (130, 23):
                            run(f"{shell}: Codex exit {code}", lambda shell=shell, root=root, code=code: exercise(str(binary), root, shell=shell, exit_code=code))
                        run(f"{shell}: current directory", lambda shell=shell, root=root: exercise(str(binary), root, shell=shell, mode=".", key=b"\r"))
                        run(f"{shell}: project view", lambda shell=shell, root=root: exercise(str(binary), root, shell=shell, mode="-p"))
            if args.case in ("all", "navigation"):
                root = base / "navigation"
                root.mkdir()
                run(f"100 sessions: scrolling and {rounds} seeded mixed rounds", lambda: navigation(str(binary), root, args.seed, rounds))
            if samples:
                fd_values = [sample[0] for sample in samples]
                rss_values = [sample[1] for sample in samples]
                selectors = sum(sample[2] for sample in samples)
                print(f"Shell resource samples: {len(samples)}; FDs {min(fd_values)}..{max(fd_values)}, RSS {min(rss_values)}..{max(rss_values)} KiB (across test processes)")
                print(f"Single-threaded selectors observed: {selectors}" if selectors else "Selector thread count: SKIPPED (no /proc child samples)")
            else:
                print("Resources: SKIPPED (/proc unavailable or no shell rounds)")
    except Exception as exc:
        print(f"FAIL: {current}: {exc}", file=sys.stderr)
        print(f"Reproduce: python3 tests/czi_stress.py --mode {args.mode} --rounds {rounds} --seed {args.seed} --case {args.case}", file=sys.stderr)
        if failed == 0:
            failed = 1
    print(f"Scenario groups: passed={passed} failed={failed} timed_out={timed_out}; elapsed={time.monotonic()-started:.2f}s")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
