use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::atomic::{AtomicBool, Ordering},
    time::UNIX_EPOCH,
};

use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use ratatui::{
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell as BufferCell,
    layout::{Constraint, Direction, Layout, Position, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame, Terminal, TerminalOptions, Viewport,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Parser, Debug)]
#[command(name = "czi", version, about = "Find and resume local Codex sessions")]
struct Cli {
    #[command(subcommand)]
    command: Option<CommandKind>,
    #[arg(
        short = 'p',
        long = "projects",
        help = "Browse sessions grouped by project"
    )]
    projects: bool,
    #[arg(long = "shell-select", hide = true)]
    shell_select: bool,
    #[arg(long = "shell-last", hide = true)]
    shell_last: bool,
    #[arg(long = "shell-current", hide = true)]
    shell_current: bool,
    #[arg(long = "shell-projects", hide = true)]
    shell_projects: bool,
    #[arg(long = "result-file", value_name = "PATH", hide = true)]
    result_file: Option<PathBuf>,
    #[arg(long = "shell-resume", value_name = "SESSION_ID", hide = true)]
    shell_resume: Option<String>,
}

#[derive(Subcommand, Debug)]
enum CommandKind {
    #[command(name = "last")]
    Last,
    #[command(name = "doctor")]
    Doctor,
    #[command(name = ".")]
    Current,
    #[command(name = "init")]
    Init { shell: ShellKind },
}

#[derive(Clone, Debug, ValueEnum)]
enum ShellKind {
    Bash,
    Fish,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct SessionEntry {
    provider: String,
    session_id: String,
    cwd: PathBuf,
    thread_name: Option<String>,
    preview: Option<String>,
    last_active: Option<DateTime<Utc>>,
    source_file: PathBuf,
    resumable: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LastState {
    provider: String,
    session_id: String,
    cwd: PathBuf,
    started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default)]
struct IndexRecord {
    title: Option<String>,
    title_updated_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    title_order: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiMode {
    Browsing,
    Searching,
    FilteredBrowsing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiAction {
    BeginSearch,
    ConfirmSearch,
    CancelSearch,
    ClearFilter,
}

fn transition_mode(mode: UiMode, action: UiAction) -> UiMode {
    match (mode, action) {
        (UiMode::Browsing, UiAction::BeginSearch)
        | (UiMode::FilteredBrowsing, UiAction::BeginSearch) => UiMode::Searching,
        (UiMode::Searching, UiAction::ConfirmSearch) => UiMode::FilteredBrowsing,
        (UiMode::Searching, UiAction::CancelSearch)
        | (UiMode::FilteredBrowsing, UiAction::ClearFilter) => UiMode::Browsing,
        _ => mode,
    }
}

fn codex_home() -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn parse_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value.and_then(Value::as_str).and_then(|s| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    })
}

fn find_string(v: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| v.get(*name).and_then(Value::as_str).map(str::to_owned))
}

fn clean_text(value: &str) -> String {
    let mut cleaned = String::new();
    let mut chars = value.chars().peekable();
    let mut pending_space = false;
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for code in chars.by_ref() {
                    if ('@'..='~').contains(&code) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !cleaned.is_empty();
            continue;
        }
        if pending_space {
            cleaned.push(' ');
            pending_space = false;
        }
        cleaned.push(ch);
    }
    cleaned.trim().to_string()
}

fn strip_injected_environment(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<environment_context>") {
        result.push_str(&remaining[..start]);
        let after_start = &remaining[start + "<environment_context>".len()..];
        let Some(end) = after_start.find("</environment_context>") else {
            result.push_str(&remaining[start..]);
            return result;
        };
        let block = &after_start[..end];
        let looks_injected = ["<cwd>", "<shell>", "<current_date>"]
            .iter()
            .all(|field| block.contains(field));
        if !looks_injected {
            result.push_str(&remaining[start..]);
            return result;
        }
        remaining = &after_start[end + "</environment_context>".len()..];
    }
    result.push_str(remaining);
    result
}

fn user_task_from_record(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = value.get("payload")?.as_object()?;
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let content = payload.get("content")?.as_array()?;
    let mut parts = Vec::new();
    for item in content {
        let object = item.as_object()?;
        if object.get("type").and_then(Value::as_str) != Some("input_text") {
            continue;
        }
        if let Some(text) = object.get("text").and_then(Value::as_str) {
            let text = strip_injected_environment(text);
            let cleaned = clean_text(&text);
            if !cleaned.is_empty() {
                parts.push(cleaned);
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn parse_session(
    path: &Path,
    titles: &std::collections::HashMap<String, IndexRecord>,
) -> Option<SessionEntry> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut id = None;
    let mut cwd = None;
    let mut created = None;
    let mut activity = None;
    let mut first_task = None;
    let mut line_number = 0usize;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        line_number += 1;
        let value: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let payload = value.get("payload").unwrap_or(&value);
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "session_meta" {
            id = find_string(payload, &["id", "session_id"]).or(id);
            cwd = find_string(payload, &["cwd", "working_directory"]).or(cwd);
            created = created.max(
                parse_time(payload.get("timestamp")).or_else(|| parse_time(value.get("timestamp"))),
            );
            activity = activity.max(parse_time(payload.get("updated_at")));
        }
        // In observed Codex 0.171 rollouts these events mark a turn or an
        // item being completed. Ignore bookkeeping and metadata timestamps.
        if kind == "event_msg"
            && matches!(
                payload.get("type").and_then(Value::as_str),
                Some(
                    "task_started"
                        | "task_complete"
                        | "turn_aborted"
                        | "user_message"
                        | "agent_message"
                        | "item_completed"
                )
            )
        {
            activity = activity.max(parse_time(value.get("timestamp")));
        }
        if first_task.is_none() && line_number <= 64 {
            first_task = user_task_from_record(&value);
        }
    }
    let session_id = id?;
    let cwd = PathBuf::from(cwd?);
    if session_id.is_empty() {
        return None;
    }
    let modified = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, d.subsec_nanos()));
    let index = titles.get(&session_id).cloned().unwrap_or_default();
    let thread_name = index.title.filter(|title| !title.is_empty());
    let preview = first_task
        .map(|text| clean_text(&text))
        .filter(|text| !text.is_empty());
    let resumable = valid_session_id(&session_id);
    Some(SessionEntry {
        provider: "codex".into(),
        session_id,
        cwd,
        thread_name,
        preview,
        last_active: activity.max(index.updated_at).or(created).or(modified),
        source_file: path.to_path_buf(),
        resumable,
    })
}

fn read_titles(home: &Path) -> std::collections::HashMap<String, IndexRecord> {
    let mut out = std::collections::HashMap::new();
    let path = home.join("session_index.jsonl");
    let Ok(file) = File::open(path) else {
        return out;
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut order = 0usize;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let current_order = order;
        order += 1;
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let Some(id) = find_string(&v, &["id", "session_id"]) else {
            continue;
        };
        let title = find_string(&v, &["thread_name"])
            .map(|title| clean_text(&title))
            .filter(|title| !title.is_empty());
        let updated = parse_time(v.get("updated_at")).max(parse_time(v.get("last_active_at")));
        let record: &mut IndexRecord = out.entry(id).or_default();
        record.updated_at = record.updated_at.max(updated);
        if let Some(title) = title {
            let replace = record.title.is_none()
                || newer_name(
                    record.title_updated_at,
                    record.title_order,
                    updated,
                    current_order,
                );
            if replace {
                record.title = Some(title);
                record.title_updated_at = updated;
                record.title_order = current_order;
            }
        }
    }
    out
}

fn newer_name(
    previous: Option<DateTime<Utc>>,
    previous_order: usize,
    candidate: Option<DateTime<Utc>>,
    candidate_order: usize,
) -> bool {
    match (previous, candidate) {
        (Some(previous), Some(candidate)) if previous != candidate => candidate > previous,
        (None, Some(_)) => true,
        (Some(_), None) => false,
        _ => candidate_order >= previous_order,
    }
}

fn discover(home: &Path) -> Vec<SessionEntry> {
    let titles = read_titles(home);
    let root = home.join("sessions");
    let mut files = Vec::new();
    fn walk(path: &Path, files: &mut Vec<PathBuf>, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, files, depth + 1);
            } else if p.extension().is_some_and(|e| e == "jsonl") {
                files.push(p);
            }
        }
    }
    walk(&root, &mut files, 0);
    let mut entries: Vec<_> = files
        .into_iter()
        .filter_map(|p| parse_session(&p, &titles))
        .collect();
    // Identical ID and canonical cwd represent one candidate. Prefer its
    // latest activity and use the path as a deterministic tie-breaker.
    entries.sort_by(|a, b| {
        a.session_id
            .cmp(&b.session_id)
            .then_with(|| normalize(&a.cwd).cmp(&normalize(&b.cwd)))
            .then_with(|| b.last_active.cmp(&a.last_active))
            .then_with(|| a.source_file.cmp(&b.source_file))
    });
    let mut result: Vec<SessionEntry> = Vec::new();
    for entry in entries {
        if let Some(previous) = result.last_mut() {
            if previous.session_id == entry.session_id
                && normalize(&previous.cwd) == normalize(&entry.cwd)
            {
                // Equal-time records with different contents cannot be
                // distinguished safely using the native ID and cwd.
                if previous.last_active == entry.last_active
                    && !same_history(&previous.source_file, &entry.source_file).unwrap_or(false)
                {
                    previous.resumable = false;
                }
                continue;
            }
        }
        result.push(entry);
    }
    result.sort_by(|a, b| {
        b.last_active
            .cmp(&a.last_active)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    result
}

fn same_history(a: &Path, b: &Path) -> io::Result<bool> {
    let mut left = File::open(a)?;
    let mut right = File::open(b)?;
    let length = left.metadata()?.len();
    let mut remaining = length;
    if length != right.metadata()?.len() {
        return Ok(false);
    }
    let mut left_buf = [0u8; 8192];
    let mut right_buf = [0u8; 8192];
    while remaining > 0 {
        let count = remaining.min(left_buf.len() as u64) as usize;
        left.read_exact(&mut left_buf[..count])?;
        right.read_exact(&mut right_buf[..count])?;
        if left_buf[..count] != right_buf[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    Ok(left.metadata()?.len() == length && right.metadata()?.len() == length)
}

fn normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn state_path() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".local/state"))
                .unwrap_or_else(|| PathBuf::from(".state"))
        })
        .join("czi/state.json")
}

