//! Индексация локальных сессий и истории git — порт `ingest.py`.
//!
//! Источники только читаются. Индексация инкрементальная и идемпотентная:
//! байтовые смещения и счётчики ходов по файлам лежат в `state` под теми же
//! ключами, что у Python, поэтому обе реализации продолжают друг за другом.
//! Карточки дедуплицируются по UNIQUE source_ref.
//!
//! Сырая карточка — фрагмент записи: у чатов один вопрос пользователя и ответы
//! на него (запись — весь разговор), у git один коммит владельца репозитория
//! (запись — рабочий день в репозитории). Служебные обёртки харнессов вырезаются,
//! секреты маскируются до записи в базу.
//!
//! Потоки: всё выполняется в вызывающем потоке; база — его соединение. Каждый
//! файл и каждый репозиторий фиксируются отдельно, чтобы долгий обход не держал
//! блокировку записи, нужную панели.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::Value;

use crate::db::{now_ts, CardDraft, Database, Result, FILE_KEY_PREFIX, SCOPE_KEY_PREFIX};
use crate::pystr;

pub const TITLE_MAX_CHARS: usize = 80;
pub const BODY_MAX_CHARS: usize = 4000;
pub const GIT_BODY_MAX_CHARS: usize = 2000;
pub const SESSION_IDLE_SECONDS: f64 = 30.0 * 60.0;
pub const GIT_MAX_DEPTH: usize = 3;
const CLAUDE_CWD_SCAN_LINES: usize = 50;
/// Каталоги, где вложенные репозитории не ищутся: worktree этой рабочей области,
/// зависимости, сборка и временные клоны чужого кода.
const GIT_SKIP_DIRS: [&str; 14] = [
    ".git", ".claude", ".venv", "node_modules", "_tmp", "target", "obj", "bin", "Binaries", "Intermediate", "Saved",
    "DerivedDataCache", "Library", "Temp",
];
/// Codex Desktop при импорте сессии Claude Code повторяет её ходы как
/// «external-import-turn-N»; оригинал уже проиндексирован из ~/.claude.
pub const IMPORTED_TURN_PREFIX: &str = "external-import-";

// -- очистка текста чата --------------------------------------------------------

/// Теги, весь блок которых — вывод харнесса, а не разговор.
const DROP_TAGS: &str = "system-reminder|task-notification|notification|environment_context|user_instructions|\
INSTRUCTIONS|in-app-browser-context|external_[a-z_]+|git-context|local-command-caveat|local-command-stdout|\
local-command-stderr|command-message|skill-loaded|skill-format|plugin_session_start|plugin-instructions|\
turn_aborted|bash-stdout|bash-stderr|ide_selection|ide_opened_file|system|image|tool_use_error|\
side-sessions-event|cross-session-message|agent-message|cron-fire";

/// Целые сообщения, которые подставил харнесс (инструкции, тела скиллов, сводки).
const NOISE_MESSAGE_PREFIXES: [&str; 6] = [
    "# AGENTS.md instructions for",
    "Base directory for this skill:",
    "This session is being continued from a previous conversation",
    "The following is the Codex agent history",
    "Skill tool loaded instructions for this request.",
    "Another Claude session sent a message:",
];

struct Patterns {
    open_tag: Regex,
    self_closing: Regex,
    unclosed_lead: Regex,
    unwrap_tag: Regex,
    orphan_tag_line: Regex,
    notice_line: Regex,
    blank_runs: Regex,
    private_key: Regex,
    api_key: Regex,
    aws_key: Regex,
    bearer: Regex,
    vendor_token: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("static pattern");
        Patterns {
            open_tag: re(&format!(r"<({DROP_TAGS})(?:\s[^>]*)?>")),
            self_closing: re(&format!(r"<(?:{DROP_TAGS})(?:\s[^>]*)?/>")),
            // Блок, обрезанный лимитом тела, остаётся без закрывающего тега; убирается
            // только ведущий — проза, которая лишь упоминает тег, остаётся.
            unclosed_lead: re(&format!(r"(?s)\A\s*<({DROP_TAGS})(?:\s[^>]*)?>.*\z")),
            unwrap_tag: re(r"</?(?:command-name|command-args|bash-input|pasted_content)(?:\s[^>]*)?>"),
            orphan_tag_line: re(r"(?m)^\s*</?[A-Za-z][\w-]*>\s*$"),
            notice_line: re(
                r"(?mi)^\s*\[(?:Image\b|external unsupported block|Request interrupted by user|Usage limit reached|Earlier usage-limit)[^\n]*$",
            ),
            blank_runs: re(r"\n{3,}"),
            private_key: re(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----"),
            api_key: re(r"\bsk-[A-Za-z0-9_\-]{8,}\b"),
            aws_key: re(r"\bAKIA[0-9A-Z]{16}\b"),
            bearer: re(r"\bBearer\s+[A-Za-z0-9._\-+/=]{8,}\b"),
            // Короче 40 символов, поэтому общие серии hex/base64 их не ловят.
            vendor_token: re(
                r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|glpat-[A-Za-z0-9_\-]{20,}|AIza[0-9A-Za-z_\-]{35}|xox[abposr]-[A-Za-z0-9\-]{10,})",
            ),
        }
    })
}

