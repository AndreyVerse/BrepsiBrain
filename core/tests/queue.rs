//! Этап 3: запись, очередь пересказа, аренда, журнал расхода и история поиска.
//! Порт ожиданий test_schema_v3.py (update_card, usage), test_queue_recovery.py
//! (одна карточка на запись, свёртка, аренда) и истории из test_answer.py.

use memory_core::db::{CardDraft, Database, Filters, UsageRun};
use memory_core::history::{forget_history_item, history, history_item, memory_ref, question_key, remember_search};

const NOW: i64 = 1_790_000_000;
const LEASE: &str = "lease:distill";

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("m.db")).unwrap();
    (dir, db)
}

fn search(db: &Database, q: &str) -> Vec<memory_core::db::Card> {
    db.search(q, 10, Some(NOW), &Filters::default()).unwrap()
}

fn turn(db: &Database, session: &str, n: i64, text: &str) -> i64 {
    let draft = CardDraft::new("claude", &format!("claude:{session}:{n}"), NOW - 3600 + n, text, text);
    assert!(db.insert_card(&draft).unwrap());
    db.get_card_by_ref(&draft.source_ref).unwrap().unwrap().id
}

fn unit_card(title: &str, body: &str) -> CardDraft {
    let mut draft = CardDraft::new("llm:claude", "", NOW, title, body).distilled();
    draft.keywords = "индекс, поиск".into();
    draft
}

#[test]
fn update_card_keeps_keyword_fts_in_sync() {
    let (_d, db) = db();
    let mut draft = CardDraft::new("s", "s:1", 100, "oldword t", "oldword b");
    draft.keywords = "oldkw".into();
    db.insert_card(&draft).unwrap();
    let id = search(&db, "oldword")[0].id;
    assert!(db.update_card(id, 200, "newword t", "newword b", Some("newkw"), true).unwrap());
    assert!(search(&db, "oldkw").is_empty());
    assert_eq!(search(&db, "newkw")[0].id, id);
    // None оставляет прежние ключевые слова и их токены в индексе.
    assert!(db.update_card(id, 300, "newword t", "newword b", None, true).unwrap());
    assert_eq!(db.get_card(id).unwrap().unwrap().keywords, "newkw");
    assert!(!search(&db, "newkw").is_empty());
    assert!(!db.update_card(999, 1, "x", "x", None, true).unwrap());
}

#[test]
fn one_card_per_conversation_with_source_turns_kept_and_folded() {
    let (_d, db) = db();
    let first = turn(&db, "s1", 1, "как устроен индекс");
    let last = turn(&db, "s1", 2, "bm25 и свежесть");
    let [state] = db.unit_states().unwrap().try_into().unwrap();
    assert_eq!((state.unit.as_str(), state.fragments, state.max_id, state.covered_id), ("claude:s1", 2, last, 0));
    assert!(!state.done());

    assert!(db.write_unit_card("claude:s1", &unit_card("Индекс памяти", "### Кратко\nПоиск по BM25."), last).unwrap());
    let card = db.get_card_by_ref("llm:claude:s1").unwrap().unwrap();
    assert_eq!((card.kind.as_str(), card.unit.as_str()), ("distilled", "claude:s1"));
    assert!(db.unit_states().unwrap()[0].done());
    assert_eq!(db.unit_fragments("claude:s1").unwrap().iter().map(|c| c.id).collect::<Vec<_>>(), vec![first, last]);
    // Покрытые фрагменты свёрнуты в карточку: её находит и текст исходного хода.
    let found = search(&db, "свежесть");
    assert_eq!(found.iter().map(|c| c.source_ref.as_str()).collect::<Vec<_>>(), vec!["llm:claude:s1"]);
}

#[test]
fn resumed_conversation_is_redistilled_in_place() {
    let (_d, db) = db();
    let last = turn(&db, "s1", 1, "начало");
    db.write_unit_card("claude:s1", &unit_card("Первая", "### Кратко\nпервая версия"), last).unwrap();
    let id = db.get_card_by_ref("llm:claude:s1").unwrap().unwrap().id;
    let newer = turn(&db, "s1", 2, "продолжение");
    assert!(!db.unit_states().unwrap()[0].done());
    assert!(!db.write_unit_card("claude:s1", &unit_card("Вторая", "### Кратко\nвторая версия"), newer).unwrap());
    let card = db.get_card_by_ref("llm:claude:s1").unwrap().unwrap();
    assert_eq!((card.id, card.title.as_str()), (id, "Вторая"));
    assert!(search(&db, "первая").is_empty());
    assert!(db.unit_states().unwrap()[0].done());
}

