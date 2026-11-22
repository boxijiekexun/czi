#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use tempfile::tempdir;

#[test]
fn regression_concurrent_last_state_writes_are_atomic() {
    let root = tempdir().unwrap();
    let sessions = root.path().join("codex/sessions");
    let bin = root.path().join("bin");
    let logs = root.path().join("logs");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&logs).unwrap();
    let codex = bin.join("codex");
    fs::write(
        &codex,
        "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$PWD\" \"$1\" \"$2\" > \"$CZI_TEST_LOGS/$2\"\n",
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var_os("PATH")
            .unwrap_or_default()
            .to_string_lossy()
    );
    let ids: Vec<_> = (0..50)
        .map(|n| format!("{n:08x}-2222-3333-4444-555555555555"))
        .collect();
    for id in &ids {
        fs::write(
            sessions.join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":{id:?},\"cwd\":{}}}}}\n",
                serde_json::to_string(root.path()).unwrap()
            ),
        )
        .unwrap();
    }
    for count in [2, 8, 24, 50] {
        let state = root.path().join(format!("state-{count}"));
        let mut children: Vec<_> = ids[..count]
            .iter()
            .map(|id| {
                Command::new(env!("CARGO_BIN_EXE_czi"))
                    .args(["--shell-resume", id])
                    .current_dir(root.path())
                    .env("CODEX_HOME", root.path().join("codex"))
                    .env("XDG_STATE_HOME", &state)
                    .env("CZI_TEST_LOGS", &logs)
                    .env("PATH", &path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let state_file = state.join("czi/state.json");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Ok(bytes) = fs::read(&state_file) {
                let live: serde_json::Value =
                    serde_json::from_slice(&bytes).expect("partially written state");
                assert!(ids[..count].iter().any(|id| live["session_id"] == *id));
                assert_eq!(live["cwd"], root.path().to_string_lossy().as_ref());
            }
            let mut all_done = true;
            for child in &mut children {
                all_done &= child.try_wait().unwrap().is_some();
            }
            if all_done {
                break;
            }
            if std::time::Instant::now() >= deadline {
                for child in &mut children {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                panic!("concurrent launches timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for child in children.drain(..) {
            let result = child.wait_with_output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
        assert!(ids[..count].iter().any(|id| saved["session_id"] == *id));
        assert_eq!(saved["provider"], "codex");
        assert_eq!(saved["cwd"], root.path().to_string_lossy().as_ref());
        assert!(
            chrono::DateTime::parse_from_rfc3339(saved["started_at"].as_str().unwrap()).is_ok()
        );
        assert_eq!(
            fs::metadata(state_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(state.join("czi")).unwrap().count(), 1);
        for id in &ids[..count] {
            assert_eq!(
                fs::read_to_string(logs.join(id)).unwrap(),
                format!("{}\nresume\n{id}\n", root.path().display())
            );
        }
    }
}

#[test]
fn duplicate_session_last_uses_saved_cwd_and_invalid_ids_never_launch() {
    let root = tempdir().unwrap();
    let sessions = root.path().join("codex/sessions");
    let a = root.path().join("a");
    let b = root.path().join("b");
    let bin = root.path().join("bin");
    let state = root.path().join("state");
    for dir in [&sessions, &a, &b, &bin, &state.join("czi")] {
        fs::create_dir_all(dir).unwrap();
    }
    let id = "11111111-2222-3333-4444-555555555555";
    for (file, cwd) in [("z", &b), ("a", &a)] {
        fs::write(
            sessions.join(format!("{file}.jsonl")),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":{id:?},\"cwd\":{}}}}}\n",
                serde_json::to_string(cwd).unwrap()
            ),
        )
        .unwrap();
    }
    fs::write(
        sessions.join("invalid.jsonl"),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"abcdef汉🙂\",\"cwd\":{}}}}}\n",
            serde_json::to_string(&b).unwrap()
        ),
    )
    .unwrap();
    fs::write(state.join("czi/state.json"), format!("{{\"provider\":\"codex\",\"session_id\":{id:?},\"cwd\":{},\"started_at\":\"2026-01-01T00:00:00Z\"}}", serde_json::to_string(&b).unwrap())).unwrap();
    let log = root.path().join("invoked");
    let codex = bin.join("codex");
    fs::write(&codex, format!("#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nprintf '%s\\n%s\\n%s\\n' \"$PWD\" \"$1\" \"$2\" > {}\n", shell_quote(&log))).unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var_os("PATH")
            .unwrap_or_default()
            .to_string_lossy()
    );
    let make = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_czi"));
        cmd.env("CODEX_HOME", root.path().join("codex"))
            .env("XDG_STATE_HOME", &state)
            .env("PATH", &path);
        cmd
    };
    let result = make().arg("--shell-last").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut expected = b.as_os_str().as_encoded_bytes().to_vec();
    expected.push(0);
    expected.extend(id.as_bytes());
    expected.push(0);
    assert_eq!(result.stdout, expected);
    let result = make()
        .args(["--shell-resume", id])
        .current_dir(&b)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!("{}\nresume\n{id}\n", b.display())
    );
    fs::remove_file(&log).unwrap();
    let metadata = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":{id:?},\"cwd\":{},\"timestamp\":\"2026-01-01T00:00:00Z\"}}}}\n", serde_json::to_string(&b).unwrap());
    fs::write(sessions.join("z.jsonl"), &metadata).unwrap();
    fs::write(sessions.join("conflict.jsonl"), format!("{metadata}{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"conflicting task\"}}]}}}}\n")).unwrap();
    for args in [vec!["--shell-last"], vec!["--shell-resume", id]] {
        let result = make().args(args).current_dir(&b).output().unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains("ambiguous"));
        assert!(!log.exists());
    }
    fs::remove_file(sessions.join("conflict.jsonl")).unwrap();
    let result = make()
        .args(["--shell-resume", "abcdef汉🙂"])
        .current_dir(&b)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("valid native UUID"));
    assert!(!log.exists());
    let blocked = root.path().join("blocked");
    fs::write(&blocked, "not a directory").unwrap();
    let result = make()
        .args(["--shell-resume", id])
        .current_dir(&b)
        .env("XDG_STATE_HOME", &blocked)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("codex started, but could not save czi last state"));
    assert!(log.exists());
}

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
fn pty_search_signals_and_invalid_ids() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("PTY regression test skipped: python3 is required");
        return;
    }
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/stability_regressions.py"
        ))
        .arg(env!("CARGO_BIN_EXE_czi"))
        .output()
        .expect("could not launch PTY regressions");
    assert!(
        output.status.success(),
        "{}{}",
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
    let codex_home = root.path().join("codex-home");
    fs::create_dir_all(codex_home.join("sessions")).unwrap();
    let session_id = "11111111-2222-3333-4444-555555555555";
    fs::write(
        codex_home.join("sessions/session.jsonl"),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":{session_id:?},\"cwd\":{}}}}}\n",
            serde_json::to_string(&cwd).unwrap()
        ),
    )
    .unwrap();
    fs::create_dir_all(state_dir.join("czi")).unwrap();
    fs::write(
        state_dir.join("czi/state.json"),
        format!(
            "{{\"provider\":\"codex\",\"session_id\":{session_id:?},\"cwd\":{},\"started_at\":\"2026-01-01T00:00:00Z\"}}",
            serde_json::to_string(Path::new(&cwd)).unwrap()
        ),
    )
    .unwrap();

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin.display(), old_path.to_string_lossy());
    let status = Command::new(env!("CARGO_BIN_EXE_czi"))
        .arg("last")
        .env("XDG_STATE_HOME", &state_dir)
        .env("CODEX_HOME", &codex_home)
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
    assert_eq!(lines[1], format!("resume {session_id}"));
}