/// `<tag ...>…</tag>` без обратной ссылки: у крейта regex её нет. Закрывающий тег
/// ищется первым после открывающего, как ленивое `.*?` в Python; без пары поиск
/// продолжается со следующего символа.
fn drop_closed_blocks(text: &str) -> String {
    let p = patterns();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut pos = 0;
    let mut closers: HashMap<String, Regex> = HashMap::new();
    while let Some(open) = p.open_tag.captures_at(text, pos) {
        let whole = open.get(0).expect("match");
        let name = open.get(1).expect("group").as_str();
        let closer = closers
            .entry(name.to_string())
            .or_insert_with(|| Regex::new(&format!(r"</{}\s*>", regex::escape(name))).expect("escaped"));
        match closer.find_at(text, whole.end()) {
            Some(close) => {
                out.push_str(&text[copied..whole.start()]);
                copied = close.end();
                pos = close.end();
            }
            None => {
                let next = text[whole.start()..].chars().next().map_or(1, char::len_utf8);
                pos = whole.start() + next;
            }
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Сообщение без служебных обёрток и уведомлений; "" — от человека и агента ничего не осталось.
pub fn clean_chat_text(text: &str) -> String {
    let lead = text.trim_start_matches(pystr::is_space);
    if NOISE_MESSAGE_PREFIXES.iter().any(|p| lead.starts_with(p)) {
        return String::new();
    }
    let p = patterns();
    let text = drop_closed_blocks(text);
    let text = p.self_closing.replace_all(&text, "");
    let text = p.unclosed_lead.replace_all(&text, "");
    let text = p.unwrap_tag.replace_all(&text, "");
    let text = p.notice_line.replace_all(&text, "");
    let text = p.orphan_tag_line.replace_all(&text, "");
    let text = p.blank_runs.replace_all(&text, "\n\n");
    pystr::strip(&text).to_string()
}

// -- маскирование секретов --------------------------------------------------------

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

fn is_b64(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '/'
}

/// Максимальные серии символов класса длиной от 40, как `(?<!c)c{40,}(?!c)` в Python
/// (крейт regex без lookaround). У base64 жадно добавляются до двух '=', если
/// после них не стоит символ класса.
fn mask_runs(text: &str, class: fn(char) -> bool, padding: bool, replacement: &str) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < chars.len() {
        if !class(chars[i].1) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && class(chars[i].1) {
            i += 1;
        }
        if i - start < 40 {
            continue;
        }
        let mut end = i;
        if padding {
            let mut eq = 0;
            while eq < 2 && end + eq < chars.len() && chars[end + eq].1 == '=' {
                eq += 1;
            }
            while eq > 0 && end + eq < chars.len() && class(chars[end + eq].1) {
                eq -= 1;
            }
            end += eq;
        }
        let byte_start = chars[start].0;
        let byte_end = chars.get(end).map_or(text.len(), |c| c.0);
        out.push_str(&text[copied..byte_start]);
        out.push_str(replacement);
        copied = byte_end;
        i = end;
    }
    out.push_str(&text[copied..]);
    out
}

pub fn mask_secrets(text: &str) -> String {
    let p = patterns();
    let text = p.private_key.replace_all(text, "[REDACTED:private-key]");
    let text = p.api_key.replace_all(&text, "[REDACTED:api-key]");
    let text = p.aws_key.replace_all(&text, "[REDACTED:aws-access-key]");
    let text = p.bearer.replace_all(&text, "Bearer [REDACTED:token]");
    let text = p.vendor_token.replace_all(&text, "[REDACTED:token]");
    let text = mask_runs(&text, is_hex, false, "[REDACTED:hex]");
    mask_runs(&text, is_b64, true, "[REDACTED:blob]")
}

// -- общие помощники --------------------------------------------------------------

pub fn truncate(text: &str, max_chars: usize) -> String {
    if pystr::char_len(text) <= max_chars {
        return text.to_string();
    }
    format!("{}…", pystr::rstrip(pystr::take_chars(text, max_chars - 1)))
}

pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let len = pystr::char_len(text);
    if len <= max_chars {
        return text.to_string();
    }
    let marker = "\n[… середина фрагмента сокращена …]\n";
    let marker_len = pystr::char_len(marker);
    if max_chars <= marker_len + 2 {
        return truncate(text, max_chars);
    }
    // Итог и проверка обычно в конце хода; начало оставляет контекст решения.
    let head = (max_chars - marker_len) / 3;
    let tail = max_chars - marker_len - head;
    let tail_start = text.char_indices().nth(len - tail).map_or(text.len(), |c| c.0);
    format!("{}{marker}{}", pystr::take_chars(text, head), &text[tail_start..])
}

pub fn make_title(user_text: &str) -> String {
    let first = pystr::splitlines(user_text).into_iter().find(|l| !pystr::strip(l).is_empty()).unwrap_or("");
    truncate(pystr::strip(&mask_secrets(first)), TITLE_MAX_CHARS)
}

pub fn make_body(user_text: &str, assistant_text: &str) -> String {
    let parts: Vec<&str> = [pystr::strip(user_text), pystr::strip(assistant_text)].into_iter().filter(|p| !p.is_empty()).collect();
    truncate_middle(&mask_secrets(&parts.join("\n\n")), BODY_MAX_CHARS)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// Секунды эпохи по дате и времени; None — несуществующая дата или время.
pub fn epoch_of(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32, offset_seconds: i64) -> Option<i64> {
    if !(1..=9999).contains(&y) || !(1..=12).contains(&mo) || d == 0 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(s) - offset_seconds)
}

fn digits(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// `datetime.fromisoformat` Python 3.11 для форм, что встречаются в логах и заметках:
/// дата YYYY-MM-DD, разделитель T или пробел, время HH[:MM[:SS[.доли]]], смещение
/// ±HH:MM[:SS] или ±HHMM. Без смещения — UTC, как в ingest.py. Доли отбрасываются,
/// как int(dt.timestamp()) для дат после 1970.
pub fn parse_iso(text: &str) -> Option<i64> {
    let (y, mo, d) = (digits(text.get(0..4)?, 4)?, text.get(4..5)?, text.get(5..7)?);
    if mo != "-" || text.get(7..8)? != "-" {
        return None;
    }
    let (mo, day) = (digits(d, 2)?, digits(text.get(8..10)?, 2)?);
    let rest = &text[10..];
    if rest.is_empty() {
        return epoch_of(i64::from(y), mo, day, 0, 0, 0, 0);
    }
    let mut chars = rest.chars();
    let sep = chars.next()?;
    if sep != 'T' && sep != ' ' {
        return None;
    }
    let rest = chars.as_str();
    let tz_at = rest.find(['+', '-']).unwrap_or(rest.len());
    let (time, tz) = rest.split_at(tz_at);
    let (clock, _fraction) = match time.split_once('.') {
        Some((clock, fraction)) if !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit()) => (clock, fraction),
        Some(_) => return None,
        None => (time, ""),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    if parts.is_empty() || parts.len() > 3 || (parts.len() < 3 && !_fraction.is_empty()) {
        return None;
    }
    let h = digits(parts[0], 2)?;
    let mi = parts.get(1).map_or(Some(0), |p| digits(p, 2))?;
    let s = parts.get(2).map_or(Some(0), |p| digits(p, 2))?;
    let offset = if tz.is_empty() {
        0
    } else {
        let sign = if tz.starts_with('-') { -1 } else { 1 };
        let body = &tz[1..];
        let fields: Vec<&str> = if body.contains(':') { body.split(':').collect() } else if body.len() == 4 && body.is_ascii() { vec![&body[..2], &body[2..]] } else { vec![body] };
        let oh = i64::from(digits(fields.first()?, 2)?);
        let om = fields.get(1).map_or(Some(0), |p| digits(p, 2))?;
        let os = fields.get(2).map_or(Some(0), |p| digits(p, 2))?;
        if fields.len() > 3 || oh > 23 || om > 59 {
            return None;
        }
        sign * (oh * 3600 + i64::from(om) * 60 + i64::from(os))
    };
    epoch_of(i64::from(y), mo, day, h, mi, s, offset)
}

/// `iso_to_epoch`: Z считается UTC.
pub fn iso_to_epoch(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?;
    if text.is_empty() {
        return None;
    }
    parse_iso(&text.replace('Z', "+00:00"))
}

/// Пути сортируются как `sorted()` у WindowsPath в Python 3.11: по частям в нижнем регистре.
pub fn py_path_key(path: &Path) -> Vec<String> {
    path.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect()
}

fn sort_paths(paths: &mut [PathBuf]) {
    paths.sort_by_cached_key(|p| py_path_key(p));
}

/// Рекурсивный поиск файлов по предикату имени, без захода по символьным ссылкам.
fn walk_files(root: &Path, wanted: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            walk_files(&path, wanted, out);
        } else if kind.is_file() && wanted(&entry.file_name().to_string_lossy()) {
            out.push(path);
        }
    }
}

