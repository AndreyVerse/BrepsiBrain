//! Перенос чатов между харнессами: список харнессов и активные чаты.
//!
//! Порт `memory_base/handoff.py` начинается с чтения: сами файлы сессий уже
//! разбираются ingest-парсерами, здесь они превращаются в брифы для панели.
//! Запись файла переноса и запуск цели — следующие части этого модуля.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::ingest::{claude_event, codex_event, codex_meta, kimi_cwd, kimi_event, mask_secrets, truncate, Event};
use crate::llm::ChatClient;
use crate::proc::quiet;

pub const DEFAULT_HOURS: i64 = 48;
pub const MAX_HOURS: i64 = 24 * 90;
pub const MAX_CHATS: usize = 40;
/// Потолок одной группы: вкладка грузит все источники параллельно.
pub const MAX_GROUP_CHATS: usize = 20;
pub const TITLE_CHARS: usize = 120;
pub const FULL_MAX_CHARS: usize = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    App,
    Terminal,
    Clipboard,
}

#[derive(Debug, Clone)]
pub struct Harness {
    pub id: String,
    pub name: String,
    pub images: Vec<String>,
    pub delivery: Delivery,
    pub can_source: bool,
    /// Смена аккаунта умеет определять только четыре старых источника.
    pub can_switch: bool,
    /// Цель добавлена пользователем в `data/harnesses.json`.
    pub custom: bool,
    /// Программа, которую надо открыть перед вставкой промпта; иначе только буфер.
    pub command: Option<PathBuf>,
}

/// Идентификаторы и подписи встроенных целей. Cursor сюда осознанно не входит:
/// его чаты не открываются ни из CLI, ни по deep link, а источником они и не
/// были. Добавить свою цель можно в «Перенос чатов» — она ляжет в
/// `data/harnesses.json` рядом с базой.
const BUILTIN: &[(&str, &str, &[&str], Delivery, bool)] = &[
    ("claude", "Claude Code", &["claude.exe", "claude"], Delivery::App, true),
    ("codex", "Codex", &["codex.exe", "codex"], Delivery::Terminal, true),
    ("kimi", "Kimi Code", &["kimi code.exe", "kimi-code", "kimi"], Delivery::Clipboard, true),
    ("qoder", "Qoder", &["qoder.exe", "qoder"], Delivery::Clipboard, true),
    ("antigravity", "Antigravity", &["antigravity.exe", "antigravity"], Delivery::Clipboard, true),
    ("opencode", "OpenCode", &["opencode.exe", "opencode", "opencode desktop.exe"], Delivery::Clipboard, true),
    ("deepseek", "DeepSeek Harness", &["dsh.exe", "dsh", "deepseek harness.exe"], Delivery::Clipboard, true),
    ("chatgpt", "ChatGPT", &["chatgpt.exe", "chatgpt"], Delivery::Clipboard, false),
    ("gigacode", "GigaCode", &["gigacode.exe", "gigacode"], Delivery::Clipboard, false),
    ("copilot", "GitHub Copilot", &["copilot.exe", "github copilot.exe", "copilot"], Delivery::Clipboard, true),
];

fn builtin() -> Vec<Harness> {
    BUILTIN
        .iter()
        .map(|(id, name, images, delivery, can_source)| Harness {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
            images: images.iter().map(|image| (*image).to_owned()).collect(),
            delivery: *delivery,
            can_source: *can_source,
            can_switch: matches!(*id, "claude" | "codex" | "kimi" | "qoder"),
            custom: false,
            command: None,
        })
        .collect()
}

fn custom_path(root: &Path) -> PathBuf {
    root.join("data").join("harnesses.json")
}

/// Свои цели: массив объектов `{id, name, command?, images?}`. Битый файл не
/// должен ломать вкладку — он читается как «своих пока нет».
fn custom_entries(root: &Path) -> Vec<Value> {
    fs::read_to_string(custom_path(root))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| {
            entry.get("id").and_then(Value::as_str).is_some_and(|id| !id.is_empty())
                && entry.get("name").and_then(Value::as_str).is_some_and(|name| !name.trim().is_empty())
        })
        .collect()
}

fn custom_harnesses(root: &Path) -> Vec<Harness> {
    custom_entries(root)
        .into_iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let name = entry.get("name")?.as_str()?.to_owned();
            let command = entry
                .get("command")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .map(|command| crate::config::windows_path(command));
            let mut images: Vec<String> = entry
                .get("images")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).map(str::to_lowercase).collect())
                .unwrap_or_default();
            if images.is_empty() {
                if let Some(exe) = command.as_ref().and_then(|path| path.file_name()).and_then(|name| name.to_str()) {
                    images.push(exe.to_lowercase());
                }
            }
            Some(Harness { id, name, images, delivery: Delivery::Clipboard, can_source: false, can_switch: false, custom: true, command })
        })
        .collect()
}

