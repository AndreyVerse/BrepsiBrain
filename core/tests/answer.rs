//! Этап 5: ответы с цитатами. Порт логики ответа из test_answer.py (без HTTP
//! панели — это этап 7) и scripts/test_llm_answer.py (CLI скилла).

mod support;

use std::sync::{Arc, Mutex};

use memory_core::answer::*;
use memory_core::config::{load_config_with_key, Config, LlmSettings, PrivacySettings};
use memory_core::db::{now_ts, CardDraft, Database};
use memory_core::llm::{ChatClient, ChatReply, LlmError};
use serde_json::json;
use support::{closed_port_base, completion, FakeProvider, DAY};

const LOCAL: &str = "http://127.0.0.1:9/v1";
const NONLOCAL: &str = "http://llm.gateway.test/v1";

struct Fixture {
    _dir: tempfile::TempDir,
    db: Database,
    ids: Vec<i64>,
    now: i64,
}

/// Карточки фикстуры `web` из test_answer.py.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("memory.db")).unwrap();
    let now = now_ts();
    let drafts = [
        CardDraft::new("kimi", "kimi:s1", now - DAY, "SQLite FTS5 search", "Поиск по памяти через SQLite FTS5 с ранжированием BM25"),
        CardDraft::new("git", "git:demo-repo:aaa111", now - 2 * DAY, "Poetry setup commit", "коммит про настройку poetry и pyproject в демо-репозитории"),
        CardDraft::new("git", "git:AcmeSecret:bbb222", now - DAY, "Acme roulette internals", "секретные детали рулетки Acme наружу нельзя"),
        CardDraft::new("obsidian", "obsidian:vault:Projects/plan.md", now - 3 * DAY, "Second brain plan", "план работы над вторым мозгом и дашбордом"),
        CardDraft::new("kimi", "kimi:s2", now - DAY, "Acme integration notes", "обычные заметки про интеграцию с Acme API без секретов"),
        CardDraft::new("kimi", "kimi:old", now - 10 * DAY, "Old SQLite note", "старая заметка про SQLite и журнал"),
    ];
    let ids = drafts
        .iter()
        .map(|d| {
            assert!(db.insert_card(d).unwrap());
            db.get_card_by_ref(&d.source_ref).unwrap().unwrap().id
        })
        .collect();
    Fixture { _dir: dir, db, ids, now }
}

/// Встроенного списка закрытых проектов нет: гейт проверяется на шаблоне из конфига.
fn provider(base_url: &str) -> Config {
    Config {
        llm: LlmSettings { base_url: Some(base_url.into()), model: Some("m1".into()), ..LlmSettings::default() },
        privacy: PrivacySettings { local_only_patterns: vec!["acme".into()] },
        ..Config::default()
    }
}

type Calls = Arc<Mutex<Vec<(String, String)>>>;

struct Recording {
    calls: Calls,
    reply: Result<ChatReply, LlmError>,
}

impl ChatClient for Recording {
    fn chat(&self, system: &str, user: &str) -> Result<ChatReply, LlmError> {
        self.calls.lock().unwrap().push((system.into(), user.into()));
        self.reply.clone()
    }

    fn base_url(&self) -> &str {
        "http://llm.gateway.test/v1"
    }

    fn model(&self) -> &str {
        "m1"
    }
}

fn ask(f: &Fixture, q: &str, history: &[HistoryTurn], config: Option<&Config>, reply: Result<ChatReply, LlmError>) -> (Answer, Vec<(String, String)>) {
    let calls: Calls = Arc::default();
    let factory_calls = Arc::clone(&calls);
    let make = move |_: &memory_core::config::Route| -> Result<Box<dyn ChatClient>, LlmError> {
        Ok(Box::new(Recording { calls: Arc::clone(&factory_calls), reply: reply.clone() }))
    };
    let result = answer(&f.db, q, DEFAULT_ANSWER_LIMIT, history, config, &make).unwrap();
    let calls = calls.lock().unwrap().clone();
    (result, calls)
}

fn ok(text: &str) -> Result<ChatReply, LlmError> {
    Ok(ChatReply::text(text))
}

fn ids(answer: &Answer) -> Vec<i64> {
    let mut ids: Vec<i64> = answer.cards.iter().map(|c| c.id).collect();
    ids.sort();
    ids
}

// -- деградация к карточкам ------------------------------------------------------------

#[test]
fn without_provider_returns_cards() {
    let f = fixture();
    let (result, calls) = ask(&f, "SQLite", &[], Some(&Config::default()), ok("x"));
    assert!(!result.used_llm() && calls.is_empty());
    assert_eq!(result.reason, Some(FallbackReason::NotConfigured));
    assert_eq!(ids(&result), [f.ids[0], f.ids[5]]);
    let (broken, calls) = ask(&f, "SQLite", &[], None, ok("x"));
    assert_eq!((broken.reason, calls.len()), (Some(FallbackReason::NotConfigured), 0));
}