fn mtime_secs(meta: &fs::Metadata) -> f64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64())
}

fn mtime_ns(meta: &fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Отпечаток первых байт файла (до 4 КБ, но не дальше прочитанного): дописывание
/// его не меняет, а замена файла — меняет. SHA-256, а не DefaultHasher: значение
/// хранится в базе и должно пережить смену версии Rust.
fn head_fingerprint(file: &mut fs::File, offset: u64) -> Option<String> {
    use sha2::{Digest, Sha256};
    let len = offset.min(4096) as usize;
    let mut head = vec![0u8; len];
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_exact(&mut head).ok()?;
    let digest = Sha256::digest(&head);
    Some(digest[..8].iter().map(|byte| format!("{byte:02x}")).collect())
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// -- общий разбор файла чата -----------------------------------------------------------

pub type Event = (String, String, Option<i64>);

/// Вопрос пользователя и следующие за ним ответы. Ответ до первого вопроса отбрасывается.
pub fn group_turns(events: &[Event]) -> Vec<(String, String, Option<i64>)> {
    let mut turns = Vec::new();
    let mut user: Option<(String, Option<i64>)> = None;
    let mut replies: Vec<String> = Vec::new();
    for (role, text, ts) in events {
        if role == "user" {
            if let Some((text, ts)) = user.take() {
                turns.push((text, replies.join("\n\n"), ts));
            }
            user = Some((text.clone(), *ts));
            replies.clear();
        } else if user.is_some() && !text.is_empty() {
            replies.push(text.clone());
        }
    }
    if let Some((text, ts)) = user {
        turns.push((text, replies.join("\n\n"), ts));
    }
    turns
}

fn state_int(db: &Database, key: &str) -> Result<i64> {
    Ok(db.get_state(key)?.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Инкрементальное чтение одного файла чата с водяными знаками. `scope` — рабочая
/// папка или слаг проекта сессии: по ней пересказ проверяет приватные шаблоны.
pub fn ingest_chat_file(
    db: &Database,
    path: &Path,
    source: &str,
    session_id: &str,
    events_from: &mut dyn FnMut(&Value) -> Option<Event>,
    idle_seconds: f64,
    scope: &str,
) -> Result<usize> {
    let offset_key = format!("offset:{source}:{}", path_str(path));
    let turns_key = format!("turns:{source}:{}", path_str(path));
    let head_key = format!("head:{source}:{}", path_str(path));
    let Ok(before) = fs::metadata(path) else {
        return Ok(0);
    };
    let size = before.len();
    // Активные сессии не трогаем: иначе вопрос попадёт в базу раньше ответа.
    // idle=0 не сравнивает часы: mtime свежего файла на Windows бывает на мс впереди.
    if idle_seconds > 0.0 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        if now - mtime_secs(&before) < idle_seconds {
            return Ok(0);
        }
    }
    let mut offset = state_int(db, &offset_key)?.max(0) as u64;
    let stored_head = db.get_state(&head_key)?.filter(|value| !value.is_empty());
    let mut data = Vec::new();
    let truncated;
    {
        let Ok(mut file) = fs::File::open(path) else {
            return Ok(0);
        };
        // Файл обрезан или заменён (отмотка в Claude Code, восстановление из копии).
        // Замену файлом не короче смещения выдаёт только отпечаток начала: без него
        // чтение шло бы с середины строки. Счётчик ходов сбрасывается вместе со
        // смещением, иначе перечитанный разговор встал бы под новыми source_ref.
        let replaced = offset <= size
            && offset > 0
            && stored_head.as_deref().is_some_and(|stored| head_fingerprint(&mut file, offset).is_some_and(|actual| actual != stored));
        truncated = offset > size || replaced;
        if truncated {
            offset = 0;
        }
        if file.seek(SeekFrom::Start(offset)).is_err() || file.read_to_end(&mut data).is_err() {
            return Ok(0);
        }
    }
    let Ok(after) = fs::metadata(path) else {
        return Ok(0);
    };
    if (before.len(), mtime_ns(&before)) != (after.len(), mtime_ns(&after)) {
        return Ok(0); // файл дописывался во время чтения: повторим, не сдвигая знаки
    }
    let mut tail_len = data.iter().rev().position(|&b| b == b'\n').unwrap_or(data.len());
    // Хвост без `\n` обычно недописан, но после простоя это может быть последняя
    // запись сессии: если она разбирается как JSON, считаем её полной строкой,
    // иначе последний ответ не попадал в базу никогда.
    if tail_len > 0 && idle_seconds > 0.0 && serde_json::from_slice::<Value>(&data[data.len() - tail_len..]).is_ok() {
        tail_len = 0;
    }
    let complete = &data[..data.len() - tail_len];
    let new_offset = offset + complete.len() as u64;

    let mut events = Vec::new();
    for raw in complete.split(|&b| b == b'\n') {
        if raw.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(&String::from_utf8_lossy(raw)) else {
            continue;
        };
        if let Some(event) = events_from(&obj) {
            events.push(event);
        }
    }

    let mtime_ts = mtime_secs(&after) as i64;
    // Ответ, дописанный после простоя, приходит куском без реплики пользователя:
    // group_turns его отбросил бы, а смещение ушло бы за него навсегда. Дописываем
    // его в последний ход этого файла (как ingest.py).
    let orphan: Vec<&str> = events
        .iter()
        .take_while(|(role, _, _)| role != "user")
        .map(|(_, text, _)| pystr::strip(text))
        .filter(|text| !text.is_empty())
        .collect();
    db.atomic(|| {
        let mut turn_index = if truncated { 0 } else { state_int(db, &turns_key)? };
        let mut inserted = 0;
        if !orphan.is_empty() && turn_index > 0 {
            if let Some(prev) = db.get_card_by_ref(&format!("{source}:{session_id}:{}", turn_index - 1))? {
                let mut parts = vec![prev.body.as_str()];
                parts.extend(orphan.iter().copied());
                let body = truncate_middle(&mask_secrets(&parts.join("\n\n")), BODY_MAX_CHARS);
                db.update_card(prev.id, prev.ts, &prev.title, &body, None, true)?;
            }
        }
        for (user_text, assistant_text, ts) in group_turns(&events) {
            let card = CardDraft::new(
                source,
                &format!("{source}:{session_id}:{turn_index}"),
                ts.unwrap_or(mtime_ts),
                &make_title(&user_text),
                &make_body(&user_text, &assistant_text),
            );
            turn_index += 1;
            if db.insert_card(&card)? {
                inserted += 1;
            }
        }
        db.set_state(&offset_key, &new_offset.to_string())?;
        db.set_state(&turns_key, &turn_index.to_string())?;
        if let Some(head) = fs::File::open(path).ok().and_then(|mut file| head_fingerprint(&mut file, new_offset)) {
            db.set_state(&head_key, &head)?;
        }
        if turn_index > 0 {
            db.set_state(&format!("{FILE_KEY_PREFIX}{source}:{session_id}"), &path_str(path))?;
            if !scope.is_empty() {
                db.set_state(&format!("{SCOPE_KEY_PREFIX}{source}:{session_id}"), scope)?;
            }
        }
        Ok(inserted)
    })
}

fn text_blocks(content: Option<&Value>, types: &[&str]) -> Vec<String> {
    content
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str).is_some_and(|t| types.contains(&t)))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn cleaned(texts: Vec<String>) -> Vec<String> {
    texts.iter().map(|t| clean_chat_text(t)).filter(|t| !t.is_empty()).collect()
}

/// Истинность значения JSON по правилам Python: пустые строки, списки, словари и ноль ложны.
pub fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn role_of(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).filter(|r| *r == "user" || *r == "assistant").map(str::to_string)
}

// -- Kimi Code wire.jsonl --------------------------------------------------------------

pub fn kimi_event(obj: &Value) -> Option<Event> {
    if obj.get("type").and_then(Value::as_str) != Some("agent.message.appended") {
        return None;
    }
    let message = obj.get("message")?.get("message")?;
    let role = role_of(message.get("role"))?;
    let texts = cleaned(text_blocks(message.get("content"), &["text"]));
    if texts.is_empty() {
        return None;
    }
    // ts // 1000 в Python: деление с округлением вниз; True считается единицей.
    let ts = match obj.get("time") {
        Some(Value::Number(n)) => n.as_f64().map(|t| (t / 1000.0).floor() as i64),
        Some(Value::Bool(b)) => Some(i64::from(*b) / 1000),
        _ => None,
    };
    Some((role, texts.join("\n"), ts))
}

pub fn kimi_cwd(session_dir: &Path) -> String {
    fs::read_to_string(session_dir.join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("cwd").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// <root>/<workDirKey>/<sessionId>/agents/<agentId>/wire.jsonl
pub fn ingest_kimi(db: &Database, root: &Path, idle_seconds: f64) -> Result<usize> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut wires = Vec::new();
    walk_files(root, &|name| name == "wire.jsonl", &mut wires);
    sort_paths(&mut wires);
    let mut inserted = 0;
    for wire in wires {
        let (Some(agent_dir), Some(agents)) = (wire.parent(), wire.parent().and_then(Path::parent)) else {
            continue;
        };
        if agents.file_name().map(|n| n.to_string_lossy() == "agents") != Some(true) {
            continue;
        }
        let Some(session_dir) = agents.parent() else {
            continue;
        };
        let session_id = format!("{}:{}", session_dir.file_name().unwrap_or_default().to_string_lossy(), agent_dir.file_name().unwrap_or_default().to_string_lossy());
        inserted += ingest_chat_file(db, &wire, "kimi", &session_id, &mut kimi_event, idle_seconds, &kimi_cwd(session_dir))?;
    }
    Ok(inserted)
}

// -- Claude Code <slug>/*.jsonl ---------------------------------------------------------

pub fn claude_event(obj: &Value) -> Option<Event> {
    let role = role_of(obj.get("type"))?;
    if obj.get("isMeta").is_some_and(py_truthy) || obj.get("isCompactSummary").is_some_and(py_truthy) {
        return None; // тела скиллов, заметки об изображениях, сводки сжатия
    }
    let content = obj.get("message").and_then(|m| m.get("content"));
    let texts = match content {
        Some(Value::String(s)) => {
            if pystr::strip(s).is_empty() {
                Vec::new()
            } else {
                vec![s.clone()]
            }
        }
        Some(Value::Array(_)) => text_blocks(content, &["text"]),
        _ => return None,
    };
    if texts.is_empty() {
        return None; // строки только с tool_result — не ходы разговора
    }
    let texts = cleaned(texts);
    if texts.is_empty() {
        return None;
    }
    Some((role, texts.join("\n"), iso_to_epoch(obj.get("timestamp"))))
}

/// Рабочая папка из первых строк сессии; "" — нет.
fn claude_cwd(path: &Path) -> String {
    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    for _ in 0..CLAUDE_CWD_SCAN_LINES {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&raw);
        if !line.contains("\"cwd\"") {
            continue;
        }
        if let Some(cwd) = serde_json::from_str::<Value>(line.trim_end_matches(['\r', '\n']))
            .ok()
            .and_then(|v| v.get("cwd").and_then(Value::as_str).map(str::to_string))
            .filter(|c| !c.is_empty())
        {
            return cwd;
        }
    }
    String::new()
}

fn sorted_children(dir: &Path, want_dirs: bool) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| if want_dirs { p.is_dir() } else { p.is_file() })
                .collect()
        })
        .unwrap_or_default();
    sort_paths(&mut out);
    out
}