fn write_custom(root: &Path, entries: &[Value]) -> Result<(), String> {
    let path = custom_path(root);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&Value::Array(entries.to_vec())).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    fs::rename(&tmp, &path).map_err(|error| error.to_string())
}

/// `id` попадает в localStorage и в имя файла переноса, поэтому только latin,
/// цифры и дефис: кириллица даёт стабильный `target-<hash>`.
fn make_id(name: &str, taken: &[String]) -> String {
    let base: String = name
        .to_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    let base = if base.is_empty() { format!("target-{:x}", md5::compute(name.as_bytes())) } else { base };
    if !taken.iter().any(|id| id == &base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|id| !taken.contains(id)).unwrap_or(base)
}

pub fn add_custom(root: &Path, name: &str, command: Option<&str>) -> Result<Harness, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err("название: от 1 до 40 символов".to_owned());
    }
    let command = command.map(str::trim).filter(|command| !command.is_empty());
    if let Some(path) = command {
        if !crate::config::windows_path(path).is_file() {
            return Err("файл программы не найден".to_owned());
        }
    }
    let mut entries = custom_entries(root);
    let mut taken: Vec<String> = builtin().iter().map(|harness| harness.id.clone()).collect();
    taken.extend(custom_harnesses(root).into_iter().map(|harness| harness.id));
    let id = make_id(name, &taken);
    let mut entry = json!({"id": id, "name": name});
    if let Some(command) = command {
        entry["command"] = Value::String(command.to_owned());
    }
    entries.push(entry);
    write_custom(root, &entries)?;
    Ok(custom_harnesses(root).into_iter().find(|harness| harness.id == id).ok_or_else(|| "не удалось сохранить".to_owned())?)
}

pub fn remove_custom(root: &Path, id: &str) -> Result<bool, String> {
    let mut entries = custom_entries(root);
    let before = entries.len();
    entries.retain(|entry| entry.get("id").and_then(Value::as_str) != Some(id));
    if entries.len() == before {
        return Ok(false);
    }
    write_custom(root, &entries)?;
    Ok(true)
}

pub fn all(root: &Path) -> Vec<Harness> {
    let mut harnesses = builtin();
    harnesses.extend(custom_harnesses(root));
    harnesses
}

pub fn find(root: &Path, id: &str) -> Option<Harness> {
    all(root).into_iter().find(|harness| harness.id == id)
}

pub fn running_images() -> Option<HashSet<String>> {
    let output = quiet("tasklist").args(["/FO", "CSV", "/NH"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut images = HashSet::new();
    for line in text.lines() {
        if let Some(name) = line.split("\",\"").next() {
            images.insert(name.trim_matches('"').to_lowercase());
        }
    }
    Some(images)
}

fn delivery(harness: &Harness, images: &Option<HashSet<String>>) -> String {
    match harness.delivery {
        Delivery::App => "app".to_owned(),
        Delivery::Clipboard => "clipboard".to_owned(),
        Delivery::Terminal => {
            let _ = images;
            "clipboard".to_owned()
        }
    }
}

/// Следы установки встроенного харнесса: папка данных в профиле, папка программы
/// в `%LOCALAPPDATA%\Programs` и префикс пакета Microsoft Store. Пути проверены на
/// живой машине для всех, кроме GigaCode и ChatGPT [VERIFY: их папки установки].
fn install_markers(id: &str) -> (&'static [&'static str], &'static [&'static str], &'static [&'static str]) {
    match id {
        "claude" => (&[".claude"], &["Claude", "claude"], &["Claude_", "AnthropicPBC.Claude"]),
        "codex" => (&[".codex"], &[], &["OpenAI.Codex"]),
        "kimi" => (&[".kimi-code"], &["Kimi Code"], &[]),
        "qoder" => (&[".qoder"], &["Qoder"], &[]),
        "antigravity" => (&[".gemini/antigravity", ".gemini/antigravity-ide"], &["Antigravity"], &[]),
        "opencode" => (&[".local/share/opencode", ".config/opencode", "AppData/Roaming/opencode"], &["ai.opencode.desktop", "OpenCode"], &[]),
        "deepseek" => (&[".dsh"], &["DeepSeek Harness"], &[]),
        "copilot" => (&[".copilot"], &[], &[]),
        "chatgpt" => (&[], &["ChatGPT"], &["OpenAI.ChatGPT"]),
        "gigacode" => (&[".gigacode"], &["GigaCode"], &[]),
        _ => (&[], &[], &[]),
    }
}

/// Программа из `images` лежит в одной из папок `PATH`.
fn on_path(images: &[String]) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        images.iter().any(|image| {
            let file = dir.join(image);
            file.is_file() || (!image.contains('.') && (dir.join(format!("{image}.exe")).is_file() || dir.join(format!("{image}.cmd")).is_file()))
        })
    })
}

