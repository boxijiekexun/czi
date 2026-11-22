#!/usr/bin/env python3
"""Small VT screen for observing real CZI PTY redraws with synthetic history."""
import codecs
import fcntl
import json
import os
import pty
import random
import re
import select
import signal
import struct
import termios
import time
import unicodedata
from pathlib import Path


class Screen:
    def __init__(self, width, height):
        self.width, self.height = width, height
        self.cells = [[" "] * width for _ in range(height)]
        self.colors = [[None] * width for _ in range(height)]
        self.foreground = None
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def resize(self, width, height):
        self.width, self.height = width, height
        self.cells = [[" "] * width for _ in range(height)]
        self.colors = [[None] * width for _ in range(height)]
        self.x = min(self.x, width - 1)
        self.y = min(self.y, height - 1)

    def feed(self, data):
        s = self.pending + self.decoder.decode(data)
        self.pending = ""
        i = 0
        while i < len(s):
            c = s[i]
            if c == "\x1b":
                if i + 1 >= len(s):
                    break
                if s[i+1] == "[":
                    match = re.match(r"\x1b\[([0-9;?]*)([ -/]*)([@-~])", s[i:])
                    if not match:
                        break
                    params, _, op = match.groups()
                    nums = [int(n) if n else 0 for n in params.lstrip("?").split(";")]
                    if op in "Hf":
                        self.y = max(0, (nums[0] if nums and nums[0] else 1) - 1)
                        self.x = max(0, (nums[1] if len(nums) > 1 and nums[1] else 1) - 1)
                    elif op in "ABCD":
                        n = nums[0] or 1
                        if op == "A": self.y = max(0, self.y - n)
                        if op == "B": self.y = min(self.height - 1, self.y + n)
                        if op == "C": self.x = min(self.width - 1, self.x + n)
                        if op == "D": self.x = max(0, self.x - n)
                    elif op == "J" and nums[0] in (0, 2):
                        for y in range(self.y if nums[0] == 0 else 0, self.height):
                            start = self.x if nums[0] == 0 and y == self.y else 0
                            self.cells[y][start:] = [" "] * (self.width - start)
                    elif op == "K":
                        self.cells[self.y][self.x:] = [" "] * (self.width - self.x)
                    elif op == "m":
                        n = 0
                        while n < len(nums):
                            code = nums[n]
                            if code in (0, 39): self.foreground = None
                            elif code == 31: self.foreground = "red"
                            elif code == 38 and n + 2 < len(nums) and nums[n+1] == 5:
                                self.foreground = "red" if nums[n+2] == 1 else "other"
                                n += 2
                            elif 30 <= code <= 37 or 90 <= code <= 97: self.foreground = "other"
                            n += 1
                    i += len(match.group(0))
                    continue
                i += 2
                continue
            if c == "\r": self.x = 0
            elif c == "\n": self.y = min(self.height - 1, self.y + 1)
            elif ord(c) >= 32 and c != "\x7f":
                w = 0 if unicodedata.combining(c) else (2 if unicodedata.east_asian_width(c) in "WF" else 1)
                if w and self.y < self.height and self.x < self.width:
                    self.cells[self.y][self.x] = c
                    self.colors[self.y][self.x] = self.foreground
                    if w == 2 and self.x + 1 < self.width: self.cells[self.y][self.x + 1] = " "
                    self.x += w
            i += 1
        self.pending = s[i:]

    def lines(self):
        return ["".join(row) for row in self.cells]


