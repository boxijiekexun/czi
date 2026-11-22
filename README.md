# czi 0.3.4

`czi` is a Linux terminal picker for local Codex sessions. It scans Codex history read-only, lists recent sessions across projects, and runs native `codex resume <SESSION_ID>` in the recorded working directory.

## Build and install

```bash
cargo build --release
install -Dm755 target/release/czi ~/.local/bin/czi
```

Ensure `~/.local/bin` is in `PATH`. The binary works directly from Bash and Fish; no shell function is required.

## Commands

```text
czi             recent sessions across all projects
czi .           sessions whose recorded cwd is the current directory
czi -p          start in project directory view
czi last        resume the last session successfully started by czi
czi doctor      inspect Codex, CODEX_HOME, and discovered sessions
czi init bash   print Bash integration code
czi init fish   print Fish integration code
```

The picker keeps Recent sessions as the first view and supports a Projects ordering with `Tab`. Wide terminals show separate Session, Preview, Path, and Updated columns. Session labels prefer the user-set Codex `thread_name`, then `Session · <short-id>`; the first real user task is shown separately as Preview. Long fields are truncated by terminal display width, including Chinese and emoji.

```text
┌─ czi · Recent · Browsing ─────────────────────────────────────────────────┐
│ / Search                                                                   │
│ Session                 Preview              Path                Updated   │
│ > Fix API auth         repair token flow     ~/work/backend       11-21 ...│
│   Session · a1b2c3d4  —                    ~/work/czi            —         │
│ ↑↓/jk move  Enter resume  PgUp/PgDn scroll  Tab view  / search  Esc back    │
│ 1/16                                                                        │
└───────────────────────────────────────────────────────────────────────────┘
```

Keyboard behavior is explicit:

- **Browsing**: `↑`/`↓` or `j`/`k` move continuously; `PageUp`/`PageDown` move one visible window; `Home`/`End` jump to the boundaries; `/` edits a search; `Enter` resumes; `Tab` switches Recent/Projects; `Esc` exits; `Ctrl-C` exits.
- **Searching**: text and Backspace edit; `Enter` confirms the filter without resuming; `Esc` cancels and restores the previous selection; `Ctrl-C` exits.
- **Filtered browsing**: `↑`/`↓`, `j`/`k`, `Home`/`End`, `PageUp`/`PageDown`, and `Enter` operate on the filtered list; `Esc` clears the filter and returns to the full list.

The selector uses Ratatui's inline viewport on the controlling terminal (`/dev/tty`), so previous terminal output remains visible and terminal drawing does not enter stdout. The internal `--shell-select` interface emits the selected cwd and full session ID as NUL-delimited bytes on stdout and exits 0; cancellation emits nothing and exits 2, while errors exit 1. The shell integration uses its private result file instead. A controlling terminal is required; if none is available CZI reports an error rather than emitting a selection. The footer reports the selected position as `selected/total` and the visible capacity adapts to terminal height, including compact two-line entries on narrow terminals. The default standalone binary cannot change its parent Shell's directory.

## Shell integration

To change the calling Shell's directory before resuming a session, install the generated integration once:

```bash
eval "$(czi init bash)"
```

```fish
czi init fish | source
```

In integration mode, `czi`, `czi .`, `czi -p`, and `czi last` run the selector or lookup directly on the terminal, then change the current Shell to the recorded cwd before starting Codex. Cancelling leaves the current directory unchanged. Other commands such as `czi doctor` and `czi --version` are delegated to `command czi`. No configuration file is modified automatically.

After upgrading CZI, reload the generated function in existing Shells (`eval "$(czi init bash)"` in Bash or `czi init fish | source` in Fish). The integrated `czi last` path also changes the parent Shell directory before resuming.

For a PTY check of terminal/output separation and the Bash/Fish integrations, run `python3 tests/tty_channel.py target/release/czi` after building. The test uses synthetic session data and a fake Codex executable; it requires Python 3 and Fish. `cargo test` runs the same check when those programs are installed, and reports a skip when either is unavailable.

To run the focused search, SIGTERM and malformed-ID PTY regressions separately, use `python3 tests/stability_regressions.py target/release/czi`.

The repeatable stability runner uses isolated HOME, `CODEX_HOME`, `XDG_STATE_HOME`, and temporary files. It never opens real Codex history or runs the real Codex executable. Install Bash, Fish, Python 3 and `stty`, build a release binary, then run:

```sh
python3 tests/czi_stress.py --mode quick                  # 10 rounds, suitable for a PR
python3 tests/czi_stress.py --mode full                   # 50 rounds, local regression
python3 tests/czi_stress.py --mode stress                 # 200 rounds, before release
python3 tests/czi_stress.py --mode stress --rounds 200 --seed 2026
python3 tests/czi_stress.py --mode full --case navigation --seed 2026
```

Each round count applies to cancellation, selection and `last` loops in each long-lived shell. The full and stress modes add nonzero exits and the `.` and `-p` paths. The navigation scenario checks actual PTY screen rows for 100 synthetic sessions, seeded key sequences and six window sizes. `/proc` FD, RSS and thread observations are reported when available; RSS is diagnostic and does not impose a fixed memory limit. These PTY simulations do not replace a manual check with the user's real terminal and Codex session. The long runner is separate from ordinary `cargo test`.

## Data and privacy

By default the scanner reads `$CODEX_HOME` or `~/.codex`, including `sessions/**/*.jsonl` and the optional `session_index.jsonl`. It never writes to that directory and does not send data over the network. CZI state only stores the last successfully spawned session at `$XDG_STATE_HOME/czi/state.json` (or `~/.local/state/czi/state.json`).

Missing or malformed records are skipped conservatively. A missing recorded cwd prevents recovery rather than silently starting a new session.

## Compatibility

See [`docs/compatibility.md`](docs/compatibility.md). Codex's local files are implementation details, so upgrades may require parser changes.

## License

CZI is available under the [MIT License](LICENSE.txt).