/// Есть ли харнесс в системе. Свои цели пользователь добавил сам — они есть всегда.
/// Запущенный процесс тоже считается: портативную копию иначе не увидеть.
fn installed(harness: &Harness, home: &Path, local: &Path, images: &Option<HashSet<String>>) -> bool {
    if harness.custom {
        return true;
    }
    if images.as_ref().is_some_and(|running| harness.images.iter().any(|image| running.contains(image))) {
        return true;
    }
    let (profile, programs, packages) = install_markers(&harness.id);
    if profile.iter().any(|relative| home.join(relative).is_dir()) {
        return true;
    }
    if programs.iter().any(|name| local.join("Programs").join(name).is_dir()) {
        return true;
    }
    if !packages.is_empty() {
        let found = fs::read_dir(local.join("Packages")).ok().is_some_and(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                packages.iter().any(|prefix| name.starts_with(prefix))
            })
        });
        if found {
            return true;
        }
    }
    if harness.id == "claude" && app_registered("claude") {
        return true;
    }
    on_path(&harness.images)
}

/// Список для вкладки переноса: харнессы, которых в системе нет, не показываются
/// ни источником, ни целью.
pub fn harnesses_json(root: &Path, images: Option<HashSet<String>>) -> Value {
    let home = home();
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData").join("Local"));
    harnesses_json_in(root, &home, &local, images)
}

/// `home` и `local` (`%LOCALAPPDATA%`) — параметры, чтобы тест не зависел от машины.
pub fn harnesses_json_in(root: &Path, home: &Path, local: &Path, images: Option<HashSet<String>>) -> Value {
    json!({
        "harnesses": all(root).iter().filter(|harness| installed(harness, home, local, &images)).map(|harness| json!({
            "id": harness.id,
            "name": harness.name,
            "running": images.as_ref().map(|images| {
                harness.images.iter().any(|image| images.contains(image))
            }),
            "can_source": harness.can_source,
            "can_switch": harness.can_switch,
            "can_target": true,
            "custom": harness.custom,
            "can_delete": harness.custom,
            "command": harness.command.as_ref().map(|path| path.display().to_string()),
            "delivery": delivery(harness, &images),
        })).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Clone)]
pub struct Chat {
    pub id: String,
    pub harness: String,
    pub harness_name: String,
    /// Имя файла или id сессии для шапки файла переноса.
    pub locator: String,
    pub title: String,
    pub cwd: String,
    pub mtime: i64,
    pub events: Vec<Event>,
}

impl Chat {
    pub fn title(&self) -> String {
        let first = self.events.iter().find(|(role, _, _)| role == "user").map(|(_, text, _)| text.as_str()).unwrap_or("");
        let source = if self.title.trim().is_empty() { first } else { self.title.as_str() };
        let line = source.lines().find(|line| !line.trim().is_empty()).unwrap_or("без названия").trim();
        truncate(&mask_secrets(line), TITLE_CHARS)
    }

    pub fn brief(&self) -> Value {
        self.brief_with(
            Some(self.events.iter().filter(|(role, _, _)| role == "user").count()),
            Some(self.events.iter().map(|(_, text, _)| text.chars().count()).sum()),
        )
    }

    /// `None` — источник не считал значение на этапе списка.
    pub fn brief_with(&self, turns: Option<usize>, chars: Option<usize>) -> Value {
        json!({
            "id": self.id,
            "harness": self.harness,
            "title": self.title(),
            "cwd": self.cwd,
            "mtime": self.mtime,
            "turns": turns,
            "chars": chars,
            "fits_full": chars.map(|chars| chars <= FULL_MAX_CHARS),
        })
    }

    pub fn transcript(&self) -> String {
        mask_secrets(&self.events.iter().map(|(role, text, _)| {
            format!("### {}\n\n{}", if role == "user" { "Пользователь" } else { "Агент" }, text.trim())
        }).collect::<Vec<_>>().join("\n\n"))
    }
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default()
}

fn root_for(home: &Path, harness: &str) -> Option<PathBuf> {
    match harness {
        "claude" => Some(home.join(".claude").join("projects")),
        "qoder" => Some(home.join(".qoder").join("projects")),
        "codex" => Some(home.join(".codex").join("sessions")),
        "kimi" => Some(home.join(".kimi-code").join("sessions")),
        _ => None,
    }
}

fn walk_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn collect_files(harness: &str, root: &Path, out: &mut Vec<PathBuf>) {
    match harness {
        "claude" | "qoder" => {
            let Ok(entries) = fs::read_dir(root) else { return };
            for entry in entries.flatten() {
                let project = entry.path();
                if !project.is_dir() {
                    continue;
                }
                let Ok(files) = fs::read_dir(project) else { continue };
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                        out.push(path);
                    }
                }
            }
        }
        "kimi" => {
            let Ok(entries) = fs::read_dir(root) else { return };
            for entry in entries.flatten() {
                let workspace = entry.path();
                if !workspace.is_dir() {
                    continue;
                }
                let Ok(sessions) = fs::read_dir(&workspace) else { continue };
                for session in sessions.flatten() {
                    let agents = session.path().join("agents");
                    let Ok(agents) = fs::read_dir(agents) else { continue };
                    let mut best: Option<(u64, PathBuf)> = None;
                    for agent in agents.flatten() {
                        let wire = agent.path().join("wire.jsonl");
                        let Ok(meta) = fs::metadata(&wire) else { continue };
                        if best.as_ref().is_none_or(|(size, _)| meta.len() > *size) {
                            best = Some((meta.len(), wire));
                        }
                    }
                    if let Some((_, wire)) = best {
                        out.push(wire);
                    }
                }
            }
        }
        "codex" => {
            walk_files(root, out);
            out.retain(|path| {
                path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("rollout-"))
            });
        }
        _ => {}
    }
}