/// <root>/<project-slug>/*.jsonl, только верхний уровень.
pub fn ingest_claude(db: &Database, root: &Path, idle_seconds: f64) -> Result<usize> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut inserted = 0;
    for project in sorted_children(root, true) {
        let project_name = project.file_name().unwrap_or_default().to_string_lossy().into_owned();
        for session in sorted_children(&project, false) {
            // glob на Windows сравнивает без учёта регистра.
            if !session.file_name().unwrap_or_default().to_string_lossy().to_lowercase().ends_with(".jsonl") {
                continue;
            }
            let cwd = claude_cwd(&session);
            let scope = if cwd.is_empty() { project_name.clone() } else { cwd };
            inserted += ingest_chat_file(db, &session, "claude", &file_stem(&session), &mut claude_event, idle_seconds, &scope)?;
        }
    }
    Ok(inserted)
}

// -- сессии Codex <yyyy>/<mm>/<dd>/rollout-*.jsonl ------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CodexMeta {
    pub session_id: String,
    pub cwd: String,
    /// Субагент проверки разрешений: его расшифровка — не разговор.
    pub guardian: bool,
}

fn read_lines_lossy(path: &Path) -> Option<impl Iterator<Item = String>> {
    let file = fs::File::open(path).ok()?;
    Some(BufReader::new(file).split(b'\n').map_while(|r| r.ok()).map(|raw| String::from_utf8_lossy(&raw).trim_end_matches('\r').to_string()))
}

