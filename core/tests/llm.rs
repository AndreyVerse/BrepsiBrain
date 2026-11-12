//! Этап 5: клиент провайдера, разбор ответов модели, приватность и пересказ.
//! Порт ожиданий test_llm_writer.py и test_llm_config.py; HTTP — настоящий,
//! на подменном сервере (support::FakeProvider).

mod support;

use std::sync::{Arc, Mutex};

use memory_core::config::{load_config_with_key, Config, LlmSettings, PrivacySettings};
use memory_core::db::{CardDraft, Database};
use memory_core::llm::*;
use serde_json::json;
use support::{closed_port_base, completion, rejection, seed_sources, FakeProvider, DAY, NOW};

const GOOD_REPLY: &str = r#"{"title": "Дистиллят заметки", "keywords": "второй мозг, sqlite, fts5", "body": "Суть в двух словах."}"#;

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("m.db")).unwrap();
    (dir, db)
}

fn config(base_url: Option<&str>, patterns: &[&str], batch_size: usize) -> Config {
    Config {
        llm: LlmSettings { base_url: base_url.map(String::from), model: base_url.map(|_| "m".into()), batch_size, ..LlmSettings::default() },
        privacy: PrivacySettings { local_only_patterns: patterns.iter().map(|p| p.to_string()).collect() },
        ..Config::default()
    }
}

fn local() -> Config {
    config(Some("http://127.0.0.1:11434/v1"), &[], 50)
}

/// Сценарный провайдер без сети; записывает запросы.
struct FakeClient {
    replies: Mutex<Vec<Result<String, LlmError>>>,
    requests: Mutex<Vec<String>>,
    /// Ответ без конца — для параллельной догонки.
    fixed: Option<Result<String, LlmError>>,
}

impl FakeClient {
    fn new(replies: Vec<&str>) -> Self {
        FakeClient { replies: Mutex::new(replies.into_iter().rev().map(|r| Ok(r.to_string())).collect()), requests: Mutex::default(), fixed: None }
    }

