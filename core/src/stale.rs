//! Устаревшие карточки: пути файлов и идентификаторы из карточки, которых больше
//! нет в рабочем дереве репозитория проекта.
//!
//! Исполняется в фоновой проверке после синхронизации (`collect::background_checks`):
//! в отдельном потоке процесса панели или в процессе `sync` планировщика, со своим
//! соединением SQLite. Панель и MCP только читают таблицу `stale_checks`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use rusqlite::params;
use serde_json::{json, Value};

use crate::db::{now_ts, Database, UnitInfo, CARD_KIND_DISTILLED};

/// Полный повторный проход раз в сутки: файл мог пропасть и без правки карточки.
const RECHECK_SECONDS: i64 = 86_400;
const MAX_REFS_PER_CARD: usize = 30;
/// Паттернов на один `git grep`: командная строка Windows ограничена ~32 тыс. символов.
const GREP_CHUNK: usize = 150;
const GIT_MIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Расширения, по которым токен признаётся путём файла. Без списка `api.example.ai`
/// и версии `1.3.1` считались бы пропавшими файлами.
const FILE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "mjs", "cjs", "jsx", "json", "toml", "md", "py", "ps1", "psm1", "css", "scss", "html",
    "cs", "cpp", "cc", "c", "h", "hpp", "gd", "tscn", "tres", "gdshader", "yml", "yaml", "sql", "sh", "bat", "cmd",
    "uproject", "uplugin", "ini", "xml", "vue", "svelte", "kt", "java", "go", "lua", "txt", "csproj", "lock",
];

/// Ссылки карточки на код: пути файлов и имена символов.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Refs {
    pub paths: Vec<String>,
    pub idents: Vec<String>,
}

