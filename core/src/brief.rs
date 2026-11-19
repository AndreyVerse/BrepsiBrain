//! Брифинг проекта по рабочей папке для MCP `project_brief`.
//!
//! Исполняется в потоке stdio MCP-сервера, запросы строго по очереди. Тяжёлый
//! `unit_index` (~0,5 с) здесь не вызывается: проект берётся из кэша `unit_roots`,
//! который пишет фоновая проверка, а записи новее кэша — прямым разбором
//! git-ссылок и рабочих папок чатов.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::params;

use crate::db::{Card, Database, Result, CARD_KIND_DISTILLED, SCOPE_KEY_PREFIX};
use crate::stale::{git_toplevel, normalize};
use crate::text::format_date;

const MAX_CARDS: usize = 60;
const MAX_DECISIONS: usize = 12;
const MAX_ERRORS: usize = 8;
const MAX_STALE: usize = 5;
const MAX_PINS: usize = 5;
const LINE_CHARS: usize = 220;
/// ~1500 токенов: смешанный русский и код дают около 4 символов на токен.
const MAX_CHARS: usize = 6_000;
const ERROR_WINDOW_DAYS: i64 = 30;

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > LINE_CHARS {
        format!("{}…", flat.chars().take(LINE_CHARS - 1).collect::<String>())
    } else {
        flat
    }
}

/// Папка `.codegraph` в корне проекта или выше, но не дальше корня git.
pub fn codegraph_dir(start: &Path, top: Option<&Path>) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join(".codegraph");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if top.is_some_and(|top| top == dir) {
            break;
        }
    }
    None
}

/// Записи проекта: из кэша `unit_roots` и записи, которых в кэше ещё нет.
fn project_units(db: &Database, root: &str) -> Result<(Vec<String>, String)> {
    let con = db.connection();
    let mut units: Vec<String> = Vec::new();
    let mut project = String::new();
    let workspace = db.get_state("checks:git_root")?;
    let root_lower = root.to_lowercase();
    // Корень рабочей папки проектом не считается (`stale::project_root`); иначе его
    // чаты попадали бы в бриф только до первой фоновой проверки, а после неё пропадали.
    if workspace.as_deref().is_some_and(|workspace| normalize(workspace).to_lowercase() == root_lower) {
        return Ok((units, project));
    }
    {
        let mut stmt = con.prepare_cached("SELECT unit, project FROM unit_roots WHERE root=?1 COLLATE NOCASE")?;
        for row in stmt.query_map([root], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (unit, name) = row?;
            if project.is_empty() {
                project = name;
            }
            units.push(unit);
        }
    }
    let known: HashSet<String> = {
        let mut stmt = con.prepare_cached("SELECT unit FROM unit_roots")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    // Коммиты: unit `git:<путь от git_root>:<день>`. Путь берётся в исходном регистре:
    // LIKE в SQLite игнорирует регистр только у ASCII, а кириллица в пути — обычное дело.
    if let Some(workspace) = workspace.map(|workspace| normalize(&workspace)) {
        let prefix = format!("{}/", workspace.to_lowercase());
        let rel = root_lower.starts_with(&prefix).then(|| root.get(prefix.len()..)).flatten();
        if let Some(rel) = rel {
            let mut stmt = con.prepare_cached(
                "SELECT DISTINCT unit FROM cards WHERE kind='raw' AND unit LIKE 'git:' || ?1 || ':%' ESCAPE '\\'",
            )?;
            let like = rel.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            for unit in stmt.query_map([like], |r| r.get::<_, String>(0))? {
                let unit = unit?;
                if !known.contains(&unit) {
                    units.push(unit);
                }
            }
        }
    }
    // Чаты новее кэша: рабочая папка внутри проекта и тот же корень git.
    let end = format!("{}\u{10FFFF}", SCOPE_KEY_PREFIX);
    let mut stmt = con.prepare_cached("SELECT key, value FROM state WHERE key>=?1 AND key<?2")?;
    for row in stmt.query_map(params![SCOPE_KEY_PREFIX, end], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (key, scope) = row?;
        let unit = &key[SCOPE_KEY_PREFIX.len()..];
        if known.contains(unit) {
            continue;
        }
        let scope_norm = normalize(&scope);
        let lower = scope_norm.to_lowercase();
        if lower != root_lower && !lower.starts_with(&format!("{root_lower}/")) {
            continue;
        }
        let top = git_toplevel(Path::new(&scope_norm)).map(|top| normalize(&top.to_string_lossy()).to_lowercase());
        if top.as_deref() == Some(root_lower.as_str()) {
            units.push(unit.to_owned());
        }
    }
    Ok((units, project))
}

fn unit_cards(db: &Database, units: &[String]) -> Result<Vec<Card>> {
    let mut cards = Vec::new();
    let mut stmt = db.connection().prepare_cached("SELECT id FROM cards WHERE source_ref=?1 AND kind=?2")?;
    for unit in units {
        let id: Option<i64> = stmt.query_row(params![format!("llm:{unit}"), CARD_KIND_DISTILLED], |r| r.get(0)).ok();
        if let Some(card) = id.map(|id| db.get_card(id)).transpose()?.flatten() {
            cards.push(card);
        }
    }
    cards.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.id.cmp(&a.id)));
    cards.truncate(MAX_CARDS);
    Ok(cards)
}

