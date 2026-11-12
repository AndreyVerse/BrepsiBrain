//! SQLite-хранилище: схема v4, FTS5 BM25 × свежесть — порт `db.py`.
//!
//! Файл базы общий с Python-реализацией на всё время переноса, поэтому схема,
//! DDL и прагмы совпадают байт в байт, а старые схемы (<4) здесь не мигрируются:
//! такой файл отвергается, миграция остаётся за Python с резервной копией.
//!
//! Потоки: `Database` владеет одним `rusqlite::Connection` (Send, не Sync) и
//! используется из одного потока; каждому потоку — своё соединение. WAL
//! позволяет читать, пока синхронизация пишет из другого процесса.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::pystr;
use crate::text::{self, DISTILLED_PREFIX};

pub const SCHEMA_VERSION: i64 = 5;
pub const CARD_KIND_RAW: &str = "raw";
pub const CARD_KIND_DISTILLED: &str = "distilled";
pub const DEFAULT_SEARCH_LIMIT: i64 = 8;
pub const DEFAULT_RECENT_DAYS: i64 = 7;
pub const DEFAULT_RECENT_LIMIT: i64 = 20;
pub const SCOPE_KEY_PREFIX: &str = "scope:";
pub const FILE_KEY_PREFIX: &str = "file:";
pub const INDEX_REVISION_KEY: &str = "index:revision";