#[test]
fn provider_failure_and_empty_reply_return_cards() {
    let f = fixture();
    let error = LlmError { message: "provider unreachable".into(), status: None };
    let (result, calls) = ask(&f, "SQLite", &[], Some(&provider(LOCAL)), Err(error));
    assert_eq!((result.answer.clone(), result.reason, calls.len()), (None, Some(FallbackReason::ProviderError), 1));
    assert_eq!(ids(&result), [f.ids[0], f.ids[5]]);
    let (empty, _) = ask(&f, "SQLite", &[], Some(&provider(LOCAL)), ok("   \n  "));
    assert_eq!((empty.used_llm(), empty.reason), (false, Some(FallbackReason::EmptyAnswer)));
}

#[test]
fn no_matching_cards_skips_provider() {
    let f = fixture();
    let (result, calls) = ask(&f, "несуществующееzzz", &[], Some(&provider(LOCAL)), ok("x"));
    assert_eq!((result.cards.len(), result.reason, calls.len()), (0, Some(FallbackReason::NoMatches), 0));
    assert_eq!(FallbackReason::NoMatches.as_str(), "no_matches");
}

// -- синтез и privacy-гейт ----------------------------------------------------------------

#[test]
fn synthesizes_via_local_provider() {
    let f = fixture();
    let (result, calls) = ask(&f, "Acme", &[], Some(&provider(LOCAL)), ok("Смотри [1] и [2]."));
    assert_eq!(result.answer.as_deref(), Some("Смотри [1] и [2]."));
    assert_eq!(ids(&result), [f.ids[2], f.ids[4]]);
    let [(system, user)] = calls.try_into().unwrap();
    assert!(system.contains("[n]") && system == ANSWER_SYSTEM_PROMPT);
    assert!(user.contains("Acme roulette internals") && user.contains("секретные детали рулетки"));
    assert!(user.starts_with("Question: Acme\n\nMemory cards:\n[1] (") && user.contains("\n\n[2] ("));
    assert!(user.contains("+00:00, git) Acme roulette internals\n"));
}

#[test]
fn withholds_local_only_from_nonlocal_provider() {
    let f = fixture();
    let (result, calls) = ask(&f, "Acme", &[], Some(&provider(NONLOCAL)), ok("Вот: [1]."));
    assert_eq!((result.used_llm(), result.reason, calls.len()), (false, Some(FallbackReason::LocalOnly), 0));
    assert_eq!(ids(&result), [f.ids[2], f.ids[4]]);
    let (result, calls) = ask(&f, "SQLite", &[], Some(&provider(NONLOCAL)), ok("Вот: [1]."));
    assert!(result.used_llm() && !calls[0].1.contains("Acme"));
    assert_eq!(result.cards.iter().map(|c| c.id).collect::<Vec<_>>(), [f.ids[0], f.ids[5]]);
    let (secret, calls) = ask(&f, "секретные", &[], Some(&provider(NONLOCAL)), ok("x"));
    assert_eq!((secret.used_llm(), calls.len(), ids(&secret)), (false, 0, vec![f.ids[2]]));
}

#[test]
fn followup_keeps_subject_and_source_cards() {
    let f = fixture();
    let history = [HistoryTurn { q: "Почему выбрали SQLite?".into(), card_ids: vec![f.ids[0]] }];
    let (result, calls) = ask(&f, "А почему именно его?", &history, Some(&provider(LOCAL)), ok("Ответ [1]."));
    assert!(result.used_llm());
    assert!(calls[0].1.starts_with("Previous questions (context only):\nПочему выбрали SQLite?\n\nQuestion: А почему именно его?"));
    assert!(result.cards.iter().any(|c| c.id == f.ids[0]));
    assert_eq!(result.query, "А почему именно его? Почему выбрали SQLite?");
}

#[test]
fn followup_does_not_send_private_context() {
    let f = fixture();
    let history = [HistoryTurn { q: "Acme internals".into(), card_ids: vec![f.ids[2]] }];
    let (result, calls) = ask(&f, "SQLite", &history, Some(&provider(NONLOCAL)), ok("Ответ [1]."));
    assert!(result.used_llm() && !calls[0].1.contains("Acme"));
}