class Picker:
    def __init__(self, binary, root, width=120, height=40, args=None):
        self.root = root
        self.output = root / "nav.out"
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            fd = os.open(self.output, os.O_CREAT | os.O_TRUNC | os.O_WRONLY, 0o600)
            os.dup2(fd, 1)
            os.close(fd)
            os.environ.pop("NO_COLOR", None)
            os.environ.update(HOME=str(root), XDG_CONFIG_HOME=str(root / "config"),
                              XDG_STATE_HOME=str(root / "state"), CODEX_HOME=str(root / "codex"),
                              PATH=str(root / "bin") + ":" + os.environ["PATH"], TERM="xterm")
            os.execv(binary, [binary, *(args if args is not None else ["--shell-select"])])
        self.screen = Screen(width, height)
        self.raw = b""
        self.query_buffer = b""
        self.operations = []
        self.resize(width, height)

    def resize(self, width, height):
        if getattr(self, "size_set", False) and (width, height) == (self.screen.width, self.screen.height):
            return
        if getattr(self, "size_set", False):
            # Finish consuming the previous frame before invalidating our
            # simulated screen; event-driven rendering has no idle redraw.
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline:
                ready, _, _ = select.select([self.master], [], [], 0.03)
                if not ready:
                    break
                self.pump()
        self.screen.resize(width, height)
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        if getattr(self, "size_set", False):
            # Some PTY runners do not assign a foreground process group.
            os.killpg(self.pid, signal.SIGWINCH)
        self.size_set = True
        self.operations.append(f"resize({width},{height})")

    def pump(self):
        ready, _, _ = select.select([self.master], [], [], 0.02)
        if ready:
            data = os.read(self.master, 65536)
            self.raw = (self.raw + data)[-4000:]
            self.query_buffer += data
            while b"\x1b[6n" in self.query_buffer:
                self.query_buffer = self.query_buffer.replace(b"\x1b[6n", b"", 1)
                os.write(self.master, b"\x1b[1;1R")
            self.query_buffer = self.query_buffer[-8:]
            self.screen.feed(data)

    def wait_selected(self, number, timeout=3):
        wanted = f"Session {number:03}"
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.pump()
            rows = self.screen.lines()
            if any(">" in row and wanted in row and self.screen.colors[y][row.index(">")]=="red"
                   for y, row in enumerate(rows)):
                return
        raise AssertionError(f"expected highlighted {wanted}, screen={rows!r}, output={self.raw[-500:]!r}")

    def key(self, label, data, number):
        self.operations.append(label)
        os.write(self.master, data)
        self.pump()
        self.wait_selected(number)

    def wait_text(self, value):
        end = time.monotonic() + 3
        while time.monotonic() < end:
            self.pump()
            if any(value in row for row in self.screen.lines()):
                return
        raise AssertionError(f"missing {value!r} on screen={self.screen.lines()!r}")

    def resources(self):
        try:
            proc = Path(f"/proc/{self.pid}")
            lines = (proc / "status").read_text().splitlines()
            fields = {line.split(":", 1)[0]: line.split(":", 1)[1].strip() for line in lines if ":" in line}
            threads = int(fields["Threads"].split()[0])
            assert threads == 1, f"selector has {threads} threads"
            return len(list((proc / "fd").iterdir())), int(fields["VmRSS"].split()[0])
        except (FileNotFoundError, PermissionError, KeyError):
            return None

    def close(self):
        try:
            os.write(self.master, b"\x03")
            end = time.monotonic() + 3
            while time.monotonic() < end:
                try:
                    self.pump()
                except OSError:
                    pass
                done, status = os.waitpid(self.pid, os.WNOHANG)
                if done:
                    assert os.waitstatus_to_exitcode(status) == 2
                    assert self.output.read_bytes() == b""
                    return
            raise TimeoutError("navigation picker did not exit")
        finally:
            try:
                os.killpg(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
            os.close(self.master)


def navigation(binary, root, seed, rounds=1):
    sessions = root / "codex" / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    bin_dir = root / "bin"
    bin_dir.mkdir(exist_ok=True)
    codex = bin_dir / "codex"
    codex.write_text("#!/bin/sh\nexit 0\n")
    codex.chmod(0o755)
    index = []
    for n in range(1, 101):
        ident = f"{n:08x}-1111-2222-3333-444444444444"
        rec = {"type":"session_meta", "payload":{"id":ident, "cwd":str(root), "timestamp":f"2026-01-01T00:{(100-n)//60:02}:{(100-n)%60:02}Z"}}
        (sessions / f"{n:03}.jsonl").write_text(json.dumps(rec) + "\n")
        index.append(json.dumps({"id":ident,"thread_name":f"Session {n:03}"}))
    (root / "codex" / "session_index.jsonl").write_text("\n".join(index) + "\n")
    picker = Picker(binary, root)
    try:
        picker.wait_selected(1)
        samples = [picker.resources()]
        current = 1
        for current in range(2, 101):
            picker.key("down", b"\x1b[B", current)
        samples.append(picker.resources())
        for current in range(99, 0, -1):
            picker.key("up", b"\x1b[A", current)
        samples.append(picker.resources())
        picker.key("page-down", b"\x1b[6~", 11)
        picker.key("page-up", b"\x1b[5~", 1)
        # Same seed always produces the same independent movement model.
        rng = random.Random(seed)
        current = 1
        project_view = False
        for round_no in range(1, rounds + 1):
            picker.operations.append(f"round {round_no}")
            for _ in range(24):
                op = rng.choice(("down", "up", "home", "end", "page-down", "page-up", "search", "tab", "resize"))
                if op == "search":
                    picker.operations.append("search 042 / confirm / clear")
                    os.write(picker.master, b"/042")
                    picker.wait_selected(42)
                    os.write(picker.master, b"\r")
                    picker.wait_text("Filtered browsing")
                    os.write(picker.master, b"\x1b")
                    current = 1
                    picker.wait_selected(current)
                elif op == "tab":
                    picker.operations.append("tab")
                    os.write(picker.master, b"\t")
                    project_view = not project_view
                    picker.wait_text("Projects" if project_view else "Recent")
                    current = 1
                    picker.wait_selected(current)
                elif op == "resize":
                    width, height = rng.choice(((160,40),(120,40),(100,24),(80,24),(55,15),(30,10)))
                    picker.resize(width, height)
                    picker.wait_selected(current)
                    picker.resize(120, 40)
                    picker.wait_selected(current)
                else:
                    current = {"down":min(100,current+1), "up":max(1,current-1), "home":1, "end":100,
                               "page-down":min(100,current+10), "page-up":max(1,current-10)}[op]
                    picker.key(op, {"down":b"\x1b[B", "up":b"\x1b[A", "home":b"\x1b[H", "end":b"\x1b[F",
                                    "page-down":b"\x1b[6~", "page-up":b"\x1b[5~"}[op], current)
        for width, height in ((160,40),(120,40),(100,24),(80,24),(55,15),(30,10)):
            picker.resize(width, height)
            picker.wait_selected(current)
            if current < 100:
                current += 1
                picker.key("down-after-resize", b"\x1b[B", current)
            current -= 1
            picker.key("up-after-resize", b"\x1b[A", current)
        samples.append(picker.resources())
        if all(sample is not None for sample in samples):
            assert samples[-1][0] <= samples[0][0] + 1, f"selector FD trend: {samples!r}"
            print(f"Selector /proc samples (FD, RSS KiB): {samples}")
        else:
            print("Selector /proc resources: SKIPPED")
    except Exception as exc:
        raise AssertionError(f"seed={seed} size={picker.screen.width}x{picker.screen.height} operations={picker.operations!r} screen={picker.screen.lines()!r} output={picker.raw[-500:]!r}: {exc}") from exc
    finally:
        picker.close()
