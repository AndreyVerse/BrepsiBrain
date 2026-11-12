//! Хранилище Obsidian: заметки markdown → карточки — порт `obsidian.py`.
//!
//! Заметки правят и удаляют, поэтому коннектор не дописывает, а приводит
//! карточки к текущему состоянию хранилища: source_ref стабилен, вставка,
//! правка или пропуск решаются сравнением содержимого, исчезнувшие удаляются.
//! Модель коннектор не вызывает: раздел заметки — обычная запись в очереди.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::config::Config;
use crate::db::{CardDraft, Database, Result};
use crate::ingest::{epoch_of, mask_secrets, parse_iso, py_path_key, truncate, BODY_MAX_CHARS};
use crate::pystr;

pub const SOURCE: &str = "obsidian";
/// Хранилище по умолчанию лежит рядом с папкой данных: у пользователя с
/// GitHub нет пути владельца, а пустой список `[obsidian] vaults` не должен
/// молча индексировать чужую машину.
pub fn default_vault() -> PathBuf {
    crate::config::project_root().join("Obsidian")
}
const TITLE_MAX_CHARS: usize = 80;
/// Заметка до этого размера остаётся одной карточкой.
const NOTE_CHUNK_CHARS: usize = BODY_MAX_CHARS;

struct Patterns {
    heading: Regex,
    wikilink: Regex,
    md_image: Regex,
    md_link: Regex,
    /// `(?<!\w)_…_(?!\w)` — lookaround, которого нет в крейте regex.
    md_inline: fancy_regex::Regex,
    bullet: Regex,
    numbered: Regex,
    blank_runs: Regex,
    slug: Regex,
    meta_key: Regex,
    meta_item: Regex,
    strptime_seconds: Regex,
    strptime_minutes: Regex,
    strptime_date: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("static pattern");
        Patterns {
            heading: re(r"^(#{1,6})\s+(.+?)\s*#*\s*$"),
            wikilink: re(r"\[\[([^\]|]*)(?:\|([^\]]*))?\]\]"),
            md_image: re(r"!\[[^\]]*\]\([^)]*\)"),
            md_link: re(r"\[([^\]]*)\]\([^)]*\)"),
            md_inline: fancy_regex::Regex::new(r"``?`([^`]+)``?`|\*\*([^*]+)\*\*|__([^_]+)__|\*([^*]+)\*|(?<!\w)_([^_]+)_(?!\w)|~~([^~]+)~~")
                .expect("static pattern"),
            bullet: re(r"^[-*+]\s+"),
            numbered: re(r"^\d+[.)]\s+"),
            blank_runs: re(r"\n{3,}"),
            slug: re(r"[^0-9A-Za-zА-Яа-я]+"),
            meta_key: re(r"^([A-Za-z_][\w-]*)\s*:\s*(.*)$"),
            meta_item: re(r"^\s*-\s+(.+)$"),
            // Регулярки strptime Python: однозначные поля допустимы, пробел — \s+.
            strptime_seconds: re(r"(?i)^(\d{4})-(\d{1,2})-(\d{1,2})\s+(\d{1,2}):(\d{1,2}):(\d{1,2})$"),
            strptime_minutes: re(r"(?i)^(\d{4})-(\d{1,2})-(\d{1,2})\s+(\d{1,2}):(\d{1,2})$"),
            strptime_date: re(r"^(\d{4})-(\d{1,2})-(\d{1,2})$"),
        }
    })
}

// -- frontmatter ------------------------------------------------------------------------

/// Значение frontmatter: строка или список, как dict из `_parse_frontmatter`.
#[derive(Debug, Clone, PartialEq)]
pub enum MetaValue {
    Str(String),
    List(Vec<MetaValue>),
}

impl MetaValue {
    /// `str(value)` Python: список печатается как repr.
    fn py_str(&self) -> String {
        match self {
            MetaValue::Str(s) => s.clone(),
            MetaValue::List(items) => format!("[{}]", items.iter().map(MetaValue::py_repr).collect::<Vec<_>>().join(", ")),
        }
    }

    fn py_repr(&self) -> String {
        match self {
            MetaValue::Str(s) => pystr::repr(s),
            MetaValue::List(_) => self.py_str(),
        }
    }
}

