//! Актуализация названий проектов.
//!
//! Индекс выводит имя проекта из ссылок и рабочих папок — оно устаревает после
//! переименования репозитория или переноса папки. Модель получает список
//! проектов с заголовками записей и кандидатов из git-корня (включая текущее
//! имя из remote URL) и предлагает имя и путь. По кнопке применяет пользователь,
//! в сборе данных (`refresh`) — сама программа, но только локальной моделью и не
//! поверх решений человека.
//! Переопределения лежат в `project_meta` и подставляются в `unit_index`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{Config, Task};
use crate::db::{Database, ProjectOverride};
use crate::llm::ChatClient;

const MAX_NAME_CHARS: usize = 80;
const MAX_TITLES: usize = 6;
const MAX_CANDIDATES: usize = 200;
/// Проектов за один сбор: остальные дождутся следующего, ответ модели ограничен 1500 токенами.
const REFRESH_BATCH: usize = 10;
const SEEN_PREFIX: &str = "projects:seen:";
const LOCKED_PREFIX: &str = "projects:locked:";

/// Ключ, текущее имя, папка, заголовки записей.
type ProjectRow = (String, String, String, Vec<String>);

#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub key: String,
    pub name: String,
    pub path: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub path: PathBuf,
    /// Имя из remote URL (`owner/repo`) или папки.
    pub repo: String,
}

/// Каталоги git-корня с `.git`: путь и имя репозитория из remote origin.
pub fn scan_git_candidates(root: &Path) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    scan_dir(root, root, 0, &mut candidates);
    candidates.truncate(MAX_CANDIDATES);
    candidates
}

fn scan_dir(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Candidate>) {
    if depth > 3 || out.len() >= MAX_CANDIDATES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut children: Vec<PathBuf> = entries.flatten().filter_map(|entry| {
        entry.file_type().ok().filter(|kind| kind.is_dir()).map(|_| entry.path())
    }).collect();
    children.sort();
    for child in children {
        let name = child.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if name.starts_with('.') || matches!(name, "node_modules" | "target" | "dist" | "build" | "obj" | "bin") {
            continue;
        }
        if child.join(".git").exists() {
            let repo = git_remote_repo(&child).unwrap_or_else(|| name.to_owned());
            out.push(Candidate { path: child.clone(), repo });
            // Вложенные репозитории тоже нужны, но не углубляемся бесконечно.
            scan_dir(root, &child, depth + 1, out);
            continue;
        }
        scan_dir(root, &child, depth + 1, out);
    }
}

/// Имя репозитория из `[remote "origin"] url`.
fn git_remote_repo(repo: &Path) -> Option<String> {
    let config = repo.join(".git").join("config");
    let text = std::fs::read_to_string(config).ok()?;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() != "url" {
            continue;
        }
        let url = value.trim().trim_end_matches(".git").trim_end_matches('/');
        if let Some(name) = url.rsplit(['/', ':']).next().filter(|name| !name.is_empty()) {
            return Some(name.to_owned());
        }
    }
    None
}

const PROJECT_SYSTEM: &str = concat!(
    "Ты приводишь в порядок названия проектов в локальной базе знаний разработчика. ",
    "На входе JSON: projects (ключ, текущее имя, папка, заголовки записей) и candidates ",
    "(папки с git-репозиториями и их имена из remote). Для каждого проекта определи, ",
    "как он называется сейчас, и в какой папке лежит. Опирайся на заголовки, пути и ",
    "совпадение имён; если уверенности нет — оставь имя прежним и путь пустым. ",
    "Ответ — строго один JSON-массив без пояснений и markdown: ",
    "[{\"key\":\"...\",\"name\":\"...\",\"path\":\"...\",\"evidence\":\"...\"}]. ",
    "name — 1-5 слов, человеческое имя продукта. path — абсолютный путь из кандидатов ",
    "или пустая строка. evidence — одна короткая причина по-русски."
);

fn strip_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let without_start = trimmed.strip_prefix("```json").or_else(|| trimmed.strip_prefix("```")).unwrap_or(trimmed);
    without_start.strip_suffix("```").unwrap_or(without_start).trim()
}