fn backtick_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`([^`\n]{2,160})`").expect("static backtick regex"))
}

fn bare_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:[A-Za-z]:[/\\])?[\w.\-]+(?:[/\\][\w.\-]+)+").expect("static path regex"))
}

fn ident_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\.)[A-Za-z_][A-Za-z0-9_]*)*$").expect("static ident regex")
    })
}

fn has_file_extension(token: &str) -> bool {
    token
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && FILE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Путь файла: со слешем и известным расширением. Голое имя (`log.md`) отброшено:
/// в разговоре оно чаще относится к соседнему проекту и давало ложные «пропажи».
/// Плейсхолдеры и шаблоны (`~/`, `<profile>`, `*.md`, `%APPDATA%`) — не пути.
fn as_path(token: &str) -> Option<String> {
    let token = token.trim().trim_end_matches([',', '.', ';', ':', ')']);
    let placeholder = token.contains(['~', '<', '>', '*', '%', '$', '{', '}']);
    if placeholder || token.contains(char::is_whitespace) || token.contains("://") || !has_file_extension(token) {
        return None;
    }
    let path = token.replace('\\', "/");
    let path = path.trim_start_matches("./");
    path.contains('/').then(|| path.to_owned())
}

/// Последний сегмент `a::b::c` / `obj.method`: в исходниках встречается именно он.
/// Обычные слова (`cargo`, `sync`) не берутся — нужен `_` или заглавная внутри имени.
fn as_ident(token: &str) -> Option<String> {
    let token = token.trim().trim_end_matches("()");
    if !ident_re().is_match(token) {
        return None;
    }
    let last = token.rsplit("::").next().unwrap_or(token).rsplit('.').next().unwrap_or(token);
    let distinctive = last.contains('_') || last.chars().skip(1).any(|c| c.is_ascii_uppercase());
    (last.chars().count() >= 4 && distinctive && last.chars().any(|c| c.is_ascii_alphabetic())).then(|| last.to_owned())
}

/// Ссылки из текста карточки: в обратных кавычках — пути и символы, вне их —
/// только пути со слешем и известным расширением.
pub fn extract_refs(text: &str) -> Refs {
    let mut refs = Refs::default();
    let mut seen = HashSet::new();
    let mut push = |list: &mut Vec<String>, value: String| {
        if seen.insert(value.to_lowercase()) {
            list.push(value);
        }
    };
    for capture in backtick_re().captures_iter(text) {
        let span = &capture[1];
        if let Some(path) = as_path(span) {
            push(&mut refs.paths, path);
        } else if let Some(ident) = as_ident(span) {
            push(&mut refs.idents, ident);
        }
    }
    let outside = backtick_re().replace_all(text, " ");
    for found in bare_path_re().find_iter(&outside) {
        // Хвост URL или пути с переменной (`github.com/…/latest.json`, `%APPDATA%/x.json`).
        let glued = outside[..found.start()].chars().next_back().is_some_and(|c| c.is_alphanumeric() || "/\\:.@%~".contains(c));
        if glued {
            continue;
        }
        if let Some(path) = as_path(found.as_str()) {
            push(&mut refs.paths, path);
        }
    }
    refs.paths.truncate(MAX_REFS_PER_CARD);
    refs.idents.truncate(MAX_REFS_PER_CARD.saturating_sub(refs.paths.len()));
    refs
}

// -- корни проектов ---------------------------------------------------------------------

pub fn normalize(path: &str) -> String {
    let mut value = path.replace('\\', "/");
    if let Some(rest) = value.strip_prefix("//?/") {
        value = rest.to_owned();
    }
    while value.len() > 1 && value.ends_with('/') && !value.ends_with(":/") {
        value.pop();
    }
    value
}

/// Ближайший предок с `.git` (папка или файл worktree), включая саму папку.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|candidate| candidate.join(".git").exists()).map(Path::to_path_buf)
}

/// Корень репозитория записи по папке проекта из индекса. Корень `git_root`
/// (общая папка всех проектов) проектом не считается: в его дереве лежат только
/// файлы обвязки, и любая ссылка на вложенный проект выглядела бы пропавшей.
pub fn project_root(project_path: &str, git_root: &Path, cache: &mut HashMap<String, Option<String>>) -> Option<String> {
    if project_path.is_empty() {
        return None;
    }
    if let Some(known) = cache.get(project_path) {
        return known.clone();
    }
    let candidate = PathBuf::from(project_path);
    let dir = if candidate.is_absolute() { candidate } else { git_root.join(candidate) };
    let workspace = normalize(&git_root.to_string_lossy()).to_lowercase();
    let root = dir
        .is_dir()
        .then(|| git_toplevel(&dir))
        .flatten()
        .map(|top| normalize(&top.to_string_lossy()))
        .filter(|top| top.to_lowercase() != workspace);
    cache.insert(project_path.to_owned(), root.clone());
    root
}

/// Переписать кэш `unit_roots` по свежему индексу записей.
pub fn refresh_unit_roots(db: &Database, index: &HashMap<String, UnitInfo>, git_root: &Path) -> Result<usize, String> {
    let mut cache = HashMap::new();
    let rows: Vec<(String, String, String)> = index
        .values()
        .map(|info| {
            let root = project_root(&info.project_path, git_root, &mut cache).unwrap_or_default();
            (info.unit.clone(), info.project.clone(), root)
        })
        .collect();
    let con = db.connection();
    db.atomic(|| {
        con.execute("DELETE FROM unit_roots", [])?;
        let mut stmt = con.prepare("INSERT INTO unit_roots(unit, project, root) VALUES(?1, ?2, ?3)")?;
        for (unit, project, root) in &rows {
            stmt.execute(params![unit, project, root])?;
        }
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    db.set_state("checks:git_root", &normalize(&git_root.to_string_lossy())).map_err(|error| error.to_string())?;
    Ok(rows.len())
}

// -- сверка с рабочим деревом -----------------------------------------------------------

/// Файлы репозитория: полные пути и индекс по имени файла, всё в нижнем регистре.
struct TreeFiles {
    full: HashSet<String>,
    by_name: HashMap<String, Vec<String>>,
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now()).max(GIT_MIN_TIMEOUT)
}

fn ls_files(root: &Path, deadline: Instant) -> Result<TreeFiles, String> {
    let mut cmd = crate::ingest::git(root);
    cmd.args(["-c", "core.quotepath=false", "ls-files", "-z"]);
    let out = crate::ingest::output_with_timeout(cmd, remaining(deadline))
        .ok_or_else(|| format!("git ls-files в {}: не запустился или превысил время", root.display()))?;
    if !out.status.success() {
        return Err(format!("git ls-files в {}: {}", root.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut files = TreeFiles { full: HashSet::new(), by_name: HashMap::new() };
    for entry in String::from_utf8_lossy(&out.stdout).split('\0').filter(|entry| !entry.is_empty()) {
        let lower = entry.to_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower).to_owned();
        files.by_name.entry(name).or_default().push(lower.clone());
        files.full.insert(lower);
    }
    Ok(files)
}

/// Какие из `idents` встречаются в отслеживаемых файлах целым словом (`git grep -F -w -o`):
/// подстрока нашла бы `removed_fn` внутри `removed_fn_v2`, и пропажа символа не заметилась бы.
fn grep_found(root: &Path, idents: &[String], deadline: Instant) -> Result<HashSet<String>, String> {
    let mut found = HashSet::new();
    for chunk in idents.chunks(GREP_CHUNK) {
        let mut cmd = crate::ingest::git(root);
        cmd.args(["grep", "-F", "-w", "-o", "-h", "-I", "--no-color"]);
        for ident in chunk {
            cmd.arg("-e").arg(ident);
        }
        let out = crate::ingest::output_with_timeout(cmd, remaining(deadline))
            .ok_or_else(|| format!("git grep в {}: не запустился или превысил время", root.display()))?;
        // Код 1 у git grep — «совпадений нет», а не сбой.
        if !out.status.success() && out.status.code() != Some(1) {
            return Err(format!("git grep в {}: {}", root.display(), String::from_utf8_lossy(&out.stderr).trim()));
        }
        found.extend(String::from_utf8_lossy(&out.stdout).lines().map(|line| line.trim().to_owned()));
    }
    Ok(found)
}

fn path_present(path: &str, root: &Path, root_norm: &str, files: &TreeFiles) -> Option<bool> {
    let lower = path.to_lowercase();
    let absolute = lower.as_bytes().get(1) == Some(&b':') || lower.starts_with('/');
    let relative = if absolute {
        // Путь вне репозитория проекта к нему не относится — не проверяем.
        lower.strip_prefix(&format!("{}/", root_norm.to_lowercase()))?.to_owned()
    } else {
        lower
    };
    // Путь от родительской папки (`Git/.shared/x.ps1`, `Programs/X/README.md`) — тоже ссылка на живой файл.
    if files.full.contains(&relative) || root.ancestors().any(|dir| dir.join(&relative).exists()) {
        return Some(true);
    }
    let name = relative.rsplit('/').next().unwrap_or(&relative);
    let tail = format!("/{relative}");
    // Путь от подпапки репозитория: `src/insights.rs` при `core/src/insights.rs`.
    Some(files.by_name.get(name).is_some_and(|candidates| candidates.iter().any(|full| full.ends_with(&tail))))
}

/// Пропавшие ссылки одной карточки при известных файлах и найденных символах.
fn missing_refs(refs: &Refs, root: &Path, root_norm: &str, files: &TreeFiles, found: &HashSet<String>) -> Vec<String> {
    let mut missing: Vec<String> =
        refs.paths.iter().filter(|path| path_present(path, root, root_norm, files) == Some(false)).cloned().collect();
    missing.extend(refs.idents.iter().filter(|ident| !found.contains(*ident)).cloned());
    missing
}

struct Candidate {
    id: i64,
    sig: String,
    root: String,
    body: String,
}

/// Инкрементальная проверка до `deadline`: новые и изменённые карточки первыми,
/// затем давно проверенные (полный проход раз в сутки). Сбой git по одному
/// репозиторию не останавливает остальные и попадает в `errors`.
pub fn check(db: &Database, now: i64, deadline: Instant) -> Value {
    let con = db.connection();
    let mut errors: Vec<String> = Vec::new();
    let _ = con.execute("DELETE FROM stale_checks WHERE card_id NOT IN (SELECT id FROM cards)", []);
    let candidates: Vec<(Candidate, bool)> = {
        let sql = "SELECT c.id, c.ts, c.title, c.body, r.root, s.sig, COALESCE(s.checked_at, 0) \
                   FROM cards c JOIN unit_roots r ON r.unit=c.unit \
                   LEFT JOIN stale_checks s ON s.card_id=c.id \
                   WHERE c.kind=?1 AND c.source_ref='llm:'||c.unit";
        let mut stmt = match con.prepare(sql) {
            Ok(stmt) => stmt,
            Err(error) => return json!({"error": error.to_string()}),
        };
        let rows = stmt.query_map([CARD_KIND_DISTILLED], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        });
        let rows = match rows {
            Ok(rows) => rows.flatten().collect::<Vec<_>>(),
            Err(error) => return json!({"error": error.to_string()}),
        };
        let mut out = Vec::new();
        let mut rootless = Vec::new();
        for (id, ts, title, body, root, stored, checked_at) in rows {
            if root.is_empty() {
                rootless.push(id);
                continue;
            }
            let sig = format!("{ts}:{}:{}|{root}", title.len(), body.len());
            let changed = stored.as_deref() != Some(sig.as_str());
            if changed || checked_at < now - RECHECK_SECONDS {
                out.push((Candidate { id, sig, root, body }, changed));
            }
        }
        for id in rootless {
            let _ = con.execute("DELETE FROM stale_checks WHERE card_id=?1", [id]);
        }
        out
    };
    let pending = candidates.len();
    // Новые и изменённые первыми; среди остальных — порядок выдачи (по id).
    let mut ordered = candidates;
    ordered.sort_by_key(|(_, changed)| !changed);
    let mut by_root: Vec<(String, Vec<Candidate>)> = Vec::new();
    for (candidate, _) in ordered {
        match by_root.iter_mut().find(|(root, _)| *root == candidate.root) {
            Some((_, list)) => list.push(candidate),
            None => by_root.push((candidate.root.clone(), vec![candidate])),
        }
    }
    let (mut checked, mut stale) = (0usize, 0usize);
    let mut timed_out = false;
    for (root_norm, cards) in by_root {
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        let root = PathBuf::from(&root_norm);
        let files = match ls_files(&root, deadline) {
            Ok(files) => files,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let refs: Vec<Refs> = cards.iter().map(|card| extract_refs(&card.body)).collect();
        let mut idents: Vec<String> = refs.iter().flat_map(|r| r.idents.iter().cloned()).collect();
        idents.sort();
        idents.dedup();
        let found = if idents.is_empty() {
            HashSet::new()
        } else {
            match grep_found(&root, &idents, deadline) {
                Ok(found) => found,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            }
        };
        let written = db.atomic(|| {
            let mut stmt = con.prepare(
                "INSERT INTO stale_checks(card_id, sig, checked_at, missing) VALUES(?1, ?2, ?3, ?4) \
                 ON CONFLICT(card_id) DO UPDATE SET sig=excluded.sig, checked_at=excluded.checked_at, missing=excluded.missing",
            )?;
            let mut stale_here = 0;
            for (card, card_refs) in cards.iter().zip(&refs) {
                let missing = missing_refs(card_refs, &root, &root_norm, &files, &found);
                stale_here += usize::from(!missing.is_empty());
                stmt.execute(params![card.id, card.sig, now, Value::from(missing).to_string()])?;
            }
            Ok(stale_here)
        });
        match written {
            Ok(stale_here) => {
                checked += cards.len();
                stale += stale_here;
            }
            Err(error) => errors.push(format!("запись stale_checks: {error}")),
        }
    }
    json!({"pending": pending, "checked": checked, "stale_found": stale, "timed_out": timed_out, "errors": errors})
}

// -- чтение для панели и MCP --------------------------------------------------------------

fn parse_missing(text: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(text).unwrap_or_default()
}

/// Пропавшие ссылки карточки; пусто — не устарела или не проверялась.
pub fn missing_for(db: &Database, card_id: i64) -> Vec<String> {
    db.connection()
        .query_row("SELECT missing FROM stale_checks WHERE card_id=?1", [card_id], |row| row.get::<_, String>(0))
        .map(|text| parse_missing(&text))
        .unwrap_or_default()
}

/// Все устаревшие карточки: id → пропавшие ссылки.
pub fn stale_cards(db: &Database) -> HashMap<i64, Vec<String>> {
    let Ok(mut stmt) = db.connection().prepare("SELECT card_id, missing FROM stale_checks WHERE missing<>'[]'") else {
        return HashMap::new();
    };
    stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
        .map(|rows| rows.flatten().map(|(id, text)| (id, parse_missing(&text))).filter(|(_, list)| !list.is_empty()).collect())
        .unwrap_or_default()
}

pub fn now_check(db: &Database, budget: Duration) -> Value {
    check(db, now_ts(), Instant::now() + budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CardDraft;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn extracts_paths_and_identifiers_but_not_words() {
        let refs = extract_refs(
            "- решение: правка в `core/src/stale.rs` и `insights::fuse_ranks`, см. app/src/main.rs\n\
             - факт: `cargo` и `sync` не символы, `api.example.ai` и `1.3.1` не файлы, `SEMANTIC_MIN` — символ\n\
             - факт: `log.md`, `~/.codex/config.toml`, `profiles/<p>/x.yml` и https://github.com/o/r/releases/latest.json не проверяются",
        );
        assert_eq!(refs.paths, vec!["core/src/stale.rs", "app/src/main.rs"]);
        assert_eq!(refs.idents, vec!["fuse_ranks", "SEMANTIC_MIN"]);
    }

    #[test]
    fn detects_vanished_paths_and_identifiers_in_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("proj");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/live.rs"), "pub fn keep_this_fn() {}\n").unwrap();
        std::fs::write(repo.join("src/gone.rs"), "pub fn removed_fn() {}\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);

        let db = Database::open(dir.path().join("m.db")).unwrap();
        let body = crate::llm::render_card_body(
            "кратко",
            &[],
            &["решение: `src/live.rs` и `keep_this_fn`".into(), "факт: `src/gone.rs` с `removed_fn`".into()],
        );
        let mut raw = CardDraft::new("claude", "claude:s:1", now_ts(), "t", "фрагмент");
        raw.unit = "claude:s".into();
        db.insert_card(&raw).unwrap();
        let mut card = CardDraft::new("claude", "llm:claude:s", now_ts(), "t", &body).distilled();
        card.unit = "claude:s".into();
        db.insert_card(&card).unwrap();
        let id = db.get_card_by_ref("llm:claude:s").unwrap().unwrap().id;
        let root = normalize(&repo.to_string_lossy());
        db.connection().execute("INSERT INTO unit_roots VALUES('claude:s', 'proj', ?1)", [&root]).unwrap();

        let first = now_check(&db, Duration::from_secs(30));
        assert_eq!(first["checked"], 1, "{first}");
        assert!(missing_for(&db, id).is_empty(), "до удаления всё на месте");

        git(&repo, &["rm", "-q", "src/gone.rs"]);
        // Карточка не менялась: её возьмёт только суточный полный проход.
        assert_eq!(check(&db, now_ts(), Instant::now() + Duration::from_secs(30))["checked"], 0);
        let later = check(&db, now_ts() + RECHECK_SECONDS + 1, Instant::now() + Duration::from_secs(30));
        assert_eq!(later["checked"], 1, "{later}");
        assert_eq!(missing_for(&db, id), vec!["src/gone.rs", "removed_fn"]);
        assert!(stale_cards(&db).contains_key(&id));
    }

    #[test]
    fn workspace_root_is_not_a_project() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::create_dir_all(dir.path().join("plain/sub")).unwrap();
        let mut cache = HashMap::new();
        assert_eq!(project_root("plain/sub", dir.path(), &mut cache), None);
        git(&dir.path().join("plain"), &["init", "-q"]);
        let mut cache = HashMap::new();
        let found = project_root("plain/sub", dir.path(), &mut cache).unwrap();
        assert!(found.ends_with("/plain"), "{found}");
    }
}