    fn fixed(reply: Result<&str, LlmError>) -> Self {
        FakeClient { replies: Mutex::default(), requests: Mutex::default(), fixed: Some(reply.map(str::to_string)) }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl ChatClient for FakeClient {
    fn chat(&self, _system: &str, user: &str) -> Result<ChatReply, LlmError> {
        self.requests.lock().unwrap().push(user.to_string());
        let reply = match &self.fixed {
            Some(fixed) => fixed.clone(),
            None => self.replies.lock().unwrap().pop().expect("no scripted reply left"),
        };
        reply.map(ChatReply::text)
    }

    fn base_url(&self) -> &str {
        "http://127.0.0.1:11434/v1"
    }

    fn model(&self) -> &str {
        "m"
    }
}

fn transport_error() -> LlmError {
    LlmError { message: "boom".into(), status: None }
}

fn distilled(db: &Database) -> Vec<(String, String, String, String, String, String)> {
    let mut stmt = db
        .connection()
        .prepare("SELECT source, source_ref, unit, title, keywords, body FROM cards WHERE kind='distilled' ORDER BY id")
        .unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

fn distill(db: &Database, config: &Config, client: &dyn ChatClient) -> DistillStats {
    distill_pending(db, config, Some(client), None, 1, Some(NOW)).unwrap()
}

fn no_sleep(log: &Arc<Mutex<Vec<f64>>>) -> Sleeper {
    let log = Arc::clone(log);
    Box::new(move |s| log.lock().unwrap().push(s))
}

fn client(base: &str, sleeps: &Arc<Mutex<Vec<f64>>>) -> OpenAiClient {
    OpenAiClient::new(base, "m", None, 5.0, DEFAULT_MAX_TOKENS, false).unwrap().with_sleeper(no_sleep(sleeps))
}

// -- config.toml -----------------------------------------------------------------------

#[test]
fn missing_config_file_yields_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nope.toml");
    let cfg = load_config_with_key(Some(&path), None).unwrap();
    // Путь запоминается и без файла: настройки из приложения пишутся туда же,
    // откуда читаются, а локальный маршрут знает свой корень.
    assert_eq!(cfg.path.as_deref(), Some(path.as_path()));
    assert!(!cfg.llm.configured());
    assert_eq!((cfg.llm.api_key.as_deref(), cfg.llm.base_url.as_deref(), cfg.llm.batch_size), (None, None, 20));
    assert!(cfg.privacy.local_only_patterns.is_empty(), "встроенных шаблонов приватности нет");
}

#[test]
fn explicit_toml_and_env_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cfg.toml");
    std::fs::write(
        &path,
        "[llm]\nbase_url = \"http://127.0.0.1:1234/v1\"\nmodel = \"local-model\"\nbatch_size = 3\napi_key = \"file-key\"\n\
         fallback_model = \"\"\n[privacy]\nlocal_only_patterns = [\"secret-project\"]\n",
    )
    .unwrap();
    let cfg = load_config_with_key(Some(&path), None).unwrap();
    assert_eq!(cfg.path.as_deref(), Some(path.as_path()));
    assert!(cfg.llm.configured());
    assert_eq!(cfg.llm.base_url.as_deref(), Some("http://127.0.0.1:1234/v1"));
    assert_eq!((cfg.llm.model.as_deref(), cfg.llm.batch_size, cfg.llm.timeout_seconds), (Some("local-model"), 3, 60.0));
    assert_eq!(cfg.llm.fallback_model, None);
    assert_eq!(cfg.privacy.local_only_patterns, ["secret-project"]);
    assert_eq!(cfg.llm.api_key.as_deref(), Some("file-key"));
    assert_eq!(load_config_with_key(Some(&path), Some("env-key".into())).unwrap().llm.api_key.as_deref(), Some("env-key"));
}

#[test]
fn malformed_or_mistyped_config_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cfg.toml");
    for text in ["[llm\nbroken", "[privacy]\nlocal_only_patterns = [1, 2]\n", "[obsidian]\nlocal_only_patterns = [\"ok\", 3]\n"] {
        std::fs::write(&path, text).unwrap();
        assert!(load_config_with_key(Some(&path), None).is_err(), "{text}");
    }
}

// -- endpoint и приватность ------------------------------------------------------------

#[test]
fn local_and_remote_endpoints() {
    for url in ["http://127.0.0.1:11434/v1", "http://localhost:1234/v1", "http://[::1]:8080/v1", "http://0.0.0.0:8000/v1"] {
        assert!(is_local_endpoint(url), "{url}");
    }
    for url in ["http://example.com/v1", "https://api.openai.com/v1", "http://192.168.1.10:11434/v1", "garbage"] {
        assert!(!is_local_endpoint(url), "{url}");
    }
}

#[test]
fn local_only_ref_matching() {
    let p = ["acme".to_string()];
    assert!(is_local_only_ref("git:AcmePlugin:abc123", &p));
    assert!(is_local_only_ref("kimi:wd-acme-x:s1:a1:0", &p));
    assert!(!is_local_only_ref("git:memory-base:abc123", &p));
    assert!(!is_local_only_ref("kimi:sess:agent:1", &p));
    // Python-синтаксис с просмотром вперёд; битый шаблон держит текст дома.
    assert!(is_local_only_ref("git:Lucky:1", &["^git:(?=Lucky)".to_string()]));
    assert!(is_local_only_ref("anything", &["(unclosed".to_string()]));
}

// -- разбор ответа модели ----------------------------------------------------------------

fn parsed(reply: &str) -> (String, String, String) {
    parse_card_response(reply).unwrap()
}

