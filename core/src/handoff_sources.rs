//! Чтение чатов из харнессов, которые хранят сессии не в JSONL-файлах.
//!
//! Модуль намеренно не знает про `Config` и UI: он отдаёт те же брифы и
//! `Chat`, что и файловые источники в `handoff.rs`. Соединения SQLite только
//! read-only, чужие базы не пишутся.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use crate::handoff::Chat;
use crate::ingest::Event;

fn open_readonly(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("не удалось открыть {}: {error}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(conn)
}

fn sql_error(error: rusqlite::Error) -> String {
    error.to_string()
}

// -- OpenCode -----------------------------------------------------------------

/// `opencode.db` лежит в XDG-каталоге данных; на Windows это обычно
/// `~/.local/share/opencode`, но старые сборки могли писать в AppData.
pub fn opencode_db(home: &Path) -> Option<PathBuf> {
    let mut candidates = vec![
        home.join(".local").join("share").join("opencode").join("opencode.db"),
        home.join("AppData").join("Roaming").join("opencode").join("opencode.db"),
        home.join("AppData").join("Local").join("opencode").join("opencode.db"),
    ];
    // Явный home важнее env: так тесты не читают живую базу владельца.
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(xdg).join("opencode").join("opencode.db"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn opencode_event(kind: &str, data: &str) -> Option<Event> {
    let value: Value = serde_json::from_str(data).ok()?;
    let role = match kind {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    let text = if role == "user" {
        value.get("text").and_then(Value::as_str)?.to_owned()
    } else {
        let parts = value.get("content").and_then(Value::as_array)?;
        parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if text.trim().is_empty() {
        return None;
    }
    let ts = value
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(Value::as_i64)
        .map(|ms| ms / 1000);
    Some((role.to_owned(), text, ts))
}

fn opencode_load(
    conn: &Connection,
    id: &str,
    title: Option<String>,
    directory: Option<String>,
    updated_ms: i64,
) -> Result<Option<Chat>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT type, data FROM session_message
             WHERE session_id = ?1
             ORDER BY seq ASC, time_created ASC",
        )
        .map_err(sql_error)?;
    let rows = stmt
        .query_map(params![id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error)?;
    let mut events = Vec::new();
    for row in rows {
        let (kind, data) = row.map_err(sql_error)?;
        if let Some(event) = opencode_event(&kind, &data) {
            events.push(event);
        }
    }
    if !events.iter().any(|(role, _, _)| role == "user") {
        return Ok(None);
    }
    Ok(Some(Chat {
        id: format!("opencode:{id}"),
        harness: "opencode".to_owned(),
        harness_name: "OpenCode".to_owned(),
        locator: id.to_owned(),
        title: title.unwrap_or_default(),
        cwd: directory.unwrap_or_default(),
        mtime: updated_ms / 1000,
        events,
    }))
}

pub fn opencode_chats(db: &Path, cutoff: i64, limit: usize) -> Result<Vec<Value>, String> {
    let conn = open_readonly(db)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, title, directory, time_updated FROM session_v2
             WHERE parent_id IS NULL AND time_archived IS NULL AND time_updated >= ?1
             ORDER BY time_updated DESC",
        )
        .map_err(sql_error)?;
    let rows = stmt
        .query_map(params![cutoff * 1000], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(sql_error)?;
    let mut chats = Vec::new();
    for row in rows {
        if chats.len() >= limit {
            break;
        }
        let Ok((id, title, directory, updated_ms)) = row else {
            continue;
        };
        if let Ok(Some(chat)) = opencode_load(&conn, &id, title, directory, updated_ms) {
            chats.push(chat.brief());
        }
    }
    Ok(chats)
}

pub fn opencode_chat(db: &Path, session_id: &str) -> Result<Chat, String> {
    let conn = open_readonly(db)?;
    let row = conn
        .query_row(
            "SELECT title, directory, time_updated FROM session_v2 WHERE id = ?1",
            params![session_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((title, directory, updated_ms)) = row else {
        return Err("неизвестный чат".to_owned());
    };
    opencode_load(&conn, session_id, title, directory, updated_ms)?
        .ok_or_else(|| "в чате нет сообщений".to_owned())
}

// -- DeepSeek Harness ---------------------------------------------------------

pub fn dsh_root(home: &Path) -> PathBuf {
    home.join(".dsh").join("sessions")
}

fn dsh_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(workspaces) = fs::read_dir(root) else {
        return files;
    };
    for workspace in workspaces.flatten() {
        let workspace = workspace.path();
        if !workspace.is_dir() {
            continue;
        }
        let Ok(sessions) = fs::read_dir(&workspace) else {
            continue;
        };
        for session in sessions.flatten() {
            let session = session.path();
            if !session.is_dir() {
                continue;
            }
            for name in ["session.v4.jsonl.zstd", "session.v3.jsonl.zstd"] {
                let path = session.join(name);
                if path.is_file() {
                    files.push(path);
                    break;
                }
            }
        }
    }
    files
}

fn mtime_seconds(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

fn dsh_lines(path: &Path) -> Result<impl BufRead, String> {
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let decoder = zstd::stream::read::Decoder::new(BufReader::new(file)).map_err(|error| error.to_string())?;
    Ok(BufReader::new(decoder))
}

fn dsh_user_text(value: &Value) -> Option<String> {
    let data = value.get("data")?;
    if let Some(kind) = data
        .get("source")
        .and_then(|source| source.get("kind"))
        .and_then(Value::as_str)
    {
        if kind != "user" {
            return None;
        }
    }
    let texts = data
        .get("content")?
        .as_array()?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if texts.trim().is_empty() {
        None
    } else {
        Some(texts)
    }
}

fn dsh_assistant_text(value: &Value) -> Option<String> {
    let content = value
        .get("data")?
        .get("message")?
        .get("content")?
        .as_array()?;
    let texts = content
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if texts.trim().is_empty() {
        None
    } else {
        Some(texts)
    }
}

fn dsh_event(value: &Value) -> Option<Event> {
    let (role, text) = match value.get("type").and_then(Value::as_str)? {
        "user/message" => ("user", dsh_user_text(value)?),
        "assistant/message" => ("assistant", dsh_assistant_text(value)?),
        _ => return None,
    };
    let ts = value.get("time").and_then(Value::as_i64).map(|ms| ms / 1000);
    Some((role.to_owned(), text, ts))
}

fn dsh_title(value: &Value) -> Option<String> {
    value
        .get("data")
        .and_then(|data| data.get("title"))
        .or_else(|| value.get("title"))
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .map(str::to_owned)
}

fn dsh_head(path: &Path, root: &Path) -> Result<Option<Chat>, String> {
    let mut lines = dsh_lines(path)?;
    let mut header: Option<Value> = None;
    let mut title = String::new();
    let mut fallback = String::new();
    let mut cwd = String::new();
    let mut has_user = false;
    let mut after_user = 0usize;
    let mut line = String::new();
    for _ in 0..400 {
        line.clear();
        // DSH дописывает активную сессию, поэтому хвост бывает обрезан: с найденным
        // заголовком чат уже собран (как в dsh_full), а без него читать нечего.
        match lines.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) if header.is_some() => break,
            Err(error) => return Err(error.to_string()),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                cwd = value.get("cwd").and_then(Value::as_str).unwrap_or("").to_owned();
                header = Some(value);
            }
            Some("session/title") => {
                if title.is_empty() {
                    title = dsh_title(&value).unwrap_or_default();
                }
            }
            Some("user/message") => {
                if let Some(text) = dsh_user_text(&value) {
                    has_user = true;
                    if fallback.is_empty() {
                        fallback = text;
                    }
                }
            }
            _ => {}
        }
        if header.is_some() && has_user {
            after_user += 1;
            // `session/title` обычно идёт после первого хода; ждём его,
            // но не распаковываем весь длинный чат ради списка.
            if !title.is_empty() || after_user > 200 {
                break;
            }
        }
    }
    let Some(header) = header else {
        return Ok(None);
    };
    let title = if title.is_empty() { fallback } else { title };
    if header.get("origin").and_then(Value::as_str) == Some("subagent") || !has_user {
        return Ok(None);
    }
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let mtime = mtime_seconds(path).unwrap_or_else(crate::handoff::now_seconds);
    Ok(Some(Chat {
        id: format!("deepseek:{relative}"),
        harness: "deepseek".to_owned(),
        harness_name: "DeepSeek Harness".to_owned(),
        locator: relative,
        title,
        cwd,
        mtime,
        events: Vec::new(),
    }))
}

pub fn dsh_chats(root: &Path, cutoff: i64, limit: usize) -> Result<Vec<Value>, String> {
    let mut files: Vec<(i64, PathBuf)> = dsh_files(root)
        .into_iter()
        .filter_map(|path| mtime_seconds(&path).filter(|mtime| *mtime >= cutoff).map(|mtime| (mtime, path)))
        .collect();
    files.sort_by_key(|entry| -entry.0);
    let mut chats = Vec::new();
    for (_, path) in files {
        if chats.len() >= limit {
            break;
        }
        if let Ok(Some(chat)) = dsh_head(&path, root) {
            chats.push(chat.brief_with(None, None));
        }
    }
    Ok(chats)
}

fn dsh_full(path: &Path, relative: &str) -> Result<Chat, String> {
    let mut lines = dsh_lines(path)?;
    let mut events = Vec::new();
    let mut title = String::new();
    let mut fallback = String::new();
    let mut cwd = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) if !events.is_empty() => break,
            Err(error) => return Err(error.to_string()),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                cwd = value.get("cwd").and_then(Value::as_str).unwrap_or("").to_owned();
            }
            Some("session/title") => {
                if title.is_empty() {
                    title = dsh_title(&value).unwrap_or_default();
                }
            }
            _ => {
                if let Some(event) = dsh_event(&value) {
                    if event.0 == "user" && fallback.is_empty() {
                        fallback = event.1.clone();
                    }
                    events.push(event);
                }
            }
        }
    }
    if !events.iter().any(|(role, _, _)| role == "user") {
        return Err("в чате нет сообщений".to_owned());
    }
    let title = if title.is_empty() { fallback } else { title };
    Ok(Chat {
        id: format!("deepseek:{relative}"),
        harness: "deepseek".to_owned(),
        harness_name: "DeepSeek Harness".to_owned(),
        locator: relative.to_owned(),
        title,
        cwd,
        mtime: mtime_seconds(path).unwrap_or_else(crate::handoff::now_seconds),
        events,
    })
}

