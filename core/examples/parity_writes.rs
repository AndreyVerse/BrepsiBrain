//! Сверка записи с Python (scripts/parity_writes.py): `write DB` выполняет тот же
//! сценарий, что Python, `dump DB` печатает JSON состояния, которое сравнивается.
//!
//!     cargo run --example parity_writes -- write  копия.db
//!     cargo run --example parity_writes -- dump   копия.db

use memory_core::db::{CardDraft, Database, Filters, UsageRun};
use memory_core::history::{history, history_item, remember_search};
use serde_json::{json, Value};

const NOW: i64 = 1_790_000_000;
const QUERIES: [&str; 4] = ["паритет записи", "сверка Rust", "второй мозг", "индекс"];

fn write(db: &Database) {
    for n in 1..=3 {
        let text = format!("паритет записи ход {n}: сверка Rust и Python");
        db.insert_card(&CardDraft::new("claude", &format!("claude:parity-s1:{n}"), NOW - 600 + n, &text, &text)).unwrap();
    }
    let last = db.get_card_by_ref("claude:parity-s1:3").unwrap().unwrap().id;
    let mut card = CardDraft::new("llm:claude", "", NOW, "Сверка записи", "### Кратко\nПаритет записи Rust и Python.\n\n### Вопросы\n- как сверить запись?").distilled();
    card.keywords = "паритет, сверка".into();
    db.write_unit_card("claude:parity-s1", &card, last).unwrap();
    let first = db.get_card_by_ref("claude:parity-s1:1").unwrap().unwrap().id;
    db.update_card(first, NOW - 599, "паритет записи ход 1", "исправленный ход: сверка Rust", None, true).unwrap();
    db.insert_card(&CardDraft::new("codex", "codex:parity-s2:1", NOW - 300, "индекс сверки", "индекс сверки")).unwrap();
    let other = db.get_card_by_ref("codex:parity-s2:1").unwrap().unwrap().id;
    db.mark_unit_failed("codex:parity-s2", other, "reply was not JSON").unwrap();
    db.mark_unit_failed("codex:parity-s2", other, "reply was not JSON again").unwrap();
    db.mark_unit_held("codex:parity-s2", other).unwrap();
    db.record_usage(&UsageRun { ts: NOW, provider: "gateway".into(), model: "m-parity".into(), cards: 1, prompt_tokens: 1234, completion_tokens: 321, cost_rub: 0.75, task: "distill".into() }).unwrap();
    let found = db.search("паритет записи", 8, Some(NOW), &Filters::default()).unwrap();
    remember_search(db, "Как сверить запись?", Some("Сверкой дампов [1]."), &found, Some(NOW)).unwrap();
    remember_search(db, "  как сверить   ЗАПИСЬ? ", None, &found, Some(NOW + 1)).unwrap();
    assert!(db.claim_lease("lease:parity", "rust-1", 600, Some(NOW)).unwrap());
}

fn dump(db: &Database) -> Value {
    let units: Vec<Value> = db
        .unit_states()
        .unwrap()
        .into_iter()
        .filter(|s| s.unit.contains("parity"))
        .map(|s| json!([s.unit, s.source, s.fragments, s.sample_ref, s.covered_id == s.max_id, s.attempts, s.attempt_id == s.max_id, s.last_error, s.held_id == s.max_id]))
        .collect();
    let search: Vec<Value> = QUERIES
        .iter()
        .map(|q| {
            let cards = db.search(q, 8, Some(NOW), &Filters::default()).unwrap();
            json!([q, cards.iter().map(|c| json!([c.source_ref, c.title, c.keywords, format!("{:.3}", c.score.unwrap())])).collect::<Vec<_>>()])
        })
        .collect();
    let usage = db.recent_usage(3).unwrap();
    let hist: Vec<Value> = history(db, 5)
        .unwrap()
        .into_iter()
        .filter(|h| h.q.contains("сверить"))
        .map(|h| {
            let (item, cards) = history_item(db, h.id).unwrap().unwrap();
            json!([h.q, h.ts, h.answered, h.cards, item.answer, item.memory_ref, cards.iter().map(|c| c.source_ref.clone()).collect::<Vec<_>>()])
        })
        .collect();
    let memory = hist
        .first()
        .and_then(|h| h[5].as_str())
        .and_then(|r| db.get_card_by_ref(r).unwrap())
        .map(|c| json!([c.source, c.kind, c.unit, c.title, c.body]));
    let scope = hist.first().and_then(|h| h[5].as_str()).map(|r| db.get_state(&format!("scope:{}", &r[4..])).unwrap());
    json!({
        "units": units,
        "search": search,
        "usage": usage.runs.iter().map(|r| json!([r.ts, r.provider, r.model, r.cards, r.prompt_tokens, r.completion_tokens, r.cost_rub])).collect::<Vec<_>>(),
        "history": hist,
        "memory": memory,
        "scope": scope,
        "lease": db.get_state("lease:parity").unwrap().map(|v| v.split('|').nth(1).unwrap_or("").to_string()),
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = Database::open(&args[2]).unwrap();
    match args[1].as_str() {
        "write" => write(&db),
        "dump" => println!("{}", serde_json::to_string_pretty(&dump(&db)).unwrap()),
        other => panic!("unknown command {other}"),
    }
}
