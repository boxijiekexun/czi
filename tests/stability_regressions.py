#!/usr/bin/env python3
"""Focused synthetic PTY regressions for termination and search input."""
import argparse
import json
import os
import select
import shlex
import shutil
import signal
import sys
import tempfile
import termios
import time
import unicodedata
from pathlib import Path
from tty_channel import exercise, setup
from pty_navigation import Picker


def search(binary, root):
    setup(binary, root)
    picker = Picker(binary, root)
    try:
        picker.wait_text("Browsing")
        os.write(picker.master, b"/")
        picker.wait_text("Searching")
        for text in ("/tmp/", "src/main.rs", "foo/bar", "//", "中文 / x"):
            os.write(picker.master, text.encode())
            visual = "".join(ch + (" " if unicodedata.east_asian_width(ch) in "WF" else "") for ch in text)
            picker.wait_text("Search: " + visual)
            os.write(picker.master, b"\x7f" * len(text))
            picker.wait_text("Search: ▏")
        os.write(picker.master, b"/\x7f/")
        picker.wait_text("Search: /▏")
        os.write(picker.master, b"\x1b")
        picker.wait_text("Browsing")
        os.write(picker.master, b"//")
        picker.wait_text("Search: /▏")
        os.write(picker.master, b"\r")
        picker.wait_text("Filtered browsing")
        os.write(picker.master, b"//")
        picker.wait_text("Search: //▏")
    finally:
        picker.close()


def termination(binary, root, initializing=False, near_exit=False):
    setup(binary, root)
    picker = Picker(binary, root)
    done = False
    try:
        if initializing:
            deadline = time.monotonic() + 5
            received = b""
            while b"\x1b[6n" not in received and time.monotonic() < deadline:
                ready, _, _ = select.select([picker.master], [], [], 0.1)
                if ready: received += os.read(picker.master, 4096)
            assert b"\x1b[6n" in received, "no cursor query during initialization"
        else:
            picker.wait_text("Browsing")
            assert not (termios.tcgetattr(picker.master)[3] & termios.ICANON), "not in raw mode"
        if near_exit:
            os.write(picker.master, b"\x1b")
        try:
            os.kill(picker.pid, signal.SIGTERM)
        except ProcessLookupError:
            assert near_exit
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try: picker.pump()
            except OSError: pass
            pid, status = os.waitpid(picker.pid, os.WNOHANG)
            if pid:
                done = True
                break
        assert done, "SIGTERM did not terminate picker"
        allowed = (-signal.SIGTERM, 2, 143) if near_exit else (143,)
        assert os.waitstatus_to_exitcode(status) in allowed, f"unexpected SIGTERM status: {status}"
        flags = termios.tcgetattr(picker.master)[3]
        assert flags & termios.ECHO and flags & termios.ICANON, "SIGTERM left raw mode active"
        if not initializing:
            assert b"\x1b[?25h" in picker.raw, "cursor not restored"
        assert picker.output.read_bytes() == b"", "terminated picker emitted a selection"
    finally:
        if not done:
            try: os.killpg(picker.pid, signal.SIGKILL)
            except ProcessLookupError: pass
            os.waitpid(picker.pid, 0)
        os.close(picker.master)


def invalid_id(binary, root):
    setup(binary, root)
    sessions = root / "codex" / "sessions"
    for name in ("first.jsonl", "second.jsonl"):
        (sessions / name).unlink()
    (sessions / "invalid.jsonl").write_text(json.dumps({"type": "session_meta", "payload": {"id": "abcdef汉🙂", "cwd": str(root)}}, ensure_ascii=False) + "\n")
    picker = Picker(binary, root)
    done = False
    try:
        picker.wait_text("Session · abcdef")
        os.write(picker.master, b"\r")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try: picker.pump()
            except OSError: pass
            pid, status = os.waitpid(picker.pid, os.WNOHANG)
            if pid:
                done = True
                break
        assert done and os.waitstatus_to_exitcode(status) == 1
        assert b"valid native UUID" in picker.raw
        assert picker.output.read_bytes() == b""
        flags = termios.tcgetattr(picker.master)[3]
        assert flags & termios.ECHO and flags & termios.ICANON
    finally:
        if not done:
            try: os.killpg(picker.pid, signal.SIGKILL)
            except ProcessLookupError: pass
            os.waitpid(picker.pid, 0)
        os.close(picker.master)


def signal_after_launch(binary, root):
    setup(binary, root)
    ready = root / "codex-ready"
    received = root / "codex-term"
    codex = root / "bin" / "codex"
    fake = root / "fake_codex_term.py"
    fake.write_text(f'import signal\nfrom pathlib import Path\ndef stop(sig, frame):\n Path({str(received)!r}).write_text(str(sig))\n raise SystemExit(143)\nsignal.signal(signal.SIGTERM, stop)\np=Path({str(ready)!r})\nt=p.with_suffix(".new")\nt.write_text("ready")\nt.replace(p)\nwhile True: signal.pause()\n')
    codex.write_text(f'#!/bin/sh\nif [ "$1" = --version ]; then exit 0; fi\nexec python3 {shlex.quote(str(fake))}\n')
    picker = Picker(binary, root, args=[])
    done = False
    try:
        picker.wait_text("Browsing")
        os.write(picker.master, b"\x1b[B\r")
        deadline = time.monotonic() + 5
        while not ready.exists() and time.monotonic() < deadline:
            try: picker.pump()
            except OSError: pass
        assert ready.exists(), "fake Codex did not start"
        flags = termios.tcgetattr(picker.master)[3]
        assert flags & termios.ECHO and flags & termios.ICANON, "TUI did not restore terminal before launch"
        os.killpg(picker.pid, signal.SIGTERM)
        while time.monotonic() < deadline:
            pid, status = os.waitpid(picker.pid, os.WNOHANG)
            if pid:
                done = True
                break
            time.sleep(0.01)
        assert done and os.waitstatus_to_exitcode(status) == -signal.SIGTERM, "CZI swallowed SIGTERM after launching Codex"
        while not received.exists() and time.monotonic() < deadline: time.sleep(0.01)
        assert received.exists(), "Codex did not receive SIGTERM"
    finally:
        if not done:
            try: os.killpg(picker.pid, signal.SIGKILL)
            except ProcessLookupError: pass
            os.waitpid(picker.pid, 0)
        os.close(picker.master)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--case", choices=("all", "search", "term", "term-init", "term-exit", "invalid-id", "term-codex", "term-shell"), default="all")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    for name in ("search", "term", "term-init", "term-exit", "invalid-id", "term-codex", "term-shell"):
        if args.case not in ("all", name): continue
        with tempfile.TemporaryDirectory(prefix="czi-regression-") as tmp:
            root = Path(tmp)
            if name == "search": search(binary, root)
            elif name == "invalid-id": invalid_id(binary, root)
            elif name == "term-codex": signal_after_launch(binary, root)
            elif name == "term-shell":
                for shell in ("bash", "fish"):
                    if shutil.which(shell) is None:
                        print(shell + " SIGTERM PTY: SKIPPED (shell unavailable)", flush=True)
                        continue
                    exercise(binary, root, shell=shell, key=b"", terminate_picker=True)
            else: termination(binary, root, initializing=name == "term-init", near_exit=name == "term-exit")
            if name in ("term", "term-init", "term-exit"): search(binary, root)
        print(name + ": PASS", flush=True)

if __name__ == "__main__":
    main()