fn load_last() -> Option<LastState> {
    serde_json::from_reader(File::open(state_path()).ok()?).ok()
}

fn save_last(session: &SessionEntry) -> io::Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let state = LastState {
        provider: session.provider.clone(),
        session_id: session.session_id.clone(),
        cwd: session.cwd.clone(),
        started_at: Utc::now(),
    };
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent")
        })?)?;
    serde_json::to_writer_pretty(&mut file, &state)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn ensure_resumable(session: &SessionEntry) -> Result<(), String> {
    if !valid_session_id(&session.session_id) {
        return Err("session ID is not a valid native UUID".into());
    }
    if !session.resumable {
        return Err(
            "conflicting session history records are ambiguous for this ID and directory".into(),
        );
    }
    Ok(())
}

fn launch(session: &SessionEntry) -> Result<ExitStatus, String> {
    ensure_resumable(session)?;
    if !session.cwd.is_dir() {
        return Err(format!(
            "session working directory does not exist: {}",
            session.cwd.display()
        ));
    }
    let mut command = Command::new("codex");
    command
        .arg("resume")
        .arg(&session.session_id)
        .current_dir(&session.cwd);
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to start codex: {e}"))?;
    if let Err(error) = save_last(session) {
        eprintln!("czi: codex started, but could not save czi last state: {error}");
    }
    let status = child
        .wait()
        .map_err(|e| format!("failed while waiting for codex: {e}"))?;
    Ok(status)
}

fn launch_checked(session: &SessionEntry) -> Result<(), String> {
    let status = launch(session)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("codex exited with status {status}"))
    }
}

fn display_time(t: Option<DateTime<Utc>>) -> String {
    t.map(|d| d.with_timezone(&Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".into())
}

fn truncate_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width <= 1 {
        return "…".chars().take(max_width).collect();
    }
    let mut result = String::new();
    let mut width = 0;
    for ch in value.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width + 1 > max_width {
            break;
        }
        result.push(ch);
        width += char_width;
    }
    result.push('…');
    result
}

fn shorten_path(path: &Path, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let display = if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        match path.strip_prefix(&home) {
            Ok(rest) => {
                if rest.as_os_str().is_empty() {
                    "~".to_string()
                } else {
                    format!("~/{}", rest.display())
                }
            }
            Err(_) => path.display().to_string(),
        }
    } else {
        path.display().to_string()
    };
    if UnicodeWidthStr::width(display.as_str()) <= max_width {
        display
    } else {
        let mut parts: Vec<String> = display.split('/').map(str::to_owned).collect();
        if parts.len() > 1 {
            let last_parent = parts.len().saturating_sub(1);
            for part in parts.iter_mut().take(last_parent).skip(1) {
                if let Some(first) = part.chars().next() {
                    *part = first.to_string();
                }
            }
            let compressed = parts.join("/");
            if UnicodeWidthStr::width(compressed.as_str()) <= max_width {
                return compressed;
            }
            let last = parts.last().map(String::as_str).unwrap_or("");
            let candidate = format!("…/{last}");
            return truncate_width(&candidate, max_width);
        }
        truncate_width(&display, max_width)
    }
}

fn short_id(sessions: &[SessionEntry], index: usize) -> String {
    let id = &sessions[index].session_id;
    let mut length = 8.min(id.chars().count());
    while length < id.chars().count()
        && sessions.iter().enumerate().any(|(other, session)| {
            other != index
                && session
                    .session_id
                    .starts_with(&id.chars().take(length).collect::<String>())
        })
    {
        length += 1;
    }
    clean_text(&id.chars().take(length).collect::<String>())
}

fn session_label(sessions: &[SessionEntry], index: usize) -> String {
    sessions[index]
        .thread_name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("Session · {}", short_id(sessions, index)))
}

fn matches(entry: &SessionEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    format!(
        "{} {} {} {}",
        entry.thread_name.as_deref().unwrap_or(""),
        entry.preview.as_deref().unwrap_or(""),
        entry.cwd.display(),
        entry.session_id
    )
    .to_lowercase()
    .contains(&q)
}

fn page_capacity(height: u16, item_height: u16) -> usize {
    let visible = height.saturating_sub(3) / item_height.max(1);
    usize::from(visible.max(1)).min(15)
}

fn clamp_selection(selected: usize, total: usize) -> usize {
    selected.min(total.saturating_sub(1))
}

fn keep_visible(selected: usize, scroll_offset: usize, capacity: usize, total: usize) -> usize {
    if total == 0 || capacity == 0 {
        return 0;
    }
    let max_offset = total.saturating_sub(capacity);
    let first_for_selection = selected.saturating_add(1).saturating_sub(capacity);
    scroll_offset
        .max(first_for_selection)
        .min(selected.min(max_offset))
}

fn move_selection(
    selected: usize,
    scroll_offset: usize,
    delta: isize,
    capacity: usize,
    total: usize,
) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    let next = if delta.is_negative() {
        selected.saturating_sub(delta.unsigned_abs())
    } else {
        selected
            .saturating_add(delta as usize)
            .min(total.saturating_sub(1))
    };
    let next = clamp_selection(next, total);
    (next, keep_visible(next, scroll_offset, capacity, total))
}

fn selected_index_by_id(
    sessions: &[SessionEntry],
    selection: Option<&(String, PathBuf)>,
) -> Option<usize> {
    selection.and_then(|(id, path)| {
        sessions
            .iter()
            .position(|session| &session.session_id == id && &session.source_file == path)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColumnLayout {
    Wide {
        session: u16,
        preview: u16,
        path: u16,
        updated: u16,
    },
    Standard {
        session: u16,
        path: u16,
        updated: u16,
    },
    Compact {
        session: u16,
        updated: u16,
    },
    Minimal {
        session: u16,
    },
}

fn column_layout(width: u16) -> ColumnLayout {
    let inner = usize::from(width.saturating_sub(2));
    if width >= 100 {
        let remaining = inner.saturating_sub(6 + 11);
        let session = (24 + usize::from(width.saturating_sub(100)) / 5).min(36);
        let preview = (26 + usize::from(width.saturating_sub(100)) / 3).min(40);
        let session = session.min(remaining.saturating_sub(2));
        let preview = preview.min(remaining.saturating_sub(session + 1));
        ColumnLayout::Wide {
            session: session as u16,
            preview: preview as u16,
            path: remaining.saturating_sub(session + preview) as u16,
            updated: 11,
        }
    } else if width >= 80 {
        let remaining = inner.saturating_sub(4 + 11);
        let session = remaining.min(24);
        ColumnLayout::Standard {
            session: session as u16,
            path: remaining.saturating_sub(session) as u16,
            updated: 11,
        }
    } else if width >= 40 {
        let updated = 11.min(inner.saturating_sub(3));
        ColumnLayout::Compact {
            session: inner.saturating_sub(updated + 2) as u16,
            updated: updated as u16,
        }
    } else {
        ColumnLayout::Minimal {
            session: width.saturating_sub(2).max(1),
        }
    }
}

fn compact_detail(session: &SessionEntry, width: usize) -> String {
    let path = shorten_path(&session.cwd, width);
    let Some(preview) = session.preview.as_deref() else {
        return path;
    };
    let path_width = UnicodeWidthStr::width(path.as_str());
    if path_width + 3 >= width {
        return path;
    }
    truncate_width(&format!("{path} · {preview}"), width)
}

struct UiRender<'a> {
    sessions: &'a [SessionEntry],
    filtered: &'a [SessionEntry],
    query: &'a str,
    mode: UiMode,
    project_view: bool,
    selected: usize,
    scroll_offset: usize,
}

fn session_cell(marker: &str, label: &str, width: usize, selected: bool) -> Cell<'static> {
    let marker_style = if selected {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };
    let label_style = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Cell::from(Line::from(vec![
        Span::styled(marker.to_string(), marker_style),
        Span::styled(truncate_width(label, width), label_style),
    ]))
}

fn draw_ui(frame: &mut Frame<'_>, state: UiRender<'_>) -> (usize, usize) {
    let UiRender {
        sessions,
        filtered,
        query,
        mode,
        project_view,
        selected,
        scroll_offset,
    } = state;
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(2),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let layout = column_layout(areas[1].width);
    let item_height = match layout {
        ColumnLayout::Compact { .. } => 2,
        _ => 1,
    };
    let capacity = page_capacity(areas[1].height, item_height);
    let start = keep_visible(selected, scroll_offset, capacity, filtered.len());
    let end = (start + capacity).min(filtered.len());
    let view_name = if project_view { "Projects" } else { "Recent" };
    let mode_name = match mode {
        UiMode::Browsing => "Browsing",
        UiMode::Searching => "Searching",
        UiMode::FilteredBrowsing => "Filtered browsing",
    };
    let search_line = match mode {
        UiMode::Browsing => "/ Search".to_string(),
        UiMode::Searching => format!("Search: {query}▏  Enter confirm · Esc cancel"),
        UiMode::FilteredBrowsing => format!("Filter: {query}  / edit · Esc clear"),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "czi  ·  {view_name}  ·  {mode_name}  ·  {} sessions",
                filtered.len()
            )),
            Line::from(search_line),
        ])
        .block(Block::default().borders(Borders::ALL)),
        areas[0],
    );

    let rows: Vec<Row> = filtered[start..end]
        .iter()
        .enumerate()
        .map(|(offset, session)| {
            let original = sessions
                .iter()
                .position(|candidate| {
                    candidate.source_file == session.source_file
                        && candidate.session_id == session.session_id
                })
                .unwrap_or(0);
            let label = session_label(sessions, original);
            let selected_style = if start + offset == selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let is_selected = start + offset == selected;
            let marker = if is_selected { "> " } else { "  " };
            let time = display_time(session.last_active);
            match layout {
                ColumnLayout::Wide {
                    session: sw,
                    preview: pw,
                    path: cw,
                    updated: _updated,
                } => Row::new([
                    session_cell(
                        marker,
                        &label,
                        usize::from(sw.saturating_sub(2)),
                        is_selected,
                    ),
                    Cell::from(truncate_width(
                        session.preview.as_deref().unwrap_or("—"),
                        usize::from(pw),
                    )),
                    Cell::from(shorten_path(&session.cwd, usize::from(cw))),
                    Cell::from(format!("{time:>11}")),
                ])
                .style(selected_style)
                .height(1),
                ColumnLayout::Standard {
                    session: sw,
                    path: cw,
                    updated: _updated,
                } => Row::new([
                    session_cell(
                        marker,
                        &label,
                        usize::from(sw.saturating_sub(2)),
                        is_selected,
                    ),
                    Cell::from(shorten_path(&session.cwd, usize::from(cw))),
                    Cell::from(format!("{time:>11}")),
                ])
                .style(selected_style)
                .height(1),
                ColumnLayout::Compact {
                    session: sw,
                    updated: _updated,
                } => Row::new([
                    Cell::from(vec![
                        Line::from(vec![
                            Span::styled(
                                marker.to_string(),
                                if is_selected {
                                    Style::default().fg(Color::Red)
                                } else {
                                    Style::default()
                                },
                            ),
                            Span::styled(
                                truncate_width(&label, usize::from(sw.saturating_sub(2))),
                                if is_selected {
                                    Style::default().add_modifier(Modifier::BOLD)
                                } else {
                                    Style::default()
                                },
                            ),
                        ]),
                        Line::from(compact_detail(session, usize::from(sw))),
                    ]),
                    Cell::from(vec![Line::from(format!("{time:>11}")), Line::from("")]),
                ])
                .style(selected_style)
                .height(2),
                ColumnLayout::Minimal { session: sw } => {
                    Row::new([session_cell(
                        marker,
                        &label,
                        usize::from(sw.saturating_sub(2)),
                        is_selected,
                    )])
                }
                .style(selected_style)
                .height(1),
            }
        })
        .collect();
    let table = match layout {
        ColumnLayout::Wide {
            session,
            preview,
            path,
            updated,
        } => Table::new(
            rows,
            [
                Constraint::Length(session),
                Constraint::Length(preview),
                Constraint::Length(path),
                Constraint::Length(updated),
            ],
        )
        .header(
            Row::new(["Session", "Preview", "Path", "Updated"]).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        ColumnLayout::Standard {
            session,
            path,
            updated,
        } => Table::new(
            rows,
            [
                Constraint::Length(session),
                Constraint::Length(path),
                Constraint::Length(updated),
            ],
        )
        .header(
            Row::new(["Session", "Path", "Updated"]).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        ColumnLayout::Compact { session, updated } => Table::new(
            rows,
            [Constraint::Length(session), Constraint::Length(updated)],
        )
        .header(
            Row::new(["Session", "Updated"]).style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        ColumnLayout::Minimal { session } => Table::new(rows, [Constraint::Length(session)])
            .header(
                Row::new(["Session"]).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ),
    };
    let table = table
        .column_spacing(2)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(if filtered.is_empty() {
                    "No matching sessions"
                } else {
                    "Sessions"
                }),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray));
    frame.render_widget(table, areas[1]);
    let footer = if mode == UiMode::Searching {
        "Type to search  Enter confirm  Esc cancel  Ctrl-C quit"
    } else if filtered.is_empty() {
        "No matching sessions  / search  Esc quit"
    } else {
        "↑↓/jk move  Enter resume  PgUp/PgDn scroll  Tab view  / search  Esc back/quit"
    };
    let position = if filtered.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}/{}", selected.saturating_add(1), filtered.len())
    };
    let page_width = 8;
    let footer_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(page_width)])
        .split(areas[2]);
    frame.render_widget(Paragraph::new(footer), footer_areas[0]);
    frame.render_widget(
        Paragraph::new(position).alignment(ratatui::layout::Alignment::Right),
        footer_areas[1],
    );
    (capacity, start)
}

