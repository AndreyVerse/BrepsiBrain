//! Противоречия между решениями: новое решение сверяется с похожими старыми
//! решениями того же проекта, классификацию пары делает LLM-провайдер.
//!
//! `run` исполняется в фоновой проверке после синхронизации (поток панели или
//! процесс `sync`), остальное — в потоке HTTP-запроса панели. Без провайдера
//! модуль ничего не сверяет и не помечает: панель показывает «нужен провайдер».

use std::collections::{HashMap, HashSet};

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::config::{Config, Route, Task};
use crate::db::{now_ts, Card, Database, Filters, CARD_KIND_DISTILLED};
use crate::llm::{card_is_local_only, record_reply_usage, ChatClient, LocalOnly};
use crate::text::format_date;

const BASELINE_KEY: &str = "conflicts:baseline";
const CANDIDATES_PER_DECISION: usize = 2;
/// Ниже этого косинуса по словам решения о разном, сверять их — пустая трата запроса.
const MIN_OVERLAP: f64 = 0.25;
const MAX_TOKENS: u32 = 600;
/// Пар в одном запросе: вердикт пары — до ~20 токенов, и при лимите сверки до 100
/// решений одна пачка не уложилась бы в `MAX_TOKENS` — обрезанный ответ не разбирается.
const PAIRS_PER_REQUEST: usize = 15;

const SYSTEM: &str = "Ты сверяешь пары технических решений одного проекта. Для каждой пары ответь, \
как новое решение относится к старому: «противоречит» — делают несовместимое, старое больше не \
действует; «уточняет» — дополняет или сужает старое без отмены; «не связано» — о разном. \
Отвечай только JSON: {\"verdicts\":[{\"pair\":1,\"verdict\":\"противоречит|уточняет|не связано\"}]}.";

/// Решение карточки, ещё не сверенное.
#[derive(Debug, Clone)]
struct Decision {
    card: Card,
    text: String,
    project: String,
}

#[derive(Debug, Clone)]
struct Pair {
    new_card: Card,
    new_text: String,
    old_card: Card,
    old_text: String,
}

fn decisions_of(card: &Card) -> Vec<String> {
    crate::insights::atoms(card).into_iter().filter(|(kind, _)| kind == "решение").map(|(_, text)| text).collect()
}

fn distilled_cards(db: &Database) -> Result<Vec<Card>, String> {
    let mut stmt = db
        .connection()
        .prepare("SELECT id FROM cards WHERE kind=?1 AND source_ref='llm:'||unit ORDER BY ts")
        .map_err(|e| e.to_string())?;
    let ids: Vec<i64> = stmt.query_map([CARD_KIND_DISTILLED], |row| row.get(0)).map_err(|e| e.to_string())?.flatten().collect();
    Ok(ids.into_iter().filter_map(|id| db.get_card(id).ok().flatten()).collect())
}