#[test]
fn indexed_session_resumes_with_full_native_id_after_empty_rename() {
    let root = tempdir().unwrap();
    let project = root.path().join("项目 with spaces");
    let codex_home = root.path().join("codex");
    let sessions = codex_home.join("sessions");
    let bin = root.path().join("bin");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let id = "11111111-2222-3333-4444-555555555555";
    fs::write(
        sessions.join("session.jsonl"),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":{id:?},\"cwd\":{}}}}}\n",
            serde_json::to_string(&project).unwrap()
        ),
    )
    .unwrap();
    fs::write(codex_home.join("session_index.jsonl"), format!("{{\"id\":{id:?},\"thread_name\":\"中文名称\",\"updated_at\":\"2026-01-01T00:00:00Z\"}}\n{{\"id\":{id:?},\"thread_name\":\"\",\"updated_at\":\"2026-01-02T00:00:00Z\"}}\n")).unwrap();
    let log = root.path().join("codex.log");
    let codex = bin.join("codex");
    fs::write(
        &codex,
        format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n' \"$PWD\" \"$1\" \"$2\" > {}\n",
            shell_quote(&log)
        ),
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var_os("PATH")
            .unwrap_or_default()
            .to_string_lossy()
    );
    let status = Command::new(env!("CARGO_BIN_EXE_czi"))
        .args(["--shell-resume", id])
        .current_dir(&project)
        .env("CODEX_HOME", &codex_home)
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("PATH", path)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(log).unwrap(),
        format!("{}\nresume\n{id}\n", project.display())
    );
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