fn inline_height(session_count: usize, terminal_height: u16, item_height: u16) -> u16 {
    let rows = session_count.min(10) as u16;
    let required = 9u16.saturating_add(rows.saturating_mul(item_height.max(1)));
    let target = (terminal_height / 5 * 2).max(8);
    required.max(target).min(terminal_height.max(1))
}

fn run_tui(
    mut sessions: Vec<SessionEntry>,
    project_view: bool,
) -> Result<Option<SessionEntry>, String> {
    let _term_signal =
        TermSignalGuard::install().map_err(|e| format!("cannot register SIGTERM handler: {e}"))?;
    if project_view {
        sessions.sort_by(|a, b| {
            a.cwd
                .cmp(&b.cwd)
                .then_with(|| b.last_active.cmp(&a.last_active))
        });
    }
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| format!("cannot open controlling terminal: {e}"))?;
    let (terminal_width, terminal_height) = size().map_err(|e| e.to_string())?;
    let item_height = if matches!(column_layout(terminal_width), ColumnLayout::Compact { .. }) {
        2
    } else {
        1
    };
    let height = inline_height(sessions.len(), terminal_height, item_height);
    let query_tty = tty.try_clone().map_err(|e| e.to_string())?;
    // Crossterm prefers stdin whenever it is a TTY. Make it use the same
    // controlling terminal even if the caller supplied a different terminal.
    let _stdin_restore = FdRestore::redirect(libc::STDIN_FILENO, tty.as_raw_fd())
        .map_err(|e| format!("cannot connect terminal input: {e}"))?;
    let result = run_tui_backend(
        TtyBackend {
            backend: CrosstermBackend::new(tty),
            query_tty,
        },
        sessions,
        project_view,
        height,
    );
    if TERMINATED.load(Ordering::Relaxed) {
        Err(TERM_ERROR.into())
    } else {
        result
    }
}

const TERM_ERROR: &str = "terminated by SIGTERM";
static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigterm(_: libc::c_int) {
    TERMINATED.store(true, Ordering::Relaxed);
}

struct TermSignalGuard(libc::sigaction);
impl TermSignalGuard {
    fn install() -> io::Result<Self> {
        TERMINATED.store(false, Ordering::Relaxed);
        // Only an atomic flag is changed in the signal handler; the previous
        // disposition is restored before a Codex child can be launched.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let mut previous: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle_sigterm as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGTERM, &action, &mut previous) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(previous))
        }
    }
}
impl Drop for TermSignalGuard {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.0, std::ptr::null_mut());
        }
    }
}

// Crossterm's cursor position query writes to process stdout, regardless of the
// backend writer. Redirect it only for that synchronous query, then restore it
// before producing the selection result. Crossterm reads the response from the tty.
// The picker is single-threaded: no other thread may write to stdout while this
// process-wide redirection is active. Revisit this if background threads are added.
struct TtyBackend {
    backend: CrosstermBackend<File>,
    query_tty: File,
}

struct FdRestore {
    saved: File,
    target: libc::c_int,
}

