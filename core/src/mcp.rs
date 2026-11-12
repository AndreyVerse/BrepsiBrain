//! MCP-сервер memory-base по stdio — замена FastMCP 1.x из `server.py`.
//!
//! Контракт снят с работающей Python-реализации (`scripts/mcp_parity.py`):
//! те же инструменты, схемы, дефолты, текст ответов, конверт `structuredContent`
//! и тексты ошибок валидации в стиле pydantic для типовых случаев.
//!
//! Поток: `serve` крутится в вызывающем потоке, читает stdin построчно и отвечает
//! в stdout по одному JSON на строку; запросы обрабатываются строго по очереди,
//! поэтому одного соединения SQLite достаточно.

use std::io::{BufRead, Write};

use serde_json::{json, Map, Value};

use crate::db::{
    Card, Database, Filters, DEFAULT_RECENT_DAYS, DEFAULT_RECENT_LIMIT, DEFAULT_SEARCH_LIMIT,
};
use crate::pystr;
use crate::text::format_date;

pub const SERVER_NAME: &str = "memory-base";
const SUPPORTED_PROTOCOLS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const LATEST_PROTOCOL: &str = "2025-11-25";
const PYDANTIC_DOCS: &str = "https://errors.pydantic.dev/2.14/v/";

fn snippet(body: &str) -> String {
    const MAX: usize = 160;
    let flat = pystr::split_whitespace(body).collect::<Vec<_>>().join(" ");
    if pystr::char_len(&flat) > MAX {
        format!("{}…", pystr::rstrip(pystr::take_chars(&flat, MAX - 1)))
    } else {
        flat
    }
}

fn card_line(card: &Card) -> String {
    let rank = card.score.map(|s| format!(" (rank {s:.3})")).unwrap_or_default();
    format!(
        "- [{}] ({}, {}) {} — {}{}",
        card.id,
        format_date(card.ts),
        card.source,
        card.title,
        snippet(&card.body),
        rank
    )
}

pub fn format_search_results(cards: &[Card], query: &str) -> String {
    if cards.is_empty() {
        return format!("No memories match {}.", pystr::repr(query));
    }
    cards.iter().map(card_line).collect::<Vec<_>>().join("\n")
}

pub fn format_recent(cards: &[Card], days: i64) -> String {
    if cards.is_empty() {
        return format!("No cards in the last {days} day(s).");
    }
    cards.iter().map(card_line).collect::<Vec<_>>().join("\n")
}

pub fn format_card(card: Option<&Card>, card_id: i64) -> String {
    match card {
        None => format!("Card {card_id} not found."),
        Some(card) => format!(
            "#{} — {}\nsource: {} | ref: {} | date: {} | ingested: {}\n\n{}",
            card.id,
            card.title,
            card.source,
            card.source_ref,
            format_date(card.ts),
            format_date(card.created_at),
            card.body
        ),
    }
}

// -- валидация аргументов в стиле pydantic (lax mode) ---------------------------

struct FieldError {
    field: &'static str,
    kind: &'static str,
    message: &'static str,
    input: Value,
}

fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => {
            if let Some(f) = n.as_f64().filter(|_| n.is_f64()) {
                format!("{f:?}")
            } else {
                n.to_string()
            }
        }
        Value::String(s) => pystr::repr(s),
        Value::Array(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter().map(|(k, v)| format!("{}: {}", pystr::repr(k), py_repr(v))).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn py_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn int_arg(args: &Map<String, Value>, field: &'static str, default: Option<i64>, errors: &mut Vec<FieldError>) -> i64 {
    let Some(value) = args.get(field) else {
        if default.is_none() {
            errors.push(FieldError {
                field,
                kind: "missing",
                message: "Field required",
                input: Value::Object(args.clone()),
            });
        }
        return default.unwrap_or(0);
    };
    let fail = |kind, message| FieldError { field, kind, message, input: value.clone() };
    match value {
        Value::Bool(b) => i64::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else {
                match n.as_f64() {
                    Some(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => f as i64,
                    Some(f) if f.is_finite() => {
                        errors.push(fail("int_from_float", "Input should be a valid integer, got a number with a fractional part"));
                        0
                    }
                    _ => {
                        errors.push(fail("int_parsing", "Input should be a valid integer, unable to parse string as an integer"));
                        0
                    }
                }
            }
        }
        Value::String(s) => match pystr::strip(s).parse::<i64>() {
            Ok(i) => i,
            Err(_) => {
                errors.push(fail("int_parsing", "Input should be a valid integer, unable to parse string as an integer"));
                0
            }
        },
        _ => {
            errors.push(fail("int_type", "Input should be a valid integer"));
            0
        }
    }
}

fn str_arg(args: &Map<String, Value>, field: &'static str, errors: &mut Vec<FieldError>) -> String {
    match args.get(field) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            errors.push(FieldError { field, kind: "string_type", message: "Input should be a valid string", input: other.clone() });
            String::new()
        }
        None => {
            errors.push(FieldError { field, kind: "missing", message: "Field required", input: Value::Object(args.clone()) });
            String::new()
        }
    }
}