fn strip_quotes(s: &str) -> &str {
    s.trim_matches(['\'', '"'])
}

fn unquote(value: &str) -> MetaValue {
    let value = pystr::strip(value);
    if value.starts_with('[') && value.ends_with(']') && value.len() >= 2 {
        let inner = pystr::strip(&value[1..value.len() - 1]);
        if inner.is_empty() {
            return MetaValue::List(Vec::new());
        }
        return MetaValue::List(
            inner
                .split(',')
                .filter(|v| !pystr::strip(v).is_empty())
                .map(|v| MetaValue::Str(strip_quotes(pystr::strip(v)).to_string()))
                .collect(),
        );
    }
    MetaValue::Str(strip_quotes(value).to_string())
}

pub fn parse_frontmatter(lines: &[&str]) -> Vec<(String, MetaValue)> {
    let p = patterns();
    let mut meta: Vec<(String, MetaValue)> = Vec::new();
    let mut key: Option<String> = None;
    let set = |meta: &mut Vec<(String, MetaValue)>, k: &str, v: MetaValue| match meta.iter_mut().find(|(name, _)| name == k) {
        Some(slot) => slot.1 = v,
        None => meta.push((k.to_string(), v)),
    };
    for raw in lines {
        let line = pystr::rstrip(raw);
        if pystr::strip(line).is_empty() || line.trim_start_matches(pystr::is_space).starts_with('#') {
            continue;
        }
        if let Some(m) = p.meta_key.captures(line) {
            let k = m[1].to_lowercase();
            let value = pystr::strip(&m[2]);
            let parsed = if !value.is_empty() {
                unquote(value)
            } else if k == "tags" {
                MetaValue::List(Vec::new())
            } else {
                MetaValue::Str(String::new())
            };
            set(&mut meta, &k, parsed);
            key = Some(k);
            continue;
        }
        if let (Some(m), Some(k)) = (p.meta_item.captures(line), key.as_ref()) {
            let item = unquote(pystr::strip(&m[1]));
            match meta.iter_mut().find(|(name, _)| name == k) {
                Some((_, MetaValue::List(items))) => items.push(item),
                Some(slot) => slot.1 = MetaValue::List(vec![item]),
                None => meta.push((k.clone(), MetaValue::List(vec![item]))),
            }
        }
    }
    meta
}

fn split_frontmatter(text: &str) -> (Vec<(String, MetaValue)>, String) {
    let lines = pystr::splitlines(text);
    if lines.is_empty() || pystr::strip(lines[0]) != "---" {
        return (Vec::new(), text.to_string());
    }
    match (1..lines.len()).find(|&i| pystr::strip(lines[i]) == "---") {
        Some(end) => (parse_frontmatter(&lines[1..end]), lines[end + 1..].join("\n")),
        None => (Vec::new(), text.to_string()),
    }
}

fn strptime(re: &Regex, text: &str) -> Option<i64> {
    let m = re.captures(text)?;
    let field = |i: usize| m.get(i).map_or(Some(0), |g| g.as_str().parse::<u32>().ok());
    let year: i64 = m[1].parse().ok()?;
    epoch_of(year, field(2)?, field(3)?, field(4)?, field(5)?, field(6)?, 0)
}

/// `_parse_date`: fromisoformat, затем форматы strptime; без зоны — UTC.
pub fn parse_date(value: &str) -> Option<i64> {
    let text = pystr::strip(value).replace('Z', "+00:00");
    if let Some(ts) = parse_iso(&text) {
        return Some(ts);
    }
    let p = patterns();
    if !text.contains('T') && text.contains(' ') {
        strptime(&p.strptime_seconds, &text).or_else(|| strptime(&p.strptime_minutes, &text))
    } else if !text.contains('T') {
        strptime(&p.strptime_date, &text)
    } else {
        None
    }
}

fn meta_get<'a>(meta: &'a [(String, MetaValue)], key: &str) -> Option<&'a MetaValue> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn mtime_or_now(path: &Path) -> i64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(crate::db::now_ts, |d| d.as_secs() as i64)
}