fn parse_proposals(text: &str, allowed: &HashMap<String, String>) -> Vec<Proposal> {
    let Some(start) = text.find('[') else { return Vec::new() };
    let Some(end) = text.rfind(']') else { return Vec::new() };
    // `]` из преамбулы раньше первого `[` дал бы start > end и панику среза.
    let Some(slice) = text.get(start..=end) else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<Value>(slice) else { return Vec::new() };
    let Some(items) = value.as_array() else { return Vec::new() };
    let mut proposals: Vec<Proposal> = Vec::new();
    for item in items {
        let key = item.get("key").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if !allowed.contains_key(key) || proposals.iter().any(|proposal| proposal.key == key) {
            continue;
        }
        let name = item.get("name").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            continue;
        }
        let path = item.get("path").and_then(Value::as_str).map(str::trim).unwrap_or("");
        let path = if path.is_empty() || valid_dir(path) { path.to_owned() } else { String::new() };
        let evidence = item.get("evidence").and_then(Value::as_str).unwrap_or("").chars().take(200).collect();
        proposals.push(Proposal { key: key.to_owned(), name: name.to_owned(), path, evidence });
    }
    proposals
}

fn valid_dir(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() && path.is_dir()
}

fn project_prompt(projects: &[ProjectRow], candidates: &[Candidate]) -> String {
    let projects: Vec<Value> = projects
        .iter()
        .map(|(key, current, path, titles)| {
            json!({"key": key, "current": current, "path": path, "titles": titles})
        })
        .collect();
    let candidates: Vec<Value> = candidates
        .iter()
        .map(|candidate| json!({"path": candidate.path, "repo": candidate.repo}))
        .collect();
    json!({"projects": projects, "candidates": candidates}).to_string()
}

/// Предложения модели; расход пишется с `task=projects`.
pub fn propose(db: &Database, config: &Config, limit: usize) -> Result<Vec<Proposal>, String> {
    let Some(route) = config.route_for(Task::Projects, false) else {
        return Err("модель не настроена: выберите локальную модель или провайдера".into());
    };
    let _guard = crate::local::job_guard(config, Task::Projects)?;
    // Сначала отсев, потом лимит: иначе ручные имена из начала списка съедают
    // его целиком, и до остальных проектов очередь не доходит.
    let mut projects = without_manual(db, project_rows(db)?)?;
    projects.truncate(limit.clamp(1, 30));
    if projects.is_empty() {
        return Ok(Vec::new());
    }
    let candidates = config.git_root.as_deref().map(scan_git_candidates).unwrap_or_default();
    let client = crate::llm::client_for(&route, 1_500).map_err(|error| error.to_string())?;
    ask_model(db, &client, &projects, &candidates)
}

fn without_manual(db: &Database, mut rows: Vec<ProjectRow>) -> Result<Vec<ProjectRow>, String> {
    let overrides = db.project_overrides().map_err(|error| error.to_string())?;
    rows.retain(|row| !overrides.get(&row.0).is_some_and(|value| value.source == "manual"));
    Ok(rows)
}

/// Проекты базы. Ключ — исходное имя: у переименованного `info.project` уже
/// отображаемое, а `project_meta` и `unit_index` сопоставляются по исходному.
fn project_rows(db: &Database) -> Result<Vec<ProjectRow>, String> {
    let index = db.unit_index().map_err(|error| error.to_string())?;
    let overrides = db.project_overrides().map_err(|error| error.to_string())?;
    let mut groups: HashMap<String, Vec<&crate::db::UnitInfo>> = HashMap::new();
    for info in index.values() {
        groups.entry(info.project.clone()).or_default().push(info);
    }
    let mut projects: Vec<ProjectRow> = groups
        .into_iter()
        .map(|(project, mut units)| {
            units.sort_by(|left, right| right.last_ts.cmp(&left.last_ts));
            let path = units.iter().map(|unit| unit.project_path.as_str()).find(|path| !path.is_empty()).unwrap_or("").to_owned();
            let titles: Vec<String> = units.iter().take(MAX_TITLES).map(|unit| unit.title.chars().take(120).collect()).collect();
            let key = overrides
                .iter()
                .find(|(_, value)| value.display_name == project)
                .map(|(key, _)| key.clone())
                .unwrap_or_else(|| project.clone());
            (key, project, path, titles)
        })
        .collect();
    projects.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(projects)
}