#[test]
fn reads_fact_from_source_when_card_does_not_contain_it() {
    let f = fixture();
    f.db.insert_card(&CardDraft::new("claude", "claude:evidence:0", f.now, "Обсудили хранилище", &format!("{}МаркерX: решили использовать WAL.", "Введение. ".repeat(350))))
        .unwrap();
    let card = CardDraft::new("llm:claude", "llm:claude:evidence", f.now, "Хранилище", "### Кратко\nОбсуждали SQLite.").distilled();
    f.db.write_unit_card("claude:evidence", &card, 99_999).unwrap();
    let (result, calls) = ask(&f, "МаркерX", &[], Some(&provider(LOCAL)), ok("WAL [1]."));
    assert!(result.used_llm());
    assert!(calls[0].1.contains("Source excerpts:\n…") && calls[0].1.contains("решили использовать WAL"));
}

#[test]
fn withholds_private_transcript_even_when_summary_omits_it() {
    let f = fixture();
    f.db.insert_card(&CardDraft::new("claude", "claude:private:0", f.now, "SQLite", "Acme confidential source details")).unwrap();
    let card = CardDraft::new("llm:claude", "llm:claude:private", f.now, "SQLite", "### Кратко\nХранилище SQLite.").distilled();
    f.db.write_unit_card("claude:private", &card, 99_999).unwrap();
    let (result, calls) = ask(&f, "SQLite", &[], Some(&provider(NONLOCAL)), ok("x [1]."));
    assert!(result.used_llm() && !calls[0].1.contains("confidential"));
    let private_id = f.db.get_card_by_ref("llm:claude:private").unwrap().unwrap().id;
    assert!(result.cards.iter().all(|c| c.id != private_id));
}

#[test]
fn answer_usage_is_recorded_per_visible_card() {
    let f = fixture();
    let mut usage = serde_json::Map::new();
    usage.insert("prompt_tokens".into(), json!(40));
    usage.insert("completion_tokens".into(), json!(true)); // bool — не число токенов
    let (result, _) = ask(&f, "SQLite", &[], Some(&provider(LOCAL)), Ok(ChatReply { text: "Ответ [1].".into(), usage: Some(usage) }));
    assert!(result.used_llm() && result.usage_error.is_none());
    let report = f.db.recent_usage(5).unwrap();
    let run = &report.runs[0];
    assert_eq!((run.provider.as_str(), run.cards, run.prompt_tokens, run.completion_tokens), ("llm.gateway.test", 2, 40, 0));
}

#[test]
fn answer_through_real_client_on_fake_server() {
    let f = fixture();
    let server = FakeProvider::reply("Ответ [1].");
    let config = provider(&server.base);
    let result = answer(&f.db, "SQLite", DEFAULT_ANSWER_LIMIT, &[], Some(&config), &default_answer_client).unwrap();
    assert_eq!(result.answer.as_deref(), Some("Ответ [1]."));
    let payload = server.requests()[0].json();
    assert_eq!((payload["max_tokens"].clone(), payload.get("response_format")), (json!(800), None));
    assert_eq!(f.db.recent_usage(5).unwrap().runs[0].provider, "127.0.0.1");
}

#[test]
fn snippet_centers_on_first_hit() {
    let terms = vec!["маркер".to_string()];
    let body = format!("### Кратко\n{}маркер в конце", "слово ".repeat(100));
    let piece = snippet(&body, &terms, 40);
    assert!(piece.starts_with('…') && piece.contains("маркер") && !piece.contains("Кратко"));
    assert_eq!(snippet("коротко  и\nясно", &[], 160), "коротко и ясно");
    assert_eq!(snippet(&"а".repeat(200), &[], 10), format!("{}…", "а".repeat(9)));
}

// -- CLI скилла (scripts/llm_answer.py) ------------------------------------------------

const NOW: i64 = 1_761_000_000;

fn skill_db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("cards.db")).unwrap();
    db.insert_card(&CardDraft::new("kimi", "kimi:sess:agent:0", NOW - DAY, "roulette planning", "memory base notes about roulette modes")).unwrap();
    db.insert_card(&CardDraft::new("git", "git:AcmeGame:abc123", NOW - 300 * DAY, "roulette wheel fix", "prod roulette wheel segment fix")).unwrap();
    (dir, db)
}

fn skill_patterns() -> Vec<String> {
    vec!["acme".into()]
}

#[test]
fn skill_fallback_without_provider() {
    let (_d, db) = skill_db();
    let text = skill_answer(&db, "roulette", 5, None, &skill_patterns(), Some(NOW), 1.0).unwrap();
    assert!(text.starts_with("[fallback: no provider configured]") && text.contains("roulette planning"));
    let empty = skill_answer(&db, "   ", 5, None, &skill_patterns(), Some(NOW), 1.0).unwrap();
    assert_eq!(empty, "[fallback: no provider configured] no memories match the question.");
}