fn file_mtime(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|duration| duration.as_secs() as i64)
}

fn parse_events(path: &Path, harness: &str) -> Vec<Event> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    let mut events = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        let event = match harness {
            "codex" => codex_event(&value),
            "kimi" => kimi_event(&value),
            _ => claude_event(&value),
        };
        if let Some(event) = event {
            events.push(event);
        }
    }
    events
}

fn chat_cwd(path: &Path, harness: &str, events: &[Event]) -> String {
    if harness == "codex" {
        return codex_meta(path).cwd;
    }
    for line in fs::read_to_string(path).unwrap_or_default().lines().take(20) {
        let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(cwd) = value.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty()) {
            return cwd.to_owned();
        }
    }
    if harness == "kimi" {
        // `ancestors().nth(3)` — каталог сессии: сам он служебный, а настоящий cwd
        // лежит в его state.json. Иначе цель получает служебную папку Kimi как проект.
        return path.ancestors().nth(3).map(kimi_cwd).unwrap_or_default();
    }
    let _ = events;
    String::new()
}

pub fn active_chats(root: &Path, harness_id: &str, hours: i64, limit: usize) -> Result<Vec<Value>, String> {
    active_chats_in(&home(), root, harness_id, hours, limit)
}

pub fn active_chats_in(home: &Path, root: &Path, harness_id: &str, hours: i64, limit: usize) -> Result<Vec<Value>, String> {
    let Some(harness) = find(root, harness_id) else {
        return Err("unknown harness".to_owned());
    };
    if !harness.can_source {
        return Err(format!("chats from {} are not readable", harness.name));
    }
    let cutoff = now_seconds() - hours.clamp(1, MAX_HOURS) * 3600;
    if harness_id == "opencode" {
        let Some(db) = crate::handoff_sources::opencode_db(home) else {
            return Ok(Vec::new());
        };
        return crate::handoff_sources::opencode_chats(&db, cutoff, limit);
    }
    if harness_id == "deepseek" {
        let root = crate::handoff_sources::dsh_root(home);
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        return crate::handoff_sources::dsh_chats(&root, cutoff, limit);
    }
    if harness_id == "copilot" {
        let db = crate::handoff_sources::copilot_db(home);
        if !db.is_file() {
            return Ok(Vec::new());
        }
        return crate::handoff_sources::copilot_chats(&db, cutoff, limit);
    }
    if harness_id == "antigravity" {
        return crate::handoff_sources::antigravity_chats(home, cutoff, limit);
    }
    let root = root_for(home, harness_id).ok_or_else(|| "no session root".to_owned())?;
    let mut files = Vec::new();
    collect_files(harness_id, &root, &mut files);
    let mut recent: Vec<(i64, PathBuf)> = files
        .into_iter()
        .filter_map(|path| file_mtime(&path).filter(|mtime| *mtime >= cutoff).map(|mtime| (mtime, path)))
        .collect();
    recent.sort_by(|left, right| right.0.cmp(&left.0));

    let mut chats = Vec::new();
    for (mtime, path) in recent {
        if chats.len() >= limit.min(MAX_CHATS) {
            break;
        }
        if harness_id == "kimi" && !path.ends_with("wire.jsonl") {
            continue;
        }
        if harness_id == "codex" && codex_meta(&path).guardian {
            continue;
        }
        let events = parse_events(&path, harness_id);
        if !events.iter().any(|(role, _, _)| role == "user") {
            continue;
        }
        let relative = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        let cwd = chat_cwd(&path, harness_id, &events);
        let chat = Chat {
            id: format!("{harness_id}:{relative}"),
            harness: harness.id.clone(),
            harness_name: harness.name.clone(),
            locator: relative.clone(),
            title: String::new(),
            cwd,
            mtime,
            events,
        };
        chats.push(chat.brief());
    }
    Ok(chats)
}

pub fn now_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_secs() as i64).unwrap_or(0)
}

/// Пути, нужные записи файла переноса (пока только хранилище handoffs).
pub fn handoff_dir(root: &Path) -> PathBuf {
    root.join("data").join("handoffs")
}

