#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use tempfile::tempdir;

#[test]
fn real_pty_keeps_terminal_and_selection_output_separate() {
    if Command::new("python3").arg("--version").output().is_err()
        || Command::new("fish").arg("--version").output().is_err()
    {
        eprintln!("PTY test skipped: python3 and fish are required");
        return;
    }
    let output = Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tty_channel.py"))
        .arg(env!("CARGO_BIN_EXE_czi"))
        .output()
        .expect("could not launch PTY test");
    assert!(
        output.status.success(),
        "PTY test failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn last_invokes_fake_codex_with_session_id_and_recorded_cwd() {
    let root = tempdir().unwrap();
    let cwd = root.path().join("project with spaces");
    fs::create_dir_all(&cwd).unwrap();
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = root.path().join("invocation.log");
    let codex = bin.join("codex");
    fs::write(
        &codex,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" > {}\nprintf '%s %s\\n' \"$1\" \"$2\" >> {}\n",
            shell_quote(&log),
            shell_quote(&log)
        ),
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();

    let state_dir = root.path().join("state");
    fs::create_dir_all(state_dir.join("czi")).unwrap();
    fs::write(
        state_dir.join("czi/state.json"),
        format!(
            "{{\"provider\":\"codex\",\"session_id\":\"session with spaces\",\"cwd\":{},\"started_at\":\"2026-01-01T00:00:00Z\"}}",
            serde_json::to_string(Path::new(&cwd)).unwrap()
        ),
    )
    .unwrap();

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin.display(), old_path.to_string_lossy());
    let status = Command::new(env!("CARGO_BIN_EXE_czi"))
        .arg("last")
        .env("XDG_STATE_HOME", &state_dir)
        .env("PATH", path)
        .status()
        .unwrap();
    assert!(status.success());

    let lines: Vec<_> = fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines[0], cwd.to_string_lossy());
    assert_eq!(lines[1], "resume session with spaces");
}