impl FdRestore {
    fn redirect(target: libc::c_int, source: libc::c_int) -> io::Result<Self> {
        // F_DUPFD_CLOEXEC avoids leaking the original descriptor to a child.
        let saved = unsafe { libc::fcntl(target, libc::F_DUPFD_CLOEXEC, 3) };
        if saved < 0 {
            return Err(io::Error::last_os_error());
        }
        let guard = Self {
            saved: unsafe { File::from_raw_fd(saved) },
            target,
        };
        if unsafe { libc::dup2(source, target) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(guard)
    }

    fn restore(&self) -> io::Result<()> {
        if unsafe { libc::dup2(self.saved.as_raw_fd(), self.target) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for FdRestore {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl Backend for TtyBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a BufferCell)>,
    {
        self.backend.draw(content)
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.backend.append_lines(n)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        io::stdout().flush()?;
        let saved = FdRestore::redirect(libc::STDOUT_FILENO, self.query_tty.as_raw_fd())?;
        let position = self.backend.get_cursor_position();
        saved.restore()?;
        position
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.backend.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.backend.clear()
    }
    fn clear_region(&mut self, kind: ClearType) -> io::Result<()> {
        self.backend.clear_region(kind)
    }
    fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.backend.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.backend)
    }
}

struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

struct TuiGuard<B: Backend>(Terminal<B>);
impl<B: Backend> Drop for TuiGuard<B> {
    fn drop(&mut self) {
        let _ = self.0.clear();
        let _ = self.0.show_cursor();
    }
}

fn run_tui_backend<B: Backend>(
    backend: B,
    sessions: Vec<SessionEntry>,
    project_view: bool,
    height: u16,
) -> Result<Option<SessionEntry>, String> {
    let options = TerminalOptions {
        viewport: Viewport::Inline(height),
    };
    enable_raw_mode().map_err(|e| e.to_string())?;
    let _raw_mode = RawModeGuard;
    let mut terminal =
        TuiGuard(Terminal::with_options(backend, options).map_err(|e| e.to_string())?);
    tui_loop(&mut terminal.0, sessions, project_view)
}

fn tui_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    mut sessions: Vec<SessionEntry>,
    mut project_view: bool,
) -> Result<Option<SessionEntry>, String> {
    let mut query = String::new();
    let mut selected = 0usize;
    let mut scroll_offset = 0usize;
    let mut mode = UiMode::Browsing;
    let mut search_restore_id: Option<(String, PathBuf)> = None;
    let mut visible_capacity = 1usize;
    let mut filtered = Vec::new();
    let mut dirty = true;
    let mut last_size = terminal.size().map_err(|e| e.to_string())?;
    loop {
        if TERMINATED.load(Ordering::Relaxed) {
            return Err(TERM_ERROR.into());
        }
        if dirty {
            filtered = sessions
                .iter()
                .filter(|s| matches(s, &query))
                .cloned()
                .collect();
            selected = clamp_selection(selected, filtered.len());
            let mut rendered_view = (visible_capacity, scroll_offset);
            terminal
                .draw(|frame| {
                    rendered_view = draw_ui(
                        frame,
                        UiRender {
                            sessions: &sessions,
                            filtered: &filtered,
                            query: &query,
                            mode,
                            project_view,
                            selected,
                            scroll_offset,
                        },
                    );
                })
                .map_err(|e| e.to_string())?;
            (visible_capacity, scroll_offset) = rendered_view;
            dirty = false;
        }
        if !event::poll(std::time::Duration::from_millis(16)).map_err(|e| e.to_string())? {
            // Some PTY hosts do not deliver SIGWINCH. Detect actual size
            // changes without redrawing unchanged frames.
            let size = terminal.size().map_err(|e| e.to_string())?;
            if size != last_size {
                last_size = size;
                dirty = true;
            }
            continue;
        }
        let input = event::read().map_err(|e| e.to_string())?;
        if matches!(
            input,
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            })
        ) {
            continue;
        }
        dirty = true;
        match input {
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            }) => match mode {
                UiMode::Searching => {
                    query.clear();
                    mode = transition_mode(mode, UiAction::CancelSearch);
                    selected =
                        selected_index_by_id(&sessions, search_restore_id.as_ref()).unwrap_or(0);
                    scroll_offset = 0;
                    search_restore_id = None;
                }
                UiMode::FilteredBrowsing => {
                    query.clear();
                    mode = transition_mode(mode, UiAction::ClearFilter);
                    selected = 0;
                    scroll_offset = 0;
                }
                UiMode::Browsing => return Ok(None),
            },
            Event::Key(KeyEvent {
                code: KeyCode::Char('/'),
                ..
            }) if mode != UiMode::Searching => {
                search_restore_id = filtered
                    .get(selected)
                    .map(|session| (session.session_id.clone(), session.source_file.clone()));
                mode = transition_mode(mode, UiAction::BeginSearch);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                ..
            }) if mode == UiMode::Searching => {
                query.push(c);
                selected = 0;
                scroll_offset = 0;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                ..
            }) if mode == UiMode::Searching => {
                query.pop();
                selected = 0;
                scroll_offset = 0;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k'),
                ..
            }) if mode != UiMode::Searching => {
                (selected, scroll_offset) = move_selection(
                    selected,
                    scroll_offset,
                    -1,
                    visible_capacity,
                    filtered.len(),
                );
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                ..
            }) if mode != UiMode::Searching => {
                (selected, scroll_offset) =
                    move_selection(selected, scroll_offset, 1, visible_capacity, filtered.len());
            }
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) if mode == UiMode::Browsing || mode == UiMode::FilteredBrowsing => {
                return Ok(filtered.get(selected).cloned())
            }
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) if mode != UiMode::Searching => {
                project_view = !project_view;
                if project_view {
                    sessions.sort_by(|a, b| {
                        a.cwd
                            .cmp(&b.cwd)
                            .then_with(|| b.last_active.cmp(&a.last_active))
                    });
                } else {
                    sessions.sort_by(|a, b| {
                        b.last_active
                            .cmp(&a.last_active)
                            .then_with(|| a.session_id.cmp(&b.session_id))
                    });
                }
                selected = 0;
                scroll_offset = 0;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) if mode == UiMode::Searching => {
                mode = transition_mode(mode, UiAction::ConfirmSearch);
                selected = 0;
                scroll_offset = 0;
                search_restore_id = None;
            }
            Event::Key(KeyEvent {
                code: KeyCode::PageDown,
                ..
            }) if mode != UiMode::Searching => {
                (selected, scroll_offset) = move_selection(
                    selected,
                    scroll_offset,
                    visible_capacity as isize,
                    visible_capacity,
                    filtered.len(),
                );
            }
            Event::Key(KeyEvent {
                code: KeyCode::PageUp,
                ..
            }) if mode != UiMode::Searching => {
                (selected, scroll_offset) = move_selection(
                    selected,
                    scroll_offset,
                    -(visible_capacity as isize),
                    visible_capacity,
                    filtered.len(),
                );
            }
            Event::Key(KeyEvent {
                code: KeyCode::Home,
                ..
            }) if mode != UiMode::Searching => {
                selected = 0;
                scroll_offset = 0;
            }
            Event::Key(KeyEvent {
                code: KeyCode::End, ..
            }) if mode != UiMode::Searching => {
                selected = filtered.len().saturating_sub(1);
                scroll_offset =
                    keep_visible(selected, scroll_offset, visible_capacity, filtered.len());
            }
            Event::Resize(_, _) => {
                terminal.autoresize().map_err(|e| e.to_string())?;
                last_size = terminal.size().map_err(|e| e.to_string())?;
            }
            _ => {}
        }
    }
}

fn doctor(home: &Path) {
    println!(
        "Codex executable: {}",
        if Command::new("codex").arg("--version").output().is_ok() {
            "available"
        } else {
            "not found"
        }
    );
    println!(
        "CODEX_HOME: {} ({})",
        home.display(),
        if home.is_dir() { "readable" } else { "missing" }
    );
    println!("Sessions discovered: {}", discover(home).len());
    println!(
        "Session index: {}",
        if home.join("session_index.jsonl").is_file() {
            "present"
        } else {
            "missing (titles optional)"
        }
    );
}

fn codex_available() -> bool {
    Command::new("codex").arg("--version").output().is_ok()
}

fn shell_resume(home: &Path, session_id: &str) -> Result<ExitStatus, String> {
    let cwd = env::current_dir().map_err(|e| format!("cannot read current directory: {e}"))?;
    let normalized = normalize(&cwd);
    let session = discover(home)
        .into_iter()
        .find(|session| session.session_id == session_id && normalize(&session.cwd) == normalized)
        .ok_or_else(|| "selected session is not recorded for the current directory".to_string())?;
    launch(&session)
}

fn shell_last(home: &Path) -> Result<SessionEntry, String> {
    let last = load_last().ok_or_else(|| "no successful CZI launch recorded".to_string())?;
    if !codex_available() {
        return Err("codex executable was not found".to_string());
    }
    let session = discover(home)
        .into_iter()
        .find(|session| {
            session.session_id == last.session_id
                && normalize(&session.cwd) == normalize(&last.cwd)
                && session.cwd.is_dir()
        })
        .ok_or_else(|| "last session is no longer available".to_string())?;
    ensure_resumable(&session)?;
    Ok(session)
}

fn write_selection(mut output: impl Write, session: &SessionEntry) -> io::Result<()> {
    output.write_all(session.cwd.as_os_str().as_bytes())?;
    output.write_all(&[0])?;
    output.write_all(session.session_id.as_bytes())?;
    output.write_all(&[0])?;
    output.flush()
}

fn write_shell_selection(path: &Path, session: &SessionEntry) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot create selection result: {e}"))?;
    write_selection(&mut file, session)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("cannot write selection result: {e}"))
}

fn shell_init_script(shell: ShellKind) -> &'static str {
    match shell {
        ShellKind::Bash => {
            r#"_czi_pick_and_resume() {
  local czi_tmp_dir czi_result czi_status=0 czi_cwd='' czi_session=''
  czi_tmp_dir=$(command mktemp -d "${TMPDIR:-/tmp}/czi.XXXXXXXXXX") || return 1
  czi_result="$czi_tmp_dir/selection"
  if [ "${1:-}" = "--last" ]; then
    shift
    if command czi --shell-last --result-file "$czi_result"; then
      czi_status=0
    else
      czi_status=$?
    fi
  else
    if command czi --shell-select --result-file "$czi_result" "$@"; then
      czi_status=0
    else
      czi_status=$?
    fi
  fi
  if [ "$czi_status" -ne 0 ]; then
    command rm -f -- "$czi_result" >/dev/null 2>&1 || :
    command rmdir -- "$czi_tmp_dir" >/dev/null 2>&1 || :
    [ "$czi_status" -ne 2 ] || return 0
    return "$czi_status"
  fi
  if [ ! -s "$czi_result" ]; then
    command rm -f -- "$czi_result" >/dev/null 2>&1 || :
    command rmdir -- "$czi_tmp_dir" >/dev/null 2>&1 || :
    printf '%s\n' 'czi: missing selection result' >&2
    return 1
  fi
  {
    IFS= read -r -d '' czi_cwd || czi_status=1
    IFS= read -r -d '' czi_session || czi_status=1
  } < "$czi_result"
  command rm -f -- "$czi_result" >/dev/null 2>&1 || :
  command rmdir -- "$czi_tmp_dir" >/dev/null 2>&1 || :
  if [ "$czi_status" -ne 0 ] || [ -z "$czi_cwd" ] || [ -z "$czi_session" ]; then
    printf '%s\n' 'czi: invalid selection result' >&2
    return 1
  fi
  builtin cd -- "$czi_cwd" || return
  command czi --shell-resume "$czi_session"
}

czi() {
  if [ "$#" -eq 0 ]; then
    _czi_pick_and_resume
    return $?
  fi
  if [ "$#" -eq 1 ] && [ "$1" = "last" ]; then
    _czi_pick_and_resume --last
    return $?
  fi
  local czi_arg czi_current=0 czi_projects=0 czi_passthrough=0
  for czi_arg in "$@"; do
    case "$czi_arg" in
      .) czi_current=1 ;;
      -p|--projects) czi_projects=1 ;;
      *) czi_passthrough=1 ;;
    esac
  done
  if [ "$czi_passthrough" -ne 0 ]; then
    command czi "$@"
    return $?
  fi
  local czi_flags=()
  [ "$czi_current" -eq 0 ] || czi_flags+=(--shell-current)
  [ "$czi_projects" -eq 0 ] || czi_flags+=(--shell-projects)
  _czi_pick_and_resume "${czi_flags[@]}"
  return $?
}
"#
        }
        ShellKind::Fish => {
            r#"function _czi_pick_and_resume
    set -l czi_tmp_base /tmp
    if set -q TMPDIR
        set czi_tmp_base $TMPDIR
    end
    set -l czi_tmp_dir (command mktemp -d "$czi_tmp_base/czi.XXXXXXXXXX")
    or return 1
    set -l czi_result "$czi_tmp_dir/selection"
    set -l czi_status 0
    if test (count $argv) -gt 0; and test "$argv[1]" = --last
        command czi --shell-last --result-file "$czi_result"
        set czi_status $status
    else
        command czi --shell-select --result-file "$czi_result" $argv
        set czi_status $status
    end
    if test $czi_status -ne 0
        command rm -f -- "$czi_result"; or true
        command rmdir -- "$czi_tmp_dir"; or true
        if test $czi_status -eq 2
            return 0
        end
        return $czi_status
    end
    if not test -s "$czi_result"
        command rm -f -- "$czi_result"; or true
        command rmdir -- "$czi_tmp_dir"; or true
        printf '%s\n' 'czi: missing selection result' >&2
        return 1
    end
    set -l czi_cwd
    set -l czi_session
    set -l czi_read_ok 1
    begin
        read -z czi_cwd; or set czi_read_ok 0
        read -z czi_session; or set czi_read_ok 0
    end < "$czi_result"
    command rm -f -- "$czi_result"; or true
    command rmdir -- "$czi_tmp_dir"; or true
    if test $czi_read_ok -ne 1; or test -z "$czi_cwd"; or test -z "$czi_session"
        printf '%s\n' 'czi: invalid selection result' >&2
        return 1
    end
    builtin cd -- "$czi_cwd"; or return
    command czi --shell-resume "$czi_session"
end

function czi
    if test (count $argv) -eq 0
        _czi_pick_and_resume
        return $status
    end
    if test (count $argv) -eq 1; and test "$argv[1]" = last
        _czi_pick_and_resume --last
        return $status
    end
    set -l czi_flags
    set -l czi_passthrough 0
    for czi_arg in $argv
        switch $czi_arg
            case .
                set -a czi_flags --shell-current
            case -p --projects
                set -a czi_flags --shell-projects
            case '*'
                set czi_passthrough 1
        end
    end
    if test $czi_passthrough -eq 1
        command czi $argv
        return $status
    end
    _czi_pick_and_resume $czi_flags
    return $status
end
"#
        }
    }
}