fn note_timestamp(meta: &[(String, MetaValue)], path: &Path) -> i64 {
    for key in ["date", "created", "updated"] {
        if let Some(MetaValue::Str(value)) = meta_get(meta, key) {
            if let Some(ts) = (!value.is_empty()).then(|| parse_date(value)).flatten() {
                return ts;
            }
        }
    }
    mtime_or_now(path)
}

// -- markdown ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// Путь от корня хранилища, прямые слэши.
    pub relpath: String,
    pub stem: String,
    pub ts: i64,
    pub tags: Vec<String>,
    /// Без frontmatter.
    pub markdown: String,
}

/// None — заметку сейчас не прочитать: ошибка ОС или файл меняется во время чтения
/// (половина правки ушла бы в пересказ). Как parse_note в obsidian.py.
pub fn parse_note(path: &Path, relpath: &str) -> Option<Note> {
    let before = fs::metadata(path).ok()?;
    let bytes = fs::read(path).ok()?;
    let after = fs::metadata(path).ok()?;
    if (before.len(), before.modified().ok()) != (after.len(), after.modified().ok()) {
        return None;
    }
    // read_text в Python переводит \r\n и \r в \n.
    let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n").replace('\r', "\n");
    let (meta, body) = split_frontmatter(&text);
    let tags = match meta_get(&meta, "tags") {
        Some(MetaValue::List(items)) => items
            .iter()
            .map(MetaValue::py_str)
            .filter(|t| !pystr::strip(t).is_empty())
            .map(|t| t.trim_start_matches('#').to_string())
            .collect(),
        _ => Vec::new(),
    };
    Some(Note {
        relpath: relpath.to_string(),
        stem: path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
        ts: note_timestamp(&meta, path),
        tags,
        markdown: body,
    })
}

fn replace_inline(text: &str) -> String {
    let p = patterns();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for caps in p.md_inline.captures_iter(text) {
        // Ошибка бэктрекинга на патологическом вводе: строка остаётся как есть.
        let Ok(caps) = caps else {
            return text.to_string();
        };
        let whole = caps.get(0).expect("match");
        out.push_str(&text[copied..whole.start()]);
        if let Some(group) = (1..caps.len()).find_map(|i| caps.get(i)) {
            out.push_str(group.as_str());
        }
        copied = whole.end();
    }
    out.push_str(&text[copied..]);
    out
}

/// Markdown → читаемый текст: разметка снята, ссылки раскрыты.
pub fn plain_text(markdown: &str) -> String {
    let p = patterns();
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in pystr::splitlines(markdown) {
        let mut s = pystr::rstrip(line).to_string();
        if pystr::strip(&s).starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            let mut stripped = pystr::strip(&s).to_string();
            if stripped.is_empty() || ["---", "***", "___"].contains(&stripped.as_str()) {
                continue;
            }
            if let Some(m) = p.heading.captures(&stripped) {
                stripped = pystr::strip(&m[2]).to_string();
            }
            if stripped.starts_with('>') {
                stripped = pystr::strip(stripped.trim_start_matches('>')).to_string();
            }
            stripped = p.bullet.replace(&stripped, "").into_owned();
            stripped = p.numbered.replace(&stripped, "").into_owned();
            s = pystr::strip(stripped.trim_matches('|')).to_string();
        }
        let s = p.wikilink.replace_all(&s, |m: &regex::Captures<'_>| {
            let label = m.get(2).map(|g| g.as_str()).filter(|g| !g.is_empty()).unwrap_or(&m[1]);
            pystr::strip(label).to_string()
        });
        let s = p.md_image.replace_all(&s, "");
        let s = p.md_link.replace_all(&s, "$1");
        out.push(replace_inline(&s));
    }
    let text = out.join("\n");
    pystr::strip(&p.blank_runs.replace_all(&text, "\n\n")).to_string()
}

fn slug(heading: &str) -> String {
    let replaced = patterns().slug.replace_all(heading, "-");
    let slug = replaced.trim_matches('-').to_lowercase();
    pystr::take_chars(if slug.is_empty() { "section" } else { &slug }, 40).to_string()
}