fn ask_model(db: &Database, client: &dyn ChatClient, projects: &[ProjectRow], candidates: &[Candidate]) -> Result<Vec<Proposal>, String> {
    let allowed: HashMap<String, String> = projects.iter().map(|(key, current, ..)| (key.clone(), current.clone())).collect();
    let prompt = project_prompt(projects, candidates);
    let reply = client.chat(PROJECT_SYSTEM, &prompt).map_err(|error| error.to_string())?;
    let _ = crate::llm::record_reply_usage(db, client, &reply, 0, Task::Projects);
    let mut proposals = parse_proposals(strip_fences(&reply.text), &allowed);
    // Модель отвечает по каждому проекту, «оставь как есть» — тоже ответ, а не предложение.
    proposals.retain(|proposal| {
        !projects.iter().any(|(key, current, path, _)| {
            *key == proposal.key && *current == proposal.name && (proposal.path.is_empty() || proposal.path == *path)
        })
    });
    Ok(proposals)
}

/// Итог актуализации названий для `/api/sync` и `sync.log`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefreshStats {
    pub checked: usize,
    /// Ключ и новое имя.
    pub applied: Vec<(String, String)>,
    /// Имя занято другим проектом или не прошло проверку.
    pub rejected: usize,
    pub no_local_model: bool,
}

impl RefreshStats {
    pub fn record(&self) -> Value {
        if self.no_local_model {
            return json!({"skipped": "no_local_model"});
        }
        json!({
            "checked": self.checked,
            "applied": self.applied.iter().map(|(key, name)| json!({"key": key, "name": name})).collect::<Vec<_>>(),
            "rejected": self.rejected,
        })
    }
}

/// Отпечаток входа модели: пока он не изменился, спрашивать про проект незачем.
fn fingerprint(row: &ProjectRow, candidates: &[Candidate]) -> String {
    let prompt = project_prompt(std::slice::from_ref(row), candidates);
    format!("{:x}", Sha256::digest(prompt.as_bytes()))
}

/// Что спрашивать у модели в этот сбор. Ручные имена и сброшенные человеком
/// проекты не трогаем, неизменившиеся пропускаем, чтобы не будить движок.
fn pending_rows(db: &Database, candidates: &[Candidate]) -> Result<Vec<ProjectRow>, String> {
    let overrides = db.project_overrides().map_err(|error| error.to_string())?;
    let mut pending = Vec::new();
    for row in project_rows(db)? {
        let key = &row.0;
        if overrides.get(key).is_some_and(|value| value.source == "manual") {
            continue;
        }
        let locked = db.get_state(&format!("{LOCKED_PREFIX}{key}")).map_err(|error| error.to_string())?;
        let seen = db.get_state(&format!("{SEEN_PREFIX}{key}")).map_err(|error| error.to_string())?;
        if locked.is_none() && seen.as_deref() != Some(fingerprint(&row, candidates).as_str()) {
            pending.push(row);
        }
    }
    pending.truncate(REFRESH_BATCH);
    Ok(pending)
}

fn refresh_rows(db: &Database, client: &dyn ChatClient, rows: &[ProjectRow], candidates: &[Candidate]) -> Result<RefreshStats, String> {
    let proposals = ask_model(db, client, rows, candidates)?;
    let overrides = db.project_overrides().map_err(|error| error.to_string())?;
    let mut stats = RefreshStats { checked: rows.len(), ..RefreshStats::default() };
    for proposal in proposals {
        let Some((key, ..)) = rows.iter().find(|row| row.0 == proposal.key) else { continue };
        // Пустой путь значит «не уверена», а не «стереть сохранённую папку».
        let path = if proposal.path.is_empty() {
            overrides.get(key).map(|value| value.path.as_str()).filter(|path| valid_dir(path)).unwrap_or("")
        } else {
            proposal.path.as_str()
        };
        match apply_from(db, key, &proposal.name, path, "auto") {
            Ok(()) => stats.applied.push((key.clone(), proposal.name)),
            Err(_) => stats.rejected += 1,
        }
    }
    // Отпечаток после переименования другой: иначе тот же проект спросили бы снова.
    let fresh = project_rows(db)?;
    for row in rows {
        let row = fresh.iter().find(|other| other.0 == row.0).unwrap_or(row);
        db.set_state(&format!("{SEEN_PREFIX}{}", row.0), &fingerprint(row, candidates)).map_err(|error| error.to_string())?;
    }
    Ok(stats)
}