/// Порядок колонок задаёт веса bm25(): название ×6, ключевые слова ×5, кратко ×4,
/// вопросы ×4, атомы ×3, текст ×1.
const FTS_WEIGHTS: &str = "6.0, 5.0, 4.0, 4.0, 3.0, 1.0";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cards(
  id INTEGER PRIMARY KEY,
  source TEXT NOT NULL,
  source_ref TEXT NOT NULL UNIQUE,
  ts INTEGER NOT NULL,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  kind TEXT NOT NULL DEFAULT 'raw',
  keywords TEXT NOT NULL DEFAULT '',
  unit TEXT NOT NULL DEFAULT ''
);
CREATE VIRTUAL TABLE IF NOT EXISTS cards_fts USING fts5(title, keywords, summary, questions, atoms, body, content='', contentless_delete=1);
CREATE TABLE IF NOT EXISTS state(
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS usage(
  id INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  cards INTEGER NOT NULL DEFAULT 0,
  prompt_tokens INTEGER NOT NULL DEFAULT 0,
  completion_tokens INTEGER NOT NULL DEFAULT 0,
  cost_rub REAL NOT NULL DEFAULT 0,
  task TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS distill_queue(
  unit TEXT PRIMARY KEY,
  covered_id INTEGER NOT NULL DEFAULT 0,
  attempts INTEGER NOT NULL DEFAULT 0,
  attempt_id INTEGER NOT NULL DEFAULT 0,
  last_error TEXT NOT NULL DEFAULT '',
  held_id INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_cards_ts ON cards(ts DESC);
CREATE INDEX IF NOT EXISTS idx_cards_unit ON cards(unit, kind);
CREATE TABLE IF NOT EXISTS search_history(
  id INTEGER PRIMARY KEY,
  qkey TEXT NOT NULL UNIQUE,
  q TEXT NOT NULL,
  ts INTEGER NOT NULL,
  answer TEXT NOT NULL DEFAULT '',
  card_ids TEXT NOT NULL DEFAULT '[]',
  memory_ref TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_search_history_ts ON search_history(ts DESC);
CREATE TABLE IF NOT EXISTS local_sessions(
  id INTEGER PRIMARY KEY,
  boot TEXT NOT NULL DEFAULT '',
  started_ts INTEGER NOT NULL,
  ended_ts INTEGER,
  last_seen_ts INTEGER NOT NULL DEFAULT 0,
  models TEXT NOT NULL DEFAULT '',
  trigger TEXT NOT NULL DEFAULT '',
  pid INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_local_sessions_started ON local_sessions(started_ts DESC);
CREATE TABLE IF NOT EXISTS model_bench(
  model TEXT PRIMARY KEY,
  ts INTEGER NOT NULL,
  tokens INTEGER NOT NULL DEFAULT 0,
  load_ms INTEGER NOT NULL DEFAULT 0,
  seconds REAL NOT NULL DEFAULT 0,
  tps REAL NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS project_meta(
  key TEXT PRIMARY KEY,
  display_name TEXT NOT NULL DEFAULT '',
  path TEXT NOT NULL DEFAULT '',
  source TEXT NOT NULL DEFAULT 'manual',
  updated_at INTEGER NOT NULL DEFAULT 0
);
";

/// v4 → v5: колонка задачи в журнале расхода и таблицы локальных сессий,
/// бенчей и переопределений проектов. Только добавления, данные не трогаются.
const MIGRATION_V5: &str = "\
ALTER TABLE usage ADD COLUMN task TEXT NOT NULL DEFAULT '';
CREATE TABLE IF NOT EXISTS local_sessions(
  id INTEGER PRIMARY KEY, boot TEXT NOT NULL DEFAULT '',
  started_ts INTEGER NOT NULL, ended_ts INTEGER, last_seen_ts INTEGER NOT NULL DEFAULT 0,
  models TEXT NOT NULL DEFAULT '', trigger TEXT NOT NULL DEFAULT '', pid INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_local_sessions_started ON local_sessions(started_ts DESC);
CREATE TABLE IF NOT EXISTS model_bench(
  model TEXT PRIMARY KEY, ts INTEGER NOT NULL, tokens INTEGER NOT NULL DEFAULT 0,
  load_ms INTEGER NOT NULL DEFAULT 0, seconds REAL NOT NULL DEFAULT 0, tps REAL NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS project_meta(
  key TEXT PRIMARY KEY, display_name TEXT NOT NULL DEFAULT '',
  path TEXT NOT NULL DEFAULT '', source TEXT NOT NULL DEFAULT 'manual', updated_at INTEGER NOT NULL DEFAULT 0
);
";

/// Сырой фрагмент скрыт, когда его покрывает карточка записи (она проиндексирована
/// вместе с текстом фрагмента) или старая карточка хода стоит вместо него.
const NOT_FOLDED: &str = "NOT (c.kind='raw' AND (\
EXISTS(SELECT 1 FROM distill_queue q WHERE q.unit=c.unit AND q.covered_id>=c.id) \
OR EXISTS(SELECT 1 FROM cards d WHERE d.source_ref='llm:'||c.source_ref)))";

const CARD_COLUMNS: &str = "c.id, c.source, c.source_ref, c.ts, c.title, c.body, c.created_at, c.kind, c.keywords, c.unit";

#[derive(Debug)]
pub enum DbError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    /// Схема старше v4 требует миграции Python-реализацией, новее — неизвестна.
    UnsupportedSchema(i64),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sqlite(e) => write!(f, "sqlite: {e}"),
            DbError::Io(e) => write!(f, "io: {e}"),
            DbError::UnsupportedSchema(v) => write!(f, "unsupported schema version {v} (expected {SCHEMA_VERSION})"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Sqlite(e)
    }
}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub id: i64,
    pub source: String,
    pub source_ref: String,
    pub ts: i64,
    pub title: String,
    pub body: String,
    pub created_at: i64,
    pub score: Option<f64>,
    pub kind: String,
    pub keywords: String,
    pub unit: String,
}

#[derive(Debug, Clone)]
pub struct CardDraft {
    pub source: String,
    pub source_ref: String,
    pub ts: i64,
    pub title: String,
    pub body: String,
    pub kind: String,
    pub keywords: String,
    /// Пусто — выводится из source_ref при вставке.
    pub unit: String,
}

impl CardDraft {
    pub fn new(source: &str, source_ref: &str, ts: i64, title: &str, body: &str) -> Self {
        CardDraft {
            source: source.into(),
            source_ref: source_ref.into(),
            ts,
            title: title.into(),
            body: body.into(),
            kind: CARD_KIND_RAW.into(),
            keywords: String::new(),
            unit: String::new(),
        }
    }

    pub fn distilled(mut self) -> Self {
        self.kind = CARD_KIND_DISTILLED.into();
        self
    }
}

/// Состояние пересказа одной записи (см. `Database::unit_states`).
#[derive(Debug, Clone, PartialEq)]
pub struct UnitState {
    pub unit: String,
    pub source: String,
    /// id новейшего фрагмента.
    pub max_id: i64,
    pub max_ts: i64,
    pub fragments: i64,
    /// Один ref фрагмента: для группировки по проекту и приватных шаблонов.
    pub sample_ref: String,
    /// До какого фрагмента покрывает карточка записи (0 — карточки нет).
    pub covered_id: i64,
    /// Неудачные попытки с последнего успеха.
    pub attempts: i64,
    /// max_id, против которого были неудачные попытки.
    pub attempt_id: i64,
    pub last_error: String,
    /// max_id, при котором запись признана только локальной (0 — нет).
    pub held_id: i64,
}

impl UnitState {
    pub fn done(&self) -> bool {
        self.covered_id >= self.max_id
    }
}

/// Одна запись глазами панели: все её сырые фрагменты и карточка знаний.
#[derive(Debug, Clone, PartialEq)]
pub struct UnitInfo {
    pub unit: String,
    pub source: String,
    pub first_ts: i64,
    pub last_ts: i64,
    pub fragments: i64,
    pub max_id: i64,
    pub covered_id: i64,
    pub project: String,
    pub project_path: String,
    pub file: String,
    pub card_id: i64,
    pub title: String,
    pub keywords: String,
}

impl UnitInfo {
    /// Карточка на графе: карточка знаний, иначе самый новый фрагмент.
    pub fn node_id(&self) -> i64 {
        if self.card_id != 0 { self.card_id } else { self.max_id }
    }
}

/// Один прогон модели в журнале расхода.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageRun {
    pub ts: i64,
    pub provider: String,
    pub model: String,
    pub cards: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_rub: f64,
    /// Какой тип данных генерировали: distill | answer | handoff | projects.
    pub task: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageTotals {
    pub runs: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_rub: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageReport {
    pub runs: Vec<UsageRun>,
    pub totals: UsageTotals,
}

/// Сессия локального движка: от запуска `ollama serve` до остановки.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalSession {
    pub id: i64,
    pub boot: String,
    pub started_ts: i64,
    pub ended_ts: Option<i64>,
    pub last_seen_ts: i64,
    pub models: String,
    pub trigger: String,
    pub pid: i64,
}

impl LocalSession {
    /// Длительность; незакрытая сессия считается до последней активности.
    pub fn seconds(&self) -> i64 {
        let end = self.ended_ts.unwrap_or(self.last_seen_ts.max(self.started_ts));
        (end - self.started_ts).max(0)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskUsage {
    pub task: String,
    pub runs: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_rub: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub runs: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_rub: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BenchResult {
    pub model: String,
    pub ts: i64,
    pub tokens: i64,
    pub load_ms: i64,
    pub seconds: f64,
    pub tps: f64,
}

/// Сводка по моделям и локальным сессиям для панели «Модели и время».
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelStats {
    pub sessions: Vec<LocalSession>,
    pub seconds_today: i64,
    pub seconds_week: i64,
    pub seconds_total: i64,
    pub tasks: Vec<TaskUsage>,
    pub models: Vec<ModelUsage>,
    pub bench: Vec<BenchResult>,
    pub runs: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_rub: f64,
}

/// Переопределение имени и папки проекта; ключ — исходное имя из индекса.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectOverride {
    pub display_name: String,
    pub path: String,
    /// manual — решение человека, автообновление его не трогает; auto — от модели.
    pub source: String,
}

/// Фильтры выдачи; фильтр по проекту появится вместе с индексом записей (этап 6).
#[derive(Debug, Default, Clone)]
pub struct Filters<'a> {
    pub ref_prefix: Option<&'a str>,
    pub source: Option<&'a str>,
}

pub fn now_ts() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn default_db_path() -> PathBuf {
    if let Some(path) = crate::config::db_path_from_env() {
        return path;
    }
    crate::config::project_root().join("data").join("memory.db")
}

const CHAT_SOURCES: [&str; 3] = ["kimi", "claude", "codex"];
const COMMIT_SOURCES: [&str; 2] = ["git", "github"];
const WORKSPACE_ROOT: &str = "Git";

/// Проект записи: git/obsidian по unit, разговор — по рабочей папке.
fn project_of(source: &str, source_ref: &str) -> String {
    if COMMIT_SOURCES.contains(&source) {
        if let Some(repo) = source_ref.split(':').nth(1).filter(|part| !part.is_empty()) {
            return repo.to_owned();
        }
    }
    if source == "obsidian" {
        if let Some(rest) = source_ref.splitn(3, ':').nth(2) {
            let segment = rest.split('/').next().unwrap_or("").split('#').next().unwrap_or("").trim();
            if !segment.is_empty() {
                return segment.to_owned();
            }
        }
    }
    source.to_owned()
}

fn normalize_scope(scope: &str) -> String {
    let mut path = scope.replace('\\', "/");
    while path.ends_with('/') {
        path.pop();
    }
    if let Some(rest) = path.strip_prefix("//?/") {
        path = rest.to_owned();
    }
    static WORKTREE: OnceLock<Regex> = OnceLock::new();
    WORKTREE
        .get_or_init(|| Regex::new(r"(?i)/\.(?:claude|codex)/worktrees/[^/]+").expect("static worktree regex"))
        .replace_all(&path, "")
        .into_owned()
}

fn prefix_end(prefix: &str) -> String {
    let mut value = prefix.to_owned();
    if let Some(last) = value.pop() {
        value.push(char::from_u32(last as u32 + 1).unwrap_or(last));
    }
    value
}

fn repo_in_path(path: &str, repos: &[String]) -> Option<String> {
    let path = path.to_lowercase();
    for repo in repos {
        let repo_lower = repo.to_lowercase();
        if path.ends_with(&format!("/{repo_lower}")) || path.contains(&format!("/{repo_lower}/")) {
            return Some(repo.clone());
        }
    }
    None
}

fn repo_mentioned(text: &str, repos: &[String]) -> Option<String> {
    let repos: Vec<&String> = repos.iter().filter(|repo| repo.as_str() != WORKSPACE_ROOT).collect();
    if repos.is_empty() {
        return None;
    }
    let flat = text.replace("\\\\", "/").replace('\\', "/");
    let alternatives = repos.iter().map(|repo| regex::escape(repo)).collect::<Vec<_>>().join("|");
    let pattern = format!(r#"(?i)/({})(?:[/\s"'`)\],.:;]|$)"#, alternatives);
    let Ok(regex) = Regex::new(&pattern) else { return None };
    let canonical: HashMap<String, &String> = repos.iter().map(|repo| (repo.to_lowercase(), *repo)).collect();
    let mut found: HashMap<&String, usize> = HashMap::new();
    for capture in regex.captures_iter(&flat) {
        let Some(name) = capture.get(1) else { continue };
        if let Some(repo) = canonical.get(&name.as_str().to_lowercase()) {
            *found.entry(repo).or_default() += 1;
        }
    }
    let mut ranked: Vec<(&String, usize)> =
        found.into_iter().filter(|(_, count)| *count >= 2).map(|(repo, count)| (repo, count)).collect();
    ranked.sort_by(|(left, left_count), (right, right_count)| {
        (left.as_str() == ".shared")
            .cmp(&(right.as_str() == ".shared"))
            .then(right_count.cmp(left_count))
            .then(left.cmp(right))
    });
    ranked.first().map(|(repo, _)| (*repo).clone())
}

fn repo_named(title: &str, repos: &[String]) -> Option<String> {
    let aliases: &[&str] = &["Brepsi Brain", "BrepsiBrain", "Second Brain", "SecondBrain", "второй мозг"];
    let mut matches = Vec::new();
    for repo in repos {
        if repo == WORKSPACE_ROOT || repo == ".shared" {
            continue;
        }
        let mut names = vec![repo.rsplit('/').next().unwrap_or(repo)];
        if repo.rsplit('/').next() == Some("memory-base") {
            names.extend_from_slice(aliases);
        }
        for name in names {
            if word_match(title, name) {
                matches.push(repo.clone());
                break;
            }
        }
    }
    matches.sort();
    matches.dedup();
    if matches.len() == 1 { matches.pop() } else { None }
}

fn word_match(haystack: &str, needle: &str) -> bool {
    if needle.chars().count() < 3 {
        return false;
    }
    let haystack = haystack.to_lowercase();
    let needle = needle.to_lowercase();
    let mut from = 0;
    while let Some(position) = haystack[from..].find(&needle) {
        let start = from + position;
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        let boundary = |ch: Option<char>| ch.is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
        if boundary(before) && boundary(after) {
            return true;
        }
        from = end;
    }
    false
}

fn unit_project(
    source: &str,
    source_ref: &str,
    scope: &str,
    repos: &[String],
    text: &str,
    title: &str,
) -> (String, String) {
    if !CHAT_SOURCES.contains(&source) {
        let project = project_of(source, source_ref);
        return (project.clone(), project);
    }
    if scope.is_empty() {
        return match repo_named(title, repos).or_else(|| repo_mentioned(text, repos)) {
            Some(repo) => (repo.clone(), repo),
            None => (source.to_owned(), source.to_owned()),
        };
    }
    let path = normalize_scope(scope);
    let name = path.rsplit('/').next().unwrap_or("").to_owned();
    if name.eq_ignore_ascii_case(WORKSPACE_ROOT) {
        if let Some(repo) = repo_named(title, repos).or_else(|| repo_mentioned(text, repos)) {
            return (repo.clone(), format!("{path}/{repo}"));
        }
        return (WORKSPACE_ROOT.to_owned(), path);
    }
    let project = repo_in_path(&path, repos).unwrap_or_else(|| name.clone());
    (project, path)
}

fn like_escape(prefix: &str) -> String {
    prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn to_card(row: &Row<'_>) -> rusqlite::Result<Card> {
    Ok(Card {
        id: row.get(0)?,
        source: row.get(1)?,
        source_ref: row.get(2)?,
        ts: row.get(3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        created_at: row.get(6)?,
        score: None,
        kind: row.get(7)?,
        keywords: row.get(8)?,
        unit: row.get(9)?,
    })
}

fn matches_source(card: &Card, source: Option<&str>) -> bool {
    let Some(source) = source.filter(|s| !s.is_empty()) else {
        return true;
    };
    // 'git' находит и карточки 'llm:git': в них свёрнуты покрытые фрагменты;
    // явный фильтр 'llm:…' по-прежнему выбирает только карточки.
    let value = if source.starts_with(DISTILLED_PREFIX) {
        card.source.as_str()
    } else {
        card.source.strip_prefix(DISTILLED_PREFIX).unwrap_or(&card.source)
    };
    // Точное имя: префикс 'git' ловил и 'github'. Префикс с ':' ('llm:') — вся группа.
    value == source || (source.ends_with(':') && value.starts_with(source))
}

pub struct Database {
    path: PathBuf,
    con: Connection,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let con = Connection::open(&path)?;
        con.busy_timeout(std::time::Duration::from_millis(5000))?;
        // LIKE должен различать регистр: на нём держится фильтр по префиксу source_ref.
        con.pragma_update(None, "case_sensitive_like", "ON")?;
        // Переход в WAL требует короткой эксклюзивной блокировки; пока другой процесс
        // пишет, файл остаётся в rollback-режиме, и это не ошибка.
        let _ = con.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()));
        let db = Database { path, con };
        db.init_schema()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn connection(&self) -> &Connection {
        &self.con
    }

    fn init_schema(&self) -> Result<()> {
        let has_cards: bool = self.con.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='cards')",
            [],
            |r| r.get(0),
        )?;
        if has_cards {
            let has_state: bool = self.con.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='state')",
                [],
                |r| r.get(0),
            )?;
            let version = if has_state { self.get_state("schema_version")? } else { None };
            let version = version.and_then(|v| v.parse::<i64>().ok()).unwrap_or(1);
            if version < SCHEMA_VERSION {
                self.migrate(version)?;
            } else if version > SCHEMA_VERSION {
                return Err(DbError::UnsupportedSchema(version));
            }
            self.con.execute_batch(SCHEMA)?;
            return Ok(());
        }
        let tx = self.con.unchecked_transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.execute(
            "INSERT INTO state(key,value) VALUES('schema_version',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Обновление старой базы добавлением таблиц и колонок. Шаги идут в
    /// транзакции: половина миграции хуже её отсутствия.
    fn migrate(&self, from: i64) -> Result<()> {
        if from != 4 {
            return Err(DbError::UnsupportedSchema(from));
        }
        self.atomic(|| {
            // Окно и sync по расписанию открывают старую базу одновременно: версию
            // перечитываем под блокировкой записи, иначе второй ловил duplicate column.
            let current = self.get_state("schema_version")?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(from);
            if current >= SCHEMA_VERSION {
                return Ok(());
            }
            self.con.execute_batch(MIGRATION_V5)?;
            self.con.execute(
                "INSERT INTO state(key,value) VALUES('schema_version',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [SCHEMA_VERSION.to_string()],
            )?;
            Ok(())
        })
    }

    // -- состояние --------------------------------------------------------------

    pub fn get_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .con
            .query_row("SELECT value FROM state WHERE key=?", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        let changed = self.con.execute(
            "INSERT INTO state(key,value) VALUES(?,?) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value<>excluded.value",
            params![key, value],
        )?;
        if changed > 0 && (key.starts_with(SCOPE_KEY_PREFIX) || key.starts_with(FILE_KEY_PREFIX)) {
            self.bump_index()?;
        }
        Ok(())
    }

    /// Ревизия индекса меняется в той же транзакции: её видят другие процессы,
    /// и она откатывается вместе с данными.
    pub fn bump_index(&self) -> Result<()> {
        self.con.execute(
            "INSERT INTO state(key,value) VALUES(?, '1') ON CONFLICT(key) \
             DO UPDATE SET value=CAST(value AS INTEGER)+1",
            [INDEX_REVISION_KEY],
        )?;
        Ok(())
    }

    /// Взять или продлить межпроцессную аренду в `state`. Один UPSERT атомарен под
    /// блокировкой записи SQLite, поэтому расписание и кнопка в панели не выиграют
    /// обе; чужая аренда переходит только после истечения. В owner не должно быть '|'.
    pub fn claim_lease(&self, name: &str, owner: &str, ttl_seconds: i64, now: Option<i64>) -> Result<bool> {
        let now = now.unwrap_or_else(now_ts);
        let changed = self.con.execute(
            "INSERT INTO state(key,value) VALUES(?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value \
             WHERE substr(state.value,1,instr(state.value,'|')-1)=?3 \
             OR CAST(substr(state.value,instr(state.value,'|')+1) AS INTEGER)<=?4",
            params![name, format!("{owner}|{}", now + ttl_seconds), owner, now],
        )?;
        Ok(changed == 1)
    }

    pub fn release_lease(&self, name: &str, owner: &str) -> Result<()> {
        self.con.execute(
            "DELETE FROM state WHERE key=? AND substr(value,1,instr(value,'|')-1)=?",
            params![name, owner],
        )?;
        Ok(())
    }

    // -- полнотекстовый индекс ----------------------------------------------------

    fn index(&self, card_id: i64) -> Result<()> {
        let row = self
            .con
            .query_row(
                "SELECT kind, source_ref, unit, title, keywords, body FROM cards WHERE id=?",
                [card_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((kind, source_ref, unit, title, keywords, body)) = row else {
            return Ok(());
        };
        let (summary, questions, atoms, text) = if kind != CARD_KIND_DISTILLED {
            (String::new(), String::new(), String::new(), body)
        } else {
            let parts = text::split_card_sections(&body);
            let source = self.source_text(&source_ref, &unit)?;
            let text = [parts.rest, source].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n");
            (parts.summary, parts.questions, parts.atoms, text)
        };
        self.con.execute(
            "INSERT INTO cards_fts(rowid,title,keywords,summary,questions,atoms,body) VALUES(?,?,?,?,?,?,?)",
            params![card_id, title, keywords, summary, questions, atoms, text],
        )?;
        Ok(())
    }

    /// Исходный текст карточки знаний: вся запись для карточки записи, один
    /// исходный фрагмент для старой карточки хода.
    fn source_text(&self, card_ref: &str, unit: &str) -> Result<String> {
        let bodies: Vec<String> = if card_ref == format!("{DISTILLED_PREFIX}{unit}") {
            let mut stmt = self.con.prepare("SELECT body FROM cards WHERE unit=? AND kind='raw' ORDER BY id")?;
            let rows = stmt.query_map([unit], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        } else {
            let origin = card_ref.strip_prefix(DISTILLED_PREFIX).unwrap_or(card_ref);
            let mut stmt = self.con.prepare("SELECT body FROM cards WHERE source_ref=? AND kind='raw'")?;
            let rows = stmt.query_map([origin], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        Ok(bodies.join("\n\n"))
    }

    // -- карточки ---------------------------------------------------------------

    /// Несколько statement'ов одной операции (строка, индекс, ревизия) — атомарно.
    /// Внутри открытой транзакции вкладывается через SAVEPOINT.
    pub fn atomic<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.con.is_autocommit() {
            // Сразу IMMEDIATE: отложенная транзакция, начавшись с чтения, при
            // повышении до записи получает SQLITE_BUSY мимо busy_timeout.
            self.con.execute_batch("BEGIN IMMEDIATE")?;
            let result = f().and_then(|value| self.con.execute_batch("COMMIT").map(|()| value).map_err(Into::into));
            // Неудачный COMMIT оставил бы транзакцию открытой, и все следующие
            // записи молча вкладывались бы в неё.
            if result.is_err() && !self.con.is_autocommit() {
                let _ = self.con.execute_batch("ROLLBACK");
            }
            return result;
        }
        self.con.execute_batch("SAVEPOINT mb_write")?;
        match f() {
            Ok(value) => {
                self.con.execute_batch("RELEASE mb_write")?;
                Ok(value)
            }
            Err(e) => {
                let _ = self.con.execute_batch("ROLLBACK TO mb_write; RELEASE mb_write");
                Err(e)
            }
        }
    }

    /// Вставка; повторный source_ref ничего не делает. true — карточка добавлена.
    pub fn insert_card(&self, card: &CardDraft) -> Result<bool> {
        self.atomic(|| self.insert_card_inner(card))
    }

    fn insert_card_inner(&self, card: &CardDraft) -> Result<bool> {
        let unit = if card.unit.is_empty() {
            text::unit_for(&card.source, &card.source_ref, card.ts, &card.kind)
        } else {
            card.unit.clone()
        };
        let inserted = self.con.execute(
            "INSERT INTO cards(source,source_ref,ts,title,body,created_at,kind,keywords,unit) \
             VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(source_ref) DO NOTHING",
            params![card.source, card.source_ref, card.ts, card.title, card.body, now_ts(), card.kind, card.keywords, unit],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        self.index(self.con.last_insert_rowid())?;
        self.bump_index()?;
        Ok(true)
    }

    /// Правка содержимого с переиндексацией. keywords=None оставляет прежние.
    /// Правка сырого фрагмента делает карточку записи устаревшей, если не
    /// invalidate=false (правки, которые только убирают шум). false — нет такого id.
    pub fn update_card(&self, card_id: i64, ts: i64, title: &str, body: &str, keywords: Option<&str>, invalidate: bool) -> Result<bool> {
        self.atomic(|| self.update_card_inner(card_id, ts, title, body, keywords, invalidate))
    }

    fn update_card_inner(&self, card_id: i64, ts: i64, title: &str, body: &str, keywords: Option<&str>, invalidate: bool) -> Result<bool> {
        let row: Option<(String, String, String)> = self
            .con
            .query_row("SELECT kind, unit, keywords FROM cards WHERE id=?", [card_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        let Some((kind, unit, stored)) = row else {
            return Ok(false);
        };
        let keywords = keywords.unwrap_or(&stored);
        self.unindex(card_id)?;
        self.con.execute(
            "UPDATE cards SET ts=?, title=?, body=?, keywords=? WHERE id=?",
            params![ts, title, body, keywords, card_id],
        )?;
        self.index(card_id)?;
        if invalidate && kind == CARD_KIND_RAW {
            self.con.execute("UPDATE distill_queue SET covered_id=0 WHERE unit=?", [&unit])?;
        }
        self.bump_index()?;
        Ok(true)
    }

    /// Удаление со строкой индекса; удаление карточки записи снова ставит запись
    /// в очередь. false — нет такого id.
    pub fn delete_card(&self, card_id: i64) -> Result<bool> {
        self.atomic(|| self.delete_card_inner(card_id))
    }

    fn delete_card_inner(&self, card_id: i64) -> Result<bool> {
        let row: Option<(String, String)> = self
            .con
            .query_row("SELECT unit, source_ref FROM cards WHERE id=?", [card_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((unit, source_ref)) = row else {
            return Ok(false);
        };
        self.unindex(card_id)?;
        self.con.execute("DELETE FROM cards WHERE id=?", [card_id])?;
        if source_ref == format!("{DISTILLED_PREFIX}{unit}") {
            self.con.execute("DELETE FROM distill_queue WHERE unit=?", [&unit])?;
        }
        self.bump_index()?;
        Ok(true)
    }

    fn unindex(&self, card_id: i64) -> Result<()> {
        self.con.execute("DELETE FROM cards_fts WHERE rowid=?", [card_id])?;
        Ok(())
    }

    /// Удалить карточки знаний тех записей, у которых не осталось ни одного сырого
    /// фрагмента. Возвращает число удалённых.
    pub fn drop_orphan_unit_cards(&self, units: &[String]) -> Result<usize> {
        let mut removed = 0;
        let mut seen = std::collections::HashSet::new();
        for unit in units.iter().filter(|u| seen.insert(u.as_str())) {
            let has_raw: bool = self
                .con
                .query_row("SELECT EXISTS(SELECT 1 FROM cards WHERE unit=? AND kind='raw')", [unit], |r| r.get(0))?;
            if has_raw {
                continue;
            }
            let orphan: Option<i64> = self
                .con
                .query_row("SELECT id FROM cards WHERE source_ref=?", [format!("{DISTILLED_PREFIX}{unit}")], |r| r.get(0))
                .optional()?;
            if let Some(id) = orphan {
                self.delete_card(id)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    // -- записи и очередь пересказа ---------------------------------------------

    /// Каждая запись, у которой есть сырые фрагменты, с её состоянием в очереди.
    pub fn unit_states(&self) -> Result<Vec<UnitState>> {
        let mut stmt = self.con.prepare(
            "SELECT c.unit, MIN(c.source), MAX(c.id), MAX(c.ts), COUNT(*), MIN(c.source_ref), \
             COALESCE(MAX(q.covered_id),0), COALESCE(MAX(q.attempts),0), COALESCE(MAX(q.attempt_id),0), \
             COALESCE(MAX(q.last_error),''), COALESCE(MAX(q.held_id),0) \
             FROM cards c LEFT JOIN distill_queue q ON q.unit=c.unit \
             WHERE c.kind='raw' GROUP BY c.unit",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(UnitState {
                unit: r.get(0)?,
                source: r.get(1)?,
                max_id: r.get(2)?,
                max_ts: r.get(3)?,
                fragments: r.get(4)?,
                sample_ref: r.get(5)?,
                covered_id: r.get(6)?,
                attempts: r.get(7)?,
                attempt_id: r.get(8)?,
                last_error: r.get(9)?,
                held_id: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Все записи с проектом и карточкой знаний; ключ — unit.
    pub fn unit_index(&self) -> Result<HashMap<String, UnitInfo>> {
        let mut stmt = self.con.prepare(
            "SELECT c.unit, MIN(c.source), MIN(c.source_ref), MIN(c.ts), MAX(c.ts), COUNT(*), MAX(c.id), \
             COALESCE(MAX(q.covered_id),0) \
             FROM cards c LEFT JOIN distill_queue q ON q.unit=c.unit \
             WHERE c.kind='raw' GROUP BY c.unit",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;

        let mut cards: HashMap<String, (i64, String, String)> = HashMap::new();
        let mut card_stmt = self.con.prepare(
            "SELECT unit, id, title, keywords FROM cards \
             WHERE kind=? AND source_ref='llm:'||unit",
        )?;
        for row in card_stmt.query_map([CARD_KIND_DISTILLED], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })? {
            let (unit, id, title, keywords) = row?;
            cards.insert(unit, (id, title, keywords));
        }

        let mut heads: HashMap<i64, String> = HashMap::new();
        let mut head_stmt = self.con.prepare(
            "SELECT id, title FROM cards WHERE id IN \
             (SELECT MAX(id) FROM cards WHERE kind='raw' GROUP BY unit)",
        )?;
        for row in head_stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, title) = row?;
            heads.insert(id, title);
        }

        let scopes = self.state_by_prefix("scope:")?;
        let files = self.state_by_prefix("file:")?;
        let overrides = self.project_overrides()?;
        let mut repos: HashSet<String> = HashSet::new();
        for (unit, source, ..) in &rows {
            if COMMIT_SOURCES.contains(&source.as_str()) && unit.matches(':').count() >= 2 {
                if let Some((_, rest)) = unit.split_once(':') {
                    if let Some((repo, _)) = rest.rsplit_once(':') {
                        repos.insert(repo.to_owned());
                    }
                }
            }
        }
        let mut repos: Vec<String> = repos.into_iter().collect();
        repos.sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));

        let mut texts_parts: HashMap<String, Vec<String>> = HashMap::new();
        let mut text_stmt = self.con.prepare(
            "SELECT unit, body FROM cards WHERE kind='raw' AND source IN ('kimi','claude','codex')",
        )?;
        for row in text_stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (unit, body) = row?;
            let include = match scopes.get(&unit) {
                None => true,
                Some(scope) => normalize_scope(scope)
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case(WORKSPACE_ROOT)),
            };
            if include {
                texts_parts.entry(unit).or_default().push(body);
            }
        }
        let texts: HashMap<String, String> =
            texts_parts.into_iter().map(|(unit, parts)| (unit, parts.join("\n"))).collect();

        let mut index = HashMap::new();
        for (unit, source, sample_ref, first_ts, last_ts, fragments, max_id, covered_id) in rows {
            let card = cards.get(&unit);
            let title = card
                .map(|c| c.1.clone())
                .or_else(|| heads.get(&max_id).cloned())
                .unwrap_or_default();
            let (mut project, mut project_path) = unit_project(
                &source,
                &sample_ref,
                scopes.get(&unit).map(String::as_str).unwrap_or(""),
                &repos,
                texts.get(&unit).map(String::as_str).unwrap_or(""),
                &title,
            );
            // Переименование/перенос проекта: ключ остаётся прежним (он выводится
            // из ref и scope), а в панель уходит актуальное имя и папка.
            if let Some(project_override) = overrides.get(&project) {
                if !project_override.display_name.is_empty() {
                    project = project_override.display_name.clone();
                }
                if !project_override.path.is_empty() {
                    project_path = project_override.path.clone();
                }
            }
            let file = files.get(&unit).cloned().unwrap_or_default();
            index.insert(
                unit.clone(),
                UnitInfo {
                    unit,
                    source,
                    first_ts,
                    last_ts,
                    fragments,
                    max_id,
                    covered_id,
                    project,
                    project_path,
                    file,
                    card_id: card.map(|c| c.0).unwrap_or(0),
                    title,
                    keywords: card.map(|c| c.2.clone()).unwrap_or_default(),
                },
            );
        }
        Ok(index)
    }

    fn state_by_prefix(&self, prefix: &str) -> Result<HashMap<String, String>> {
        let end = prefix_end(prefix);
        let mut stmt = self.con.prepare("SELECT key, value FROM state WHERE key >= ? AND key < ?")?;
        let rows = stmt.query_map(params![prefix, end], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut values = HashMap::new();
        for row in rows {
            let (key, value) = row?;
            values.insert(key[prefix.len()..].to_owned(), value);
        }
        Ok(values)
    }

    pub fn unit_fragments(&self, unit: &str) -> Result<Vec<Card>> {
        let sql = format!("SELECT {CARD_COLUMNS} FROM cards c WHERE c.unit=? AND c.kind='raw' ORDER BY c.id");
        let mut stmt = self.con.prepare(&sql)?;
        let rows = stmt.query_map([unit], to_card)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Вставить или переписать карточку записи (source_ref 'llm:<unit>'), убрать
    /// старые карточки ходов этой записи и отметить, что карточка покрывает
    /// фрагменты до covered_id. true — карточка создана заново.
    pub fn write_unit_card(&self, unit: &str, draft: &CardDraft, covered_id: i64) -> Result<bool> {
        self.atomic(|| {
            let card_ref = format!("{DISTILLED_PREFIX}{unit}");
            let existing: Option<i64> = self
                .con
                .query_row("SELECT id FROM cards WHERE source_ref=?", [&card_ref], |r| r.get(0))
                .optional()?;
            let created = match existing {
                Some(id) => {
                    self.update_card_inner(id, draft.ts, &draft.title, &draft.body, Some(&draft.keywords), true)?;
                    false
                }
                None => {
                    let mut card = draft.clone();
                    card.source_ref = card_ref.clone();
                    card.unit = unit.to_string();
                    card.kind = CARD_KIND_DISTILLED.into();
                    self.insert_card_inner(&card)?
                }
            };
            let legacy: Vec<i64> = {
                let mut stmt = self.con.prepare("SELECT id FROM cards WHERE unit=? AND kind=? AND source_ref<>?")?;
                let rows = stmt.query_map(params![unit, CARD_KIND_DISTILLED, card_ref], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for id in legacy {
                self.delete_card_inner(id)?;
            }
            self.con.execute(
                "INSERT INTO distill_queue(unit,covered_id,attempts,attempt_id,last_error,held_id,updated_at) \
                 VALUES(?,?,0,0,'',0,?) ON CONFLICT(unit) DO UPDATE SET \
                 covered_id=excluded.covered_id, attempts=0, attempt_id=0, last_error='', \
                 held_id=0, updated_at=excluded.updated_at",
                params![unit, covered_id, now_ts()],
            )?;
            Ok(created)
        })
    }

    /// Снова в очередь с чистым счётчиком попыток; карточка читается до переписи.
    /// Приватная отметка тоже снимается: следующий прогон судит по текущим шаблонам.
    pub fn requeue_unit(&self, unit: &str) -> Result<()> {
        self.atomic(|| {
            self.con.execute(
                "UPDATE distill_queue SET covered_id=0, attempts=0, attempt_id=0, last_error='', \
                 held_id=0, updated_at=? WHERE unit=?",
                params![now_ts(), unit],
            )?;
            self.bump_index()
        })
    }

    /// Неудачная попытка против текущих фрагментов; новые фрагменты (больший
    /// max_id) начинают счёт заново. Ошибка обрезается до 300 символов, как в Python.
    pub fn mark_unit_failed(&self, unit: &str, max_id: i64, error: &str) -> Result<()> {
        self.con.execute(
            "INSERT INTO distill_queue(unit,attempts,attempt_id,last_error,updated_at) \
             VALUES(?,1,?,?,?) ON CONFLICT(unit) DO UPDATE SET \
             attempts=CASE WHEN distill_queue.attempt_id=excluded.attempt_id \
             THEN distill_queue.attempts+1 ELSE 1 END, \
             attempt_id=excluded.attempt_id, last_error=excluded.last_error, \
             updated_at=excluded.updated_at",
            params![unit, max_id, pystr::take_chars(error, 300), now_ts()],
        )?;
        Ok(())
    }

    /// Фрагменты до max_id — только для локальной модели (приватные шаблоны).
    pub fn mark_unit_held(&self, unit: &str, max_id: i64) -> Result<()> {
        self.con.execute(
            "INSERT INTO distill_queue(unit,held_id,updated_at) VALUES(?,?,?) \
             ON CONFLICT(unit) DO UPDATE SET held_id=excluded.held_id, updated_at=excluded.updated_at",
            params![unit, max_id, now_ts()],
        )?;
        Ok(())
    }

    // -- журнал расхода модели ---------------------------------------------------

    pub fn record_usage(&self, run: &UsageRun) -> Result<()> {
        self.con.execute(
            "INSERT INTO usage(ts,provider,model,cards,prompt_tokens,completion_tokens,cost_rub,task) VALUES(?,?,?,?,?,?,?,?)",
            params![run.ts, run.provider, run.model, run.cards, run.prompt_tokens, run.completion_tokens, run.cost_rub, run.task],
        )?;
        Ok(())
    }

    /// Последние `limit` прогонов (новые первыми) и итоги за всё время.
    pub fn recent_usage(&self, limit: i64) -> Result<UsageReport> {
        let mut stmt = self.con.prepare(
            "SELECT ts,provider,model,cards,prompt_tokens,completion_tokens,cost_rub,task FROM usage ORDER BY id DESC LIMIT ?",
        )?;
        let runs = stmt
            .query_map([limit], |r| {
                Ok(UsageRun {
                    ts: r.get(0)?,
                    provider: r.get(1)?,
                    model: r.get(2)?,
                    cards: r.get(3)?,
                    prompt_tokens: r.get(4)?,
                    completion_tokens: r.get(5)?,
                    cost_rub: r.get(6)?,
                    task: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let totals = self.con.query_row(
            "SELECT COUNT(*), COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0), \
             COALESCE(SUM(cost_rub),0) FROM usage",
            [],
            |r| {
                Ok(UsageTotals {
                    runs: r.get(0)?,
                    prompt_tokens: r.get(1)?,
                    completion_tokens: r.get(2)?,
                    cost_rub: r.get(3)?,
                })
            },
        )?;
        Ok(UsageReport { runs, totals })
    }

    pub fn count_cards(&self) -> Result<i64> {
        Ok(self.con.query_row("SELECT COUNT(*) FROM cards", [], |r| r.get(0))?)
    }

    // -- локальные сессии, бенчи и проекты ---------------------------------------

    /// Сессия движка открыта: `ended_ts` остаётся NULL до остановки. Зависшие
    /// сессии прошлых запусков закрывает `sweep_local_sessions`.
    pub fn start_local_session(&self, boot: &str, model: &str, trigger: &str, pid: u32, now: i64) -> Result<i64> {
        self.con.execute(
            "INSERT INTO local_sessions(boot,started_ts,last_seen_ts,models,trigger,pid) VALUES(?,?,?,?,?,?)",
            params![boot, now, now, model, trigger, i64::from(pid)],
        )?;
        Ok(self.con.last_insert_rowid())
    }

    /// Отметка активности и модель этой сессии (без дублей в строке).
    pub fn touch_local_session(&self, id: i64, model: &str, now: i64) -> Result<()> {
        let current: String = self
            .con
            .query_row("SELECT models FROM local_sessions WHERE id=?", [id], |r| r.get(0))
            .optional()?
            .unwrap_or_default();
        let known = current.split(',').any(|item| item == model);
        let models = if model.is_empty() || known {
            current
        } else if current.is_empty() {
            model.to_owned()
        } else {
            format!("{current},{model}")
        };
        self.con.execute(
            "UPDATE local_sessions SET last_seen_ts=?, models=? WHERE id=?",
            params![now, models, id],
        )?;
        Ok(())
    }

    pub fn end_local_session(&self, id: i64, now: i64) -> Result<()> {
        self.con.execute(
            "UPDATE local_sessions SET ended_ts=?, last_seen_ts=? WHERE id=? AND ended_ts IS NULL",
            params![now, now, id],
        )?;
        Ok(())
    }

    /// Закрыть сессии прошлых запусков приложения: иначе они висели бы вечно
    /// и «часы работы» росли бы вместе с ними.
    pub fn sweep_local_sessions(&self, boot: &str) -> Result<usize> {
        Ok(self.con.execute(
            "UPDATE local_sessions SET ended_ts=CASE WHEN last_seen_ts>started_ts THEN last_seen_ts ELSE started_ts END \
             WHERE ended_ts IS NULL AND boot<>? AND last_seen_ts < ?",
            params![boot, now_ts() - 3600],
        )?)
    }

    pub fn record_bench(&self, bench: &BenchResult) -> Result<()> {
        self.con.execute(
            "INSERT INTO model_bench(model,ts,tokens,load_ms,seconds,tps) VALUES(?,?,?,?,?,?) \
             ON CONFLICT(model) DO UPDATE SET ts=excluded.ts, tokens=excluded.tokens, \
             load_ms=excluded.load_ms, seconds=excluded.seconds, tps=excluded.tps",
            params![bench.model, bench.ts, bench.tokens, bench.load_ms, bench.seconds, bench.tps],
        )?;
        Ok(())
    }

    pub fn project_overrides(&self) -> Result<HashMap<String, ProjectOverride>> {
        let mut stmt = self.con.prepare("SELECT key,display_name,path,source FROM project_meta")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, ProjectOverride { display_name: r.get(1)?, path: r.get(2)?, source: r.get(3)? }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_project_meta(&self, key: &str, display_name: &str, path: &str) -> Result<()> {
        self.set_project_meta_from(key, display_name, path, "manual")
    }

    pub fn set_project_meta_from(&self, key: &str, display_name: &str, path: &str, source: &str) -> Result<()> {
        self.con.execute(
            "INSERT INTO project_meta(key,display_name,path,source,updated_at) VALUES(?,?,?,?,?) \
             ON CONFLICT(key) DO UPDATE SET display_name=excluded.display_name, path=excluded.path, \
             source=excluded.source, updated_at=excluded.updated_at",
            params![key, display_name, path, source, now_ts()],
        )?;
        self.bump_index()
    }

    pub fn delete_project_meta(&self, value: &str) -> Result<bool> {
        let removed = self
            .con
            .execute("DELETE FROM project_meta WHERE key=? OR display_name=?", [value, value])?;
        if removed > 0 {
            self.bump_index()?;
        }
        Ok(removed > 0)
    }

    /// Секунды сессий в окне [cutoff, now], с обрезкой по границам окна.
    fn session_seconds(&self, cutoff: i64, now: i64) -> Result<i64> {
        Ok(self.con.query_row(
            "SELECT COALESCE(SUM(MAX(0, MIN(COALESCE(ended_ts,last_seen_ts,started_ts), ?1) - MAX(started_ts, ?2))),0) \
             FROM local_sessions WHERE started_ts <= ?1 AND COALESCE(ended_ts,last_seen_ts,started_ts) >= ?2",
            params![now, cutoff],
            |r| r.get(0),
        )?)
    }

    /// Сводка «часы, задачи, модели, бенчи». `since=0` — всё время.
    pub fn model_stats(&self, since: i64) -> Result<ModelStats> {
        let now = now_ts();
        // Полночь по местному времени, а не UTC: в UTC+3 «сегодня» иначе начиналось
        // в 03:00. Часовой пояс и переход на летнее время SQLite берёт у ОС.
        let today: i64 = self
            .con
            .query_row("SELECT CAST(strftime('%s', ?1, 'unixepoch', 'localtime', 'start of day', 'utc') AS INTEGER)", [now], |r| r.get(0))
            .unwrap_or(now - now.rem_euclid(86_400));
        let week = now - 7 * 86_400;
        let mut stats = ModelStats {
            seconds_today: self.session_seconds(today, now)?,
            seconds_week: self.session_seconds(week, now)?,
            seconds_total: self.session_seconds(0, now)?,
            ..ModelStats::default()
        };

        let mut stmt = self.con.prepare(
            "SELECT id,boot,started_ts,ended_ts,last_seen_ts,models,trigger,pid FROM local_sessions \
             WHERE started_ts>=? OR ended_ts IS NULL ORDER BY started_ts DESC LIMIT 50",
        )?;
        stats.sessions = stmt
            .query_map([since], |r| {
                Ok(LocalSession {
                    id: r.get(0)?,
                    boot: r.get(1)?,
                    started_ts: r.get(2)?,
                    ended_ts: r.get(3)?,
                    last_seen_ts: r.get(4)?,
                    models: r.get(5)?,
                    trigger: r.get(6)?,
                    pid: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut stmt = self.con.prepare(
            "SELECT task,COUNT(*),COALESCE(SUM(prompt_tokens),0),COALESCE(SUM(completion_tokens),0),COALESCE(SUM(cost_rub),0) \
             FROM usage WHERE ts>=? GROUP BY task ORDER BY COUNT(*) DESC, task",
        )?;
        stats.tasks = stmt
            .query_map([since], |r| {
                Ok(TaskUsage {
                    task: r.get(0)?,
                    runs: r.get(1)?,
                    prompt_tokens: r.get(2)?,
                    completion_tokens: r.get(3)?,
                    cost_rub: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut stmt = self.con.prepare(
            "SELECT model,COUNT(*),COALESCE(SUM(prompt_tokens),0),COALESCE(SUM(completion_tokens),0),COALESCE(SUM(cost_rub),0) \
             FROM usage WHERE ts>=? GROUP BY model ORDER BY COUNT(*) DESC, model",
        )?;
        stats.models = stmt
            .query_map([since], |r| {
                Ok(ModelUsage {
                    model: r.get(0)?,
                    runs: r.get(1)?,
                    prompt_tokens: r.get(2)?,
                    completion_tokens: r.get(3)?,
                    cost_rub: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut stmt = self.con.prepare(
            "SELECT model,ts,tokens,load_ms,seconds,tps FROM model_bench ORDER BY ts DESC",
        )?;
        stats.bench = stmt
            .query_map([], |r| {
                Ok(BenchResult {
                    model: r.get(0)?,
                    ts: r.get(1)?,
                    tokens: r.get(2)?,
                    load_ms: r.get(3)?,
                    seconds: r.get(4)?,
                    tps: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        let (runs, prompt_tokens, completion_tokens, cost_rub) = self.con.query_row(
            "SELECT COUNT(*),COALESCE(SUM(prompt_tokens),0),COALESCE(SUM(completion_tokens),0),COALESCE(SUM(cost_rub),0) \
             FROM usage WHERE ts>=?",
            [since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        stats.runs = runs;
        stats.prompt_tokens = prompt_tokens;
        stats.completion_tokens = completion_tokens;
        stats.cost_rub = cost_rub;
        Ok(stats)
    }

    pub fn get_card(&self, card_id: i64) -> Result<Option<Card>> {
        let sql = format!("SELECT {CARD_COLUMNS} FROM cards c WHERE c.id=?");
        Ok(self.con.query_row(&sql, [card_id], to_card).optional()?)
    }

    pub fn get_card_by_ref(&self, source_ref: &str) -> Result<Option<Card>> {
        let sql = format!("SELECT {CARD_COLUMNS} FROM cards c WHERE c.source_ref=?");
        Ok(self.con.query_row(&sql, [source_ref], to_card).optional()?)
    }

    // -- запросы ----------------------------------------------------------------

    /// BM25 × свежесть; полное совпадение названия выше; одна запись — один результат.
    pub fn search(&self, query: &str, limit: i64, now: Option<i64>, filters: &Filters<'_>) -> Result<Vec<Card>> {
        let Some(fts) = text::build_fts_query(query) else {
            return Ok(Vec::new());
        };
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let now = now.unwrap_or_else(now_ts);
        let mut sql = format!(
            "SELECT {CARD_COLUMNS}, bm25(cards_fts, {FTS_WEIGHTS}) AS bm \
             FROM cards_fts JOIN cards c ON c.id=cards_fts.rowid \
             WHERE cards_fts MATCH ?1 AND {NOT_FOLDED}"
        );
        let like = filters.ref_prefix.filter(|p| !p.is_empty()).map(|p| like_escape(p) + "%");
        if like.is_some() {
            sql.push_str(" AND c.source_ref LIKE ?2 ESCAPE '\\'");
        }
        let mut stmt = self.con.prepare(&sql)?;
        let map = |r: &Row<'_>| -> rusqlite::Result<(Card, f64)> { Ok((to_card(r)?, r.get(10)?)) };
        let rows: Vec<(Card, f64)> = match &like {
            Some(like) => stmt.query_map(params![fts, like], map)?.collect::<rusqlite::Result<_>>()?,
            None => stmt.query_map(params![fts], map)?.collect::<rusqlite::Result<_>>()?,
        };
        let query_words = text::search_terms(query);
        let mut scored: Vec<(bool, f64, Card)> = rows
            .into_iter()
            .filter(|(card, _)| matches_source(card, filters.source))
            .map(|(card, bm)| {
                let score = -bm * text::freshness_factor(card.ts, now);
                (text::search_terms(&card.title) == query_words, score, card)
            })
            .collect();
        // Стабильная сортировка по убыванию, как sorted(reverse=True) в Python:
        // равные ключи сохраняют порядок строк из SQLite.
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut seen = std::collections::HashSet::new();
        let mut cards = Vec::new();
        for (_, score, mut card) in scored {
            if !seen.insert(card.unit.clone()) {
                continue;
            }
            card.score = Some(score);
            cards.push(card);
            if cards.len() as i64 == limit {
                break;
            }
        }
        Ok(cards)
    }

    /// Новейшие карточки за `days` дней. Строки читаются потоком до `limit`
    /// подходящих: LIMIT до фильтров прятал старые записи отфильтрованных групп.
    pub fn recent(&self, days: i64, limit: i64, now: Option<i64>, filters: &Filters<'_>) -> Result<Vec<Card>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let now = now.unwrap_or_else(now_ts);
        let cutoff = now.saturating_sub(days.saturating_mul(86400));
        let mut sql = format!("SELECT {CARD_COLUMNS} FROM cards c WHERE ts>=?1 AND {NOT_FOLDED}");
        let like = filters.ref_prefix.filter(|p| !p.is_empty()).map(|p| like_escape(p) + "%");
        if like.is_some() {
            sql.push_str(" AND source_ref LIKE ?2 ESCAPE '\\'");
        }
        sql.push_str(" ORDER BY ts DESC, id DESC");
        let mut stmt = self.con.prepare(&sql)?;
        let mut rows = match &like {
            Some(like) => stmt.query(params![cutoff, like])?,
            None => stmt.query(params![cutoff])?,
        };
        let mut cards = Vec::new();
        while let Some(row) = rows.next()? {
            let card = to_card(row)?;
            if matches_source(&card, filters.source) {
                cards.push(card);
                if cards.len() as i64 == limit {
                    break;
                }
            }
        }
        Ok(cards)
    }
}