#[test]
fn bash_and_fish_shell_integrations_cd_before_real_shell_resume() {
    let root = tempdir().unwrap();
    let cwd_a = root.path().join("project A");
    let cwd_b = root.path().join("项目 B with $special's\nand newline");
    let bin = root.path().join("bin");
    let codex_home = root.path().join("codex-home");
    fs::create_dir_all(&cwd_a).unwrap();
    fs::create_dir_all(&cwd_b).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let sessions = codex_home.join("sessions/2026/01");
    fs::create_dir_all(&sessions).unwrap();
    let session_id = "11111111-2222-3333-4444-555555555555";
    fs::write(
        sessions.join("session.jsonl"),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":{},\"cwd\":{}}}}}\n",
            serde_json::to_string(session_id).unwrap(),
            serde_json::to_string(&cwd_b).unwrap()
        ),
    )
    .unwrap();
    let log = root.path().join("codex.log");
    let codex = bin.join("codex");
    fs::write(
        &codex,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" > {}\nprintf '%s %s\\n' \"$1\" \"$2\" >> {}\nexit \"${{CZI_FAKE_EXIT:-0}}\"\n",
            shell_quote(&log),
            shell_quote(&log)
        ),
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();

    let real_czi = env!("CARGO_BIN_EXE_czi");
    let wrapper = bin.join("czi");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--shell-select\" ]; then\n  shift\n  result=\n  while [ \"$#\" -gt 0 ]; do\n    if [ \"$1\" = \"--result-file\" ]; then result=$2; shift 2; else shift; fi\n  done\n  if [ \"${{CZI_CANCEL:-0}}\" = 1 ]; then exit 2; fi\n  if [ \"${{CZI_INVALID:-0}}\" = 1 ]; then rmdir -- {}; fi\n  printf '%s\\0%s\\0' {} {} > \"$result\"\n  exit 0\nfi\nexec {} \"$@\"\n",
            shell_quote(&cwd_b),
            shell_quote(&cwd_b),
            shell_quote_text(session_id),
            shell_quote(Path::new(real_czi))
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin.display(), old_path.to_string_lossy());

    let state_dir = root.path().join("xdg-state");
    for exit_code in [0, 130, 23] {
        fs::write(&log, "").unwrap();
        let bash = Command::new("bash")
            .current_dir(&cwd_a)
            .arg("-c")
            .arg(format!(
                "eval \"$({} init bash)\"; czi; pwd",
                shell_quote(Path::new(real_czi))
            ))
            .env("PATH", &path)
            .env("CODEX_HOME", &codex_home)
            .env("XDG_STATE_HOME", &state_dir)
            .env("CZI_FAKE_EXIT", exit_code.to_string())
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&bash.stdout).contains(cwd_b.to_string_lossy().as_ref()),
            "bash exit {exit_code}, stdout: {} stderr: {}",
            String::from_utf8_lossy(&bash.stdout),
            String::from_utf8_lossy(&bash.stderr)
        );
        let bash_log = fs::read_to_string(&log).unwrap();
        assert!(bash_log.contains(&format!("{}\nresume {}", cwd_b.display(), session_id)));

        if Command::new("fish").arg("--version").output().is_ok() {
            fs::write(&log, "").unwrap();
            let fish = Command::new("fish")
                .current_dir(&cwd_a)
                .args(["--no-config", "-c"])
                .arg(format!(
                    "source ({} init fish | psub); czi; pwd",
                    shell_quote(Path::new(real_czi))
                ))
                .env("PATH", &path)
                .env("CODEX_HOME", &codex_home)
                .env("XDG_STATE_HOME", &state_dir)
                .env("CZI_FAKE_EXIT", exit_code.to_string())
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&fish.stdout).contains(cwd_b.to_string_lossy().as_ref()),
                "fish exit {exit_code}, stdout: {} stderr: {}",
                String::from_utf8_lossy(&fish.stdout),
                String::from_utf8_lossy(&fish.stderr)
            );
            let fish_log = fs::read_to_string(&log).unwrap();
            assert!(fish_log.contains(&format!("{}\nresume {}", cwd_b.display(), session_id)));
        }
    }

    let current_bash = Command::new("bash")
        .current_dir(&cwd_a)
        .arg("-c")
        .arg(format!(
            "eval \"$({} init bash)\"; czi .; pwd",
            shell_quote(Path::new(real_czi))
        ))
        .env("PATH", &path)
        .env("CZI_FAKE_EXIT", "0")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&current_bash.stdout).contains(cwd_b.to_string_lossy().as_ref())
    );
    if Command::new("fish").arg("--version").output().is_ok() {
        let projects_fish = Command::new("fish")
            .current_dir(&cwd_a)
            .args(["--no-config", "-c"])
            .arg(format!(
                "source ({} init fish | psub); czi -p; pwd",
                shell_quote(Path::new(real_czi))
            ))
            .env("PATH", &path)
            .env("CZI_FAKE_EXIT", "0")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&projects_fish.stdout)
            .contains(cwd_b.to_string_lossy().as_ref()));
    }

    fs::write(&log, "").unwrap();
    let invalid_bash = Command::new("bash")
        .current_dir(&cwd_a)
        .arg("-c")
        .arg(format!(
            "eval \"$({} init bash)\"; czi; pwd",
            shell_quote(Path::new(real_czi))
        ))
        .env("PATH", &path)
        .env("CZI_INVALID", "1")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&invalid_bash.stdout)
        .lines()
        .any(|line| line == cwd_a.to_string_lossy()));
    assert!(fs::read_to_string(&log).unwrap().is_empty());
    fs::create_dir_all(&cwd_b).unwrap();

    let cancel_bash = Command::new("bash")
        .current_dir(&cwd_a)
        .arg("-c")
        .arg(format!(
            "eval \"$({} init bash)\"; czi; pwd",
            shell_quote(Path::new(real_czi))
        ))
        .env("PATH", &path)
        .env("CZI_CANCEL", "1")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&cancel_bash.stdout)
        .lines()
        .any(|line| line == cwd_a.to_string_lossy()));

    let bash_last = Command::new("bash")
        .current_dir(&cwd_a)
        .arg("-c")
        .arg(format!(
            "eval \"$({} init bash)\"; czi last; pwd",
            shell_quote(Path::new(real_czi))
        ))
        .env("PATH", &path)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_STATE_HOME", &state_dir)
        .env("CZI_FAKE_EXIT", "130")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&bash_last.stdout).contains(cwd_b.to_string_lossy().as_ref()));

    if Command::new("fish").arg("--version").output().is_ok() {
        let cancel_fish = Command::new("fish")
            .current_dir(&cwd_a)
            .args(["--no-config", "-c"])
            .arg(format!(
                "source ({} init fish | psub); czi; pwd",
                shell_quote(Path::new(real_czi))
            ))
            .env("PATH", &path)
            .env("CZI_CANCEL", "1")
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&cancel_fish.stdout)
            .lines()
            .any(|line| line == cwd_a.to_string_lossy()));

        fs::write(&log, "").unwrap();
        let fish_last = Command::new("fish")
            .current_dir(&cwd_a)
            .args(["--no-config", "-c"])
            .arg(format!(
                "source ({} init fish | psub); czi last; pwd",
                shell_quote(Path::new(real_czi))
            ))
            .env("PATH", &path)
            .env("CODEX_HOME", &codex_home)
            .env("XDG_STATE_HOME", &state_dir)
            .env("CZI_FAKE_EXIT", "130")
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&fish_last.stdout).contains(cwd_b.to_string_lossy().as_ref())
        );
        assert!(fs::read_to_string(&log).unwrap().contains(&format!(
            "{}\nresume {}",
            cwd_b.display(),
            session_id
        )));
    }
}

fn shell_quote(path: &Path) -> String {
    shell_quote_text(&path.to_string_lossy())
}

fn shell_quote_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
