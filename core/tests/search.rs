//! Порт tests/test_search.py и поисковой части test_schema_v3.py: те же входы и ожидания.
//! Фикстуры ingest заменены прямой вставкой фрагментов с тем же текстом.

use memory_core::db::{CardDraft, Database, DbError, Filters};
use memory_core::text::{build_fts_query, query_stem, split_card_sections};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("m.db")).unwrap();
    (dir, db)
}

fn now() -> i64 {
    memory_core::db::now_ts()
}

fn ingest_like(db: &Database) {
    // Те же тексты, что дают синтетические сессии Kimi/Claude/Codex из conftest.py.
    let t = now() - 3600;
    db.insert_card(&CardDraft::new("kimi", "kimi:sess-1:agent-1:1", t, "Как настроить второй мозг для агентов?",
        "Как настроить второй мозг для агентов?\n\nЧерез SQLite FTS5 с BM25.\nИ freshness-полураспад 120 дней.")).unwrap();
    db.insert_card(&CardDraft::new("kimi", "kimi:sess-1:agent-1:2", t + 10, "А секреты?",
        "А секреты?\n\nМаскируются regex-редактированием до записи.")).unwrap();
    db.insert_card(&CardDraft::new("claude", "claude:s1:1", t + 20, "Индекс", "FTS5 external content и bm25")).unwrap();
}

fn refs(cards: &[memory_core::db::Card]) -> Vec<&str> {
    cards.iter().map(|c| c.source_ref.as_str()).collect()
}

#[test]
fn search_finds_by_keyword() {
    let (_d, db) = db();
    ingest_like(&db);
    let hits = db.search("FTS5", 5, None, &Filters::default()).unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().any(|h| h.body.contains("FTS5")));
    assert!(!db.search("второй мозг", 5, None, &Filters::default()).unwrap().is_empty());
}

#[test]
fn search_sanitizes_special_characters() {
    let (_d, db) = db();
    ingest_like(&db);
    // Синтаксическая ошибка FTS5 упала бы здесь; должен прийти обычный список.
    db.search("FTS5 AND (\"\" OR) * :", 5, None, &Filters::default()).unwrap();
}

#[test]
fn query_is_word_plus_base_plus_phrase() {
    assert_eq!(
        build_fts_query("ошибки клубного бота").unwrap(),
        "\"ошибки\" OR \"ошиб\"* OR \"клубного\" OR \"клубно\"* OR \"бота\" OR \"бот\"* OR \"ошибки клубного бота\""
    );
    assert_eq!(
        build_fts_query("второй \"мозг\"").unwrap(),
        "\"второй\" OR \"втор\"* OR \"мозг\" OR \"моз\"* OR \"второй мозг\""
    );
    assert_eq!(build_fts_query("   "), None);
    assert_eq!(build_fts_query(""), None);
    assert_eq!(build_fts_query("\" * :"), None);
}

#[test]
fn query_drops_stopwords_and_punctuation() {
    // «ошибки?» не теряет префиксный поиск; служебные слова не попадают в запрос.
    assert_eq!(build_fts_query("ошибки?").unwrap(), "\"ошибки\" OR \"ошиб\"*");
    assert_eq!(build_fts_query("что и как"), None);
    assert_eq!(build_fts_query("snake_case_name").unwrap(), "\"snake_case_name\"");
}

#[test]
fn query_stem_rules() {
    assert_eq!(query_stem("ошибки").as_deref(), Some("ошиб"));
    assert_eq!(query_stem("бота").as_deref(), Some("бот"));
    assert_eq!(query_stem("API"), None);
    assert_eq!(query_stem("v5.8"), None);
}

#[test]
fn base_finds_other_word_forms() {
    let (_d, db) = db();
    db.insert_card(&CardDraft::new("test", "test:1", now(), "Разбор", "одна ошибка клубного бота")).unwrap();
    let hits = db.search("ошибки клубного бота", 8, None, &Filters::default()).unwrap();
    assert_eq!(refs(&hits), vec!["test:1"]);
}

#[test]
fn field_weights_rank_title_over_summary_over_text() {
    let (_d, db) = db();
    let t = now();
    db.insert_card(&CardDraft::new("test", "test:text", t, "Другое", "упоминание редкослово в тексте")).unwrap();
    db.insert_card(
        &CardDraft::new("llm:test", "llm:test:summary", t, "Другое", "### Кратко\nредкослово в кратком\n\n### Атомы\n- факт: прочее")
            .distilled(),
    )
    .unwrap();
    db.insert_card(&CardDraft::new("test", "test:title", t, "редкослово в названии", "прочее")).unwrap();
    let hits = db.search("редкослово", 3, None, &Filters::default()).unwrap();
    assert_eq!(refs(&hits), vec!["test:title", "llm:test:summary", "test:text"]);
}