const BRIEF_HEAD_CHARS: usize = 6_000;
const BRIEF_TAIL_CHARS: usize = 22_000;
const FULL_FILE_MAX_CHARS: usize = 400_000;
const BRIEF_SYSTEM: &str = "Ты готовишь передачу незавершённой работы другому AI-агенту. \
Пиши по-русски, термины, пути и команды оставляй как есть. Только факты из чата. \
Формат — markdown с разделами: ## Цель, ## Контекст, ## Сделано, ## Осталось сделать \
(чеклист `- [ ]`), ## Открытые вопросы и риски.";

fn head_tail(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= BRIEF_HEAD_CHARS + BRIEF_TAIL_CHARS {
        return text.to_owned();
    }
    let head: String = chars[..BRIEF_HEAD_CHARS].iter().collect();
    let tail: String = chars[chars.len() - BRIEF_TAIL_CHARS..].iter().collect();
    format!("{head}\n\n[… середина чата пропущена …]\n\n{tail}")
}

pub fn open_chat(root: &Path, id: &str) -> Result<Chat, String> {
    open_chat_in(&home(), root, id)
}

pub fn open_chat_in(home: &Path, root: &Path, id: &str) -> Result<Chat, String> {
    let (harness_id, relative) = id.split_once(':').ok_or_else(|| "неизвестный чат".to_owned())?;
    let harness = find(root, harness_id).ok_or_else(|| "неизвестный чат".to_owned())?;
    if !harness.can_source || relative.is_empty() {
        return Err("неизвестный чат".to_owned());
    }
    if harness_id == "opencode" {
        let db = crate::handoff_sources::opencode_db(home).ok_or_else(|| "неизвестный чат".to_owned())?;
        return crate::handoff_sources::opencode_chat(&db, relative);
    }
    if harness_id == "deepseek" {
        return crate::handoff_sources::dsh_chat(&crate::handoff_sources::dsh_root(home), relative);
    }
    if harness_id == "copilot" {
        return crate::handoff_sources::copilot_chat(&crate::handoff_sources::copilot_db(home), relative);
    }
    if harness_id == "antigravity" {
        return crate::handoff_sources::antigravity_chat(home, relative);
    }
    let root = root_for(home, harness_id).ok_or_else(|| "неизвестный чат".to_owned())?;
    let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let root = root.canonicalize().map_err(|_| "неизвестный чат".to_owned())?;
    let path = path.canonicalize().map_err(|_| "неизвестный чат".to_owned())?;
    if !path.starts_with(&root) || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") || !path.is_file() {
        return Err("неизвестный чат".to_owned());
    }
    let events = parse_events(&path, harness_id);
    if !events.iter().any(|(role, _, _)| role == "user") {
        return Err("в чате нет сообщений".to_owned());
    }
    let mtime = file_mtime(&path).unwrap_or_else(now_seconds);
    Ok(Chat {
        id: id.to_owned(),
        harness: harness.id.clone(),
        harness_name: harness.name.clone(),
        locator: relative.to_owned(),
        title: String::new(),
        cwd: chat_cwd(&path, harness_id, &events),
        mtime,
        events,
    })
}

fn make_brief(chat: &Chat, config: &crate::config::Config) -> Result<Option<String>, String> {
    // Ошибку локального движка не показываем как ошибку переноса: ниже
    // останется полный чат с пометкой.
    let _guard = crate::local::job_guard(config, crate::config::Task::Handoff).ok().flatten();
    let Some(mut route) = config.route_for(crate::config::Task::Handoff, false) else {
        return Ok(None);
    };
    let transcript = chat.transcript();
    // Тот же fail-closed гейт, что у пересказа: шаблон, который не компилируется,
    // тоже считается совпавшим, иначе приватный чат ушёл бы наружу.
    if !route.local {
        let gate = crate::llm::LocalOnly::new(&config.privacy.local_only_patterns);
        if gate.any([chat.cwd.as_str(), transcript.as_str()]) {
            // Локальный маршрут лучше полного чата: выжимка не уходит наружу.
            match config.local_route(crate::config::Task::Handoff) {
                Some(local) => route = local,
                None => return Ok(None),
            }
        }
    }
    // Выжимка длинного чата — самый долгий запрос приложения.
    route.timeout_seconds = route.timeout_seconds.max(120.0);
    let client = crate::llm::client_for(&route, 1_800).map_err(|error| error.to_string())?;
    let prompt = format!(
        "Харнесс: {}\nРабочая папка: {}\nЧат:\n\n{}",
        chat.harness_name,
        if chat.cwd.is_empty() { "неизвестна" } else { &chat.cwd },
        head_tail(&transcript),
    );
    match client.chat(BRIEF_SYSTEM, &prompt) {
        Ok(reply) => {
            // Расход переноса виден в журнале; сбой записи ответ не отменяет.
            let db_path = crate::config::db_path_from_env()
                .unwrap_or_else(|| config.root().join("data").join("memory.db"));
            if let Ok(db) = crate::db::Database::open(db_path) {
                let _ = crate::llm::record_reply_usage(&db, &client, &reply, 0, crate::config::Task::Handoff);
            }
            Ok(Some(reply.text.trim().to_owned()).filter(|text| !text.is_empty()))
        }
        Err(_) => Ok(None),
    }
}