pub fn dsh_chat(root: &Path, relative: &str) -> Result<Chat, String> {
    let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let root = root.canonicalize().map_err(|_| "неизвестный чат".to_owned())?;
    let path = path.canonicalize().map_err(|_| "неизвестный чат".to_owned())?;
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    if !path.starts_with(&root) || !path.is_file() || !name.starts_with("session.") || !name.ends_with(".jsonl.zstd") {
        return Err("неизвестный чат".to_owned());
    }
    dsh_full(&path, relative)
}

// -- Copilot ------------------------------------------------------------------

pub fn copilot_db(home: &Path) -> PathBuf {
    home.join(".copilot").join("session-store.db")
}

fn copilot_load(
    conn: &Connection,
    session_id: &str,
    cwd: Option<String>,
    summary: Option<String>,
    mtime: i64,
) -> Result<Option<Chat>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT user_message, assistant_response,
                    CAST(strftime('%s', timestamp) AS INTEGER)
             FROM turns WHERE session_id = ?1 ORDER BY turn_index ASC",
        )
        .map_err(sql_error)?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(sql_error)?;
    let mut events = Vec::new();
    for row in rows {
        let (user, assistant, ts) = row.map_err(sql_error)?;
        if let Some(text) = user.filter(|text| !text.trim().is_empty()) {
            events.push(("user".to_owned(), text, ts));
        }
        if let Some(text) = assistant.filter(|text| !text.trim().is_empty()) {
            events.push(("assistant".to_owned(), text, ts));
        }
    }
    if !events.iter().any(|(role, _, _)| role == "user") {
        return Ok(None);
    }
    let title = summary
        .or_else(|| events.iter().find(|(role, _, _)| role == "user").map(|(_, text, _)| text.clone()))
        .unwrap_or_default();
    Ok(Some(Chat {
        id: format!("copilot:{session_id}"),
        harness: "copilot".to_owned(),
        harness_name: "Copilot".to_owned(),
        locator: session_id.to_owned(),
        title,
        cwd: cwd.unwrap_or_default(),
        mtime,
        events,
    }))
}