#[test]
fn freshness_ranking_prefers_recent() {
    let (_d, db) = db();
    ingest_like(&db);
    let t = now();
    db.insert_card(&CardDraft::new("test", "test:old", t - 360 * 86400, "memory report", "memory notes alpha")).unwrap();
    db.insert_card(&CardDraft::new("test", "test:fresh", t, "memory report", "memory notes alpha")).unwrap();
    let hits = db.search("memory report", 20, None, &Filters::default()).unwrap();
    assert_eq!(hits[0].source_ref, "test:fresh");
    let old = hits.iter().find(|h| h.source_ref == "test:old").unwrap();
    assert!(hits[0].score.unwrap() > old.score.unwrap());
}

#[test]
fn full_title_match_wins_over_long_old_record() {
    let (_d, db) = db();
    let t = now();
    db.insert_card(&CardDraft::new("test", "test:noise", t, "журнал", &"перенос чатов ".repeat(50))).unwrap();
    db.insert_card(&CardDraft::new("test", "test:named", t - 300 * 86400, "Перенос чатов", "коротко")).unwrap();
    let hits = db.search("перенос чатов", 2, None, &Filters::default()).unwrap();
    assert_eq!(hits[0].source_ref, "test:named");
}

#[test]
fn recent_filters_by_days() {
    let (_d, db) = db();
    ingest_like(&db);
    assert!(!db.recent(3650, 50, None, &Filters::default()).unwrap().is_empty());
    assert!(db.recent(0, 50, None, &Filters::default()).unwrap().is_empty());
}

#[test]
fn get_card() {
    let (_d, db) = db();
    ingest_like(&db);
    let hit = db.search("второй мозг", 1, None, &Filters::default()).unwrap().remove(0);
    let full = db.get_card(hit.id).unwrap().unwrap();
    assert_eq!(full.source_ref, hit.source_ref);
    assert!(db.get_card(999_999).unwrap().is_none());
}

#[test]
fn one_record_is_one_result_and_covered_fragments_fold() {
    let (_d, db) = db();
    let t = now();
    db.insert_card(&CardDraft::new("claude", "claude:s:1", t, "ход один", "уникслово первый ход")).unwrap();
    db.insert_card(&CardDraft::new("claude", "claude:s:2", t, "ход два", "уникслово второй ход")).unwrap();
    let hits = db.search("уникслово", 8, None, &Filters::default()).unwrap();
    assert_eq!(hits.len(), 1, "ходы одного разговора — один результат");

    // Карточка записи покрывает оба фрагмента: находится она, фрагменты скрыты.
    db.insert_card(&CardDraft::new("llm:claude", "llm:claude:s", t, "Итог", "### Кратко\nитог").distilled()).unwrap();
    let max_id: i64 = db.connection().query_row("SELECT MAX(id) FROM cards WHERE kind='raw'", [], |r| r.get(0)).unwrap();
    db.connection()
        .execute("INSERT INTO distill_queue(unit,covered_id) VALUES('claude:s',?)", [max_id])
        .unwrap();
    let hits = db.search("уникслово", 8, None, &Filters::default()).unwrap();
    assert_eq!(refs(&hits), vec!["llm:claude:s"], "карточка проиндексирована с текстом записи");
    let recent = db.recent(1, 10, None, &Filters::default()).unwrap();
    assert_eq!(refs(&recent), vec!["llm:claude:s"]);
}

#[test]
fn source_and_prefix_filters() {
    let (_d, db) = db();
    let t = now();
    db.insert_card(&CardDraft::new("git", "git:repo:abc", t, "фильтрслово", "x")).unwrap();
    db.insert_card(&CardDraft::new("claude", "claude:z:1", t, "фильтрслово", "y")).unwrap();
    let git = Filters { source: Some("git"), ..Filters::default() };
    assert_eq!(refs(&db.search("фильтрслово", 8, None, &git).unwrap()), vec!["git:repo:abc"]);
    let prefix = Filters { ref_prefix: Some("claude:"), ..Filters::default() };
    assert_eq!(refs(&db.recent(1, 8, None, &prefix).unwrap()), vec!["claude:z:1"]);
    // LIKE с case_sensitive_like: '_' и '%' в префиксе — буквально, регистр различается.
    let literal = Filters { ref_prefix: Some("CLAUDE:"), ..Filters::default() };
    assert!(db.recent(1, 8, None, &literal).unwrap().is_empty());
}