fn py_str(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.as_f64() != Some(0.0) => Some(n.to_string()),
        _ => None,
    }
}

pub fn codex_meta(path: &Path) -> CodexMeta {
    let fallback = CodexMeta { session_id: file_stem(path), cwd: String::new(), guardian: false };
    let Some(lines) = read_lines_lossy(path) else {
        return fallback;
    };
    for line in lines {
        let Ok(obj) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = obj.get("type");
        if kind.and_then(Value::as_str) == Some("session_meta") {
            let payload = obj.get("payload");
            let subagent = payload.and_then(|p| p.get("source")).and_then(|s| s.get("subagent"));
            return CodexMeta {
                session_id: py_str(payload.and_then(|p| p.get("session_id"))).unwrap_or_else(|| file_stem(path)),
                cwd: py_str(payload.and_then(|p| p.get("cwd"))).unwrap_or_default(),
                guardian: subagent.and_then(|s| s.get("other")).and_then(Value::as_str) == Some("guardian"),
            };
        }
        if kind.is_some_and(|k| !k.is_null()) {
            break;
        }
    }
    fallback
}

pub fn codex_event(obj: &Value) -> Option<Event> {
    if obj.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = obj.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let role = role_of(payload.get("role"))?;
    let texts = text_blocks(payload.get("content"), &["text", "input_text", "output_text"]);
    if texts.is_empty() {
        return None;
    }
    let texts = cleaned(texts);
    if texts.is_empty() {
        return None;
    }
    Some((role, texts.join("\n"), iso_to_epoch(obj.get("timestamp"))))
}

/// Some(true/false) на начале хода (импортированного или своего), иначе None.
fn imported_turn_start(obj: &Value) -> Option<bool> {
    if obj.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = obj.get("payload").filter(|p| p.is_object())?;
    if payload.get("type").and_then(Value::as_str) != Some("task_started") {
        return None;
    }
    let turn = match payload.get("turn_id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    Some(turn.starts_with(IMPORTED_TURN_PREFIX))
}

fn has_imported_turns(path: &Path) -> bool {
    read_lines_lossy(path).is_some_and(|mut lines| {
        lines.any(|line| {
            line.contains(IMPORTED_TURN_PREFIX)
                && serde_json::from_str::<Value>(&line).ok().and_then(|o| imported_turn_start(&o)) == Some(true)
        })
    })
}

pub fn ingest_codex(db: &Database, root: &Path, idle_seconds: f64) -> Result<usize> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut rollouts = Vec::new();
    walk_files(root, &|name| is_rollout(name), &mut rollouts);
    sort_paths(&mut rollouts);
    let mut inserted = 0;
    for rollout in rollouts {
        let meta = codex_meta(&rollout);
        if meta.guardian {
            continue;
        }
        // Файл читается только после тишины, поэтому импорт не делится между прогонами.
        let mut imported = false;
        let mut event = |obj: &Value| {
            if let Some(start) = imported_turn_start(obj) {
                imported = start;
            }
            if imported {
                None
            } else {
                codex_event(obj)
            }
        };
        inserted += ingest_chat_file(db, &rollout, "codex", &meta.session_id, &mut event, idle_seconds, &meta.cwd)?;
    }
    Ok(inserted)
}

fn is_rollout(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("rollout-") && lower.ends_with(".jsonl")
}