#[test]
fn parse_card_response_variants() {
    let good = ("Дистиллят заметки".to_string(), "второй мозг, sqlite, fts5".to_string(), "Суть в двух словах.".to_string());
    assert_eq!(parsed(GOOD_REPLY), good);
    assert_eq!(parsed(&format!("```json\n{GOOD_REPLY}\n```")), good);
    assert_eq!(parsed(r#"{"title": "t", "body": "b"}"#), ("t".into(), "".into(), "b".into()));
    assert_eq!(parsed(r#"{"title": "t", "keywords": ["bm25", " поиск ", ""], "body": "b"}"#).1, "bm25, поиск");
    assert_eq!(parsed(r#"{"title": "t", "keywords": 42, "body": "b"}"#).1, "");
}

#[test]
fn parse_card_response_structured_and_repaired() {
    let reply = "Вот: {\"title\": \" Индекс \", \"summary\": \"Суть\nв две строки.\", \"questions\": \"- Как?\\n* Почему?\", \
                 \"atoms\": [\"факт:  путь   C:\\Git\", 7, true]} и ещё текст";
    let (title, keywords, body) = parsed(reply);
    assert_eq!((title.as_str(), keywords.as_str()), ("Индекс", ""));
    assert_eq!(body, "### Кратко\nСуть\nв две строки.\n\n### Вопросы\n- Как?\n- Почему?\n\n### Атомы\n- факт: путь C:\\Git\n- 7\n- True");
}

#[test]
fn parse_card_response_rejects_garbage() {
    for reply in ["not json at all", "[1, 2]", r#"{"title": "only title"}"#, r#"{"title": "  ", "body": "x"}"#, r#"{"title": "x", "body": 5}"#] {
        assert!(parse_card_response(reply).is_err(), "{reply}");
    }
    assert!(parse_card_response("garbage").unwrap_err().contains("not valid JSON"));
}

#[test]
fn clean_card_body_cases() {
    assert_eq!(clean_card_body("первая строка   \n\n\n\n\nвторая строка\t\n\n\nтретья", ""), "первая строка\n\nвторая строка\n\nтретья");
    let chatter = "Вот карточка по твоей заметке:\nSure, here is the distilled card.\nКонечно!\n### Кратко\nСуть без воды.";
    assert_eq!(clean_card_body(chatter, ""), "### Кратко\nСуть без воды.");
    let long = "Вводная мысль длиннее шестидесяти символов, которую сохраняем целиком.\n### Кратко\nСуть.";
    assert!(clean_card_body(long, "").starts_with("Вводная мысль"));
    let trailing = "### Кратко\nСуть.\n\nНадеюсь, это поможет.\nПишите, если нужно что-то ещё.\nLet me know!";
    assert_eq!(clean_card_body(trailing, ""), "### Кратко\nСуть.");
    assert_eq!(clean_card_body("Дистиллят заметки\n### Кратко\nСуть.", "Дистиллят заметки"), "### Кратко\nСуть.");
    assert_eq!(clean_card_body("# Дистиллят заметки\n### Кратко\nСуть.", "Дистиллят заметки"), "### Кратко\nСуть.");
    assert!(clean_card_body("Другая первая строка\n### Кратко\nСуть.", "Дистиллят заметки").starts_with("Другая первая строка"));
}

// -- HTTP-клиент на подменном сервере --------------------------------------------------

#[test]
fn client_request_shape() {
    let server = FakeProvider::reply(GOOD_REPLY);
    let client = OpenAiClient::new(&format!("{}/", server.base), "m", Some("k"), 7.0, DEFAULT_MAX_TOKENS, true).unwrap();
    assert_eq!(client.chat("sys", "usr").unwrap().text, GOOD_REPLY);
    let [request] = server.requests().try_into().unwrap();
    assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/v1/chat/completions"));
    assert_eq!(request.header("authorization"), Some("Bearer k"));
    let payload = request.json();
    assert_eq!(payload["model"], "m");
    assert_eq!(payload["messages"], json!([{"role": "system", "content": "sys"}, {"role": "user", "content": "usr"}]));
    assert_eq!((payload["temperature"].clone(), payload["max_tokens"].clone()), (json!(0.2), json!(800)));
    assert_eq!(payload["response_format"], json!({"type": "json_object"}));
}

#[test]
fn client_adapts_to_a_reasoning_model() {
    // Ответы openai/gpt-6-luna через шлюз Timeweb, 04.11.2026.
    let server = FakeProvider::start(|request, _| {
        let payload = request.json();
        if payload.get("max_tokens").is_some() {
            return (400, rejection("Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead."));
        }
        if payload.get("temperature").is_some() {
            return (400, rejection("Unsupported value: 'temperature' does not support 0.2 with this model. Only the default (1) value is supported."));
        }
        (200, completion(GOOD_REPLY))
    });
    let client = OpenAiClient::new(&server.base, "m", None, 5.0, 1500, false).unwrap();
    assert_eq!(client.chat("s", "u").unwrap().text, GOOD_REPLY);
    let sent = server.requests();
    assert_eq!(sent.len(), 3);
    let last = sent[2].json();
    assert_eq!(last["max_completion_tokens"], json!(1500 + REASONING_HEADROOM_TOKENS));
    assert!(last.get("temperature").is_none());
    client.chat("s", "u").unwrap();
    assert_eq!(server.requests().len(), 4, "выученная форма переиспользуется");
}

#[test]
fn client_does_not_resend_other_rejections() {
    let server = FakeProvider::start(|_, _| (400, rejection("context length exceeded")));
    let error = OpenAiClient::new(&server.base, "m", None, 5.0, 800, false).unwrap().chat("s", "u").unwrap_err();
    assert_eq!((error.message.as_str(), error.status), ("LLM endpoint returned HTTP 400: context length exceeded", Some(400)));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn client_without_key_sends_no_auth_and_keeps_usage() {
    let body = json!({"choices": [{"message": {"content": "x"}}], "usage": {"prompt_tokens": 12, "completion_tokens": 3}}).to_string();
    let server = FakeProvider::start(move |_, _| (200, body.clone()));
    let reply = client(&server.base, &Arc::default()).chat("s", "u").unwrap();
    assert_eq!(reply.usage.unwrap()["prompt_tokens"], 12);
    assert_eq!(server.requests()[0].header("authorization"), None);
}

#[test]
fn client_http_error_after_retries() {
    let sleeps = Arc::default();
    let server = FakeProvider::start(|_, _| (500, "oops".into()));
    let error = client(&server.base, &sleeps).chat("s", "u").unwrap_err();
    assert!(error.message.starts_with("LLM endpoint returned HTTP 500"), "{error}");
    assert_eq!(*sleeps.lock().unwrap(), [5.0, 15.0]);
    assert_eq!(server.requests().len(), 3);
}

#[test]
fn client_connection_error() {
    let sleeps = Arc::default();
    let error = client(&closed_port_base(), &sleeps).chat("s", "u").unwrap_err();
    assert!(error.message.starts_with("cannot reach LLM endpoint"), "{error}");
    assert_eq!(error.status, None);
    assert_eq!(sleeps.lock().unwrap().len(), 2);
}

#[test]
fn client_bad_response_shape() {
    let server = FakeProvider::start(|_, _| (200, r#"{"unexpected": 1}"#.into()));
    assert!(client(&server.base, &Arc::default()).chat("s", "u").unwrap_err().message.contains("response shape"));
}

#[test]
fn client_requires_configuration() {
    assert!(OpenAiClient::new("", "", None, 5.0, 800, false).is_err());
}

#[test]
fn retries_transient_404_then_succeeds() {
    let sleeps = Arc::default();
    let server = FakeProvider::start(|_, n| if n == 0 { (404, "{}".into()) } else { (200, completion(GOOD_REPLY)) });
    assert_eq!(client(&server.base, &sleeps).chat("s", "u").unwrap().text, GOOD_REPLY);
    assert_eq!(server.requests().len(), 2);
    assert_eq!(*sleeps.lock().unwrap(), [5.0]);
}

#[test]
fn gives_up_after_max_attempts_and_skips_auth_errors() {
    let sleeps = Arc::default();
    let server = FakeProvider::start(|_, _| (404, "{}".into()));
    assert!(client(&server.base, &sleeps).chat("s", "u").unwrap_err().message.contains("HTTP 404"));
    assert_eq!(*sleeps.lock().unwrap(), [5.0, 15.0]);

    let sleeps = Arc::default();
    let server = FakeProvider::start(|_, _| (401, "{}".into()));
    assert!(client(&server.base, &sleeps).chat("s", "u").unwrap_err().message.contains("HTTP 401"));
    assert!(sleeps.lock().unwrap().is_empty());
}

#[test]
fn list_models_parses_sorts_and_reports_errors() {
    let server = FakeProvider::start(|_, _| (200, r#"{"data": [{"id": "b"}, {"id": "a"}, {"object": "model"}]}"#.into()));
    assert_eq!(list_models(&format!("{}/", server.base), Some("k"), 5.0).unwrap(), ["a", "b"]);
    let [request] = server.requests().try_into().unwrap();
    assert_eq!((request.method.as_str(), request.path.as_str(), request.header("authorization")), ("GET", "/v1/models", Some("Bearer k")));

    let server = FakeProvider::start(|_, _| (401, "{}".into()));
    let error = list_models(&server.base, None, 5.0).unwrap_err();
    assert!(error.message.contains("HTTP 401") && server.requests()[0].header("authorization").is_none());
}

// -- пересказ очереди ------------------------------------------------------------------

#[test]
fn distill_unconfigured_is_noop() {
    let (_d, db) = db();
    seed_sources(&db);
    let stats = distill(&db, &config(None, &[], 50), &FakeClient::new(vec![]));
    assert!(!stats.configured && stats.sent == 0 && stats.written == 0);
    assert_eq!(db.count_cards().unwrap(), 8);
    assert_eq!(format_stats(&stats), "llm: provider not configured, skipping distillation");
}

#[test]
fn distill_writes_one_card_per_record() {
    let (_d, db) = db();
    seed_sources(&db);
    let client = FakeClient::new(vec![GOOD_REPLY; 5]);
    let stats = distill(&db, &local(), &client);
    assert_eq!((stats.sent, stats.written, stats.failed), (5, 5, 0));
    assert_eq!(format_stats(&stats), "llm: distilled 5/5 records");
    let rows = distilled(&db);
    let sources: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(sources.into_iter().collect::<Vec<_>>(), ["llm:claude", "llm:codex", "llm:git", "llm:kimi"]);
    let kimi = rows.iter().find(|r| r.0 == "llm:kimi").unwrap();
    assert_eq!((kimi.1.as_str(), kimi.2.as_str()), ("llm:kimi:wd1:s1:a1", "kimi:wd1:s1:a1"));
    assert_eq!((kimi.3.as_str(), kimi.4.as_str()), ("Дистиллят заметки", "второй мозг, sqlite, fts5"));
    let kimi_prompt = client.requests().into_iter().find(|p| p.contains("разговор в kimi")).unwrap();
    assert!(kimi_prompt.contains("второй мозг") && kimi_prompt.contains("А секреты?"));
    let git: Vec<String> = client.requests().into_iter().filter(|p| p.contains("день коммитов в репозитории demo-repo, коммитов: 1")).collect();
    assert_eq!(git.len(), 2);
    assert!(git.iter().any(|p| p.contains("\n\n- feat: поиск\n\n")));
    assert!(git.iter().any(|p| p.contains("\n\n- fix: свежесть\n  fix: свежесть\n\nполураспад 120 дней\n\n")));

    let again = distill(&db, &local(), &FakeClient::new(vec![GOOD_REPLY]));
    assert_eq!((again.candidates, again.sent, again.written), (0, 0, 0));
    assert_eq!(db.count_cards().unwrap(), 13);
}

#[test]
fn distill_cleans_and_masks_before_storing() {
    let (_d, db) = db();
    seed_sources(&db);
    let dirty = r#"{"title": "Чистая карточка", "keywords": "чистка, sk-test-not-a-real-key", "body": "Вот карточка:\n\n\n### Кратко\nСуть без воды.\n\nНадеюсь, поможет."}"#;
    let stats = distill(&db, &config(Some("http://127.0.0.1:11434/v1"), &[], 1), &FakeClient::new(vec![dirty]));
    assert_eq!(stats.written, 1);
    let row = &distilled(&db)[0];
    assert_eq!((row.5.as_str(), row.4.as_str()), ("### Кратко\nСуть без воды.", "чистка, [REDACTED:api-key]"));
}

#[test]
fn distill_rerun_after_queue_reset_rewrites_in_place() {
    let (_d, db) = db();
    seed_sources(&db);
    distill(&db, &local(), &FakeClient::new(vec![GOOD_REPLY; 5]));
    db.connection().execute("DELETE FROM distill_queue", []).unwrap();
    let stats = distill(&db, &local(), &FakeClient::new(vec![GOOD_REPLY; 5]));
    assert_eq!((stats.sent, stats.written, distilled(&db).len()), (5, 5, 5));
}

#[test]
fn distill_privacy_gate_holds_for_remote_provider() {
    let (_d, db) = db();
    db.insert_card(&CardDraft::new("git", "git:memory-base:aaa", 1, "normal", "normal card")).unwrap();
    db.insert_card(&CardDraft::new("git", "git:AcmePlugin:bbb", 2, "acme secret", "confidential acme text")).unwrap();
    let remote = config(Some("http://example.com/v1"), &["acme"], 50);
    let client = FakeClient::new(vec![GOOD_REPLY]);
    let stats = distill(&db, &remote, &client);
    assert_eq!((stats.sent, stats.skipped_local_only, stats.written), (1, 1, 1));
    assert!(client.requests().iter().all(|p| !p.contains("confidential acme text")));
    assert_eq!(distill(&db, &remote, &FakeClient::new(vec![])).skipped_local_only, 0, "отложенная запись не судится заново");
    let status = queue_status(&db, Some(&remote), Some(NOW)).unwrap();
    assert_eq!((status.records, status.done, status.held_local), (2, 1, 1));
}

#[test]
fn distill_local_provider_may_receive_local_only() {
    let (_d, db) = db();
    db.insert_card(&CardDraft::new("git", "git:AcmePlugin:bbb", 2, "acme", "acme text")).unwrap();
    let client = FakeClient::new(vec![GOOD_REPLY]);
    let stats = distill(&db, &config(Some("http://127.0.0.1:11434/v1"), &["acme"], 50), &client);
    assert_eq!((stats.sent, stats.written, stats.skipped_local_only), (1, 1, 0));
}

#[test]
fn distill_transport_error_aborts_and_retries_next_run() {
    let (_d, db) = db();
    seed_sources(&db);
    let stats = distill(&db, &local(), &FakeClient::fixed(Err(transport_error())));
    assert!(stats.aborted && stats.sent == 0 && stats.written == 0);
    assert_eq!(format_stats(&stats), "llm: distilled 0/5 records, sent 0, aborted on provider error");
    assert_eq!(distill(&db, &local(), &FakeClient::new(vec![GOOD_REPLY; 5])).written, 5);
}

#[test]
fn distill_parse_failure_is_retried() {
    let (_d, db) = db();
    seed_sources(&db);
    let stats = distill(&db, &config(Some("http://127.0.0.1:11434/v1"), &[], 2), &FakeClient::new(vec!["garbage", GOOD_REPLY]));
    assert_eq!((stats.failed, stats.written, stats.sent), (1, 1, 2));
    let failed: Vec<(String, i64, String)> = {
        let mut stmt = db.connection().prepare("SELECT unit, attempts, last_error FROM distill_queue WHERE attempts>0").unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(failed.len(), 1);
    assert!(failed[0].2.contains("not valid JSON"));
    assert_eq!(distill(&db, &local(), &FakeClient::new(vec![GOOD_REPLY; 4])).written, 4);
    let attempts: i64 = db.connection().query_row("SELECT attempts FROM distill_queue WHERE unit=?", [&failed[0].0], |r| r.get(0)).unwrap();
    assert_eq!(attempts, 0);
}

#[test]
fn distill_rejected_record_goes_to_fallback_model_once() {
    let (_d, db) = db();
    seed_sources(&db);
    let rejected = LlmError { message: "LLM endpoint returned HTTP 400: moderation".into(), status: Some(400) };
    let main = FakeClient::fixed(Err(rejected.clone()));
    let fallback = FakeClient::new(vec![GOOD_REPLY, "garbage", GOOD_REPLY, GOOD_REPLY, GOOD_REPLY]);
    let stats = distill_pending(&db, &local(), Some(&main), Some(&fallback), 1, Some(NOW)).unwrap();
    assert_eq!((stats.sent, stats.written, stats.failed, stats.aborted), (5, 4, 1, false));
    assert_eq!((main.requests().len(), fallback.requests().len()), (5, 5));

    // Без запасной модели отказ — неудачная попытка, пачка идёт дальше.
    let (_d2, db2) = crate::db();
    seed_sources(&db2);
    let stats = distill(&db2, &local(), &main);
    assert_eq!((stats.failed, stats.sent, stats.aborted), (5, 0, false));
}

#[test]
fn distill_counts_an_unusable_200_body_as_an_attempt() {
    // Транспорт цел, но тело непригодно: повтор того же запроса даст то же самое.
    // Без попытки запись вечно остаётся в очереди, и каждый sync платит заново.
    let server = FakeProvider::start(|_, _| (200, r#"{"choices": []}"#.into()));
    let client = OpenAiClient::new(&server.base, "m", None, 5.0, 1500, false).unwrap();
    let (_d, db) = db();
    seed_sources(&db);
    let stats = distill_pending(&db, &local(), Some(&client), None, 1, Some(NOW)).unwrap();
    assert_eq!((stats.failed, stats.written, stats.sent, stats.aborted), (5, 0, 0, false));
    let mut stmt = db.connection().prepare("SELECT unit, attempts FROM distill_queue WHERE attempts>0").unwrap();
    let failed = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).unwrap();
    assert_eq!(failed.collect::<Result<Vec<_>, _>>().unwrap().len(), 5);
}

#[test]
fn distill_parallel_workers_backfill_and_abort() {
    let (_d, db) = db();
    seed_sources(&db);
    let stats = distill_pending(&db, &local(), Some(&FakeClient::fixed(Ok(GOOD_REPLY))), None, 4, Some(NOW)).unwrap();
    assert_eq!((stats.sent, stats.written, stats.failed), (5, 5, 0));
    assert_eq!(distilled(&db).len(), 5);
    assert_eq!(db.get_state(DISTILL_LEASE_KEY).unwrap(), None, "аренда снята после прогона");

    let (_d2, db2) = crate::db();
    seed_sources(&db2);
    let stats = distill_pending(&db2, &local(), Some(&FakeClient::fixed(Err(transport_error()))), None, 4, Some(NOW)).unwrap();
    assert!(stats.aborted && stats.sent == 0 && stats.written == 0);
    assert!(distilled(&db2).is_empty());
}

#[test]
fn distill_skips_when_another_process_holds_the_lease() {
    let (_d, db) = db();
    seed_sources(&db);
    assert!(db.claim_lease(DISTILL_LEASE_KEY, "other", LEASE_TTL_SECONDS, None).unwrap());
    let stats = distill(&db, &local(), &FakeClient::new(vec![]));
    assert!(stats.busy);
    assert_eq!(format_stats(&stats), "llm: another sync holds the distillation lease, skipping");
}

#[test]
fn distill_through_real_client_records_usage() {
    let (_d, db) = db();
    seed_sources(&db);
    let body = json!({"choices": [{"message": {"content": GOOD_REPLY}}], "usage": {"prompt_tokens": 100, "completion_tokens": 20, "cost_rub": 0.5}}).to_string();
    let server = FakeProvider::start(move |_, _| (200, body.clone()));
    let cfg = config(Some(&server.base), &[], 50);
    let stats = distill_pending(&db, &cfg, None, None, 1, Some(NOW)).unwrap();
    assert_eq!(stats.written, 5);
    let requests = server.requests();
    assert!(requests.iter().all(|r| r.json()["max_tokens"] == json!(UNIT_MAX_TOKENS)));
    assert!(requests.iter().all(|r| r.json()["messages"][0]["content"] == SYSTEM_PROMPT));
    let usage = db.recent_usage(10).unwrap();
    assert_eq!((usage.totals.runs, usage.totals.prompt_tokens, usage.totals.completion_tokens), (5, 500, 100));
    assert_eq!((usage.runs[0].provider.as_str(), usage.runs[0].model.as_str(), usage.runs[0].cards), ("127.0.0.1", "m", 1));
    assert!((usage.totals.cost_rub - 2.5).abs() < 1e-9);
}

// -- очередь и язык ------------------------------------------------------------------------

#[test]
fn git_day_waits_until_it_is_over() {
    let (_d, db) = db();
    let noon = 1_790_035_200 + 12 * 3600; // 2026-09-22 12:00 UTC
    db.insert_card(&CardDraft::new("git", "git:r:abc", noon, "c", "c")).unwrap();
    db.insert_card(&CardDraft::new("kimi", "kimi:s:a:0", noon, "q", "q")).unwrap();
    let states = db.unit_states().unwrap();
    let git = states.iter().find(|s| s.source == "git").unwrap();
    let chat = states.iter().find(|s| s.source == "kimi").unwrap();
    assert_eq!(unit_quiet_until(git), noon + 12 * 3600);
    assert_eq!(unit_quiet_until(chat), noon + UNIT_QUIET_SECONDS);
    assert_eq!(unit_status(git, noon + 3600, true), UnitStatus::Waiting);
    assert_eq!(unit_status(chat, noon + 3600, true), UnitStatus::Pending);
    let status = queue_status(&db, Some(&local()), Some(noon + 3600)).unwrap();
    assert_eq!((status.configured, status.records, status.pending, status.waiting, status.fragments), (true, 2, 1, 1, 2));
    assert!(!queue_status(&db, None, None).unwrap().configured);
}

#[test]
fn requeue_non_russian_and_all() {
    let (_d, db) = db();
    seed_sources(&db);
    let english = r#"{"title": "Search index", "summary": "The index uses SQLite FTS5."}"#;
    let mixed = r#"{"title": "Индекс SQLite FTS5 BM25 ranking", "summary": "Поиск по SQLite FTS5 с BM25 и свежестью."}"#;
    distill(&db, &local(), &FakeClient::new(vec![english, mixed, GOOD_REPLY, GOOD_REPLY, GOOD_REPLY]));
    assert!(is_russian_text("Индекс SQLite FTS5") && !is_russian_text("Search index") && is_russian_text("123"));
    assert_eq!(requeue_non_russian(&db).unwrap(), 1);
    assert_eq!(queue_status(&db, Some(&local()), Some(NOW)).unwrap().pending, 1);
    assert_eq!(requeue_all(&db).unwrap(), 5);
    assert_eq!(queue_status(&db, Some(&local()), Some(NOW + DAY)).unwrap().pending, 5);
}

#[test]
fn fit_fragments_keeps_head_and_tail() {
    let texts: Vec<String> = (0..100).map(|i| format!("{i}:{}", "x".repeat(500))).collect();
    let fitted = fit_fragments(&texts, 24_000);
    assert_eq!(fitted.len(), 58 + 1);
    assert_eq!(fitted[29], "[… пропущено фрагментов: 42 …]");
    assert!(fitted[0].starts_with("0:") && fitted.last().unwrap().starts_with("99:"));
    assert_eq!(fitted.iter().map(|t| t.chars().count()).sum::<usize>() + 2 * fitted.len(), 23_986);
    assert_eq!(fit_fragments(&texts[..2], 24_000), texts[..2]);
    assert!(fit_fragments(&texts, 0).is_empty());
}
