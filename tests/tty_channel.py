#!/usr/bin/env python3
"""Exercise real selectors and shell functions on a PTY with synthetic history."""
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import statistics
import struct
import sys
import tempfile
import termios
import time
from pathlib import Path

SESSION = "11111111-2222-3333-4444-555555555555"


def setup(binary, root):
    project = root / "项目 $quote's\nspace"
    project.mkdir(exist_ok=True)
    sessions = root / "codex" / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    for name, session_id, cwd, timestamp in [
        ("first", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", root, "2026-01-02T00:00:00Z"),
        ("second", SESSION, project, "2026-01-01T00:00:00Z"),
    ]:
        record = {"type": "session_meta", "payload": {"id": session_id, "cwd": str(cwd), "timestamp": timestamp}}
        (sessions / (name + ".jsonl")).write_text(json.dumps(record) + "\n")
    bin_dir = root / "bin"
    bin_dir.mkdir(exist_ok=True)
    codex = bin_dir / "codex"
    codex.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then exit 0; fi\nprintf "%s\\0%s\\0%s\\0%s\\0%s\\0" "$PWD" "$#" "$1" "$2" "$CZI_TEST_EXIT" >> "$CZI_TEST_LOG"\nif [ "$CZI_TEST_EXIT" = signal ]; then kill -INT $$; fi\nif [ "$CZI_TEST_EXIT" = waitsig ]; then exec python3 "$CZI_TEST_SIGNAL"; fi\nexit "$CZI_TEST_EXIT"\n')
    codex.chmod(0o755)
    (root / "czi_fake_signal.py").write_text('import os, signal\nfrom pathlib import Path\np=Path(os.environ["CZI_TEST_PID"])\ndef stop(s, f):\n Path(os.environ["CZI_TEST_GOT_SIGNAL"]).write_text(str(s))\n raise SystemExit(130)\nsignal.signal(signal.SIGINT, stop)\ntmp=p.with_suffix(".new")\ntmp.write_text(str(os.getpid()))\ntmp.replace(p)\nwhile True: signal.pause()\n')
    link = bin_dir / "czi"
    if not link.exists():
        link.symlink_to(binary)
    return project, bin_dir


def exercise(binary, root, *, shell=None, key=b"\x1b[B\r", exit_code=0, mode="", count=1, redirect_input=False, timeout_query=False):
    project, bin_dir = setup(binary, root)
    (root / "tmp").mkdir(exist_ok=True)
    log, baseline = (root / name for name in ("codex.log", "termios.json"))
    log.unlink(missing_ok=True)
    signal_pid, got_signal = root / "signal.pid", root / "signal.received"
    signal_pid.unlink(missing_ok=True)
    got_signal.unlink(missing_ok=True)
    result_read, result_write = os.pipe()
    pid, master = pty.fork()
    if pid == 0:
        baseline.write_text(json.dumps(termios.tcgetattr(0), default=lambda b: list(b)))
        os.close(result_read)
        os.dup2(result_write, 1)
        os.close(result_write)
        if redirect_input:
            fd = os.open(os.devnull, os.O_RDONLY)
            os.dup2(fd, 0)
            os.close(fd)
        for name in ("BASH_ENV", "ENV", "CDPATH"):
            os.environ.pop(name, None)
        os.environ.update(HOME=str(root), TMPDIR=str(root / "tmp"), XDG_CONFIG_HOME=str(root / "config"), CODEX_HOME=str(root / "codex"), XDG_STATE_HOME=str(root / "state"),
                          PATH=str(bin_dir) + ":" + os.environ["PATH"], TERM="xterm",
                          CZI_TEST_LOG=str(log), CZI_TEST_EXIT=str(exit_code), CZI_TEST_ROOT=str(root),
                          CZI_TEST_SIGNAL=str(root / "czi_fake_signal.py"), CZI_TEST_PID=str(signal_pid), CZI_TEST_GOT_SIGNAL=str(got_signal))
        if shell:
            os.chdir(project if mode == "." else root)
            if shell == "bash":
                init = 'eval "$(command czi init bash)"; '
                record = 's=$?; t=$(stty -g </dev/tty); printf "%s\\0%s\\0%s\\0" "$s" "$PWD" "$t"'
                if count > 1:
                    action = f'for ((i=0;i<{count};i++)); do builtin cd -- "$CZI_TEST_ROOT"; czi {mode}; {record}; done'
                    code = init + 't=$(stty -g </dev/tty); printf "%s\\0" "$t"; ' + action
                else:
                    code = init + "czi " + mode + '; s=$?; printf "%s\\0%s\\0" "$s" "$PWD"'
                args = [shell, "--noprofile", "--norc", "-c", code]
            else:
                init = 'command czi init fish | source; '
                record = 'set -l s $status; set -l t (stty -g </dev/tty); printf "%s\\0%s\\0%s\\0" "$s" "$PWD" "$t"'
                if count > 1:
                    action = f'for i in (seq {count}); builtin cd -- "$CZI_TEST_ROOT"; czi {mode}; {record}; end'
                    code = init + 'set -l t (stty -g </dev/tty); printf "%s\\0" "$t"; ' + action
                else:
                    code = init + "czi " + mode + '; set -l s $status; printf "%s\\0%s\\0" "$s" "$PWD"'
                args = [shell, "--no-config", "-c", code]
            os.execvp(shell, args)
        os.execv(binary, [binary, "--shell-select"])
    os.close(result_write)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    data = b""
    view = b""
    sent = queries = 0
    samples = []
    result_output = b""
    pipe_open = True
    status = None
    deadline = time.monotonic() + max(8, count * 2)
    sent_signal = False
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master] + ([result_read] if pipe_open else []), [], [], 0.1)
            if result_read in ready:
                chunk = os.read(result_read, 65536)
                result_output += chunk
                pipe_open = bool(chunk)
            if master in ready:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    chunk = b""
                data += chunk
                view += chunk
                while b"\x1b[6n" in view:
                    view = view.replace(b"\x1b[6n", b"", 1)
                    queries += 1
                    if not timeout_query:
                        os.write(master, b"\x1b[1;1R")
                if sent < count and b"Browsing" in view:
                    try:
                        proc = Path(f"/proc/{pid}")
                        status_lines = (proc / "status").read_text().splitlines()
                        rss = next(int(line.split()[1]) for line in status_lines if line.startswith("VmRSS:"))
                        fds = len(list((proc / "fd").iterdir()))
                        selector_seen = False
                        children = (proc / "task" / str(pid) / "children").read_text().split()
                        for child in children:
                            child_status = Path(f"/proc/{child}/status")
                            if child_status.exists():
                                lines = child_status.read_text().splitlines()
                                name = next((line.split()[1] for line in lines if line.startswith("Name:")), "")
                                if name == "czi":
                                    selector_seen = True
                                    threads = next(int(line.split()[1]) for line in lines if line.startswith("Threads:"))
                                    assert threads == 1, f"selector spawned {threads} threads"
                        samples.append((fds, rss, selector_seen))
                    except (FileNotFoundError, PermissionError, StopIteration, ProcessLookupError):
                        pass
                    os.write(master, key)
                    sent += 1
                    view = b""
            if exit_code == "waitsig" and not sent_signal and signal_pid.exists():
                os.kill(int(signal_pid.read_text()), signal.SIGINT)
                sent_signal = True
            done, result = os.waitpid(pid, os.WNOHANG)
            if done:
                status = result
                break
        assert status is not None, f"picker timed out: round={sent + 1}/{count}, queries={queries}, terminal={data[-600:]!r}"
        if pipe_open:
            ready, _, _ = select.select([result_read], [], [], 1)
            assert ready, "result pipe still open after shell exited (possible child leak)"
            while True:
                chunk = os.read(result_read, 65536)
                if not chunk:
                    break
                result_output += chunk
                ready, _, _ = select.select([result_read], [], [], 1)
                assert ready, "result pipe still open after shell exited (possible child leak)"
        selected = key.endswith(b"\r")
        expected_exit = 1 if timeout_query else (0 if shell or selected else 2)
        assert os.waitstatus_to_exitcode(status) == expected_exit, (status, data[-600:])
        if timeout_query:
            assert queries >= 1 and sent == 0 and b"could not be read" in data
        elif mode != "last":
            assert sent == count and queries >= count, (sent, queries, data[-600:])
            assert b"could not be read" not in data
            assert data.count(b"\x1b[?25h") >= count, "cursor was not restored"
        result = result_output
        if timeout_query:
            assert result == b"", repr(result)
        elif shell:
            expected_cwd = project if selected or mode in ("last", ".") else root
            shell_status = (130 if exit_code in ("signal", "waitsig") else exit_code) if selected or mode == "last" else 0
            if count > 1:
                fields = result.split(b"\0")
                assert len(fields) == 2 + count * 3, repr(result)
                base = fields[0]
                for i in range(count):
                    assert fields[1+i*3:4+i*3] == [str(shell_status).encode(), os.fsencode(expected_cwd), base], (i, fields[1+i*3:4+i*3])
            else:
                assert result == str(shell_status).encode() + b"\0" + os.fsencode(expected_cwd) + b"\0", repr(result)
            if selected or mode == "last":
                assert log.read_bytes() == (os.fsencode(project) + b"\0" + b"2\0resume\0" + SESSION.encode() + b"\0" + str(exit_code).encode() + b"\0") * count
                if exit_code == "waitsig":
                    assert sent_signal and int(got_signal.read_text()) == int(signal.SIGINT)
            else:
                assert not log.exists()
        else:
            expected = os.fsencode(project) + b"\0" + SESSION.encode() + b"\0" if selected else b""
            assert result == expected, repr(result)
        assert json.dumps(termios.tcgetattr(master), default=lambda b: list(b)) == baseline.read_text(), "terminal mode not restored"
        assert not list((root / "tmp").iterdir()), "temporary result directory was not cleaned"
        if count >= 10 and len(samples) >= 10:
            first = statistics.median(sample[0] for sample in samples[:5])
            last = statistics.median(sample[0] for sample in samples[-5:])
            assert last <= first + 1, f"shell FDs grew from {first} to {last}"
        return samples
    except BaseException:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        raise
    finally:
        if status is None:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
        os.close(master)
        os.close(result_read)


if __name__ == "__main__":
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        exercise(binary, root, redirect_input=True)
        exercise(binary, root, timeout_query=True)
        for cancel in (b"\x1b", b"\x03"):
            exercise(binary, root, key=cancel)
        for shell in ("bash", "fish"):
            if not shutil.which(shell):
                raise RuntimeError(shell + " is required for the shell PTY tests")
            for exit_code in (0, 130, 23):
                exercise(binary, root, shell=shell, exit_code=exit_code)
            exercise(binary, root, shell=shell, exit_code="signal")
            for cancel in (b"\x1b", b"\x03"):
                exercise(binary, root, shell=shell, key=cancel)
            exercise(binary, root, shell=shell, mode="-p")
            exercise(binary, root, shell=shell, mode=".", key=b"\r")
            exercise(binary, root, shell=shell, mode="last", key=b"\r", exit_code=130)
            exercise(binary, root, shell=shell, key=b"\x1b", count=20)
    print("PTY selection, cancellation, Bash/Fish, exit status, last and 20 restarts passed")