#[test]
fn editing_a_fragment_or_deleting_the_card_requeues_the_unit() {
    let (_d, db) = db();
    let last = turn(&db, "s1", 1, "текст");
    db.write_unit_card("claude:s1", &unit_card("Карточка", "### Кратко\nx"), last).unwrap();
    assert!(db.update_card(last, NOW, "текст", "текст без шума", None, false).unwrap());
    assert!(db.unit_states().unwrap()[0].done(), "invalidate=false keeps the card current");
    assert!(db.update_card(last, NOW, "текст", "новый текст", None, true).unwrap());
    assert!(!db.unit_states().unwrap()[0].done());

    db.write_unit_card("claude:s1", &unit_card("Карточка", "### Кратко\nx"), last).unwrap();
    let id = db.get_card_by_ref("llm:claude:s1").unwrap().unwrap().id;
    assert!(db.delete_card(id).unwrap());
    assert!(db.get_card(id).unwrap().is_none());
    assert_eq!(db.unit_states().unwrap()[0].covered_id, 0);
    assert!(!db.delete_card(id).unwrap());
}

#[test]
fn legacy_turn_cards_are_retired_by_the_unit_card() {
    let (_d, db) = db();
    let last = turn(&db, "s1", 1, "ход");
    let mut legacy = CardDraft::new("llm:claude", "llm:claude:s1:1", NOW, "Старая карточка хода", "старое").distilled();
    legacy.unit = "claude:s1".into();
    db.insert_card(&legacy).unwrap();
    db.write_unit_card("claude:s1", &unit_card("Карточка записи", "### Кратко\nновое"), last).unwrap();
    assert!(db.get_card_by_ref("llm:claude:s1:1").unwrap().is_none());
    assert!(search(&db, "старое").is_empty());
}

#[test]
fn failed_attempts_count_per_fragment_set_and_holds_are_lifted_by_requeue() {
    let (_d, db) = db();
    let first = turn(&db, "s1", 1, "текст");
    db.mark_unit_failed("claude:s1", first, "bad reply").unwrap();
    db.mark_unit_failed("claude:s1", first, &"е".repeat(500)).unwrap();
    let state = &db.unit_states().unwrap()[0];
    assert_eq!((state.attempts, state.attempt_id), (2, first));
    assert_eq!(state.last_error.chars().count(), 300);
    let second = turn(&db, "s1", 2, "ещё");
    db.mark_unit_failed("claude:s1", second, "again").unwrap();
    assert_eq!(db.unit_states().unwrap()[0].attempts, 1, "new fragments start the count over");

    db.mark_unit_held("claude:s1", second).unwrap();
    assert_eq!(db.unit_states().unwrap()[0].held_id, second);
    db.requeue_unit("claude:s1").unwrap();
    let state = &db.unit_states().unwrap()[0];
    assert_eq!((state.attempts, state.held_id, state.last_error.as_str()), (0, 0, ""));
}

#[test]
fn lease_blocks_a_second_owner_until_it_expires() {
    let (dir, db) = db();
    let other = Database::open(dir.path().join("m.db")).unwrap();
    assert!(other.claim_lease(LEASE, "scheduler-1", 600, Some(NOW)).unwrap());
    assert!(!db.claim_lease(LEASE, "web-1", 600, Some(NOW + 599)).unwrap());
    assert!(other.claim_lease(LEASE, "scheduler-1", 600, Some(NOW + 300)).unwrap(), "the holder renews");
    assert!(db.claim_lease(LEASE, "web-1", 600, Some(NOW + 901)).unwrap());
    assert!(!other.claim_lease(LEASE, "scheduler-1", 600, Some(NOW + 902)).unwrap());
    other.release_lease(LEASE, "scheduler-1").unwrap();
    assert!(db.get_state(LEASE).unwrap().is_some(), "only the owner releases");
    db.release_lease(LEASE, "web-1").unwrap();
    assert!(db.get_state(LEASE).unwrap().is_none());
}

#[test]
fn recent_usage_returns_newest_runs_and_lifetime_totals() {
    let (_d, db) = db();
    let empty = db.recent_usage(50).unwrap();
    assert!(empty.runs.is_empty());
    assert_eq!((empty.totals.runs, empty.totals.prompt_tokens, empty.totals.cost_rub), (0, 0, 0.0));
    for (provider, model, cards, p, c, cost, ts) in [
        ("ollama", "m1", 3, 100, 50, 0.0, NOW - 200),
        ("gateway", "m2", 1, 300, 150, 1.5, NOW - 100),
        ("gateway", "m2", 2, 500, 250, 2.5, NOW),
    ] {
        db.record_usage(&UsageRun {
            ts,
            provider: provider.into(),
            model: model.into(),
            cards,
            prompt_tokens: p,
            completion_tokens: c,
            cost_rub: cost,
            task: "distill".into(),
        })
        .unwrap();
    }
    let usage = db.recent_usage(2).unwrap();
    assert_eq!(usage.runs.iter().map(|r| r.ts).collect::<Vec<_>>(), vec![NOW, NOW - 100]);
    assert_eq!((usage.runs[0].cards, usage.runs[0].prompt_tokens, usage.runs[0].cost_rub), (2, 500, 2.5));
    assert_eq!(
        (usage.totals.runs, usage.totals.prompt_tokens, usage.totals.completion_tokens, usage.totals.cost_rub),
        (3, 900, 450, 4.0)
    );
}