#[test]
fn duplicate_ref_is_not_inserted_twice() {
    let (_d, db) = db();
    let card = CardDraft::new("test", "test:dup", now(), "a", "b");
    assert!(db.insert_card(&card).unwrap());
    assert!(!db.insert_card(&card).unwrap());
    assert_eq!(db.count_cards().unwrap(), 1);
}

#[test]
fn card_sections_split_like_python() {
    let parts = split_card_sections("Вступление\n### Кратко\nсуть\n## Вопросы:\n- как?\n### Атомы\n- факт: x\n### Прочее\nхвост");
    assert_eq!(parts.summary, "суть");
    assert_eq!(parts.questions, "- как?");
    assert_eq!(parts.atoms, "- факт: x");
    assert_eq!(parts.rest, "Вступление\n### Прочее\nхвост");
}

#[test]
fn fresh_file_gets_schema_v5_and_v4_is_migrated() {
    let (dir, db) = db();
    assert_eq!(db.get_state("schema_version").unwrap().as_deref(), Some("5"));
    drop(db);

    let old = dir.path().join("old.db");
    let con = rusqlite::Connection::open(&old).unwrap();
    con.execute_batch(
        "CREATE TABLE cards(id INTEGER PRIMARY KEY, source TEXT, source_ref TEXT UNIQUE, ts INTEGER, title TEXT, body TEXT, created_at INTEGER);
         CREATE TABLE state(key TEXT PRIMARY KEY, value TEXT); INSERT INTO state VALUES('schema_version','3');",
    )
    .unwrap();
    drop(con);
    assert!(matches!(Database::open(&old), Err(DbError::UnsupportedSchema(3))));

    // v4: колонки task ещё нет, новых таблиц нет — открытие должно дополнить.
    let v4 = dir.path().join("v4.db");
    let con = rusqlite::Connection::open(&v4).unwrap();
    con.execute_batch(
        "CREATE TABLE cards(id INTEGER PRIMARY KEY, source TEXT NOT NULL, source_ref TEXT NOT NULL UNIQUE,
           ts INTEGER NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, created_at INTEGER NOT NULL,
           kind TEXT NOT NULL DEFAULT 'raw', keywords TEXT NOT NULL DEFAULT '', unit TEXT NOT NULL DEFAULT '');
         CREATE TABLE state(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO state VALUES('schema_version','4');
         CREATE TABLE usage(id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
           cards INTEGER NOT NULL DEFAULT 0, prompt_tokens INTEGER NOT NULL DEFAULT 0,
           completion_tokens INTEGER NOT NULL DEFAULT 0, cost_rub REAL NOT NULL DEFAULT 0);
         INSERT INTO usage(ts,provider,model) VALUES(1,'old','m');
         CREATE TABLE distill_queue(unit TEXT PRIMARY KEY, covered_id INTEGER NOT NULL DEFAULT 0,
           attempts INTEGER NOT NULL DEFAULT 0, attempt_id INTEGER NOT NULL DEFAULT 0,
           last_error TEXT NOT NULL DEFAULT '', held_id INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE search_history(id INTEGER PRIMARY KEY, qkey TEXT NOT NULL UNIQUE, q TEXT NOT NULL,
           ts INTEGER NOT NULL, answer TEXT NOT NULL DEFAULT '', card_ids TEXT NOT NULL DEFAULT '[]',
           memory_ref TEXT NOT NULL DEFAULT '');",
    )
    .unwrap();
    drop(con);
    let db = Database::open(&v4).unwrap();
    assert_eq!(db.get_state("schema_version").unwrap().as_deref(), Some("5"));
    // Индекс объявлен в SCHEMA: раньше его получала только база, созданная с нуля,
    // и на мигрированной выборка фрагментов по unit шла полным сканом cards.
    let index: String = db
        .connection()
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='cards' AND name='idx_cards_unit'",
            [],
            |r| r.get(0),
        )
        .expect("idx_cards_unit есть и на мигрированной базе");
    assert_eq!(index, "idx_cards_unit");
    db.insert_card(&CardDraft::new("test", "test:after-migration", now(), "t", "b")).unwrap();
    db.record_usage(&memory_core::db::UsageRun {
        ts: 2,
        provider: "new".into(),
        model: "m".into(),
        cards: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        cost_rub: 0.0,
        task: "distill".into(),
    })
    .unwrap();
    let run = db.recent_usage(5).unwrap().runs;
    assert_eq!(run.iter().map(|r| r.task.as_str()).collect::<Vec<_>>(), ["distill", ""]);
}