fn document(chat: &Chat, target: &Harness, mode: &str, body: &str, note: Option<&str>) -> String {
    let target_name = &target.name;
    let origin = &chat.harness_name;
    let mut head = vec![
        format!("# Перенос чата: {}", chat.title()),
        String::new(),
        format!("- Откуда: {origin}, сессия `{}`", chat.locator),
        format!("- Куда: {target_name}"),
        format!("- Рабочая папка: `{}`", if chat.cwd.is_empty() { "неизвестна" } else { &chat.cwd }),
        format!("- Формат: {}", if mode == "full" { "полный чат" } else { "выжимка: контекст и оставшиеся задачи" }),
    ];
    if let Some(note) = note {
        head.push(format!("- Примечание: {note}"));
    }
    let instruction = if mode == "full" {
        "Ниже весь чат. Восстанови из него цель, сделанное и оставшиеся задачи, затем продолжи с первой невыполненной."
    } else {
        "Продолжи с первой невыполненной задачи из «Осталось сделать». Перед правкой сверь описанное состояние с файлами."
    };
    format!("{}\n\n> {instruction}\n\n---\n\n{body}\n", head.join("\n"))
}

pub fn build(root: &Path, chat: &Chat, target: &str, mode: &str, out_dir: &Path, config: &crate::config::Config) -> Result<Value, String> {
    let Some(target_harness) = find(root, target) else {
        return Err("неизвестный целевой харнесс".to_owned());
    };
    if !matches!(mode, "auto" | "full" | "brief") {
        return Err("mode: auto, full или brief".to_owned());
    }
    let size: usize = chat.events.iter().map(|(_, text, _)| text.chars().count()).sum();
    let wanted = if mode == "auto" { if size <= FULL_MAX_CHARS { "full" } else { "brief" } } else { mode };
    let mut used = wanted;
    let mut note: Option<&str> = None;
    let mut body = String::new();
    if wanted == "brief" {
        match make_brief(chat, config)? {
            Some(text) => body = text,
            None => {
                used = "full";
                note = Some(if config.route_for(crate::config::Task::Handoff, false).is_some() {
                    "модель не ответила, перенесён полный чат"
                } else {
                    "модель не настроена, перенесён полный чат"
                });
            }
        }
    }
    if used == "full" {
        let transcript = chat.transcript();
        let chars: Vec<char> = transcript.chars().collect();
        body = if chars.len() > FULL_FILE_MAX_CHARS {
            let head: String = chars[..FULL_FILE_MAX_CHARS / 4].iter().collect();
            let tail: String = chars[chars.len() - FULL_FILE_MAX_CHARS * 3 / 4..].iter().collect();
            format!("{head}\n\n[… середина чата пропущена …]\n\n{tail}")
        } else {
            transcript
        };
    }
    fs::create_dir_all(out_dir).map_err(|error| error.to_string())?;
    let digest = format!("{:x}", md5::compute(chat.id.as_bytes()));
    let path = out_dir.join(format!(
        "{}-{}-to-{}-{}.md",
        now_seconds(),
        chat.harness,
        target,
        &digest[..8]
    ));
    fs::write(&path, document(chat, &target_harness, used, &body, note)).map_err(|error| error.to_string())?;
    let prompt = format!(
        "Продолжи работу, перенесённую из {}: «{}». Контекст и оставшиеся задачи — в файле {}. Прочитай его целиком и продолжай.",
        chat.harness_name,
        chat.title(),
        path.display(),
    );
    Ok(json!({
        "chat": chat.id, "title": chat.title(), "cwd": chat.cwd, "mode": used, "requested": mode,
        "note": note, "path": path.display().to_string(), "prompt": prompt, "chars": size,
    }))
}