pub fn copilot_chats(db: &Path, cutoff: i64, limit: usize) -> Result<Vec<Value>, String> {
    let conn = open_readonly(db)?;
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.cwd, s.summary,
                    COALESCE(CAST(strftime('%s', s.updated_at) AS INTEGER), 0)
             FROM sessions s
             WHERE EXISTS (SELECT 1 FROM turns t WHERE t.session_id = s.id)
               AND COALESCE(CAST(strftime('%s', s.updated_at) AS INTEGER), 0) >= ?1
             ORDER BY s.updated_at DESC",
        )
        .map_err(sql_error)?;
    let rows = stmt
        .query_map(params![cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(sql_error)?;
    let mut chats = Vec::new();
    for row in rows {
        if chats.len() >= limit {
            break;
        }
        let Ok((id, cwd, summary, mtime)) = row else {
            continue;
        };
        if let Ok(Some(chat)) = copilot_load(&conn, &id, cwd, summary, mtime) {
            chats.push(chat.brief());
        }
    }
    Ok(chats)
}

pub fn copilot_chat(db: &Path, session_id: &str) -> Result<Chat, String> {
    let conn = open_readonly(db)?;
    let row = conn
        .query_row(
            "SELECT cwd, summary,
                    COALESCE(CAST(strftime('%s', updated_at) AS INTEGER), 0)
             FROM sessions WHERE id = ?1",
            params![session_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((cwd, summary, mtime)) = row else {
        return Err("неизвестный чат".to_owned());
    };
    copilot_load(&conn, session_id, cwd, summary, mtime)?
        .ok_or_else(|| "в чате нет сообщений".to_owned())
}

// -- Antigravity --------------------------------------------------------------

pub fn antigravity_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".gemini").join("antigravity"),
        home.join(".gemini").join("antigravity-ide"),
    ]
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn antigravity_cwd(value: &str) -> String {
    let Ok(uris) = serde_json::from_str::<Value>(value) else {
        return String::new();
    };
    let Some(uri) = uris.as_array().and_then(|items| items.first()).and_then(Value::as_str) else {
        return String::new();
    };
    let rest = uri.strip_prefix("file:///").unwrap_or(uri);
    percent_decode(rest).trim_start_matches('/').to_owned()
}

fn strip_tag_block(mut text: String, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    while let Some(start) = text.find(&open) {
        let Some(relative_end) = text[start..].find(&close) else {
            break;
        };
        let end = start + relative_end + close.len();
        text.replace_range(start..end, "");
    }
    text
}

fn clean_antigravity_user(text: &str) -> String {
    let mut cleaned = text.to_owned();
    if let Some(start) = cleaned.find("<USER_REQUEST>") {
        if let Some(relative_end) = cleaned[start..].find("</USER_REQUEST>") {
            let content_start = start + "<USER_REQUEST>".len();
            let content_end = start + relative_end;
            cleaned = cleaned[content_start..content_end].to_owned();
        }
    }
    cleaned = strip_tag_block(cleaned, "ADDITIONAL_METADATA");
    cleaned = strip_tag_block(cleaned, "USER_SETTINGS_CHANGE");
    cleaned.trim().to_owned()
}

fn safe_component(id: &str) -> bool {
    let mut components = Path::new(id).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn antigravity_transcript(root: &Path, conversation_id: &str) -> Option<PathBuf> {
    if !safe_component(conversation_id) {
        return None;
    }
    let brain = root.join("brain");
    let logs = brain
        .join(conversation_id)
        .join(".system_generated")
        .join("logs");
    let brain = brain.canonicalize().ok()?;
    for name in ["transcript.jsonl", "transcript_full.jsonl"] {
        let path = logs.join(name);
        let Ok(path) = path.canonicalize() else {
            continue;
        };
        if path.starts_with(&brain) && path.is_file() {
            return Some(path);
        }
    }
    None
}

fn antigravity_events(path: &Path) -> Result<Vec<Event>, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut events = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let content = value.get("content").and_then(Value::as_str).unwrap_or("");
        match kind {
            "USER_INPUT" => {
                let cleaned = clean_antigravity_user(content);
                if !cleaned.is_empty() {
                    events.push(("user".to_owned(), cleaned, None));
                }
            }
            "PLANNER_RESPONSE" => {
                let cleaned = content.trim();
                if !cleaned.is_empty() {
                    events.push(("assistant".to_owned(), cleaned.to_owned(), None));
                }
            }
            _ => {}
        }
    }
    Ok(events)
}