/// Актуализация названий в сборе данных: только локальной моделью, платный
/// провайдер за фоновую работу расход не получает.
pub fn refresh(db: &Database, config: &Config) -> Result<RefreshStats, String> {
    let route = config
        .local_route(Task::Projects)
        .or_else(|| config.route_for(Task::Projects, false).filter(|route| route.local));
    let Some(route) = route else {
        return Ok(RefreshStats { no_local_model: true, ..RefreshStats::default() });
    };
    let candidates = config.git_root.as_deref().map(scan_git_candidates).unwrap_or_default();
    let rows = pending_rows(db, &candidates)?;
    if rows.is_empty() {
        return Ok(RefreshStats::default());
    }
    let _guard = crate::local::job_guard(config, Task::Projects)?;
    let client = crate::llm::client_for(&route, 1_500).map_err(|error| error.to_string())?;
    refresh_rows(db, &client, &rows, &candidates)
}

/// Применить переопределение. Имя уникально, путь — существующая папка.
pub fn apply(db: &Database, key: &str, name: &str, path: &str) -> Result<(), String> {
    apply_from(db, key, name, path, "manual")
}

fn apply_from(db: &Database, key: &str, name: &str, path: &str, source: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err("имя проекта: 1-80 символов без управляющих знаков".into());
    }
    let path = path.trim();
    if !path.is_empty() && !valid_dir(path) {
        return Err("папка проекта должна существовать".into());
    }
    let index = db.unit_index().map_err(|error| error.to_string())?;
    if index.values().any(|info| info.project == name && info.project != key) {
        return Err(format!("проект с именем «{name}» уже есть: два проекта слились бы"));
    }
    let overrides: HashMap<String, ProjectOverride> = db.project_overrides().map_err(|error| error.to_string())?;
    if overrides.iter().any(|(other, value)| other != key && value.display_name == name) {
        return Err(format!("имя «{name}» уже занято другим проектом"));
    }
    db.set_project_meta_from(key, name, path, source).map_err(|error| error.to_string())
}