/// Удалить фрагменты сессий Codex с импортированными ходами и перемотать их файлы:
/// следующая индексация оставит только свои ходы. `roots` включает и
/// archived_sessions: Codex переносит туда уже проиндексированные сессии.
pub fn prune_imported_codex(db: &Database, roots: &[PathBuf]) -> Result<usize> {
    let mut rollouts = Vec::new();
    for root in roots.iter().filter(|r| r.is_dir()) {
        walk_files(root, &|name| is_rollout(name), &mut rollouts);
    }
    sort_paths(&mut rollouts);
    db.atomic(|| {
        let mut removed = 0;
        let mut touched = Vec::new();
        for rollout in &rollouts {
            if !has_imported_turns(rollout) {
                continue;
            }
            let prefix = format!("codex:{}:", codex_meta(rollout).session_id);
            for (id, unit) in fragments_with_prefix(db, "codex", &prefix)? {
                db.delete_card(id)?;
                touched.push(unit);
                removed += 1;
            }
            for key in [format!("offset:codex:{}", path_str(rollout)), format!("turns:codex:{}", path_str(rollout))] {
                if db.get_state(&key)?.is_some() {
                    db.set_state(&key, "0")?;
                }
            }
        }
        db.drop_orphan_unit_cards(&touched)?;
        Ok(removed)
    })
}