fn antigravity_load(
    root: &Path,
    conversation_id: &str,
    title: String,
    cwd: String,
    mtime: i64,
) -> Result<Option<Chat>, String> {
    let Some(transcript) = antigravity_transcript(root, conversation_id) else {
        return Ok(None);
    };
    let events = antigravity_events(&transcript)?;
    if !events.iter().any(|(role, _, _)| role == "user") {
        return Ok(None);
    }
    Ok(Some(Chat {
        id: format!("antigravity:{conversation_id}"),
        harness: "antigravity".to_owned(),
        harness_name: "Antigravity".to_owned(),
        locator: conversation_id.to_owned(),
        title,
        cwd,
        mtime,
        events,
    }))
}

pub fn antigravity_chats(home: &Path, cutoff: i64, limit: usize) -> Result<Vec<Value>, String> {
    let mut chats = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut found_db = false;
    for root in antigravity_roots(home) {
        let db = root.join("conversation_summaries.db");
        if !db.is_file() {
            continue;
        }
        found_db = true;
        let conn = open_readonly(&db)?;
        let mut stmt = conn
            .prepare(
                "SELECT conversation_id, title, workspace_uris
                 FROM conversation_summaries
                 WHERE COALESCE(parent_conversation_id, '') = ''
                   AND COALESCE(nesting_depth, 0) = 0
                 ORDER BY last_modified_time DESC",
            )
            .map_err(sql_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(sql_error)?;
        for row in rows {
            if chats.len() >= limit {
                break;
            }
            let Ok((id, title, uris)) = row else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            let Some(transcript) = antigravity_transcript(&root, &id) else {
                continue;
            };
            let mtime = mtime_seconds(&transcript).unwrap_or(0);
            if mtime < cutoff {
                continue;
            }
            let cwd = uris.as_deref().map(antigravity_cwd).unwrap_or_default();
            if let Ok(Some(chat)) = antigravity_load(&root, &id, title.unwrap_or_default(), cwd, mtime) {
                chats.push(chat.brief());
            }
        }
    }
    if !found_db {
        return Ok(Vec::new());
    }
    chats.sort_by(|left, right| {
        right["mtime"].as_i64().unwrap_or(0).cmp(&left["mtime"].as_i64().unwrap_or(0))
    });
    chats.truncate(limit);
    Ok(chats)
}

pub fn antigravity_chat(home: &Path, conversation_id: &str) -> Result<Chat, String> {
    if !safe_component(conversation_id) {
        return Err("неизвестный чат".to_owned());
    }
    for root in antigravity_roots(home) {
        let Some(transcript) = antigravity_transcript(&root, conversation_id) else {
            continue;
        };
        let mut title = String::new();
        let mut cwd = String::new();
        let db = root.join("conversation_summaries.db");
        if db.is_file() {
            if let Ok(conn) = open_readonly(&db) {
                if let Ok(row) = conn.query_row(
                    "SELECT title, workspace_uris FROM conversation_summaries WHERE conversation_id = ?1",
                    params![conversation_id],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
                ) {
                    title = row.0.unwrap_or_default();
                    cwd = row.1.as_deref().map(antigravity_cwd).unwrap_or_default();
                }
            }
        }
        let mtime = mtime_seconds(&transcript).unwrap_or_else(crate::handoff::now_seconds);
        return antigravity_load(&root, conversation_id, title, cwd, mtime)?
            .ok_or_else(|| "в чате нет сообщений".to_owned());
    }
    Err("неизвестный чат".to_owned())
}