/// (суффикс ref, название, текст); суффикс "" или "#slug". Небольшая заметка —
/// одна карточка, большая делится по заголовкам H1/H2: глубже большие
/// проектные документы рассыпались бы на сотни заглушек.
pub fn cards_for_note(note: &Note) -> Vec<(String, String, String)> {
    let p = patterns();
    let mut sections: Vec<(Option<String>, String)> = Vec::new();
    let mut heading: Option<String> = None;
    let mut buf: Vec<&str> = Vec::new();
    for line in pystr::splitlines(&note.markdown) {
        match p.heading.captures(line) {
            Some(m) if m[1].len() <= 2 => {
                sections.push((heading.take(), buf.join("\n")));
                heading = Some(pystr::strip(&m[2]).to_string());
                buf.clear();
            }
            _ => buf.push(line),
        }
    }
    sections.push((heading, buf.join("\n")));
    let sections: Vec<(Option<String>, String)> = sections
        .into_iter()
        .filter(|(head, chunk)| head.is_some() || !pystr::strip(chunk).is_empty())
        .map(|(head, chunk)| (head, plain_text(&chunk)))
        .collect();
    let total: usize = sections.iter().map(|(_, chunk)| pystr::char_len(chunk)).sum();
    if sections.len() <= 1 || total <= NOTE_CHUNK_CHARS {
        let title = sections.iter().find_map(|(head, _)| head.clone().filter(|h| !h.is_empty())).unwrap_or_else(|| note.stem.clone());
        return vec![(String::new(), title, plain_text(&note.markdown))];
    }
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut cards = Vec::new();
    for (head, chunk) in sections {
        let base = slug(head.as_deref().filter(|h| !h.is_empty()).unwrap_or(&note.stem));
        let count = match seen.iter_mut().find(|(b, _)| *b == base) {
            Some(slot) => {
                slot.1 += 1;
                slot.1
            }
            None => {
                seen.push((base.clone(), 1));
                1
            }
        };
        let slug = if count == 1 { base } else { format!("{base}-{count}") };
        let title = match head.as_deref().filter(|h| !h.is_empty()) {
            Some(h) => format!("{} — {h}", note.stem),
            None => note.stem.clone(),
        };
        cards.push((format!("#{slug}"), title, chunk));
    }
    cards
}

fn collect_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            collect_md(&path, out);
        } else if kind.is_file() && entry.file_name().to_string_lossy().to_lowercase().ends_with(".md") {
            out.push(path);
        }
    }
}

pub fn iter_notes(vault: &Path, exclude_dirs: &[String]) -> Vec<Note> {
    iter_notes_with_unreadable(vault, exclude_dirs).0
}

/// Заметки и relpath тех, что есть, но сейчас не читаются: их карточки prune не трогает.
pub fn iter_notes_with_unreadable(vault: &Path, exclude_dirs: &[String]) -> (Vec<Note>, std::collections::HashSet<String>) {
    let mut unreadable = std::collections::HashSet::new();
    let mut paths = Vec::new();
    collect_md(vault, &mut paths);
    paths.sort_by_cached_key(|p| py_path_key(p));
    let mut notes = Vec::new();
    for path in paths {
        let Ok(rel) = path.strip_prefix(vault) else {
            continue;
        };
        let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let (name, dirs) = parts.split_last().expect("file has a name");
        if dirs.iter().any(|d| exclude_dirs.contains(d) || d.starts_with('.')) || name.starts_with('.') {
            continue;
        }
        let relpath = parts.join("/");
        match parse_note(&path, &relpath) {
            Some(note) => {
                if !pystr::strip(&note.markdown).is_empty() || !note.tags.is_empty() {
                    notes.push(note);
                }
            }
            None => {
                unreadable.insert(relpath);
            }
        }
    }
    (notes, unreadable)
}

// -- запись карточек ----------------------------------------------------------------------