fn fragments_with_prefix(db: &Database, source: &str, prefix: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = db
        .connection()
        .prepare("SELECT id, unit FROM cards WHERE source=? AND kind='raw' AND substr(source_ref,1,?)=?")?;
    let rows = stmt.query_map(rusqlite::params![source, pystr::char_len(prefix) as i64, prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// -- история git ------------------------------------------------------------------------

fn is_link(path: &Path) -> bool {
    // std считает ссылкой и junction, Python 3.11 — нет; junction в рабочей области
    // есть только внутри node_modules, который обход и так пропускает.
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Репозитории в корне и ниже (до max_depth уровней) в порядке путей; служебные
/// каталоги и символьные ссылки не обходятся.
/// Перекрытие окна `git log --since`, сек: две недели (как ingest.py).
const GIT_SINCE_OVERLAP: i64 = 14 * 24 * 3600;

pub fn iter_git_repos(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut repos = Vec::new();
    if !root.is_dir() {
        return repos;
    }
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((path, depth)) = stack.pop() {
        if path.join(".git").exists() && !worktree_of_repo_under(&path, root) {
            repos.push(path.clone());
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        sort_paths(&mut children);
        for child in children.into_iter().rev() {
            let name = child.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if !GIT_SKIP_DIRS.contains(&name.as_str()) && !is_link(&child) {
                stack.push((child, depth + 1));
            }
        }
    }
    repos
}

/// Связанный worktree, чей основной репозиторий тоже под root: его коммиты — коммиты
/// основного, второй обход проиндексировал бы их как 'git:<wt>:<sha>' ещё раз.
/// У сабмодуля .git тоже файл, но gitdir у него .git/modules/…
fn worktree_of_repo_under(path: &Path, root: &Path) -> bool {
    let dot = path.join(".git");
    if !dot.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&dot) else {
        return false;
    };
    let Some(gitdir) = text.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let full = path.join(gitdir.trim());
    let parts: Vec<_> = full.components().collect();
    let Some(at) = parts.iter().rposition(|c| c.as_os_str() == "worktrees") else {
        return false;
    };
    let main_git: PathBuf = parts[..at].iter().collect();
    let Some(main) = main_git.parent() else {
        return false;
    };
    match (main.canonicalize(), root.canonicalize()) {
        (Ok(main), Ok(root)) => main.starts_with(root),
        _ => false,
    }
}

/// Устойчивый id репозитория в source_ref: путь от корня git ('.shared',
/// 'apps/_active/notes'), для самого корня — его имя.
pub fn git_repo_id(root: &Path, repo: &Path) -> String {
    let rel: Vec<String> = repo
        .strip_prefix(root)
        .map(|r| r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    if rel.is_empty() {
        root.file_name().unwrap_or_default().to_string_lossy().into_owned()
    } else {
        rel.join("/")
    }
}

fn git(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Индексация идёт и из GUI-процесса: без флага каждый git мигал бы консолью.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// `output()` с дедлайном (как `timeout=120` у subprocess.run в ingest.py): git, вставший
/// на сетевом диске или чужом lock-файле, иначе держал бы весь sync навсегда.
/// stdout/stderr читаются в своих потоках — непрочитанный пайп остановил бы git.
fn output_with_timeout(mut cmd: Command, limit: std::time::Duration) -> Option<std::process::Output> {
    use std::process::Stdio;
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    Some(std::process::Output { status, stdout: out.join().ok()?, stderr: err.join().ok()? })
}

const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// user.name и user.email владельца репозитория (действующая конфигурация).
pub fn git_identity(repo: &Path) -> Vec<String> {
    ["user.name", "user.email"]
        .iter()
        .filter_map(|key| {
            let mut cmd = git(repo);
            cmd.args(["config", key]);
            let out = output_with_timeout(cmd, GIT_TIMEOUT)?;
            let value = pystr::strip(&String::from_utf8_lossy(&out.stdout)).to_string();
            (out.status.success() && !value.is_empty()).then_some(value)
        })
        .collect()
}

pub type Commit = (String, String, String, String);

/// (hash, author_ts, subject, body) коммитов любого из `authors` (строки,
/// без учёта регистра; пусто — все), None при ошибке git.
pub fn git_log(repo: &Path, authors: &[String], since: i64) -> Option<Vec<Commit>> {
    let mut cmd = git(repo);
    cmd.args(["-c", "core.quotepath=false", "log", "--pretty=format:%H%x1f%at%x1f%s%x1f%b%x1e"]);
    if !authors.is_empty() {
        cmd.args(["--fixed-strings", "--regexp-ignore-case"]);
        cmd.args(authors.iter().map(|a| format!("--author={a}")));
    }
    if since > 0 {
        // --since=@0 на git 2.56 не находит ничего; первый прогон читает всю историю.
        // --since сравнивает дату коммитера: коммит, приехавший pull'ом со старой датой,
        // прошлый знак уже перешагнул. Перекрытие окна гасит дедуп по source_ref (как ingest.py).
        cmd.arg(format!("--since=@{}", (since - GIT_SINCE_OVERLAP).max(1)));
    }
    let out = output_with_timeout(cmd, GIT_TIMEOUT)?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut commits = Vec::new();
    for record in text.split('\u{1e}') {
        let record = record.trim_matches(['\r', '\n']);
        if pystr::strip(record).is_empty() {
            continue;
        }
        let parts: Vec<&str> = record.split('\u{1f}').collect();
        if parts.len() < 3 {
            continue;
        }
        let field = |i: usize| parts.get(i).map_or(String::new(), |p| pystr::strip(p).to_string());
        commits.push((field(0), field(1), field(2), field(3)));
    }
    Some(commits)
}

/// Коммиты владельца (или `authors`) во всех репозиториях под root. У каждого
/// репозитория свой водяной знак: новый репозиторий читается с первого коммита.
pub fn ingest_git(db: &Database, root: &Path, authors: Option<&[String]>) -> Result<usize> {
    let now = now_ts();
    let mut inserted = 0;
    for repo in iter_git_repos(root, GIT_MAX_DEPTH) {
        let repo_id = git_repo_id(root, &repo);
        let watermark_key = format!("git:last_run:{repo_id}");
        let since = state_int(db, &watermark_key)?;
        let wanted = authors.map_or_else(|| git_identity(&repo), <[String]>::to_vec);
        // Идентичность не прочиталась: пустой фильтр значит «все авторы», и в память ушла бы
        // чужая история. Знак не двигаем — повторим следующим прогоном (как ingest.py).
        if authors.is_none() && wanted.is_empty() {
            continue;
        }
        let Some(commits) = git_log(&repo, &wanted, since) else {
            continue; // git не ответил: знак остаётся, повтор в следующий раз
        };
        inserted += db.atomic(|| {
            let mut inserted = 0;
            for (hash, author_ts, subject, body) in &commits {
                let masked = mask_secrets(subject);
                let subject = if masked.is_empty() { "(no subject)".to_string() } else { masked };
                let body = mask_secrets(body);
                let ts = if !author_ts.is_empty() && author_ts.bytes().all(|b| b.is_ascii_digit()) { author_ts.parse().unwrap_or(now) } else { now };
                let card = CardDraft::new(
                    "git",
                    &format!("git:{repo_id}:{hash}"),
                    ts,
                    &truncate(&subject, TITLE_MAX_CHARS),
                    &if body.is_empty() { subject.clone() } else { truncate(&body, GIT_BODY_MAX_CHARS) },
                );
                if db.insert_card(&card)? {
                    inserted += 1;
                }
            }
            db.set_state(&watermark_key, &now.to_string())?;
            Ok(inserted)
        })?;
    }
    Ok(inserted)
}

/// Удалить git-фрагменты существующих репозиториев, которые текущее правило
/// авторов не индексировало бы (например, клон чужого upstream).
pub fn prune_foreign_commits(db: &Database, root: &Path, authors: Option<&[String]>) -> Result<usize> {
    db.atomic(|| {
        let mut removed = 0;
        let mut touched = Vec::new();
        for repo in iter_git_repos(root, GIT_MAX_DEPTH) {
            let repo_id = git_repo_id(root, &repo);
            let wanted = authors.map_or_else(|| git_identity(&repo), <[String]>::to_vec);
            if wanted.is_empty() {
                continue;
            }
            let Some(own) = git_log(&repo, &wanted, 0) else {
                continue;
            };
            let keep: std::collections::HashSet<String> = own.into_iter().map(|c| c.0).collect();
            let prefix = format!("git:{repo_id}:");
            let mut stmt = db
                .connection()
                .prepare("SELECT id, source_ref, unit FROM cards WHERE source='git' AND kind='raw' AND substr(source_ref,1,?)=?")?;
            let rows: Vec<(i64, String, String)> = stmt
                .query_map(rusqlite::params![pystr::char_len(&prefix) as i64, prefix], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, source_ref, unit) in rows {
                let hash = &source_ref[prefix.len()..];
                if !hash.contains(':') && !keep.contains(hash) {
                    db.delete_card(id)?;
                    touched.push(unit);
                    removed += 1;
                }
            }
        }
        db.drop_orphan_unit_cards(&touched)?;
        Ok(removed)
    })
}

// -- точка входа ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Sources {
    pub kimi_sessions: PathBuf,
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    pub git_root: PathBuf,
    pub session_idle_seconds: f64,
    /// Авторы коммитов (имя или почта, без учёта регистра); None — владелец каждого репозитория.
    pub git_authors: Option<Vec<String>>,
}

fn home_dir() -> PathBuf {
    // Path.home() Python на Windows берёт USERPROFILE.
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default()
}

/// Пути строятся из частей, как в Python.
fn build_sources(git_root: PathBuf) -> Sources {
    let home = home_dir();
    Sources {
        kimi_sessions: home.join(".kimi-code").join("sessions"),
        claude_projects: home.join(".claude").join("projects"),
        codex_sessions: home.join(".codex").join("sessions"),
        git_root,
        session_idle_seconds: SESSION_IDLE_SECONDS,
        git_authors: None,
    }
}

/// Конфиг важнее: `[sources] git_root` в config.toml.
pub fn sources_for(config: &crate::config::Config) -> Sources {
    build_sources(config.git_root.clone().unwrap_or_else(crate::config::project_root))
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct IngestCounts {
    pub kimi: usize,
    pub claude: usize,
    pub codex: usize,
    pub git: usize,
    pub obsidian: usize,
}

pub fn ingest_all(db: &Database, sources: &Sources, config: &crate::config::Config) -> Result<IngestCounts> {
    Ok(IngestCounts {
        kimi: ingest_kimi(db, &sources.kimi_sessions, sources.session_idle_seconds)?,
        claude: ingest_claude(db, &sources.claude_projects, sources.session_idle_seconds)?,
        codex: ingest_codex(db, &sources.codex_sessions, sources.session_idle_seconds)?,
        git: ingest_git(db, &sources.git_root, sources.git_authors.as_deref())?,
        obsidian: crate::obsidian::ingest_obsidian_sources(db, config)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_truncated_session_file_does_not_resurrect_its_turns() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("memory.db")).unwrap();
        let path = dir.path().join("chat.jsonl");
        let first = "{\"role\":\"user\",\"text\":\"первый\"}\n{\"role\":\"assistant\",\"text\":\"ответ\"}\n";
        fs::write(&path, format!("{first}{{\"role\":\"user\",\"text\":\"второй\"}}\n{{\"role\":\"assistant\",\"text\":\"ответ два\"}}\n")).unwrap();
        let mut events = |value: &Value| {
            Some((value.get("role")?.as_str()?.to_owned(), value.get("text")?.as_str()?.to_owned(), Some(1_790_000_000)))
        };
        assert_eq!(ingest_chat_file(&db, &path, "claude", "s1", &mut events, 0.0, "").unwrap(), 2);

        // Отмотка: файл стал короче, и смещение ушло за его конец.
        fs::write(&path, first).unwrap();
        assert_eq!(ingest_chat_file(&db, &path, "claude", "s1", &mut events, 0.0, "").unwrap(), 0);
        assert!(db.get_card_by_ref("claude:s1:0").unwrap().is_some(), "первый ход остаётся в базе");
        assert!(db.get_card_by_ref("claude:s1:2").unwrap().is_none(), "отмотанный ход не должен встать второй копией");
    }

    fn line(role: &str, text: &str) -> String {
        format!("{}\n", serde_json::json!({"role": role, "text": text}))
    }

    #[test]
    fn a_replaced_session_file_is_reread_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("memory.db")).unwrap();
        let path = dir.path().join("chat.jsonl");
        let mut events = |value: &Value| {
            Some((value.get("role")?.as_str()?.to_owned(), value.get("text")?.as_str()?.to_owned(), Some(1_790_000_000)))
        };
        let turn = |n: usize| format!("{}{}", line("user", &format!("вопрос {n} {}", "z".repeat(100))), line("assistant", &format!("ответ {n}")));
        // Старый файл подогнан так, что его конец падает внутрь вопроса 2 нового:
        // чтение со смещения потеряло бы вопрос и приклеило «ответ 2» к чужому ходу.
        let head = format!("{}{}{}", line("user", "a"), line("assistant", "b"), line("user", "c"));
        let target = turn(1).len() + 10;
        let old = format!("{head}{}", line("assistant", &"y".repeat(target - head.len() - line("assistant", "").len())));
        assert_eq!(old.len(), target);
        fs::write(&path, &old).unwrap();
        assert_eq!(ingest_chat_file(&db, &path, "claude", "s1", &mut events, 0.0, "").unwrap(), 2);

        fs::write(&path, format!("{}{}{}", turn(1), turn(2), turn(3))).unwrap();
        assert_eq!(ingest_chat_file(&db, &path, "claude", "s1", &mut events, 0.0, "").unwrap(), 1);
        let second = db.get_card_by_ref("claude:s1:1").unwrap().expect("второй ход на месте");
        assert!(!second.body.contains("ответ 2"), "{}", second.body);
        let third = db.get_card_by_ref("claude:s1:2").unwrap().expect("третий ход добавлен");
        assert!(third.body.contains("ответ 3"), "{}", third.body);
    }

    #[test]
    fn a_complete_last_line_without_newline_is_ingested_after_idle() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("memory.db")).unwrap();
        let path = dir.path().join("chat.jsonl");
        fs::write(&path, format!("{}{}", line("user", "q"), line("assistant", "ответ").trim_end())).unwrap();
        let mut events = |value: &Value| {
            Some((value.get("role")?.as_str()?.to_owned(), value.get("text")?.as_str()?.to_owned(), Some(1_790_000_000)))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(ingest_chat_file(&db, &path, "claude", "s1", &mut events, 0.01, "").unwrap(), 1);
        let card = db.get_card_by_ref("claude:s1:0").unwrap().expect("ход сохранён");
        assert!(card.body.contains("ответ"), "{}", card.body);
    }

    #[test]
    fn linked_worktree_of_repo_under_root_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Git");
        fs::create_dir_all(root.join("app/.git/worktrees/wt")).unwrap();
        fs::create_dir_all(root.join("app-wt")).unwrap();
        let gitdir = root.join("app/.git/worktrees/wt");
        fs::write(root.join("app-wt/.git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/.git"), "gitdir: ../app/.git/modules/sub\n").unwrap();
        let found: Vec<String> = iter_git_repos(&root, GIT_MAX_DEPTH)
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found, ["app", "sub"]);
    }

    #[test]
    fn iso_forms() {
        assert_eq!(parse_iso("2026-11-06T01:21:00+00:00"), Some(1_793_928_060));
        assert_eq!(parse_iso("2026-11-06T04:21:00.123456+03:00"), Some(1_793_928_060));
        assert_eq!(parse_iso("2026-11-06 01:21"), Some(1_793_928_060));
        assert_eq!(parse_iso("2026-11-06"), Some(1_793_923_200));
        assert_eq!(parse_iso("2026-02-30"), None);
        assert_eq!(parse_iso("06.11.2026"), None);
    }

    #[test]
    fn runs_are_masked_only_when_maximal_and_long() {
        let hex = "a".repeat(40);
        assert_eq!(mask_secrets(&format!("x {hex} y")), "x [REDACTED:hex] y");
        assert_eq!(mask_secrets(&"a".repeat(39)), "a".repeat(39));
        let blob = format!("{}==", "Q".repeat(44));
        assert_eq!(mask_secrets(&format!("k={blob};")), "k=[REDACTED:blob];");
        assert_eq!(mask_secrets(&format!("{}=x", "Q".repeat(44))), "[REDACTED:blob]=x");
    }

    #[test]
    fn vendor_tokens_shorter_than_runs_are_masked() {
        for token in [
            format!("ghp_{}", "a1B2".repeat(9)),
            format!("glpat-{}", "x".repeat(20)),
            format!("AIza{}", "Z".repeat(35)),
            "xoxb-1234567890-abcdef".to_string(),
        ] {
            assert_eq!(mask_secrets(&format!("t={token} ok")), "t=[REDACTED:token] ok");
        }
    }

    #[test]
    fn closed_blocks_need_their_own_closing_tag() {
        assert_eq!(clean_chat_text("до <system-reminder>шум</system-reminder> после"), "до  после");
        assert_eq!(clean_chat_text("a <image x>не</system-reminder> b"), "a <image x>не</system-reminder> b");
        assert_eq!(clean_chat_text("<system-reminder>обрезано без конца"), "");
    }
}