fn print_shell_init(shell: ShellKind) {
    let script = shell_init_script(shell);
    print!("{script}");
}

fn main() {
    let cli = Cli::parse();
    let home = codex_home();
    let current_only = matches!(cli.command, Some(CommandKind::Current));
    if let Some(session_id) = cli.shell_resume.as_deref() {
        match shell_resume(&home, session_id) {
            Ok(status) => std::process::exit(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
            ),
            Err(error) => {
                eprintln!("czi: {error}");
                std::process::exit(1);
            }
        }
    }
    if cli.shell_last {
        match shell_last(&home) {
            Ok(session) => match cli.result_file.as_deref() {
                Some(path) => {
                    if let Err(error) = write_shell_selection(path, &session) {
                        eprintln!("czi: {error}");
                        std::process::exit(1);
                    }
                }
                None => {
                    if let Err(error) = write_selection(io::stdout().lock(), &session) {
                        eprintln!("czi: cannot write selection result: {error}");
                        std::process::exit(1);
                    }
                }
            },
            Err(error) => {
                eprintln!("czi: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if cli.shell_select {
        let result = if !codex_available() {
            Err("codex executable was not found".to_string())
        } else {
            {
                let mut sessions = discover(&home);
                if cli.shell_current {
                    let cwd = normalize(&env::current_dir().unwrap_or_default());
                    sessions.retain(|session| normalize(&session.cwd) == cwd);
                }
                run_tui(sessions, cli.shell_projects || cli.projects).and_then(|selection| {
                    if let Some(session) = selection {
                        ensure_resumable(&session)?;
                        if !session.cwd.is_dir() {
                            return Err(format!(
                                "session working directory does not exist: {}",
                                session.cwd.display()
                            ));
                        }
                        if let Some(result_file) = cli.result_file.as_deref() {
                            write_shell_selection(result_file, &session)?;
                        } else {
                            write_selection(io::stdout().lock(), &session)
                                .map_err(|e| format!("cannot write selection result: {e}"))?;
                        }
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                })
            }
        };
        match result {
            Ok(true) => {}
            Ok(false) => std::process::exit(2),
            Err(error) => {
                eprintln!("czi: {error}");
                std::process::exit(if error == TERM_ERROR { 143 } else { 1 });
            }
        }
        return;
    }
    let result = match cli.command {
        Some(CommandKind::Doctor) => {
            doctor(&home);
            Ok(())
        }
        Some(CommandKind::Last) => shell_last(&home).and_then(|s| launch_checked(&s)),
        Some(CommandKind::Init { shell }) => {
            print_shell_init(shell);
            Ok(())
        }
        _ => {
            let mut sessions = discover(&home);
            if current_only {
                let cwd = normalize(&env::current_dir().unwrap_or_default());
                sessions.retain(|s| normalize(&s.cwd) == cwd);
            }
            run_tui(sessions, cli.projects)
                .and_then(|s| s.map_or(Ok(()), |session| launch_checked(&session)))
        }
    };
    if let Err(error) = result {
        eprintln!("czi: {error}");
        std::process::exit(if error == TERM_ERROR { 143 } else { 1 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::io::Write;
    use tempfile::tempdir;
    #[test]
    fn discovers_titles_and_multiple_sessions() {
        let dir = tempdir().unwrap();
        let day = dir.path().join("sessions/2026/01/01");
        fs::create_dir_all(&day).unwrap();
        let cwd = dir.path().join("项目 with space");
        fs::create_dir_all(&cwd).unwrap();
        let mut idx = File::create(dir.path().join("session_index.jsonl")).unwrap();
        writeln!(
            idx,
            r#"{{"id":"a","thread_name":"Build API","updated_at":"2026-01-01T01:00:00Z"}}"#
        )
        .unwrap();
        for (id, ts) in [("a", "2026-01-01T02:00:00Z"), ("b", "2026-01-01T03:00:00Z")] {
            let mut f = File::create(day.join(format!("{id}.jsonl"))).unwrap();
            writeln!(f, r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"{}","timestamp":"{ts}"}}}}"#, cwd.display()).unwrap();
        }
        let got = discover(dir.path());
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].session_id, "b");
        assert_eq!(
            got.iter()
                .find(|s| s.session_id == "a")
                .unwrap()
                .thread_name
                .as_deref(),
            Some("Build API")
        );
    }
    #[test]
    fn malformed_lines_are_skipped() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sessions/2026/01");
        fs::create_dir_all(&p).unwrap();
        let mut f = File::create(p.join("x.jsonl")).unwrap();
        writeln!(f, "not json").unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn missing_cwd_is_not_resumable_record() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sessions/2026/01");
        fs::create_dir_all(&p).unwrap();
        let mut f = File::create(p.join("missing-cwd.jsonl")).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"id":"no-cwd"}}}}"#
        )
        .unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn duplicate_session_ids_are_deduplicated() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sessions/2026/01");
        fs::create_dir_all(&p).unwrap();
        for name in ["first.jsonl", "second.jsonl"] {
            let mut f = File::create(p.join(name)).unwrap();
            writeln!(
                f,
                r#"{{"type":"session_meta","payload":{{"id":"same","cwd":"/tmp"}}}}"#
            )
            .unwrap();
        }
        assert_eq!(discover(dir.path()).len(), 1);
    }

    #[test]
    fn regression_duplicate_id_preserves_different_cwds() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        for (file, cwd) in [("first", "/tmp/a"), ("second", "/tmp/b")] {
            fs::write(sessions.join(format!("{file}.jsonl")), format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"11111111-2222-3333-4444-555555555555\",\"cwd\":\"{cwd}\"}}}}\n")).unwrap();
        }
        assert_eq!(discover(dir.path()).len(), 2);
    }

    #[test]
    fn regression_unicode_short_id_never_panics() {
        let session = SessionEntry {
            provider: "codex".into(),
            session_id: "abcdef汉🙂".into(),
            cwd: PathBuf::from("/tmp"),
            thread_name: None,
            preview: None,
            last_active: None,
            source_file: PathBuf::new(),
            resumable: false,
        };
        assert!(session_label(&[session], 0).starts_with("Session · "));
    }

    #[test]
    fn malformed_ids_are_displayable_but_cannot_resume() {
        let good = "01234567-89ab-cdef-0123-456789abcdef";
        assert!(valid_session_id(good));
        for id in [
            "",
            "short",
            "abcdef汉",
            "中english",
            "🙂🙂🙂",
            "01234567-89ab-cdef-0123-456789abcdeg",
        ] {
            let session = SessionEntry {
                provider: "codex".into(),
                session_id: id.into(),
                cwd: PathBuf::from("/tmp"),
                thread_name: None,
                preview: None,
                last_active: None,
                source_file: PathBuf::new(),
                resumable: false,
            };
            assert!(!valid_session_id(id));
            assert!(ensure_resumable(&session).is_err());
            let _ = session_label(&[session], 0);
        }
    }

    #[test]
    fn duplicate_selection_is_deterministic_and_equal_time_conflicts_are_blocked() {
        for reverse in [false, true] {
            let dir = tempdir().unwrap();
            let sessions = dir.path().join("sessions");
            fs::create_dir_all(&sessions).unwrap();
            let rows = [("z", "2026-01-01T00:00:00Z"), ("a", "2026-01-02T00:00:00Z")];
            for (file, time) in if reverse {
                rows.into_iter().rev().collect::<Vec<_>>()
            } else {
                rows.to_vec()
            } {
                fs::write(sessions.join(format!("{file}.jsonl")), format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"11111111-2222-3333-4444-555555555555\",\"cwd\":\"/tmp\",\"timestamp\":\"{time}\"}}}}\n")).unwrap();
            }
            fs::write(sessions.join("damaged.jsonl"), "not json\n").unwrap();
            let found = discover(dir.path());
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].source_file.file_name().unwrap(), "a.jsonl");
            assert!(found[0].resumable);
            fs::copy(sessions.join("a.jsonl"), sessions.join("z.jsonl")).unwrap();
            assert!(discover(dir.path())[0].resumable);
            writeln!(
                OpenOptions::new()
                    .append(true)
                    .open(sessions.join("z.jsonl"))
                    .unwrap(),
                "{{\"type\":\"unrelated\"}}"
            )
            .unwrap();
            assert!(!discover(dir.path())[0].resumable);
            fs::write(sessions.join("z.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"11111111-2222-3333-4444-555555555555\",\"cwd\":\"/tmp\",\"timestamp\":\"2026-01-01T00:00:00Z\"}}\n").unwrap();
            let mut file = OpenOptions::new()
                .append(true)
                .open(sessions.join("z.jsonl"))
                .unwrap();
            writeln!(file, "{{\"type\":\"event_msg\",\"timestamp\":\"2026-01-02T00:00:00Z\",\"payload\":{{\"type\":\"task_complete\"}}}}").unwrap();
            writeln!(file, "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"different task\"}}]}}}}").unwrap();
            let found = discover(dir.path());
            assert!(!found[0].resumable);
            assert!(ensure_resumable(&found[0])
                .unwrap_err()
                .contains("ambiguous"));
        }
    }

    #[test]
    fn latest_thread_name_wins_and_empty_name_falls_back_to_prompt() {
        let dir = tempdir().unwrap();
        let day = dir.path().join("sessions/2026/01");
        fs::create_dir_all(&day).unwrap();
        let cwd = dir.path().to_string_lossy();
        let mut index = File::create(dir.path().join("session_index.jsonl")).unwrap();
        writeln!(
            index,
            r#"{{"id":"rename","thread_name":"Old","updated_at":"2026-01-01T00:00:00Z"}}"#
        )
        .unwrap();
        writeln!(index, r#"{{"id":"rename","thread_name":"New \u001b[31mName\u001b[0m","updated_at":"2026-01-02T00:00:00Z"}}"#).unwrap();
        let mut session = File::create(day.join("rename.jsonl")).unwrap();
        writeln!(
            session,
            r#"{{"type":"session_meta","payload":{{"id":"rename","cwd":"{cwd}"}}}}"#
        )
        .unwrap();
        writeln!(
            session,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Fallback task\nwith line break"}}]}}}}"#
        )
        .unwrap();
        let found = discover(dir.path());
        assert_eq!(found[0].thread_name.as_deref(), Some("New Name"));
        assert_eq!(
            found[0].preview.as_deref(),
            Some("Fallback task with line break")
        );

        writeln!(
            index,
            r#"{{"id":"rename","thread_name":"","updated_at":"2026-01-03T00:00:00Z"}}"#
        )
        .unwrap();
        let mut session = File::create(day.join("rename.jsonl")).unwrap();
        writeln!(
            session,
            r#"{{"type":"session_meta","payload":{{"id":"rename","cwd":"{cwd}"}}}}"#
        )
        .unwrap();
        writeln!(
            session,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Fallback task\nwith line break"}}]}}}}"#
        )
        .unwrap();
        assert_eq!(
            discover(dir.path())[0].preview.as_deref(),
            Some("Fallback task with line break")
        );
        assert_eq!(
            discover(dir.path())[0].thread_name.as_deref(),
            Some("New Name")
        );
    }

    #[test]
    fn name_and_activity_are_aggregated_independently() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("session_index.jsonl"), concat!(
            "{\"id\":\"a\",\"thread_name\":\"较新🙂\",\"updated_at\":\"2026-11-21T10:00:00Z\"}\n",
            "{\"id\":\"a\",\"thread_name\":\"较旧\",\"updated_at\":\"2026-11-20T10:00:00Z\"}\n",
            "{\"id\":\"a\",\"thread_name\":\"同一时刻\",\"updated_at\":\"2026-11-21T18:00:00+08:00\"}\n",
            "{\"id\":\"a\",\"thread_name\":\"无效时间\",\"updated_at\":\"invalid\"}\n",
            "{\"id\":\"a\",\"thread_name\":null,\"updated_at\":\"2026-11-22T10:00:00Z\"}\n",
            "{\"id\":\"a\",\"thread_name\":\"  \",\"updated_at\":\"2026-11-23T10:00:00Z\"}\n",
            "{\"id\":\"a\",\"updated_at\":\"2026-11-24T10:00:00Z\"}\n",
            "{\"id\":\"b\",\"thread_name\":\"无时间一\"}\n",
            "{\"id\":\"b\",\"thread_name\":\"无时间二\",\"updated_at\":\"bad\"}\n",
            "{\"id\":\"c\",\"thread_name\":\"single\"}\n",
            "{bad json\n",
        )).unwrap();
        let index = read_titles(dir.path());
        assert_eq!(index["a"].title.as_deref(), Some("同一时刻"));
        assert_eq!(
            index["a"].updated_at,
            parse_time(Some(&Value::String("2026-11-24T10:00:00Z".into())))
        );
        assert_eq!(index["b"].title.as_deref(), Some("无时间二"));
        assert_eq!(index["c"].title.as_deref(), Some("single"));
    }

    #[test]
    fn invalid_utf8_jsonl_line_does_not_hide_later_records() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(dir.path().join("session_index.jsonl"), b"\xff\n{\"id\":\"fixture\",\"thread_name\":\"valid\",\"updated_at\":\"2026-01-02T00:00:00Z\"}\n").unwrap();
        fs::write(sessions.join("session.jsonl"), b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"fixture\",\"cwd\":\"/tmp\"}}\n\xff\n{\"type\":\"event_msg\",\"timestamp\":\"2026-01-03T00:00:00Z\",\"payload\":{\"type\":\"task_complete\"}}\n").unwrap();
        let found = discover(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].thread_name.as_deref(), Some("valid"));
        assert_eq!(
            found[0].last_active,
            parse_time(Some(&Value::String("2026-01-03T00:00:00Z".into())))
        );
    }

    #[test]
    fn scan_orders_activity_and_keeps_native_names_and_ids() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::copy(
            "tests/fixtures/activity_order/session_index.jsonl",
            dir.path().join("session_index.jsonl"),
        )
        .unwrap();
        for name in ["session-a.jsonl", "session-b.jsonl", "session-c.jsonl"] {
            fs::copy(
                Path::new("tests/fixtures/activity_order").join(name),
                sessions.join(name),
            )
            .unwrap();
        }
        // An event after the old 64-line scan and a freshly modified file must
        // not mask the later index activity of another session.
        let mut b = OpenOptions::new()
            .append(true)
            .open(sessions.join("session-b.jsonl"))
            .unwrap();
        for _ in 0..65 {
            writeln!(b, "{{\"type\":\"unrelated\"}}").unwrap();
        }
        writeln!(b, "{{\"type\":\"event_msg\",\"timestamp\":\"2026-11-21T11:00:00Z\",\"payload\":{{\"type\":\"task_complete\"}}}}").unwrap();
        b.set_times(
            std::fs::FileTimes::new()
                .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(4_102_444_800)),
        )
        .unwrap();
        let found = discover(dir.path());
        assert_eq!(found.len(), 3);
        assert_eq!(
            found
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "fixture-A-00000000-1111-2222-333333333333",
                "fixture-C-00000000-1111-2222-333333333333",
                "fixture-B-00000000-1111-2222-333333333333"
            ]
        );
        assert_eq!(found[0].thread_name.as_deref(), Some("最新名称🙂"));
        assert_eq!(found[1].thread_name.as_deref(), Some("数据库优化"));
        assert_eq!(
            found[1].last_active,
            parse_time(Some(&Value::String("2026-11-22T13:00:00Z".into())))
        );
        assert_eq!(
            found[2].last_active,
            parse_time(Some(&Value::String("2026-11-21T11:00:00Z".into())))
        );
        assert!(matches(
            &found[0],
            "fixture-A-00000000-1111-2222-333333333333"
        ));
        assert!(matches(&found[1], "数据库"));
        let mut terminal = Terminal::new(TestBackend::new(120, 19)).unwrap();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    UiRender {
                        sessions: &found,
                        filtered: &found,
                        query: "",
                        mode: UiMode::Browsing,
                        project_view: false,
                        selected: 0,
                        scroll_offset: 0,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let first_row: String = (0..120).map(|x| buffer[(x, 6)].symbol()).collect();
        assert!(first_row.contains(&display_time(found[0].last_active)));
    }

    #[test]
    fn missing_activity_uses_creation_then_modification_and_ties_are_stable() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        for id in ["tie-b", "tie-a"] {
            fs::write(sessions.join(format!("{id}.jsonl")), format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/tmp\",\"timestamp\":\"2026-01-01T00:00:00Z\"}}}}\n")).unwrap();
        }
        fs::write(sessions.join("mtime.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"mtime\",\"cwd\":\"/tmp\",\"timestamp\":\"invalid\"}}\n").unwrap();
        let found = discover(dir.path());
        let tied: Vec<_> = found
            .iter()
            .filter(|s| s.session_id.starts_with("tie-"))
            .collect();
        assert_eq!(
            tied.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["tie-a", "tie-b"]
        );
        assert_eq!(
            tied[0].last_active,
            parse_time(Some(&Value::String("2026-01-01T00:00:00Z".into())))
        );
        assert!(found
            .iter()
            .find(|s| s.session_id == "mtime")
            .unwrap()
            .last_active
            .is_some());
    }

    #[test]
    fn unicode_truncation_and_pagination_are_bounded() {
        assert_eq!(truncate_width("中文🙂abc", 5), "中文…");
        assert_eq!(truncate_width("abcdef", 1), "…");
        assert_eq!(page_capacity(24, 1), 15);
        assert_eq!(page_capacity(15, 2), 6);
        assert_eq!(clamp_selection(99, 0), 0);
        assert_eq!(clamp_selection(99, 3), 2);
    }

    #[test]
    fn continuous_scroll_moves_one_item_and_stays_in_bounds() {
        let (selected, offset) = move_selection(9, 0, 1, 10, 36);
        assert_eq!((selected, offset), (10, 1));
        let (selected, offset) = move_selection(selected, offset, 1, 10, 36);
        assert_eq!((selected, offset), (11, 2));
        let (selected, offset) = move_selection(selected, offset, -1, 10, 36);
        assert_eq!((selected, offset), (10, 2));
        let (selected, offset) = move_selection(0, 0, -100, 10, 36);
        assert_eq!((selected, offset), (0, 0));
        let (selected, offset) = move_selection(35, 25, 1, 10, 36);
        assert_eq!((selected, offset), (35, 26));
    }

    #[test]
    fn continuous_scroll_page_keys_and_resize_keep_selection_visible() {
        let (selected, offset) = move_selection(0, 0, 10, 10, 1000);
        assert_eq!((selected, offset), (10, 1));
        let resized = keep_visible(selected, offset, 4, 1000);
        assert_eq!(resized, 7);
        let (selected, offset) = move_selection(selected, resized, -4, 4, 1000);
        assert_eq!((selected, offset), (6, 6));
    }

    #[test]
    fn continuous_scroll_boundaries_cover_empty_small_and_large_lists() {
        for total in [0, 1, 10, 11, 36, 1000] {
            let (mut selected, mut offset) = (0, 0);
            for _ in 0..30 {
                (selected, offset) = move_selection(selected, offset, 1, 10, total);
            }
            if total == 0 {
                assert_eq!((selected, offset), (0, 0));
                continue;
            }
            assert_eq!(selected, total.saturating_sub(1).min(30));
            assert!(offset <= selected);
            assert!(selected < offset.saturating_add(10));
            for _ in 0..30 {
                (selected, offset) = move_selection(selected, offset, -1, 10, total);
            }
            assert_eq!((selected, offset), (0, 0));
        }
    }

    #[test]
    fn path_compression_keeps_tail_and_handles_wide_text() {
        assert_eq!(
            shorten_path(Path::new("/tmp/study/project/czi"), 7),
            "…/czi"
        );
        assert_eq!(shorten_path(Path::new("/tmp/a/b/c"), 20), "/tmp/a/b/c");
        assert_eq!(truncate_width("目录🙂组合", 7), "目录🙂…");
    }

    #[test]
    fn inline_height_scales_with_content_and_terminal_limits() {
        assert_eq!(inline_height(0, 24, 1), 9);
        assert_eq!(inline_height(1, 24, 1), 10);
        assert_eq!(inline_height(10, 24, 1), 19);
        assert_eq!(inline_height(100, 12, 2), 12);
    }

    #[test]
    fn shell_init_scripts_use_command_and_quoted_selection_fields() {
        let bash = shell_init_script(ShellKind::Bash);
        let fish = shell_init_script(ShellKind::Fish);
        assert!(bash.contains("mktemp -d"));
        assert!(bash.contains("command czi --shell-select --result-file"));
        assert!(bash.contains("read -r -d '' czi_cwd"));
        assert!(bash.contains("builtin cd -- \"$czi_cwd\""));
        assert!(bash.contains("command czi --shell-last"));
        assert!(bash.contains("--shell-current"));
        assert!(bash.contains("--shell-projects"));
        assert!(fish.contains("command czi --shell-resume"));
        assert!(fish.contains("command czi --shell-select --result-file"));
        assert!(fish.contains("read -z czi_cwd"));
        assert!(fish.contains("command czi --shell-last"));
        assert!(fish.contains("builtin cd -- \"$czi_cwd\""));
        assert!(!fish.contains("(command czi --shell-select"));
    }

    #[test]
    fn shell_selection_file_preserves_newlines_and_rejects_existing_paths() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = tempdir().unwrap();
        let result = dir.path().join("private-result");
        let session = SessionEntry {
            provider: "codex".into(),
            session_id: "full-session-id".into(),
            cwd: PathBuf::from("/tmp/项目 with $pecial's\nand newline"),
            thread_name: None,
            preview: None,
            last_active: None,
            source_file: PathBuf::new(),
            resumable: true,
        };
        write_shell_selection(&result, &session).unwrap();
        let mut expected = session.cwd.as_os_str().as_bytes().to_vec();
        expected.push(0);
        expected.extend_from_slice(session.session_id.as_bytes());
        expected.push(0);
        assert_eq!(fs::read(&result).unwrap(), expected);
        assert_eq!(
            fs::metadata(&result).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_shell_selection(&result, &session).is_err());

        let target = dir.path().join("target");
        fs::write(&target, "untouched").unwrap();
        let link = dir.path().join("symlink-result");
        symlink(&target, &link).unwrap();
        assert!(write_shell_selection(&link, &session).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
    }

    #[test]
    fn environment_context_is_removed_but_real_task_is_kept() {
        let record = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "用户任务：修复标题解析\n<environment_context>\n  <cwd>/tmp/project</cwd>\n  <shell>bash</shell>\n  <current_date>2026-11-21</current_date>\n</environment_context>"
                }]
            }
        });
        assert_eq!(
            user_task_from_record(&record).as_deref(),
            Some("用户任务：修复标题解析")
        );
    }

    #[test]
    fn environment_only_message_is_not_a_title() {
        let record = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "<environment_context>\n  <cwd>/tmp/project</cwd>\n  <shell>bash</shell>\n  <current_date>2026-11-21</current_date>\n</environment_context>"
                }]
            }
        });
        assert!(user_task_from_record(&record).is_none());
    }

    #[test]
    fn user_text_that_mentions_environment_context_is_preserved() {
        let record = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "请解释 environment_context 标签和 XML 用法"
                }]
            }
        });
        assert_eq!(
            user_task_from_record(&record).as_deref(),
            Some("请解释 environment_context 标签和 XML 用法")
        );
    }

    #[test]
    fn title_regression_fixture_keeps_native_name_and_skips_bad_index_line() {
        let dir = tempdir().unwrap();
        let sessions = dir.path().join("sessions/2026/01");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            dir.path().join("session_index.jsonl"),
            include_str!("../tests/fixtures/title_regression/session_index.jsonl"),
        )
        .unwrap();
        fs::write(
            sessions.join("session.jsonl"),
            include_str!("../tests/fixtures/title_regression/session.jsonl"),
        )
        .unwrap();
        assert_eq!(
            discover(dir.path())[0].thread_name.as_deref(),
            Some("User rename")
        );
        assert_eq!(discover(dir.path())[0].session_id, "fixture-named");
    }

    #[test]
    fn real_codex_history_has_no_environment_titles_when_enabled() {
        if env::var_os("CZI_REAL_DATA_TEST").is_none() {
            return;
        }
        let sessions = discover(&codex_home());
        let bad = sessions.iter().filter(|session| {
            session
                .thread_name
                .as_deref()
                .is_some_and(|title| title.contains("<environment_context>"))
        });
        assert_eq!(bad.count(), 0);
        let named = sessions
            .iter()
            .filter(|session| session.thread_name.is_some() || session.preview.is_some())
            .count();
        eprintln!(
            "validated {} local Codex sessions without environment titles; {} have resolved titles",
            sessions.len(),
            named
        );
    }

    #[test]
    fn real_data_activity_audit_when_enabled() {
        if env::var_os("CZI_ACTIVITY_AUDIT").is_none() {
            return;
        }
        use std::collections::{HashMap, HashSet};
        let home = codex_home();
        let mut paths = Vec::new();
        fn collect(dir: &Path, depth: usize, paths: &mut Vec<PathBuf>) {
            if depth > 8 {
                return;
            }
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        collect(&path, depth + 1, paths);
                    } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                        paths.push(path);
                    }
                }
            }
        }
        collect(&home.join("sessions"), 0, &mut paths);
        let mut old_index: HashMap<String, (Option<DateTime<Utc>>, usize)> = HashMap::new();
        let mut ids = HashSet::new();
        let (mut duplicate, mut empty, mut invalid, mut missing, mut malformed) = (0, 0, 0, 0, 0);
        if let Ok(file) = File::open(home.join("session_index.jsonl")) {
            for (order, line) in BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .enumerate()
            {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    malformed += 1;
                    continue;
                };
                let Some(id) = find_string(&value, &["id", "session_id"]) else {
                    continue;
                };
                if !ids.insert(id.clone()) {
                    duplicate += 1;
                }
                if find_string(&value, &["thread_name"])
                    .map(|s| clean_text(&s))
                    .is_none_or(|s| s.is_empty())
                {
                    empty += 1;
                }
                let raw = value
                    .get("updated_at")
                    .or_else(|| value.get("last_active_at"));
                let updated = parse_time(raw);
                if raw.is_none() {
                    missing += 1;
                }
                for key in ["updated_at", "last_active_at"] {
                    if value
                        .get(key)
                        .is_some_and(|raw| parse_time(Some(raw)).is_none())
                    {
                        invalid += 1;
                    }
                }
                let replace = old_index
                    .get(&id)
                    .is_none_or(|(time, old_order)| newer_name(*time, *old_order, updated, order));
                if replace {
                    old_index.insert(id, (updated, order));
                }
            }
        }
        let sessions = discover(&home);
        let new_index = read_titles(&home);
        let mut old = Vec::new();
        let mut changed = 0usize;
        let mut event_newer = 0usize;
        let mut index_newer = 0usize;
        for session in &sessions {
            let mut old_meta = None;
            let mut got_id = false;
            let mut got_cwd = false;
            let mut task = false;
            let mut legacy_done = false;
            if let Ok(file) = File::open(&session.source_file) {
                for (line_no, line) in BufReader::new(file)
                    .lines()
                    .map_while(Result::ok)
                    .enumerate()
                {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        malformed += 1;
                        continue;
                    };
                    let payload = value.get("payload").unwrap_or(&value);
                    if line_no < 64
                        && !legacy_done
                        && value.get("type").and_then(Value::as_str) == Some("session_meta")
                    {
                        got_id |= find_string(payload, &["id", "session_id"]).is_some();
                        got_cwd |= find_string(payload, &["cwd", "working_directory"]).is_some();
                        old_meta = parse_time(
                            payload
                                .get("updated_at")
                                .or_else(|| payload.get("timestamp")),
                        )
                        .or(old_meta);
                    }
                    if line_no < 64 && !legacy_done && !task {
                        task = user_task_from_record(&value).is_some();
                    }
                    if let Some(raw) = value.get("timestamp") {
                        if parse_time(Some(raw)).is_none() {
                            invalid += 1;
                        }
                    }
                    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
                        for key in ["timestamp", "updated_at"] {
                            if payload
                                .get(key)
                                .is_some_and(|raw| parse_time(Some(raw)).is_none())
                            {
                                invalid += 1;
                            }
                        }
                    }
                    // Continue for statistics but stop applying old parser state at its original break.
                    if got_id && got_cwd && task {
                        legacy_done = true;
                    }
                }
            }
            let modified = fs::metadata(&session.source_file)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| {
                    DateTime::<Utc>::from_timestamp(d.as_secs() as i64, d.subsec_nanos())
                });
            let old_time = old_meta
                .or(old_index
                    .get(&session.session_id)
                    .and_then(|(time, _)| *time))
                .or(modified);
            if old_time != session.last_active {
                changed += 1;
                let index_time = new_index
                    .get(&session.session_id)
                    .and_then(|r| r.updated_at);
                if index_time.is_some_and(|t| Some(t) == session.last_active) {
                    index_newer += 1;
                } else {
                    event_newer += 1;
                }
            }
            old.push((session.session_id.clone(), old_time));
        }
        old.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let old_positions: HashMap<_, _> =
            old.iter().enumerate().map(|(i, (id, _))| (id, i)).collect();
        let moved = sessions
            .iter()
            .enumerate()
            .filter(|(i, s)| old_positions.get(&s.session_id) != Some(i))
            .count();
        let named = sessions.iter().filter(|s| s.thread_name.is_some()).count();
        eprintln!("activity audit: scanned={} parsed={} named={} unnamed={} duplicate_index={} empty_names={} invalid_times={} missing_index_times={} malformed_lines={} changed_times={} moved_positions={} index_latest={} other_latest={}",
            paths.len(), sessions.len(), named, sessions.len() - named, duplicate, empty, invalid, missing, malformed, changed, moved, index_newer, event_newer);
    }

    #[test]
    fn search_mode_transitions_are_explicit() {
        assert_eq!(
            transition_mode(UiMode::Browsing, UiAction::BeginSearch),
            UiMode::Searching
        );
        assert_eq!(
            transition_mode(UiMode::Searching, UiAction::ConfirmSearch),
            UiMode::FilteredBrowsing
        );
        assert_eq!(
            transition_mode(UiMode::Searching, UiAction::CancelSearch),
            UiMode::Browsing
        );
        assert_eq!(
            transition_mode(UiMode::FilteredBrowsing, UiAction::ClearFilter),
            UiMode::Browsing
        );
        assert_eq!(
            transition_mode(UiMode::Searching, UiAction::BeginSearch),
            UiMode::Searching
        );
    }

    #[test]
    fn table_backend_renders_all_supported_sizes_without_overflow() {
        let sessions = vec![
            SessionEntry {
                provider: "codex".into(),
                session_id: "aaaaaaaa-1111".into(),
                cwd: PathBuf::from("/tmp/czi-fixture/中文目录"),
                thread_name: Some("命名会话🙂".into()),
                preview: Some("这是一个很长的用户任务预览，用于检查列宽不会互相覆盖".into()),
                last_active: Some(Utc::now()),
                source_file: PathBuf::new(),
                resumable: true,
            },
            SessionEntry {
                provider: "codex".into(),
                session_id: "aaaaaaaa-2222".into(),
                cwd: PathBuf::from("/tmp/czi-fixture/another path"),
                thread_name: None,
                preview: None,
                last_active: None,
                source_file: PathBuf::new(),
                resumable: true,
            },
        ];
        for (width, height) in [
            (160, 40),
            (120, 32),
            (100, 24),
            (80, 24),
            (55, 15),
            (30, 10),
        ] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw_ui(
                        frame,
                        UiRender {
                            sessions: &sessions,
                            filtered: &sessions,
                            query: "",
                            mode: UiMode::Browsing,
                            project_view: false,
                            selected: 0,
                            scroll_offset: 0,
                        },
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rendered: String = (0..height)
                .flat_map(|y| (0..width).map(move |x| buffer[(x, y)].symbol().to_string()))
                .collect();
            assert!(
                rendered.contains("Session"),
                "missing Session header at {width}x{height}"
            );
            assert!(
                rendered.contains("命") && rendered.contains("会"),
                "missing selected name at {width}x{height}"
            );
            assert!(
                rendered.contains(">"),
                "missing selection marker at {width}x{height}"
            );
            let marker_is_red = (0..height).any(|y| {
                (0..width).any(|x| {
                    let cell = &buffer[(x, y)];
                    cell.symbol() == ">" && cell.fg == Color::Red
                })
            });
            assert!(
                marker_is_red,
                "selection marker is not red at {width}x{height}"
            );
            if width >= 80 {
                assert!(
                    rendered.contains("Updated"),
                    "missing Updated header at {width}x{height}"
                );
            }
            if width >= 100 {
                let header: String = (0..width).map(|x| buffer[(x, 5)].symbol()).collect();
                let session_at = header.find("Session").unwrap();
                let preview_at = header.find("Preview").unwrap();
                let path_at = header.find("Path").unwrap();
                let updated_at = header.find("Updated").unwrap();
                assert!(session_at < preview_at && preview_at < path_at && path_at < updated_at);
            }
        }
    }

    #[test]
    fn table_backend_keeps_columns_stable_for_long_preview() {
        let session = SessionEntry {
            provider: "codex".into(),
            session_id: "stable-column".into(),
            cwd: PathBuf::from("/tmp/project"),
            thread_name: Some("Short".into()),
            preview: Some("中文中文中文中文中文中文中文中文中文中文".into()),
            last_active: None,
            source_file: PathBuf::new(),
            resumable: true,
        };
        let sessions = vec![session];
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_ui(
                    frame,
                    UiRender {
                        sessions: &sessions,
                        filtered: &sessions,
                        query: "",
                        mode: UiMode::Browsing,
                        project_view: false,
                        selected: 0,
                        scroll_offset: 0,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..120).map(|x| buffer[(x, 6)].symbol()).collect();
        assert!(row.contains("Short"));
        assert!(row.contains("/tmp/project"));
        assert!(row.contains("—"));
    }

    #[test]
    fn table_backend_scrolls_visible_rows_with_selection() {
        let sessions: Vec<_> = (1..=36)
            .map(|number| SessionEntry {
                provider: "codex".into(),
                session_id: format!("session-{number:02}"),
                cwd: PathBuf::from("/tmp/project"),
                thread_name: Some(format!("Session {number:02}")),
                preview: None,
                last_active: None,
                source_file: PathBuf::new(),
                resumable: true,
            })
            .collect();
        let backend = TestBackend::new(120, 19);
        let mut terminal = Terminal::new(backend).unwrap();

        let render = |terminal: &mut Terminal<TestBackend>, selected, scroll_offset| {
            let mut rendered_view = (0, 0);
            terminal
                .draw(|frame| {
                    rendered_view = draw_ui(
                        frame,
                        UiRender {
                            sessions: &sessions,
                            filtered: &sessions,
                            query: "",
                            mode: UiMode::Browsing,
                            project_view: false,
                            selected,
                            scroll_offset,
                        },
                    );
                })
                .unwrap();
            rendered_view
        };
        let row_text = |terminal: &Terminal<TestBackend>, y| -> String {
            (0..120)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect()
        };

        let (capacity, offset) = render(&mut terminal, 9, 0);
        assert_eq!(page_capacity(40u16.saturating_sub(6), 1), 15);
        assert_eq!((capacity, offset), (10, 0));
        assert!(row_text(&terminal, 6).contains("Session 01"));
        assert!(row_text(&terminal, 15).contains("Session 10"));
        let initial_visible = (6..16)
            .map(|y| row_text(&terminal, y))
            .collect::<Vec<_>>()
            .join("");
        assert!(!initial_visible.contains("Session 11"));

        let (selected, offset) = move_selection(9, 0, 1, capacity, sessions.len());
        assert_eq!((selected, offset), (10, 1));
        let (capacity, offset) = render(&mut terminal, selected, offset);
        let after_down = terminal.backend().buffer();
        let visible = (6..16)
            .map(|y| row_text(&terminal, y))
            .collect::<Vec<_>>()
            .join("");
        assert!(!visible.contains("Session 01"));
        for number in 2..=11 {
            assert!(visible.contains(&format!("Session {number:02}")));
        }
        let selected_row = 15;
        assert!(row_text(&terminal, selected_row).contains("Session 11"));
        assert!((0..120).any(|x| {
            let cell = &after_down[(x, selected_row)];
            cell.symbol() == ">" && cell.fg == Color::Red
        }));
        let (selected, offset) = move_selection(selected, offset, 1, capacity, sessions.len());
        assert_eq!((selected, offset), (11, 2));
        let (capacity, offset) = render(&mut terminal, selected, offset);
        let visible = (6..16)
            .map(|y| row_text(&terminal, y))
            .collect::<Vec<_>>()
            .join("");
        assert!(!visible.contains("Session 02"));
        for number in 3..=12 {
            assert!(visible.contains(&format!("Session {number:02}")));
        }

        let (selected, offset) = move_selection(selected, offset, -1, capacity, sessions.len());
        assert_eq!((selected, offset), (10, 2));
        let _ = render(&mut terminal, selected, offset);
        assert!(row_text(&terminal, 14).contains("Session 11"));
    }

    #[test]
    fn table_backend_keeps_every_selected_session_visible_while_scrolling() {
        let sessions: Vec<_> = (1..=100)
            .map(|number| SessionEntry {
                provider: "codex".into(),
                session_id: format!("session-{number:03}"),
                cwd: PathBuf::from("/tmp/project"),
                thread_name: Some(format!("Session {number:03}")),
                preview: None,
                last_active: None,
                source_file: PathBuf::new(),
                resumable: true,
            })
            .collect();
        for (width, height) in [(120, 19), (55, 15), (30, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let (mut selected, mut offset) = (0, 0);
            for step in 0..199 {
                let mut view = (0, 0);
                terminal
                    .draw(|frame| {
                        view = draw_ui(
                            frame,
                            UiRender {
                                sessions: &sessions,
                                filtered: &sessions,
                                query: "",
                                mode: UiMode::Browsing,
                                project_view: false,
                                selected,
                                scroll_offset: offset,
                            },
                        );
                    })
                    .unwrap();
                let (capacity, start) = view;
                let buffer = terminal.backend().buffer();
                let label = format!("Session {:03}", selected + 1);
                let found = (0..height).any(|y| {
                    let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                    row.contains(&label)
                        && (0..width).any(|x| {
                            buffer[(x, y)].symbol() == ">" && buffer[(x, y)].fg == Color::Red
                        })
                });
                assert!(
                    found,
                    "{width}x{height} step {step}: selected {label} not visibly highlighted"
                );
                assert!(start <= selected && selected < start + capacity);
                if step < 99 {
                    (selected, offset) =
                        move_selection(selected, start, 1, capacity, sessions.len());
                } else {
                    (selected, offset) =
                        move_selection(selected, start, -1, capacity, sessions.len());
                }
            }
            assert_eq!(selected, 0);
        }
    }
}