#[test]
fn skill_sends_only_visible_cards_to_remote_provider() {
    let (_d, db) = skill_db();
    let body = completion("SYNTH ANSWER");
    let server = FakeProvider::start_at("127.0.0.2", move |_, _| (200, body.clone()));
    let cfg = ProviderConfig { base_url: server.base.clone(), api_key: Some("secret-key".into()), model: Some("m".into()) };
    let text = skill_answer(&db, "roulette", 5, Some(&cfg), &skill_patterns(), Some(NOW), 5.0).unwrap();
    assert!(text.starts_with("SYNTH ANSWER

Sources: [") && text.ends_with("
[1 local-only card(s) withheld from a non-local provider]"));
    let request = &server.requests()[0];
    let body = request.json()["messages"][1]["content"].as_str().unwrap().to_string();
    assert!(body.contains("roulette planning") && !body.contains("roulette wheel fix"));
    assert_eq!(request.header("authorization"), Some("Bearer secret-key"));
}

#[test]
fn skill_sends_all_cards_to_local_provider() {
    let (_d, db) = skill_db();
    let server = FakeProvider::reply("LOCAL ANSWER");
    let cfg = ProviderConfig { base_url: server.base.clone(), api_key: None, model: Some("qwen3".into()) };
    let text = skill_answer(&db, "roulette", 5, Some(&cfg), &skill_patterns(), Some(NOW), 5.0).unwrap();
    assert!(text.starts_with("LOCAL ANSWER\n\nSources: [") && !text.contains("withheld"));
    let request = &server.requests()[0];
    assert_eq!(request.path, "/v1/chat/completions");
    let payload = request.json();
    assert_eq!((payload["model"].clone(), payload["max_tokens"].clone()), (json!("qwen3"), json!(800)));
    assert!(payload["messages"][1]["content"].as_str().unwrap().contains("roulette wheel fix"));
}

#[test]
fn skill_falls_back_when_provider_errors() {
    let (_d, db) = skill_db();
    let cfg = ProviderConfig { base_url: closed_port_base(), api_key: None, model: Some("qwen3".into()) };
    let text = skill_answer(&db, "roulette", 5, Some(&cfg), &skill_patterns(), Some(NOW), 2.0).unwrap();
    assert!(text.starts_with("[fallback: provider unreachable]") && text.contains("roulette planning"));
}

#[test]
fn skill_default_model_from_models_endpoint() {
    let server = FakeProvider::start(|request, _| {
        if request.method == "GET" && request.path == "/v1/models" {
            (200, r#"{"data": [{"id": "qwen3"}, {"id": "llama3"}]}"#.into())
        } else {
            (200, completion("x"))
        }
    });
    let cfg = ProviderConfig { base_url: server.base.clone(), api_key: None, model: None };
    assert_eq!(default_model(&cfg, 5.0).as_deref(), Some("qwen3"));
    let down = ProviderConfig { base_url: closed_port_base(), api_key: None, model: None };
    assert_eq!(default_model(&down, 2.0), None);
}

#[test]
fn skill_provider_and_patterns_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cfg.toml");
    std::fs::write(&path, "[llm]\nbase_url = \"http://127.0.0.1:1/v1\"\nmodel = \"file-model\"\napi_key = \"file-key\"\n[privacy]\nlocal_only_patterns = [\"roulette\"]\n").unwrap();
    let cfg = load_config_with_key(Some(&path), None).unwrap();
    let none = |_: &str| None;
    let got = load_provider_config(&none, &cfg).unwrap();
    assert_eq!((got.base_url.as_str(), got.model.as_deref(), got.api_key.as_deref()), ("http://127.0.0.1:1/v1", Some("file-model"), Some("file-key")));
    let env = |key: &str| match key {
        ENV_BASE_URL => Some("http://127.0.0.1:9999/v1/".to_string()),
        ENV_MODEL => Some("env-model".to_string()),
        "BREPSI_BRAIN_API_KEY" => Some("env-key".to_string()),
        ENV_LOCAL_ONLY => Some("^kimi:;;  ".to_string()),
        _ => None,
    };
    let got = load_provider_config(&env, &cfg).unwrap();
    assert_eq!((got.base_url.as_str(), got.model.as_deref(), got.api_key.as_deref()), ("http://127.0.0.1:9999/v1", Some("env-model"), Some("env-key")));
    assert_eq!(skill_local_only_patterns(&none, &cfg), ["roulette"]);
    assert_eq!(skill_local_only_patterns(&env, &cfg), ["^kimi:"]);
    assert!(skill_local_only_patterns(&none, &Config::default()).is_empty(), "без config.toml встроенных шаблонов нет");
    assert!(load_provider_config(&none, &Config { privacy: PrivacySettings::default(), ..Config::default() }).is_none());
}