fn finalize(title: &str, body: &str, tags: &[String]) -> (String, String) {
    let body = if tags.is_empty() {
        body.to_string()
    } else {
        format!("Tags: {}\n\n{body}", tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "))
    };
    (truncate(&mask_secrets(title), TITLE_MAX_CHARS), truncate(&mask_secrets(&body), BODY_MAX_CHARS))
}

#[derive(Debug, PartialEq)]
enum Upsert {
    New,
    Updated,
    Same,
}

fn upsert_card(db: &Database, card: &CardDraft) -> Result<Upsert> {
    let Some(existing) = db.get_card_by_ref(&card.source_ref)? else {
        db.insert_card(card)?;
        return Ok(Upsert::New);
    };
    if existing.ts == card.ts && existing.title == card.title && existing.body == card.body {
        return Ok(Upsert::Same);
    }
    db.update_card(existing.id, card.ts, &card.title, &card.body, None, true)?;
    Ok(Upsert::Updated)
}

fn prune_cards(
    db: &Database,
    key: &str,
    live: &std::collections::HashSet<String>,
    keep: &std::collections::HashSet<String>,
) -> Result<usize> {
    let prefix = format!("{SOURCE}:{key}:");
    let like = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_") + "%";
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = db.connection().prepare("SELECT id, source_ref, unit FROM cards WHERE source=? AND source_ref LIKE ? ESCAPE '\\'")?;
        let rows = stmt.query_map(rusqlite::params![SOURCE, like], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut removed = 0;
    let mut gone = Vec::new();
    for (id, source_ref, unit) in rows {
        if live.contains(&source_ref) {
            continue;
        }
        // Заметка есть, но не прочиталась: её карточки и знания не удаляем
        // `#` бывает и в имени файла (`C# notes.md`), а не только перед разделом:
        // проверяем каждый префикс до `#`, иначе карточка такой заметки удалялась.
        let rest = &source_ref[prefix.len()..];
        if keep.contains(rest) || rest.match_indices('#').any(|(index, _)| keep.contains(&rest[..index])) {
            continue;
        }
        db.delete_card(id)?;
        gone.push(unit);
        removed += 1;
    }
    // Удалённая заметка уносит и свою карточку знаний.
    db.drop_orphan_unit_cards(&gone)?;
    Ok(removed)
}

/// Как `str(Path.resolve())` в Python на Windows: без префикса \\?\.
fn resolved(vault: &Path) -> String {
    let path = fs::canonicalize(vault).unwrap_or_else(|_| vault.to_path_buf());
    let text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        text.strip_prefix(r"\\?\").map_or(text.clone(), str::to_string)
    }
}

/// Читаемое и уникальное имя хранилища: имя папки и хеш пути.
pub fn vault_key(vault: &Path) -> String {
    let digest = format!("{:x}", md5::compute(resolved(vault).replace('\\', "/").as_bytes()));
    format!("{}-{}", vault.file_name().unwrap_or_default().to_string_lossy(), &digest[..8])
}

/// Одно хранилище; возвращает число новых карточек.
pub fn ingest_obsidian(db: &Database, vault: &Path, exclude_dirs: &[String]) -> Result<usize> {
    if !vault.is_dir() {
        return Ok(0);
    }
    let key = vault_key(vault);
    let (notes, unreadable) = iter_notes_with_unreadable(vault, exclude_dirs);
    db.atomic(|| {
        let mut live = std::collections::HashSet::new();
        let mut inserted = 0;
        for note in &notes {
            for (suffix, title, body) in cards_for_note(note) {
                let source_ref = format!("{SOURCE}:{key}:{}{suffix}", note.relpath);
                live.insert(source_ref.clone());
                let (title, body) = finalize(&title, &body, &note.tags);
                if upsert_card(db, &CardDraft::new(SOURCE, &source_ref, note.ts, &title, &body))? == Upsert::New {
                    inserted += 1;
                }
            }
        }
        prune_cards(db, &key, &live, &unreadable)?;
        Ok(inserted)
    })
}

/// Все настроенные хранилища, без них — встроенное, если оно есть.
pub fn ingest_obsidian_sources(db: &Database, config: &Config) -> Result<usize> {
    let mut vaults = config.obsidian.vaults.clone();
    let default = default_vault();
    if vaults.is_empty() && default.is_dir() {
        vaults.push(default);
    }
    let mut inserted = 0;
    for vault in vaults {
        inserted += ingest_obsidian(db, &vault, &config.obsidian.exclude_dirs)?;
    }
    Ok(inserted)
}