/// Сброс — решение человека: автообновление этот проект больше не переименует,
/// иначе модель вернула бы то же имя в следующий сбор.
pub fn reset(db: &Database, value: &str) -> Result<bool, String> {
    let value = value.trim();
    let overrides = db.project_overrides().map_err(|error| error.to_string())?;
    if let Some((key, _)) = overrides.iter().find(|(key, meta)| key.as_str() == value || meta.display_name == value) {
        db.set_state(&format!("{LOCKED_PREFIX}{key}"), "1").map_err(|error| error.to_string())?;
    }
    db.delete_project_meta(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn bracket_before_array_start_is_not_a_panic() {
        let allowed = std::collections::HashMap::new();
        assert!(super::parse_proposals("1] ответ: [{\"key\":", &allowed).is_empty());
    }

    use super::*;

    fn temp_repo(dir: &Path, name: &str, remote: &str) -> PathBuf {
        let repo = dir.join(name);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git").join("config"), format!("[remote \"origin\"]\n\turl = {remote}\n")).unwrap();
        repo
    }

    #[test]
    fn scans_repos_and_reads_remote_names() {
        let dir = tempfile::tempdir().unwrap();
        temp_repo(dir.path(), "memory-base", "https://github.com/example/BrepsiBrain.git");
        temp_repo(dir.path(), "clou", "git@github.com:example/Notely.git");
        let found = scan_git_candidates(dir.path());
        let names: Vec<&str> = found.iter().map(|candidate| candidate.repo.as_str()).collect();
        assert!(names.contains(&"BrepsiBrain"), "{names:?}");
        assert!(names.contains(&"Notely"), "{names:?}");
        assert!(found.iter().all(|candidate| candidate.path.is_absolute()));
    }

    #[test]
    fn proposals_are_validated_against_known_keys() {
        let mut allowed = HashMap::new();
        allowed.insert("tools/memory-base".to_owned(), "tools/memory-base".to_owned());
        let text = r#"```json
[{"key":"tools/memory-base","name":"Brepsi Brain","path":"C:\\no-such-brepsi-dir","evidence":"remote"},
 {"key":"unknown","name":"X","path":"","evidence":"no"}]
```"#;
        let proposals = parse_proposals(strip_fences(text), &allowed);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].name, "Brepsi Brain");
        // Несуществующая папка отбрасывается, предложение остаётся.
        assert_eq!(proposals[0].path, "");
    }

    struct FakeClient {
        reply: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ChatClient for FakeClient {
        fn chat(&self, _system: &str, _user: &str) -> Result<crate::llm::ChatReply, crate::llm::LlmError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(crate::llm::ChatReply::text(self.reply.clone()))
        }
        fn base_url(&self) -> &str {
            "http://127.0.0.1:9/v1"
        }
        fn model(&self) -> &str {
            "fake"
        }
    }

    fn fake(name: &str) -> FakeClient {
        FakeClient {
            reply: format!(r#"[{{"key":"old-name","name":"{name}","path":"","evidence":"remote"}}]"#),
            calls: Default::default(),
        }
    }

    fn row(key: &str) -> ProjectRow {
        (key.to_owned(), key.to_owned(), String::new(), vec!["заголовок".to_owned()])
    }

    #[test]
    fn refresh_applies_as_auto_and_marks_project_seen() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        let client = fake("New Name");
        let stats = refresh_rows(&db, &client, &[row("old-name")], &[]).unwrap();
        assert_eq!(stats.applied, vec![("old-name".to_owned(), "New Name".to_owned())]);
        let overrides = db.project_overrides().unwrap();
        assert_eq!(overrides["old-name"].display_name, "New Name");
        assert_eq!(overrides["old-name"].source, "auto");
        assert!(db.get_state("projects:seen:old-name").unwrap().is_some());
    }

    #[test]
    fn refresh_keeps_saved_path_when_model_is_unsure() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        let saved = dir.path().display().to_string();
        db.set_project_meta_from("old-name", "Prev", &saved, "auto").unwrap();
        refresh_rows(&db, &fake("Next"), &[row("old-name")], &[]).unwrap();
        assert_eq!(db.project_overrides().unwrap()["old-name"].path, saved);
    }

    #[test]
    fn same_name_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        let stats = refresh_rows(&db, &fake("old-name"), &[row("old-name")], &[]).unwrap();
        assert!(stats.applied.is_empty());
        assert!(db.project_overrides().unwrap().is_empty());
    }

    #[test]
    fn current_name_is_not_proposed_unless_path_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        assert!(ask_model(&db, &fake("old-name"), &[row("old-name")], &[]).unwrap().is_empty());
        let found = dir.path().display().to_string().replace('\\', "\\\\");
        let client = FakeClient {
            reply: format!(r#"[{{"key":"old-name","name":"old-name","path":"{found}","evidence":"remote"}}]"#),
            calls: Default::default(),
        };
        assert_eq!(ask_model(&db, &client, &[row("old-name")], &[]).unwrap().len(), 1);
    }

    #[test]
    fn manual_names_are_not_sent_for_proposals() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        db.set_project_meta_from("mine", "Моё имя", "", "manual").unwrap();
        db.set_project_meta_from("auto", "Авто", "", "auto").unwrap();
        let rows = without_manual(&db, vec![row("mine"), row("auto"), row("plain")]).unwrap();
        assert_eq!(rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(), ["auto", "plain"]);
    }

    #[test]
    fn reset_locks_project_against_auto_rename() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        db.set_project_meta_from("old-name", "New Name", "", "auto").unwrap();
        assert!(reset(&db, "New Name").unwrap());
        assert_eq!(db.get_state("projects:locked:old-name").unwrap().as_deref(), Some("1"));
        assert!(db.project_overrides().unwrap().is_empty());
    }

    #[test]
    fn refresh_without_local_model_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        let mut config = Config::default();
        config.llm.base_url = Some("https://cloud.example/v1".into());
        config.llm.model = Some("cloud".into());
        assert!(refresh(&db, &config).unwrap().no_local_model);
    }

    #[test]
    fn bad_names_are_rejected() {
        let mut allowed = HashMap::new();
        allowed.insert("k".to_owned(), "k".to_owned());
        for text in [r#"[{"key":"k","name":""}]"#, r#"[{"key":"k","name":"a\nb"}]"#] {
            assert!(parse_proposals(text, &allowed).is_empty(), "{text}");
        }
    }
}