fn app_registered(target: &str) -> bool {
    if target != "claude" {
        return false;
    }
    quiet("reg")
        .args(["query", r"HKEY_CLASSES_ROOT\claude\shell\open\command"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn set_clipboard(text: &str) -> bool {
    let Ok(mut child) = quiet("clip").stdin(Stdio::piped()).spawn() else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        // clip.exe разбирает stdin по кодовой странице консоли, а не как UTF-8:
        // русский промпт переноса доезжает до буфера кракозябрами. UTF-16LE с BOM
        // он распознаёт явно.
        let mut bytes = Vec::with_capacity(2 + text.len() * 2);
        bytes.extend_from_slice(&[0xff, 0xfe]);
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let _ = stdin.write_all(&bytes);
    }
    child.wait().is_ok_and(|status| status.success())
}

/// Доставка в цель: Claude — deep link в приложение, своя цель — запуск её
/// программы, остальные — буфер обмена.
const APP_PROMPT_MAX_CHARS: usize = 14_336;

/// Общий промпт для пачки чатов в приложение (как batch_prompt в Python-версии).
fn batch_prompt(items: &[(String, String)]) -> String {
    let chats = items.iter().enumerate().map(|(index, (prompt, _))| format!("{}) {prompt}", index + 1)).collect::<Vec<_>>().join(" ");
    format!(
        "Перенесено чатов: {}. Запусти каждый в отдельной новой сессии Claude Code с его промптом; если запустить сессию нельзя, продолжай чаты здесь по очереди. {chats}",
        items.len()
    )
}

/// Доставка нескольких переносов: `(prompt, cwd)` на чат. Второе значение —
/// общий промпт, если чаты ушли одной сессией приложения. `claude://code/new`
/// только подставляет черновик, и следующая ссылка его затирает: из восьми ссылок
/// подряд открывались две — поэтому в приложение уходит одна ссылка на всех.
/// Это поведение было в Python-версии и потерялось при переносе на Rust.
pub fn launch_many(root: &Path, target: &str, items: &[(String, String)]) -> (Vec<Value>, Option<String>) {
    let app = find(root, target).is_some_and(|harness| harness.delivery == Delivery::App);
    if !app || items.len() < 2 {
        return (items.iter().map(|(prompt, cwd)| launch(root, target, prompt, cwd)).collect(), None);
    }
    let prompt = batch_prompt(items);
    let first_cwd = items[0].1.as_str();
    let cwd = if items.iter().all(|(_, cwd)| cwd == first_cwd) { first_cwd } else { "" };
    let delivery = if prompt.chars().count() <= APP_PROMPT_MAX_CHARS {
        launch(root, target, &prompt, cwd)
    } else if set_clipboard(&prompt) {
        json!({"delivered": "clipboard"})
    } else {
        json!({"delivered": "none", "launch_error": "не удалось положить промпт в буфер обмена"})
    };
    (items.iter().map(|_| delivery.clone()).collect(), Some(prompt))
}

pub fn launch(root: &Path, target: &str, prompt: &str, cwd: &str) -> Value {
    let harness = find(root, target);
    if target == "claude" && prompt.chars().count() <= APP_PROMPT_MAX_CHARS && app_registered(target) {
        let mut url = format!("claude://code/new?q={}", percent_encode(prompt));
        if !cwd.is_empty() && Path::new(cwd).is_dir() {
            url.push_str(&format!("&folder={}", percent_encode(cwd)));
        }
        if crate::proc::shell_open(&url) {
            return json!({"delivered": "app"});
        }
    }
    if let Some(command) = harness.as_ref().and_then(|harness| harness.command.as_ref()) {
        let mut child = quiet(command.to_string_lossy().as_ref());
        if !cwd.is_empty() && Path::new(cwd).is_dir() {
            child.current_dir(cwd);
        }
        let opened = child
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok();
        if opened && set_clipboard(prompt) {
            return json!({"delivered": "command"});
        }
    }
    if set_clipboard(prompt) {
        return json!({"delivered": "clipboard"});
    }
    // Без признака неудачи resume_switch и панель считают доставку успешной и
    // теряют ожидание переноса.
    json!({"delivered": "none", "launch_error": "не удалось положить промпт в буфер обмена"})
}

fn profile_json(path: &Path) -> Value {
    fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or(Value::Null)
}

fn newest_mtime(path: &Path) -> Option<i64> {
    if path.is_file() {
        return file_mtime(path);
    }
    let mut newest = None;
    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Some(value) = newest_mtime(&entry.path()) {
                    newest = Some(newest.map_or(value, |current: i64| current.max(value)));
                }
            }
        }
    }
    newest
}

pub fn account(harness: &str) -> Result<Value, String> {
    let home = home();
    match harness {
        "claude" => {
            let profile = profile_json(&home.join(".claude.json"));
            let oauth = profile.get("oauthAccount").cloned().unwrap_or(Value::Null);
            let id = oauth.get("accountUuid").or_else(|| oauth.get("emailAddress")).cloned().unwrap_or(Value::Null);
            Ok(json!({"harness": harness, "exact": true, "id": id, "label": oauth.get("emailAddress").cloned().unwrap_or(Value::Null)}))
        }
        "qoder" => {
            let status = profile_json(&home.join(".qoder").join(".qoder-app-status.json"));
            let logged = status.get("logged_in").and_then(Value::as_bool).unwrap_or(false);
            let email = status.get("email").cloned().unwrap_or(Value::Null);
            Ok(json!({"harness": harness, "exact": true, "id": if logged { email.clone() } else { Value::Null }, "label": email}))
        }
        "codex" => {
            let stamp = newest_mtime(&home.join(".codex").join("auth.json"));
            Ok(json!({"harness": harness, "exact": false, "id": stamp.map(|value| value.to_string()), "label": Value::Null}))
        }
        "kimi" => {
            let stamp = newest_mtime(&home.join(".kimi-code").join("credentials"));
            Ok(json!({"harness": harness, "exact": false, "id": stamp.map(|value| value.to_string()), "label": Value::Null}))
        }
        _ => Err("неизвестный харнесс".to_owned()),
    }
}