fn validation_text(tool: &str, errors: &[FieldError]) -> String {
    let count = errors.len();
    let mut out = format!(
        "Error executing tool {tool}: {count} validation error{} for {tool}Arguments",
        if count == 1 { "" } else { "s" }
    );
    for e in errors {
        out.push_str(&format!(
            "\n{}\n  {} [type={}, input_value={}, input_type={}]\n    For further information visit {PYDANTIC_DOCS}{}",
            e.field,
            e.message,
            e.kind,
            py_repr(&e.input),
            py_type(&e.input),
            e.kind
        ));
    }
    out
}

// -- протокол -------------------------------------------------------------------

fn text_result(text: String, is_error: bool) -> Value {
    if is_error {
        json!({"content": [{"type": "text", "text": text}], "isError": true})
    } else {
        json!({"content": [{"type": "text", "text": text}], "structuredContent": {"result": text}, "isError": false})
    }
}

fn tools() -> Value {
    serde_json::from_str(include_str!("mcp_tools.json")).expect("embedded tools schema")
}

/// Один вызов инструмента: Ok — текст ответа, Err — текст ошибки (isError).
fn call_tool(db: &Database, now: Option<i64>, name: &str, args: &Map<String, Value>) -> Result<String, String> {
    let mut errors = Vec::new();
    let result = match name {
        "search_memory" => {
            let query = str_arg(args, "query", &mut errors);
            let limit = int_arg(args, "limit", Some(DEFAULT_SEARCH_LIMIT), &mut errors);
            if !errors.is_empty() {
                return Err(validation_text(name, &errors));
            }
            db.search(&query, limit, now, &Filters::default()).map(|cards| format_search_results(&cards, &query))
        }
        "recent" => {
            let days = int_arg(args, "days", Some(DEFAULT_RECENT_DAYS), &mut errors);
            let limit = int_arg(args, "limit", Some(DEFAULT_RECENT_LIMIT), &mut errors);
            if !errors.is_empty() {
                return Err(validation_text(name, &errors));
            }
            db.recent(days, limit, now, &Filters::default()).map(|cards| format_recent(&cards, days))
        }
        "get_card" => {
            let id = int_arg(args, "id", None, &mut errors);
            if !errors.is_empty() {
                return Err(validation_text(name, &errors));
            }
            db.get_card(id).map(|card| format_card(card.as_ref(), id))
        }
        _ => return Err(format!("Unknown tool: {name}")),
    };
    result.map_err(|e| format!("Error executing tool {name}: {e}"))
}

/// Ответ на одно сообщение; None — уведомление, отвечать не нужно.
/// `now` закрепляет время для сверки с Python; в работе — None.
pub fn handle(db: &Database, now: Option<i64>, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if SUPPORTED_PROTOCOLS.contains(&asked) { asked } else { LATEST_PROTOCOL };
            json!({
                "protocolVersion": version,
                "capabilities": {
                    "experimental": {},
                    "prompts": {"listChanged": false},
                    "resources": {"subscribe": false, "listChanged": false},
                    "tools": {"listChanged": false}
                },
                "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")}
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "prompts/list" => json!({"prompts": []}),
        "resources/list" => json!({"resources": []}),
        "resources/templates/list" => json!({"resourceTemplates": []}),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let empty = Map::new();
            let args = params.get("arguments").and_then(Value::as_object).unwrap_or(&empty);
            match call_tool(db, now, name, args) {
                Ok(text) => text_result(text, false),
                Err(text) => text_result(text, true),
            }
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32602, "message": "Invalid request parameters", "data": ""}
            }))
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Цикл stdio до закрытия stdin. Ошибка записи в stdout — клиент ушёл, выходим.
pub fn serve(db: &Database, now: Option<i64>, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    for line in input.split(b'\n') {
        let line = line?;
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(text) {
            // Batch из протокола 2025-03-26: ответы без уведомлений одним массивом.
            Ok(Value::Array(messages)) if messages.is_empty() => {
                Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": "Invalid Request"}}))
            }
            Ok(Value::Array(messages)) => {
                let replies: Vec<Value> = messages.iter().filter_map(|message| handle(db, now, message)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(message) => handle(db, now, &message),
            Err(_) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}})),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut output, &reply)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}
