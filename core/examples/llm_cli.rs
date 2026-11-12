//! Пересказ и ответы Rust-ядром — для сверки с Python (tests/test_rust_llm_parity.py).
//!
//!     llm_cli distill DB CONFIG NOW [WORKERS]   статистика прогона JSON-строкой
//!     llm_cli answer DB CONFIG QUESTION [HISTORY_JSON]
//!     llm_cli skill DB QUESTION BASE_URL MODEL NOW [PATTERN]...
//!     llm_cli models CONFIG                     каталог провайдера: проверка TLS на живом endpoint

use std::path::Path;

use memory_core::answer::{answer, default_answer_client, skill_answer, HistoryTurn, ProviderConfig, DEFAULT_ANSWER_LIMIT, SKILL_SEARCH_LIMIT};
use memory_core::config::load_config_with_key;
use memory_core::db::Database;
use memory_core::llm::{distill_pending, list_models};
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args[0] == "models" {
        let llm = load_config_with_key(Some(Path::new(&args[1])), memory_core::config::api_key_from_env()).expect("config").llm;
        let models = list_models(llm.base_url.as_deref().unwrap_or(""), llm.api_key.as_deref(), 30.0).expect("models");
        let configured = llm.model.is_some_and(|m| models.contains(&m));
        println!("{}", json!({"models": models.len(), "configured_model_listed": configured}));
        return;
    }
    let db = Database::open(&args[1]).expect("open database");
    let output = match args[0].as_str() {
        "distill" => {
            let config = load_config_with_key(Some(Path::new(&args[2])), None).expect("config");
            let now = args[3].parse().expect("now");
            let workers = args.get(4).map_or(1, |w| w.parse().expect("workers"));
            distill_pending(&db, &config, None, None, workers, Some(now)).expect("distill").record()
        }
        "answer" => {
            let config = load_config_with_key(Some(Path::new(&args[2])), None).expect("config");
            let history: Vec<HistoryTurn> = args.get(4).map_or_else(Vec::new, |raw| {
                let turns: Value = serde_json::from_str(raw).expect("history json");
                turns
                    .as_array()
                    .expect("history list")
                    .iter()
                    .map(|t| HistoryTurn {
                        q: t["q"].as_str().expect("q").to_string(),
                        card_ids: t["card_ids"].as_array().expect("card_ids").iter().map(|id| id.as_i64().expect("id")).collect(),
                    })
                    .collect()
            });
            let result = answer(&db, &args[3], DEFAULT_ANSWER_LIMIT, &history, Some(&config), &default_answer_client).expect("answer");
            json!({
                "answer": result.answer,
                "used_llm": result.used_llm(),
                "card_ids": result.cards.iter().map(|c| c.id).collect::<Vec<_>>(),
                "reason": result.reason.map(|r| r.as_str()),
            })
        }
        "skill" => {
            let provider = ProviderConfig { base_url: args[3].clone(), api_key: None, model: Some(args[4].clone()) };
            let now = args[5].parse().expect("now");
            let text = skill_answer(&db, &args[2], SKILL_SEARCH_LIMIT, Some(&provider), &args[6..], Some(now), 30.0).expect("skill answer");
            json!(text)
        }
        other => panic!("unknown command {other}"),
    };
    println!("{output}");
}