fn mark_checked(db: &Database, items: &[(i64, String)]) -> Result<(), String> {
    let now = now_ts();
    db.atomic(|| {
        let mut stmt = db
            .connection()
            .prepare("INSERT INTO decision_checks(card_id, text, checked_at) VALUES(?1, ?2, ?3) ON CONFLICT DO NOTHING")?;
        for (card, text) in items {
            stmt.execute(params![card, text, now])?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
}

/// Несверенные решения, старые первыми. Первый запуск только запоминает уже
/// существующие решения: сверять всю историю разом — сотни платных запросов.
fn pending(db: &Database, projects: &HashMap<String, String>, limit: usize) -> Result<Vec<Decision>, String> {
    let cards = distilled_cards(db)?;
    let checked: HashSet<(i64, String)> = {
        let mut stmt = db.connection().prepare("SELECT card_id, text FROM decision_checks").map_err(|e| e.to_string())?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    let fresh: Vec<(i64, String)> = cards
        .iter()
        .flat_map(|card| decisions_of(card).into_iter().map(move |text| (card.id, text)))
        .filter(|key| !checked.contains(key))
        .collect();
    if db.get_state(BASELINE_KEY).map_err(|e| e.to_string())?.is_none() {
        mark_checked(db, &fresh)?;
        db.set_state(BASELINE_KEY, &now_ts().to_string()).map_err(|e| e.to_string())?;
        return Ok(Vec::new());
    }
    let by_id: HashMap<i64, &Card> = cards.iter().map(|card| (card.id, card)).collect();
    Ok(fresh
        .into_iter()
        .take(limit)
        .filter_map(|(id, text)| {
            let card = (*by_id.get(&id)?).clone();
            let project = projects.get(&card.unit).cloned().unwrap_or_default();
            Some(Decision { card, text, project })
        })
        .collect())
}

/// Похожие старые решения того же проекта: BM25 по тексту решения, затем
/// косинус по словам решения (`similarity`), лучшие `CANDIDATES_PER_DECISION`.
fn candidates(db: &Database, decision: &Decision, projects: &HashMap<String, String>) -> Vec<Pair> {
    if decision.project.is_empty() {
        return Vec::new();
    }
    let Ok(hits) = db.search(&decision.text, 30, None, &Filters::default()) else { return Vec::new() };
    let mine = crate::similarity::token_set(&decision.text);
    let mut scored: Vec<(f64, Card, String)> = Vec::new();
    for hit in hits {
        let same_project = projects.get(&hit.unit) == Some(&decision.project);
        if hit.kind != CARD_KIND_DISTILLED || hit.unit == decision.card.unit || !same_project || hit.ts >= decision.card.ts {
            continue;
        }
        for old in decisions_of(&hit) {
            let theirs = crate::similarity::token_set(&old);
            let weights: HashMap<String, f64> = mine.union(&theirs).map(|token| (token.clone(), 1.0)).collect();
            let score = crate::similarity::cosine(
                &crate::similarity::vectorize(&mine, &weights),
                &crate::similarity::vectorize(&theirs, &weights),
            );
            if score >= MIN_OVERLAP {
                scored.push((score, hit.clone(), old));
            }
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(CANDIDATES_PER_DECISION)
        .map(|(_, old_card, old_text)| Pair {
            new_card: decision.card.clone(),
            new_text: decision.text.clone(),
            old_card,
            old_text,
        })
        .collect()
}

fn prompt(pairs: &[Pair]) -> String {
    pairs
        .iter()
        .enumerate()
        .map(|(i, pair)| {
            format!(
                "Пара {}:\nСтарое ({}): {}\nНовое ({}): {}",
                i + 1,
                format_date(pair.old_card.ts),
                pair.old_text,
                format_date(pair.new_card.ts),
                pair.new_text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Номера пар, которые модель назвала противоречием. Неразобранный ответ — ошибка:
/// иначе решения пометились бы сверенными без вердикта.
pub fn parse_verdicts(text: &str, count: usize) -> Result<HashSet<usize>, String> {
    let start = text.find('{').ok_or("в ответе модели нет JSON")?;
    let end = text.rfind('}').ok_or("в ответе модели нет JSON")?;
    let value: Value = serde_json::from_str(&text[start..=end]).map_err(|e| format!("ответ модели не JSON: {e}"))?;
    let list = value.get("verdicts").and_then(Value::as_array).ok_or("в ответе модели нет verdicts")?;
    Ok(list
        .iter()
        .filter_map(|item| {
            let pair = item.get("pair").and_then(Value::as_u64)? as usize;
            let verdict = item.get("verdict").and_then(Value::as_str)?.to_lowercase();
            let contradicts = verdict.starts_with("противореч") || verdict.starts_with("contradict");
            (contradicts && (1..=count).contains(&pair)).then_some(pair - 1)
        })
        .collect())
}

fn classify(db: &Database, client: &dyn ChatClient, pairs: &[Pair]) -> Result<usize, String> {
    let reply = client.chat(SYSTEM, &prompt(pairs)).map_err(|e| format!("провайдер: {e}"))?;
    let contradictions = parse_verdicts(&reply.text, pairs.len())?;
    // Расход виден в журнале обработки; сбой записи расхода вердикт не отменяет.
    let _ = record_reply_usage(db, client, &reply, pairs.len() as i64, Task::Distill);
    let now = now_ts();
    db.atomic(|| {
        let mut stmt = db.connection().prepare(
            "INSERT INTO decision_conflicts(new_card, new_text, old_card, old_text, status, created_at) \
             VALUES(?1, ?2, ?3, ?4, 'open', ?5) ON CONFLICT DO NOTHING",
        )?;
        for index in &contradictions {
            let pair = &pairs[*index];
            stmt.execute(params![pair.new_card.id, pair.new_text, pair.old_card.id, pair.old_text, now])?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    Ok(contradictions.len())
}

fn client_for(config: &Config, route: &Route) -> Result<(Box<dyn ChatClient>, Option<crate::local::JobGuard>), String> {
    let guard = if route.local { crate::local::job_guard(config, Task::Distill)? } else { None };
    let client = crate::llm::client_for(route, MAX_TOKENS).map_err(|e| e.to_string())?;
    Ok((Box::new(client), guard))
}

pub fn provider_configured(config: &Config) -> bool {
    config.route_for(Task::Distill, false).is_some()
}

/// Сверить до `limit` новых решений пачками по `PAIRS_PER_REQUEST` пар на маршрут. `projects` — unit → проект.
/// `chat` подменяет провайдера в тестах; None — настоящий маршрут из конфига.
pub fn run(db: &Database, config: &Config, projects: &HashMap<String, String>, chat: Option<&dyn ChatClient>) -> Value {
    let limit = config.insights.conflict_checks_per_sync as usize;
    if limit == 0 {
        return json!({"skipped": "disabled"});
    }
    let Some(remote) = config.route_for(Task::Distill, false).or_else(|| chat.map(|_| test_route())) else {
        return json!({"needs_provider": true});
    };
    let decisions = match pending(db, projects, limit) {
        Ok(decisions) => decisions,
        Err(error) => return json!({"error": error}),
    };
    let gate = LocalOnly::new(&config.privacy.local_only_patterns);
    let mut no_pairs: Vec<(i64, String)> = Vec::new();
    let mut public: Vec<Pair> = Vec::new();
    let mut private: Vec<Pair> = Vec::new();
    for decision in &decisions {
        let pairs = candidates(db, decision, projects);
        if pairs.is_empty() {
            no_pairs.push((decision.card.id, decision.text.clone()));
        }
        for pair in pairs {
            let local_only = !remote.local
                && (card_is_local_only(db, &pair.new_card, &gate).unwrap_or(true)
                    || card_is_local_only(db, &pair.old_card, &gate).unwrap_or(true));
            if local_only { private.push(pair) } else { public.push(pair) }
        }
    }
    let mut errors: Vec<String> = Vec::new();
    if let Err(error) = mark_checked(db, &no_pairs) {
        errors.push(error);
    }
    let mut found = 0;
    let mut skipped_private = 0;
    // Пары одного решения могут уйти в разные пачки и маршруты: решение помечается
    // сверенным, только если не упала ни одна из них, иначе непроверенная пара
    // больше никогда не попала бы в очередь.
    let mut all_keys: Vec<(i64, String)> = Vec::new();
    let mut failed: HashSet<(i64, String)> = HashSet::new();
    let local = config.local_route(Task::Distill);
    for (pairs, route) in [(public, Some(remote.clone())), (private, local)] {
        if pairs.is_empty() {
            continue;
        }
        all_keys.extend(pairs.iter().map(|pair| (pair.new_card.id, pair.new_text.clone())));
        let Some(route) = route else {
            // Приватную пару наружу не отправляем, а локальной модели нет: решение
            // помечается сверенным, чтобы не перебирать его каждую синхронизацию.
            skipped_private += pairs.len();
            continue;
        };
        let client = match chat {
            Some(_) => None,
            None => match client_for(config, &route) {
                Ok(client) => Some(client),
                Err(error) => {
                    errors.push(error);
                    failed.extend(pairs.iter().map(|pair| (pair.new_card.id, pair.new_text.clone())));
                    continue;
                }
            },
        };
        let client: &dyn ChatClient = match (chat, &client) {
            (Some(chat), _) => chat,
            (None, Some((client, _guard))) => client.as_ref(),
            (None, None) => continue,
        };
        for batch in pairs.chunks(PAIRS_PER_REQUEST) {
            match classify(db, client, batch) {
                Ok(count) => found += count,
                // Несверенные решения останутся в очереди до следующей синхронизации.
                Err(error) => {
                    errors.push(error);
                    failed.extend(batch.iter().map(|pair| (pair.new_card.id, pair.new_text.clone())));
                }
            }
        }
    }
    let mut seen = HashSet::new();
    let checked: Vec<(i64, String)> =
        all_keys.into_iter().filter(|key| !failed.contains(key) && seen.insert(key.clone())).collect();
    if let Err(error) = mark_checked(db, &checked) {
        errors.push(error);
    }
    json!({"decisions": decisions.len(), "contradictions": found, "skipped_private": skipped_private, "errors": errors})
}

fn test_route() -> Route {
    Route {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "test".into(),
        api_key: None,
        timeout_seconds: 1.0,
        json_mode: false,
        local: true,
        task: Task::Distill,
    }
}

// -- панель и MCP -------------------------------------------------------------------------

fn conflict_row(db: &Database, row: (i64, i64, String, i64, String, String)) -> Value {
    let (id, new_card, new_text, old_card, old_text, status) = row;
    let title_ts = |card: i64| -> (String, i64) {
        db.connection()
            .query_row("SELECT title, ts FROM cards WHERE id=?1", [card], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or_default()
    };
    let (new_title, new_ts) = title_ts(new_card);
    let (old_title, old_ts) = title_ts(old_card);
    json!({
        "id": id, "status": status,
        "new_card": new_card, "new_text": new_text, "new_title": new_title, "new_date": format_date(new_ts),
        "old_card": old_card, "old_text": old_text, "old_title": old_title, "old_date": format_date(old_ts),
    })
}

type Row = (i64, i64, String, i64, String, String);

fn rows(db: &Database, sql: &str, param: i64) -> Vec<Row> {
    let Ok(mut stmt) = db.connection().prepare(sql) else { return Vec::new() };
    stmt.query_map([param], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

const COLUMNS: &str = "id, new_card, new_text, old_card, old_text, status";

/// Расхождения, в которых участвует карточка: как новое (открытые) и как старое.
pub fn for_card(db: &Database, card_id: i64) -> Value {
    let open = rows(db, &format!("SELECT {COLUMNS} FROM decision_conflicts WHERE new_card=?1 AND status='open' ORDER BY id"), card_id);
    let superseded: Vec<String> = rows(
        db,
        &format!("SELECT {COLUMNS} FROM decision_conflicts WHERE old_card=?1 AND status='superseded' ORDER BY id"),
        card_id,
    )
    .into_iter()
    .map(|row| row.4)
    .collect();
    json!({"open": open.into_iter().map(|row| conflict_row(db, row)).collect::<Vec<_>>(), "superseded": superseded})
}

/// Открытые расхождения для панели, новые первыми.
pub fn open_list(db: &Database) -> Vec<Value> {
    rows(db, &format!("SELECT {COLUMNS} FROM decision_conflicts WHERE status='open' AND id>?1 ORDER BY id DESC LIMIT 200"), 0)
        .into_iter()
        .map(|row| conflict_row(db, row))
        .collect()
}

/// Решения, отменённые более новыми: (card_id, текст решения).
pub fn superseded_set(db: &Database) -> HashSet<(i64, String)> {
    let Ok(mut stmt) = db.connection().prepare("SELECT old_card, old_text FROM decision_conflicts WHERE status='superseded'") else {
        return HashSet::new();
    };
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()).unwrap_or_default()
}

/// Решение человека по расхождению: `superseded` — старое отменено (понижается в
/// поиске и уходит из брифинга), `dismissed` — не противоречие.
pub fn resolve(db: &Database, id: i64, action: &str) -> Result<bool, String> {
    if !matches!(action, "superseded" | "dismissed") {
        return Err("action must be superseded or dismissed".into());
    }
    let exists = db
        .connection()
        .query_row("SELECT 1 FROM decision_conflicts WHERE id=?1", [id], |_| Ok(()))
        .optional()
        .map_err(|e| e.to_string())?
        .is_some();
    if !exists {
        return Ok(false);
    }
    db.connection()
        .execute("UPDATE decision_conflicts SET status=?1 WHERE id=?2", params![action, id])
        .map_err(|e| e.to_string())?;
    db.bump_index().map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CardDraft;
    use crate::llm::{ChatReply, LlmError};

    struct Fixed(&'static str);
    impl ChatClient for Fixed {
        fn chat(&self, _system: &str, _user: &str) -> Result<ChatReply, LlmError> {
            Ok(ChatReply::text(self.0))
        }
        fn base_url(&self) -> &str {
            "http://127.0.0.1:9/v1"
        }
        fn model(&self) -> &str {
            "test"
        }
    }

    fn card(db: &Database, unit: &str, ts: i64, title: &str, atoms: &[&str]) -> Card {
        let body = crate::llm::render_card_body("кратко", &[], &atoms.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let mut raw = CardDraft::new("claude", &format!("{unit}:1"), ts, "фрагмент", "фрагмент");
        raw.unit = unit.to_owned();
        db.insert_card(&raw).unwrap();
        let mut draft = CardDraft::new("claude", &format!("llm:{unit}"), ts, title, &body).distilled();
        draft.unit = unit.to_owned();
        db.insert_card(&draft).unwrap();
        db.get_card_by_ref(&format!("llm:{unit}")).unwrap().unwrap()
    }

    #[test]
    fn superseded_decision_drops_in_search_and_needs_provider_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("m.db")).unwrap();
        let now = now_ts();
        let old = card(&db, "u-old", now - 86_400, "журнал sqlite", &["решение: журнал sqlite в режиме rollback"]);
        let projects = HashMap::from([("u-old".to_owned(), "p".to_owned()), ("u-new".to_owned(), "p".to_owned())]);

        // Без провайдера ничего не сверяется и не запоминается.
        assert_eq!(run(&db, &Config::default(), &projects, None)["needs_provider"], true);
        assert!(db.get_state(BASELINE_KEY).unwrap().is_none());

        // Первый запуск с провайдером только запоминает имеющиеся решения.
        let model = Fixed(r#"{"verdicts":[{"pair":1,"verdict":"противоречит"}]}"#);
        assert_eq!(run(&db, &Config::default(), &projects, Some(&model))["decisions"], 0);
        let new = card(&db, "u-new", now, "журнал sqlite", &["решение: журнал sqlite в режиме WAL вместо rollback"]);
        let result = run(&db, &Config::default(), &projects, Some(&model));
        assert_eq!(result["contradictions"], 1, "{result}");
        assert_eq!(run(&db, &Config::default(), &projects, Some(&model))["decisions"], 0, "повторно не сверяется");

        let listed = open_list(&db);
        assert_eq!(listed[0]["old_card"], old.id);
        assert_eq!(for_card(&db, new.id)["open"][0]["old_text"], "журнал sqlite в режиме rollback");

        let score = |id: i64| {
            db.search("журнал sqlite", 10, Some(now), &Filters::default()).unwrap().iter().find(|c| c.id == id).unwrap().score.unwrap()
        };
        let before = score(old.id);
        assert!(resolve(&db, listed[0]["id"].as_i64().unwrap(), "superseded").unwrap());
        assert!((score(old.id) - before * crate::db::SUPERSEDED_PENALTY).abs() < 1e-9, "отменённое решение понижено");
        assert_eq!(for_card(&db, old.id)["superseded"][0], "журнал sqlite в режиме rollback");
        assert!(superseded_set(&db).contains(&(old.id, "журнал sqlite в режиме rollback".to_owned())));
        assert!(open_list(&db).is_empty());
        assert!(resolve(&db, 1, "nope").is_err());
    }

    #[test]
    fn failed_request_keeps_the_decision_in_queue() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("m.db")).unwrap();
        let now = now_ts();
        let projects = HashMap::from([("u-old".to_owned(), "p".to_owned()), ("u-new".to_owned(), "p".to_owned())]);
        card(&db, "u-old", now - 86_400, "журнал sqlite", &["решение: журнал sqlite в режиме rollback"]);
        let model = Fixed(r#"{"verdicts":[]}"#);
        run(&db, &Config::default(), &projects, Some(&model));
        card(&db, "u-new", now, "журнал sqlite", &["решение: журнал sqlite в режиме WAL вместо rollback"]);

        // Обрезанный ответ не разбирается: решение не помечается сверенным.
        let truncated = Fixed(r#"{"verdicts":[{"pair":1,"verdict":"про"#);
        let result = run(&db, &Config::default(), &projects, Some(&truncated));
        assert_eq!(result["errors"].as_array().unwrap().len(), 1, "{result}");
        assert_eq!(run(&db, &Config::default(), &projects, Some(&model))["decisions"], 1, "решение осталось в очереди");
        assert_eq!(run(&db, &Config::default(), &projects, Some(&model))["decisions"], 0);
    }

    #[test]
    fn verdict_parser_keeps_only_contradictions() {
        let text = "ответ: {\"verdicts\":[{\"pair\":1,\"verdict\":\"уточняет\"},{\"pair\":2,\"verdict\":\"Противоречит\"},{\"pair\":9,\"verdict\":\"противоречит\"}]}";
        assert_eq!(parse_verdicts(text, 2).unwrap(), HashSet::from([1]));
        assert!(parse_verdicts("нет json", 1).is_err());
    }
}