/// Выжимка проекта для агента. Неизвестная папка — короткий ответ без ошибки.
pub fn project_brief(db: &Database, cwd: &str, now: i64) -> Result<String> {
    let cwd_path = PathBuf::from(cwd.trim());
    let top = git_toplevel(&cwd_path);
    let root = normalize(&top.as_deref().unwrap_or(&cwd_path).to_string_lossy());
    let codegraph = codegraph_dir(&cwd_path, top.as_deref()).map(|dir| {
        format!(
            "Структура кода: у проекта есть индекс CodeGraph (`{}`) — символы, вызовы и влияние правок смотреть через CodeGraph MCP, а не здесь.",
            normalize(&dir.to_string_lossy())
        )
    });
    let (units, project) = if root.is_empty() { (Vec::new(), String::new()) } else { project_units(db, &root)? };
    let cards = unit_cards(db, &units)?;
    let mut out = Vec::new();
    if cards.is_empty() {
        out.push(format!("No project memory for {}.", if root.is_empty() { cwd } else { &root }));
        out.extend(codegraph);
        return Ok(out.join("\n"));
    }
    let name = if project.is_empty() { root.rsplit('/').next().unwrap_or(&root).to_owned() } else { project };
    out.push(format!(
        "# Project brief: {name}\nroot: {root} | records: {} | last: {}\nDetails: get_card(id).",
        units.len(),
        format_date(cards[0].ts)
    ));
    out.extend(codegraph);

    let pins: HashSet<String> = crate::insights::pinned_units(db).into_iter().collect();
    let pinned: Vec<String> = cards
        .iter()
        .filter(|card| pins.contains(&card.unit))
        .take(MAX_PINS)
        .map(|card| format!("- [{}] {} ({})", card.id, clip(&card.title), format_date(card.ts)))
        .collect();
    if !pinned.is_empty() {
        out.push(format!("\n## Pinned\n{}", pinned.join("\n")));
    }

    let superseded = crate::conflicts::superseded_set(db);
    let mut seen = HashSet::new();
    let (mut decisions, mut errors) = (Vec::new(), Vec::new());
    for card in &cards {
        for (kind, text) in crate::insights::atoms(card) {
            let line = format!("- {} [{}, {}]", clip(&text), card.id, format_date(card.ts));
            match kind.as_str() {
                "решение" if decisions.len() < MAX_DECISIONS
                    && !superseded.contains(&(card.id, text.clone()))
                    && seen.insert(text.to_lowercase()) => decisions.push(line),
                "ошибка" if errors.len() < MAX_ERRORS
                    && card.ts >= now - ERROR_WINDOW_DAYS * 86_400
                    && seen.insert(text.to_lowercase()) => errors.push(line),
                _ => {}
            }
        }
    }
    if !decisions.is_empty() {
        out.push(format!("\n## Decisions in force\n{}", decisions.join("\n")));
    }
    if !errors.is_empty() {
        out.push(format!("\n## Recent errors and pitfalls ({ERROR_WINDOW_DAYS} days)\n{}", errors.join("\n")));
    }
    let stale = crate::stale::stale_cards(db);
    let stale_lines: Vec<String> = cards
        .iter()
        .filter_map(|card| stale.get(&card.id).map(|missing| (card, missing)))
        .take(MAX_STALE)
        .map(|(card, missing)| {
            let list = missing.iter().take(4).map(|item| format!("`{item}`")).collect::<Vec<_>>().join(", ");
            format!("- [{}] {} — missing in the tree: {list}", card.id, clip(&card.title))
        })
        .collect();
    if !stale_lines.is_empty() {
        out.push(format!("\n## Possibly stale\n{}", stale_lines.join("\n")));
    }
    let mut text = out.join("\n");
    if text.chars().count() > MAX_CHARS {
        text = format!("{}\n…", text.chars().take(MAX_CHARS).collect::<String>());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{now_ts, CardDraft};

    fn card(db: &Database, unit: &str, ts: i64, atoms: &[&str]) -> Card {
        let body = crate::llm::render_card_body("кратко", &[], &atoms.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let mut raw = CardDraft::new("claude", &format!("{unit}:1"), ts, "t", "фрагмент");
        raw.unit = unit.to_owned();
        db.insert_card(&raw).unwrap();
        let mut draft = CardDraft::new("claude", &format!("llm:{unit}"), ts, "Запись", &body).distilled();
        draft.unit = unit.to_owned();
        db.insert_card(&draft).unwrap();
        db.get_card_by_ref(&format!("llm:{unit}")).unwrap().unwrap()
    }

    #[test]
    fn brief_by_path_skips_superseded_and_unknown_path_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let db = Database::open(dir.path().join("m.db")).unwrap();
        let now = now_ts();
        let old = card(&db, "claude:a", now - 100, &["решение: старый путь", "ошибка: падал sync"]);
        card(&db, "claude:b", now, &["решение: новый путь"]);
        let root = normalize(&repo.to_string_lossy());
        for unit in ["claude:a", "claude:b"] {
            db.connection().execute("INSERT INTO unit_roots VALUES(?1, 'proj', ?2)", params![unit, root]).unwrap();
        }
        // Запись новее кэша находится по рабочей папке чата.
        card(&db, "claude:c", now - 50, &["решение: из свежего чата"]);
        db.set_state("scope:claude:c", &format!("{}\\src", repo.display())).unwrap();
        db.connection()
            .execute(
                "INSERT INTO decision_conflicts(new_card,new_text,old_card,old_text,status,created_at) VALUES(0,'x',?1,'старый путь','superseded',0)",
                [old.id],
            )
            .unwrap();

        let text = project_brief(&db, &repo.join("src").to_string_lossy(), now).unwrap();
        assert!(text.contains("# Project brief: proj"), "{text}");
        assert!(text.contains("- новый путь") && text.contains("- из свежего чата"), "{text}");
        assert!(!text.contains("старый путь"), "{text}");
        assert!(text.contains("- падал sync"));
        assert!(!text.contains("CodeGraph"));

        let elsewhere = dir.path().join("other");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let empty = project_brief(&db, &elsewhere.to_string_lossy(), now).unwrap();
        assert!(empty.starts_with("No project memory for"), "{empty}");
        assert!(project_brief(&db, "Q:/no/such/dir", now).unwrap().starts_with("No project memory"));
    }

    #[test]
    fn codegraph_index_is_mentioned_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert_eq!(codegraph_dir(&repo.join("src"), Some(&repo)), None);
        // Индекс выше корня git не относится к проекту.
        std::fs::create_dir_all(dir.path().join(".codegraph")).unwrap();
        assert_eq!(codegraph_dir(&repo.join("src"), Some(&repo)), None);
        std::fs::create_dir_all(repo.join(".codegraph")).unwrap();
        assert_eq!(codegraph_dir(&repo.join("src"), Some(&repo)), Some(repo.join(".codegraph")));
        let db = Database::open(dir.path().join("m.db")).unwrap();
        let text = project_brief(&db, &repo.join("src").to_string_lossy(), now_ts()).unwrap();
        assert!(text.contains("индекс CodeGraph"), "{text}");
    }
}