#[test]
fn question_key_ignores_case_and_spacing() {
    assert_eq!(question_key("Почему SQLite?"), question_key("  почему   sqlite? "));
    assert_eq!(question_key("Почему SQLite?").len(), 16);
    assert!(memory_ref("x").starts_with("llm:search:"));
}

#[test]
fn answer_lands_in_history_and_becomes_memory() {
    let (_d, db) = db();
    turn(&db, "s1", 1, "SQLite FTS5 search");
    let source = search(&db, "SQLite");
    let card_ref = remember_search(&db, "Почему SQLite?", Some("Выбрали SQLite FTS5 ради BM25 [1]."), &source, Some(NOW))
        .unwrap()
        .unwrap();
    let [entry] = history(&db, 30).unwrap().try_into().unwrap();
    assert_eq!((entry.q.as_str(), entry.answered, entry.cards), ("Почему SQLite?", true, 1));
    let (item, cards) = history_item(&db, entry.id).unwrap().unwrap();
    assert_eq!(item.answer.as_deref(), Some("Выбрали SQLite FTS5 ради BM25 [1]."));
    assert_eq!((item.memory_ref.as_str(), cards[0].id), (card_ref.as_str(), source[0].id));
    assert!(search(&db, "ради BM25").iter().any(|c| c.source == "llm:search"));
    // Приватность наследуется: scope карточки памяти перечисляет refs источников.
    let unit = card_ref.strip_prefix("llm:").unwrap();
    assert_eq!(db.get_state(&format!("scope:{unit}")).unwrap().as_deref(), Some("claude:s1:1"));
}

#[test]
fn repeated_question_updates_one_entry_and_one_card_without_citing_itself() {
    let (_d, db) = db();
    turn(&db, "s1", 1, "SQLite FTS5 search");
    let source = search(&db, "SQLite");
    remember_search(&db, "Почему SQLite?", Some("Первый ответ [1]."), &source, Some(NOW)).unwrap();
    let again = search(&db, "SQLite");
    assert!(again.iter().any(|c| c.source == "llm:search"));
    remember_search(&db, "  почему   sqlite? ", Some("Второй ответ [1]."), &again, Some(NOW + 10)).unwrap();
    assert_eq!(history(&db, 30).unwrap().len(), 1);
    let rows: Vec<String> = db
        .connection()
        .prepare("SELECT body FROM cards WHERE source='llm:search'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains("Второй ответ"));
    assert!(!rows[0].split("### Источники").nth(1).unwrap().contains("Почему"));
}

#[test]
fn search_without_answer_still_grows_memory_and_empty_search_is_not_remembered() {
    let (_d, db) = db();
    turn(&db, "s1", 1, "SQLite FTS5 search");
    let source = search(&db, "SQLite");
    remember_search(&db, "SQLite", None, &source, Some(NOW)).unwrap();
    let body: String = db
        .connection()
        .query_row("SELECT body FROM cards WHERE source='llm:search'", [], |r| r.get(0))
        .unwrap();
    assert!(body.contains("По запросу нашлось записей: 1. SQLite FTS5 search"));
    assert_eq!(remember_search(&db, "квантовая гравитация", None, &[], Some(NOW)).unwrap(), None);
    assert_eq!(history(&db, 30).unwrap().len(), 1);
}

#[test]
fn forgetting_history_keeps_the_memory_card() {
    let (_d, db) = db();
    turn(&db, "s1", 1, "SQLite FTS5 search");
    let source = search(&db, "SQLite");
    let card_ref = remember_search(&db, "SQLite", None, &source, Some(NOW)).unwrap().unwrap();
    let id = history(&db, 30).unwrap()[0].id;
    assert!(forget_history_item(&db, id).unwrap());
    assert!(!forget_history_item(&db, id).unwrap());
    assert!(history(&db, 30).unwrap().is_empty());
    assert!(db.get_card_by_ref(&card_ref).unwrap().is_some());
    assert!(history_item(&db, id).unwrap().is_none());
}