fn switch_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn switch_path(out_dir: &Path) -> PathBuf {
    out_dir.join("account-switch.json")
}

/// Файла ещё нет до первой смены аккаунта: `Null` вместо `{}` ронял подготовку.
fn read_switches(out_dir: &Path) -> Value {
    Some(profile_json(&switch_path(out_dir))).filter(Value::is_object).unwrap_or_else(|| json!({}))
}

fn write_switches(out_dir: &Path, value: &Value) -> Result<(), String> {
    fs::create_dir_all(out_dir).map_err(|error| error.to_string())?;
    let tmp = out_dir.join("account-switch.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value).unwrap_or_default()).map_err(|error| error.to_string())?;
    fs::rename(&tmp, switch_path(out_dir)).map_err(|error| error.to_string())
}

pub fn switch_status(root: &Path, harness: &str, out_dir: &Path) -> Result<Value, String> {
    if !self::find(root, harness).is_some_and(|h| h.can_switch) {
        return Err("неизвестный харнесс".to_owned());
    }
    let pending = read_switches(out_dir).get(harness).cloned().filter(|value| !value.is_null());
    let now = account(harness)?;
    let changed = pending.as_ref().and_then(|pending| {
        let before = pending.get("account")?.get("id")?;
        let after = now.get("id")?;
        if after.is_null() { None } else { Some(before != after) }
    });
    Ok(json!({"pending": pending, "account": now, "changed": changed}))
}

pub fn prepare_switch(root: &Path, harness: &str, chat_ids: &[String], mode: &str, out_dir: &Path, config: &crate::config::Config) -> Result<Value, String> {
    if !self::find(root, harness).is_some_and(|h| h.can_switch) {
        return Err("неизвестный харнесс".to_owned());
    }
    if chat_ids.is_empty() {
        return Err("не выбраны чаты".to_owned());
    }
    let mut items = Vec::new();
    for id in chat_ids {
        let chat = open_chat(root, id)?;
        if chat.harness != harness {
            return Err("чаты из другого харнесса".to_owned());
        }
        items.push(build(root, &chat, harness, mode, out_dir, config)?);
    }
    let pending = json!({"harness": harness, "created": now_seconds(), "account": account(harness)?, "items": items});
    let _guard = switch_lock().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut switches = read_switches(out_dir);
    let object = switches.as_object_mut().ok_or_else(|| "account-switch.json: not an object".to_owned())?;
    object.insert(harness.to_owned(), pending.clone());
    write_switches(out_dir, &switches)?;
    Ok(pending)
}

pub fn resume_switch(root: &Path, harness: &str, launch_items: bool, out_dir: &Path) -> Result<Value, String> {
    let _guard = switch_lock().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut switches = read_switches(out_dir);
    let pending = switches.get(harness).cloned().filter(|value| !value.is_null()).ok_or_else(|| "нет подготовленного переноса для этого харнесса".to_owned())?;
    let items = pending.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let field = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let pairs: Vec<(String, String)> = items.iter().map(|item| (field(item, "prompt"), field(item, "cwd"))).collect();
    let (deliveries, shared_prompt) = if launch_items {
        launch_many(root, harness, &pairs)
    } else {
        (items.iter().map(|_| json!({"delivered": "clipboard"})).collect(), None)
    };
    let mut delivered = Vec::new();
    for (item, delivery) in items.into_iter().zip(deliveries) {
        let mut merged = item.clone();
        if let (Some(target), Some(source)) = (merged.as_object_mut(), delivery.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        delivered.push(merged);
    }
    let failed = delivered.iter().any(|item| item.get("launch_error").is_some());
    if !failed {
        if let Some(object) = switches.as_object_mut() {
            object.remove(harness);
        }
        write_switches(out_dir, &switches)?;
    }
    Ok(json!({"harness": harness, "items": delivered, "prompt": shared_prompt}))
}

pub fn cancel_switch(harness: &str, out_dir: &Path) -> Result<bool, String> {
    let _guard = switch_lock().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut switches = read_switches(out_dir);
    let removed = switches.as_object_mut().and_then(|object| object.remove(harness)).is_some();
    if removed {
        write_switches(out_dir, &switches)?;
    }
    Ok(removed)
}

pub fn title_counts(chats: &[Value]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for chat in chats {
        if let Some(harness) = chat.get("harness").and_then(Value::as_str) {
            *counts.entry(harness.to_owned()).or_default() += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    #[test]
    fn batch_prompt_numbers_every_chat() {
        let items = vec![("первый".to_owned(), String::new()), ("второй".to_owned(), String::new())];
        let prompt = super::batch_prompt(&items);
        assert!(prompt.starts_with("Перенесено чатов: 2."), "{prompt}");
        assert!(prompt.contains("1) первый 2) второй"), "{prompt}");
    }
}
