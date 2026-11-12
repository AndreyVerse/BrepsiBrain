//! MCP по stdio: формат ответов server.py и конверт FastMCP. Байтовая сверка с
//! Python на живой базе — scripts/mcp_parity.py; здесь — то, что должно держаться всегда.

use memory_core::db::{CardDraft, Database};
use memory_core::mcp;
use serde_json::{json, Value};

fn db_with_card() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("m.db")).unwrap();
    db.insert_card(&CardDraft::new("claude", "claude:s:1", 1_793_000_000 - 100, "Второй мозг", &"слово ".repeat(60)))
        .unwrap();
    (dir, db)
}

fn roundtrip(db: &Database, requests: &[Value]) -> Vec<Value> {
    let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
    let mut out = Vec::new();
    mcp::serve(db, Some(1_793_000_000), input.as_bytes(), &mut out).unwrap();
    String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn call(id: i64, name: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})
}

fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"].as_str().unwrap()
}

#[test]
fn handshake_lists_three_tools_and_skips_notifications() {
    let (_d, db) = db_with_card();
    let replies = roundtrip(&db, &[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {"protocolVersion": "1999-01-01"}}),
    ]);
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "memory-base");
    let names: Vec<_> = replies[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["search_memory", "recent", "get_card"]);
    assert_eq!(replies[1]["result"]["tools"][0]["inputSchema"]["properties"]["limit"]["default"], 8);
    assert_eq!(replies[2]["result"]["protocolVersion"], "2025-11-25");
}

#[test]
fn search_line_format_and_structured_content() {
    let (_d, db) = db_with_card();
    let replies = roundtrip(&db, &[call(1, "search_memory", json!({"query": "второй мозг"}))]);
    let line = text(&replies[0]);
    assert!(line.starts_with("- [1] (2026-10-26, claude) Второй мозг — слово слово"), "{line}");
    assert!(line.contains("… (rank "), "сниппет обрезан до 160 символов с многоточием: {line}");
    assert_eq!(replies[0]["result"]["structuredContent"]["result"], line);
    assert_eq!(replies[0]["result"]["isError"], false);
}

#[test]
fn empty_results_and_missing_card() {
    let (_d, db) = db_with_card();
    let replies = roundtrip(&db, &[
        call(1, "search_memory", json!({"query": "it's"})),
        call(2, "recent", json!({"days": 0})),
        call(3, "get_card", json!({"id": 42})),
    ]);
    assert_eq!(text(&replies[0]), "No memories match \"it's\".");
    assert_eq!(text(&replies[1]), "No cards in the last 0 day(s).");
    assert_eq!(text(&replies[2]), "Card 42 not found.");
}

#[test]
fn full_card_text() {
    let (_d, db) = db_with_card();
    let replies = roundtrip(&db, &[call(1, "get_card", json!({"id": "1"}))]);
    let body = text(&replies[0]);
    assert!(body.starts_with("#1 — Второй мозг\nsource: claude | ref: claude:s:1 | date: 2026-10-26 | ingested: "));
    assert!(body.ends_with(&"слово ".repeat(60)));
}

#[test]
fn validation_errors_follow_pydantic_text() {
    let (_d, db) = db_with_card();
    let replies = roundtrip(&db, &[
        call(1, "get_card", json!({})),
        call(2, "get_card", json!({"id": "abc"})),
        call(3, "nope", json!({})),
        json!({"jsonrpc": "2.0", "id": 4, "method": "unknown/method"}),
    ]);
    assert_eq!(
        text(&replies[0]),
        "Error executing tool get_card: 1 validation error for get_cardArguments\nid\n  Field required \
         [type=missing, input_value={}, input_type=dict]\n    For further information visit \
         https://errors.pydantic.dev/2.14/v/missing"
    );
    assert!(text(&replies[1]).contains("[type=int_parsing, input_value='abc', input_type=str]"));
    assert_eq!(replies[1]["result"]["isError"], true);
    assert!(replies[1]["result"].get("structuredContent").is_none());
    assert_eq!(text(&replies[2]), "Unknown tool: nope");
    assert_eq!(replies[3]["error"]["code"], -32602);
}

#[test]
fn garbage_line_gets_parse_error_and_server_keeps_going() {
    let (_d, db) = db_with_card();
    let mut out = Vec::new();
    let input = "{не json\n{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n";
    mcp::serve(&db, None, input.as_bytes(), &mut out).unwrap();
    let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["error"]["code"], -32700);
    assert_eq!(lines[1], json!({"jsonrpc": "2.0", "id": 7, "result": {}}));
}
