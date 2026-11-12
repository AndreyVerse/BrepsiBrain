//! HTTP-сервер панели: статика и JSON API.
//!
//! Порт `memory_base/web.py` начинается с каркаса: `std::net` без внешних
//! зависимостей, по потоку на соединение. Соединение однократное
//! (`Connection: close`), поэтому SQLite-соединение и разбор запроса не
//! переживают запрос. Сервер слушает только loopback и проверяет Host/Origin.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::db::{Card, Database, Filters, UnitInfo};
use crate::landscape;
use crate::text::{format_date, split_card_sections};
use crate::answer::HistoryTurn;

const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const UI_LANGS: [&str; 3] = ["auto", "ru", "en"];

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    fn json(status: u16, value: Value) -> Self {
        Response {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn text(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Response { status, content_type, body: body.into() }
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// Запустить сервер; блокирует поток.
pub fn serve(root: PathBuf, addr: &str) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let root = Arc::new(root);
    // Закрываем сессии прошлых запусков и держим watchdog локального движка.
    let config = config_for(&root);
    crate::local::sweep_stale(&config);
    crate::local::start_watchdog(root.as_ref().clone());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Сокет, который открыли и ничего не прислали, без таймаута держит
                // поток навсегда: клиент loopback бывает и сбойным, и чужим.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                let root = Arc::clone(&root);
                // Соединение короткое, база не шарится: поток на запрос безопасен.
                thread::spawn(move || {
                    let _ = handle_connection(stream, &root);
                });
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, root: &Path) -> io::Result<()> {
    let request = match read_request(&mut stream)? {
        Incoming::Request(request) => request,
        Incoming::Reject(response) => return write_response(&mut stream, &response),
        Incoming::Closed => return Ok(()),
    };
    // Только нативный клиент (без Origin): оболочка гасит сервер, запущенный не
    // ею, перед перезапуском в новую версию — иначе новое окно осталось бы со
    // старым API и статикой. Не в `route`, чтобы тесты маршрутов не завершали процесс.
    if request.method == "POST"
        && request.target.split('?').next() == Some("/api/shutdown")
        && !request.headers.contains_key("origin")
        && host_allowed(request.headers.get("host").map(String::as_str))
    {
        write_response(&mut stream, &Response::json(200, json!({"stopping": true})))?;
        let _ = crate::local::stop();
        std::process::exit(0);
    }
    let response = route(
        &request.method,
        &request.target,
        &request.headers,
        &request.body,
        root,
    );
    write_response(&mut stream, &response)
}

/// Отказ — ответ с кодом, а не обрыв соединения: клиент видел «connection reset»
/// вместо понятной ошибки.
enum Incoming {
    Request(Request),
    Reject(Response),
    Closed,
}

fn reject(status: u16, error: &str) -> io::Result<Incoming> {
    Ok(Incoming::Reject(Response::json(status, json!({"error": error}))))
}

fn read_request(stream: &mut TcpStream) -> io::Result<Incoming> {
    let mut buffer = Vec::with_capacity(4096);
    let header_end = loop {
        if let Some(end) = find_header_end(&buffer) {
            break end;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return reject(431, "request headers too large");
        }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(Incoming::Closed);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else { return reject(400, "bad request line") };
    let Some(target) = parts.next() else { return reject(400, "bad request line") };
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    // Chunked не поддерживаем: тело молча читалось пустым и давало «invalid JSON».
    if headers.contains_key("transfer-encoding") {
        return reject(411, "Transfer-Encoding is not supported; send Content-Length");
    }
    let content_length = match headers.get("content-length").map(|value| value.parse::<usize>()) {
        None => 0,
        Some(Ok(length)) => length,
        Some(Err(_)) => return reject(400, "bad Content-Length"),
    };
    if content_length > MAX_BODY_BYTES {
        return reject(413, "request body too large");
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(Incoming::Request(Request {
        method: method.to_ascii_uppercase(),
        target: target.to_owned(),
        headers,
        body,
    }))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        202 => "Accepted",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

/// Разбор запроса без сети — для тестов и повторного использования.
pub fn route(
    method: &str,
    target: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
    root: &Path,
) -> Response {
    if !host_allowed(headers.get("host").map(String::as_str)) {
        return Response::json(403, json!({"error": "forbidden"}));
    }
    // Любой изменяющий запрос проверяет Origin: браузерный CSRF приходит с чужим
    // Origin, а нативные клиенты (трей, MCP, тесты) его не шлют вовсе.
    if method != "GET" && !origin_allowed(headers, root) {
        return Response::json(403, json!({"error": "forbidden"}));
    }
    let path = target.split('?').next().unwrap_or("/");
    if path == "/api/ui" {
        if method == "GET" {
            return Response::json(200, json!({"lang": ui_lang(root)}));
        }
        if method == "POST" {
            return save_ui_lang(root, body);
        }
        return Response::json(405, json!({"error": "method not allowed"}));
    }
    if path == "/api/landscape" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let limit = match query.get("limit") {
            None => None,
            Some(value) => match value.parse::<usize>() {
                Ok(value) if (1..=2000).contains(&value) => Some(value),
                Ok(_) => return Response::json(400, json!({"error": "limit must be 1..2000"})),
                Err(_) => return Response::json(400, json!({"error": "limit must be an integer"})),
            },
        };
        let days = match query.get("days") {
            None => 0,
            Some(value) => match value.parse::<i64>() {
                Ok(value) if (0..=36500).contains(&value) => value,
                Ok(_) => return Response::json(400, json!({"error": "days must be 0..36500"})),
                Err(_) => return Response::json(400, json!({"error": "days must be an integer"})),
            },
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        return match landscape::build_landscape(&db, limit, days, None) {
            Ok(value) => Response::json(200, value),
            Err(error) => Response::json(500, json!({"error": error})),
        };
    }
    if path == "/api/stats" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        return Response::json(200, stats_value(&db, root, true));
    }
    if path == "/api/search" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let Some(question) = query.get("q").filter(|value| !value.trim().is_empty()) else {
            return Response::json(400, json!({"error": "missing required parameter: q"}));
        };
        let limit = match int_param(&query, "limit", 8, 1, 200) {
            Some(value) => value as usize,
            None => return Response::json(400, json!({"error": "limit must be 1..200"})),
        };
        let days = match optional_int_param(&query, "days", 1, 36500) {
            Ok(value) => value,
            Err(()) => return Response::json(400, json!({"error": "days must be 1..36500"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let filters = Filters {
            ref_prefix: query.get("ref_prefix").map(String::as_str),
            source: query.get("source").map(String::as_str),
        };
        let mut cards = match db.search(question, 10_000, None, &filters) {
            Ok(cards) => cards,
            Err(error) => return Response::json(500, json!({"error": error.to_string()})),
        };
        if let Some(days) = days {
            let cutoff = crate::db::now_ts() - days * 86_400;
            cards.retain(|card| card.ts >= cutoff);
        }
        let project = query.get("project");
        if project.is_some() {
            let index = db.unit_index().unwrap_or_default();
            cards.retain(|card| index.get(&card.unit).is_some_and(|info| project == Some(&info.project)));
        }
        let index = db.unit_index().unwrap_or_default();
        let terms = crate::similarity::tokenize(question);
        let cards: Vec<Value> = cards.iter().take(limit).map(|card| card_brief(card, &terms, &index, &db)).collect();
        return Response::json(200, json!({"cards": cards}));
    }
    if path == "/api/recent" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let days = match int_param(&query, "days", 7, 1, 36500) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "days must be 1..36500"})),
        };
        let limit = match int_param(&query, "limit", 50, 1, 1000) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "limit must be 1..1000"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let filters = Filters {
            ref_prefix: query.get("ref_prefix").map(String::as_str),
            source: query.get("source").map(String::as_str),
        };
        let cards = match db.recent(days, limit, None, &filters) {
            Ok(cards) => cards,
            Err(error) => return Response::json(500, json!({"error": error.to_string()})),
        };
        let index = db.unit_index().unwrap_or_default();
        let cards: Vec<Value> = cards.iter().map(|card| card_brief(card, &[], &index, &db)).collect();
        return Response::json(200, json!({"cards": cards}));
    }
    if path == "/api/records" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let offset = match int_param(&query, "offset", 0, 0, i64::MAX) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "offset must be an integer"})),
        };
        let limit = match int_param(&query, "limit", 50, 1, 500) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "limit must be 1..500"})),
        };
        let days = match optional_int_param(&query, "days", 1, 36500) {
            Ok(value) => value,
            Err(()) => return Response::json(400, json!({"error": "days must be 1..36500"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let index = db.unit_index().unwrap_or_default();
        let mut units: Vec<&UnitInfo> = index.values().collect();
        units.sort_by(|left, right| right.last_ts.cmp(&left.last_ts).then(right.max_id.cmp(&left.max_id)));
        let source_filter = query.get("source");
        let project_filter = query.get("project");
        let cutoff = days.map(|days| crate::db::now_ts() - days * 86_400);
        units.retain(|info| {
            cutoff.is_none_or(|cutoff| info.last_ts >= cutoff)
                && source_filter.is_none_or(|source| info.source == *source)
                && project_filter.is_none_or(|project| info.project == *project)
        });
        let total = units.len();
        let records: Vec<Value> = units
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|info| {
                let shown = if info.card_id != 0 {
                    db.get_card(info.card_id).ok().flatten()
                } else {
                    db.get_card(info.max_id).ok().flatten()
                };
                record_row(info, shown.as_ref(), &index, &db)
            })
            .collect();
        return Response::json(200, json!({"records": records, "total": total, "offset": offset, "limit": limit}));
    }
    if path == "/api/card" {
        let query = query_params(target);
        let Some(id) = query.get("id").and_then(|value| value.parse::<i64>().ok()) else {
            return Response::json(400, json!({"error": "id must be an integer"}));
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let Some(card) = db.get_card(id).ok().flatten() else {
            return Response::json(404, json!({"error": "not found"}));
        };
        let index = db.unit_index().unwrap_or_default();
        return Response::json(200, card_full(&card, &index, &db));
    }
    if path == "/api/usage" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let limit = match int_param(&query, "limit", 20, 1, 1000) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "limit must be 1..1000"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let report = match db.recent_usage(limit) {
            Ok(report) => report,
            Err(error) => return Response::json(500, json!({"error": error.to_string()})),
        };
        return Response::json(200, json!({
            "runs": report.runs.iter().map(|run| json!({
                "ts": run.ts, "provider": run.provider, "model": run.model, "cards": run.cards,
                "prompt_tokens": run.prompt_tokens, "completion_tokens": run.completion_tokens,
                "cost_rub": run.cost_rub,
            })).collect::<Vec<_>>(),
            "totals": {
                "runs": report.totals.runs,
                "prompt_tokens": report.totals.prompt_tokens,
                "completion_tokens": report.totals.completion_tokens,
                "cost_rub": report.totals.cost_rub,
            },
        }));
    }
    if path == "/api/tray" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let lang = query.get("lang").map(String::as_str).unwrap_or("ru");
        if lang != "ru" && lang != "en" {
            return Response::json(400, json!({"error": "lang must be ru or en"}));
        }
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        return Response::json(200, tray_value(&db, root, lang));
    }
    if path == "/api/settings" {
        if method == "POST" {
            return save_settings(body, root);
        }
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return Response::json(200, settings_value(root));
    }
    if path == "/api/settings/tasks" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(models) = value.get("models").and_then(Value::as_object) else {
            return Response::json(400, json!({"error": "models must be an object"}));
        };
        let mut changes: Vec<(crate::config::Task, Option<String>)> = Vec::new();
        for (name, model) in models {
            let Some(task) = crate::config::Task::from_str(name) else {
                return Response::json(400, json!({"error": format!("unknown task: {name}")}));
            };
            let model = match model {
                Value::Null => None,
                Value::String(text) if !text.contains(['\r', '\n']) => {
                    let text = text.trim();
                    if text.chars().count() > 200 {
                        return Response::json(400, json!({"error": "model name is too long"}));
                    }
                    (!text.is_empty()).then(|| text.to_owned())
                }
                _ => return Response::json(400, json!({"error": format!("{name}: model must be a string")})),
            };
            changes.push((task, model));
        }
        let path = crate::settings_file::path_for(root);
        for (task, model) in changes {
            if let Err(error) = crate::settings_file::set_task_model(&path, task.as_str(), model.as_deref()) {
                return Response::json(500, json!({"error": error}));
            }
        }
        return Response::json(200, settings_value(root));
    }
    if path == "/api/models" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return models_value(body, root);
    }
    if path == "/api/local/status" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return Response::json(200, crate::local::status(&config_for(root)));
    }
    if path == "/api/local/recommended" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return apply_recommended(root);
    }
    if path == "/api/local/install" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(id) = body.get("id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else {
            return Response::json(400, json!({"error": "id must be a non-empty string"}));
        };
        let config = config_for(root);
        return match crate::local::start_install(&config, id) {
            Ok(()) => Response::json(202, crate::local::status(&config)),
            Err(error) => {
                let status = if error.contains("уже идёт") { 409 } else if error.contains("неизвестная") { 400 } else { 500 };
                Response::json(status, json!({"error": error}))
            }
        };
    }
    if path == "/api/local/cancel" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return Response::json(200, json!({"cancelled": crate::local::cancel()}));
    }
    if path == "/api/local/runtime" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let config = config_for(root);
        return match body.get("action").and_then(Value::as_str) {
            Some("install") => match crate::local::install_runtime_only(&config) {
                Ok(()) => Response::json(202, crate::local::status(&config)),
                Err(error) => Response::json(409, json!({"error": error})),
            },
            Some("remove") => match crate::local::remove_runtime(&config) {
                Ok(()) => Response::json(200, crate::local::status(&config)),
                Err(error) => Response::json(500, json!({"error": error})),
            },
            _ => Response::json(400, json!({"error": "action must be install or remove"})),
        };
    }
    if path == "/api/local/start" || path == "/api/local/stop" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let config = config_for(root);
        if path.ends_with("stop") {
            return match crate::local::stop() {
                Ok(()) => Response::json(200, crate::local::status(&config)),
                Err(error) => Response::json(500, json!({"error": error})),
            };
        }
        return match crate::local::start(&config) {
            Ok(_) => Response::json(200, crate::local::status(&config)),
            Err(error) => Response::json(500, json!({"error": error})),
        };
    }
    if path == "/api/local/models/remove" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(id) = body.get("id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else {
            return Response::json(400, json!({"error": "id must be a non-empty string"}));
        };
        let config = config_for(root);
        return match crate::local::remove_model(&config, id) {
            Ok(()) => Response::json(200, crate::local::status(&config)),
            Err(error) => Response::json(500, json!({"error": error})),
        };
    }
    if path == "/api/local/settings" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return save_local_settings(body, root);
    }
    if path == "/api/local/route" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return save_local_route(body, root);
    }
    if path == "/api/local/bench" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(id) = body.get("id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else {
            return Response::json(400, json!({"error": "id must be a non-empty string"}));
        };
        let config = config_for(root);
        return match crate::local::start_bench(&config, id) {
            Ok(()) => Response::json(202, crate::local::status(&config)),
            Err(error) => Response::json(409, json!({"error": error})),
        };
    }
    if path == "/api/models/stats" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let days = match int_param(&query, "days", 30, 0, 36500) {
            Some(days) => days,
            None => return Response::json(400, json!({"error": "days must be 0..36500"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let since = if days == 0 { 0 } else { crate::db::now_ts() - days * 86_400 };
        return match db.model_stats(since) {
            Ok(stats) => Response::json(200, model_stats_value(days, &stats)),
            Err(error) => Response::json(500, json!({"error": error.to_string()})),
        };
    }
    if path == "/api/timeline" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let weeks = match int_param(&query, "weeks", 12, 0, 520) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "weeks must be 0..520"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        return Response::json(200, timeline_value(&db, weeks));
    }
    if path == "/api/projects" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        // 1000 — столько запрашивает страница «Проекты» (PROJECTS_LIMIT): прежний
        // потолок 200 отдавал ей 400 и «Проекты не загрузились».
        let limit = match int_param(&query, "limit", 50, 1, 1000) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "limit must be 1..1000"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        return Response::json(200, projects_value(&db, limit as usize));
    }
    if path == "/api/projects/propose" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let body = serde_json::from_slice::<Value>(body).unwrap_or_else(|_| json!({}));
        let limit = body.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 30) as usize;
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let config = config_for(root);
        return match crate::projects::propose(&db, &config, limit) {
            Ok(proposals) => Response::json(200, json!({
                "proposals": proposals.iter().map(|proposal| json!({
                    "key": proposal.key, "name": proposal.name,
                    "path": proposal.path, "evidence": proposal.evidence,
                })).collect::<Vec<_>>(),
            })),
            Err(error) => Response::json(400, json!({"error": error})),
        };
    }
    if path == "/api/projects/apply" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let (Some(key), Some(name)) = (body.get("key").and_then(Value::as_str), body.get("name").and_then(Value::as_str)) else {
            return Response::json(400, json!({"error": "key and name must be strings"}));
        };
        let path = body.get("path").and_then(Value::as_str).unwrap_or("");
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        return match crate::projects::apply(&db, key, name, path) {
            Ok(()) => Response::json(200, json!({"ok": true})),
            Err(error) => Response::json(400, json!({"error": error})),
        };
    }
    if path == "/api/projects/reset" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(project) = body.get("project").and_then(Value::as_str) else {
            return Response::json(400, json!({"error": "project must be a string"}));
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        return match crate::projects::reset(&db, project) {
            Ok(removed) => Response::json(200, json!({"removed": removed})),
            Err(error) => Response::json(500, json!({"error": error})),
        };
    }
    if path == "/api/graph" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let Some(seed) = query.get("seed").filter(|seed| !seed.trim().is_empty()) else {
            return Response::json(400, json!({"error": "missing required parameter: seed"}));
        };
        let limit = match int_param(&query, "limit", 12, 1, 200) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "limit must be 1..200"})),
        };
        let k = match int_param(&query, "k", 3, 1, 20) {
            Some(value) => value as usize,
            None => return Response::json(400, json!({"error": "k must be 1..20"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let seed_card = match seed.parse::<i64>() {
            Ok(id) => db.get_card(id).ok().flatten(),
            Err(_) => db.search(seed, 1, None, &Filters::default()).ok().and_then(|cards| cards.into_iter().next()),
        };
        let Some(seed_card) = seed_card else {
            return Response::json(404, json!({"error": "seed not found"}));
        };
        return Response::json(200, graph_value(&db, seed_card, limit as usize, k));
    }
    if path == "/api/answer" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(q) = body.get("q").and_then(Value::as_str).map(str::trim).filter(|q| !q.is_empty()) else {
            return Response::json(400, json!({"error": "q must be a non-empty string"}));
        };
        if q.chars().count() > 4000 {
            return Response::json(400, json!({"error": "q is too long"}));
        }
        let limit = body.get("limit").and_then(Value::as_i64).unwrap_or(8);
        if !(1..=1000).contains(&limit) {
            return Response::json(400, json!({"error": "limit out of range"}));
        }
        let mut history = Vec::new();
        if let Some(turns) = body.get("history").and_then(Value::as_array) {
            if turns.len() > 4 {
                return Response::json(400, json!({"error": "history must contain up to 4 questions"}));
            }
            for turn in turns {
                let Some(question) = turn.get("q").and_then(Value::as_str).filter(|q| !q.trim().is_empty() && q.chars().count() <= 4000) else {
                    return Response::json(400, json!({"error": "history question length out of range"}));
                };
                let Some(ids) = turn.get("card_ids").and_then(Value::as_array) else {
                    return Response::json(400, json!({"error": "history card_ids must be an array"}));
                };
                let mut card_ids = Vec::new();
                for id in ids {
                    let Some(id) = id.as_i64().filter(|id| *id > 0) else {
                        return Response::json(400, json!({"error": "history card_ids must be positive integers"}));
                    };
                    card_ids.push(id);
                }
                history.push(HistoryTurn { q: question.to_owned(), card_ids });
            }
        }
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let config = Some(config_for(root));
        let result = match crate::answer::answer(&db, q, limit, &history, config.as_ref(), &|route| {
            crate::answer::default_answer_client(route)
        }) {
            Ok(result) => result,
            Err(error) => return Response::json(500, json!({"error": error.to_string()})),
        };
        let _ = crate::history::remember_search(&db, q, result.answer.as_deref(), &result.cards, None);
        let index = db.unit_index().unwrap_or_default();
        let terms = crate::similarity::tokenize(&result.query);
        let cards: Vec<Value> = result.cards.iter().map(|card| card_brief(card, &terms, &index, &db)).collect();
        return Response::json(200, json!({
            "answer": result.answer,
            "used_llm": result.used_llm(),
            "cards": cards,
            "reason": result.reason.map(|reason| reason.as_str()),
        }));
    }
    if path == "/api/history" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let limit = match int_param(&query, "limit", 30, 1, 1000) {
            Some(value) => value,
            None => return Response::json(400, json!({"error": "limit must be 1..1000"})),
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let items = crate::history::history(&db, limit).unwrap_or_default();
        return Response::json(200, json!({
            "items": items.iter().map(|item| json!({
                "id": item.id, "q": item.q, "ts": item.ts, "date": format_date(item.ts),
                "answered": item.answered, "cards": item.cards,
            })).collect::<Vec<_>>(),
        }));
    }
    if path == "/api/history/item" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let Some(id) = int_param(&query, "id", 0, 1, i64::MAX) else {
            return Response::json(400, json!({"error": "id must be a positive integer"}));
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let Some((item, cards)) = crate::history::history_item(&db, id).unwrap_or(None) else {
            return Response::json(404, json!({"error": "history item not found"}));
        };
        let index = db.unit_index().unwrap_or_default();
        let terms = crate::similarity::tokenize(&item.q);
        return Response::json(200, json!({
            "id": item.id, "q": item.q, "ts": item.ts, "date": format_date(item.ts),
            "answer": item.answer, "memory_ref": item.memory_ref,
            "used_llm": item.answer.is_some(),
            "cards": cards.iter().map(|card| card_brief(card, &terms, &index, &db)).collect::<Vec<_>>(),
        }));
    }
    if path == "/api/history/delete" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(id) = body.get("id").and_then(Value::as_i64).filter(|id| *id > 0) else {
            return Response::json(400, json!({"error": "id must be a positive integer"}));
        };
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        let removed = crate::history::forget_history_item(&db, id).unwrap_or(false);
        return Response::json(200, json!({"removed": removed}));
    }
    if path == "/api/queue/failed" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let db = match open_db(root) {
            Ok(db) => db,
            Err(error) => return Response::json(500, json!({"error": error})),
        };
        return Response::json(200, queue_failed_value(&db, root));
    }
    if path == "/api/queue/retry" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return queue_retry_value(body, root);
    }
    if path == "/api/log" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return log_value(target, root);
    }
    if path == "/api/sync" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return sync_value(root);
    }
    if path == "/api/sync/schedule" {
        if method == "GET" {
            return Response::json(200, schedule_state_value(root));
        }
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        return sync_schedule_value(body, root);
    }
    if path == "/api/handoff/harnesses" {
        if method == "GET" {
            return Response::json(200, crate::handoff::harnesses_json(root, crate::handoff::running_images()));
        }
        if method == "POST" {
            let Ok(body) = serde_json::from_slice::<Value>(body) else {
                return Response::json(400, json!({"error": "invalid JSON"}));
            };
            let name = body.get("name").and_then(Value::as_str).unwrap_or("");
            let command = body.get("command").and_then(Value::as_str);
            return match crate::handoff::add_custom(root, name, command) {
                Ok(_) => Response::json(200, crate::handoff::harnesses_json(root, crate::handoff::running_images())),
                Err(error) => Response::json(400, json!({"error": error})),
            };
        }
        if method == "DELETE" {
            let query = query_params(target);
            let Some(id) = query.get("id").filter(|id| !id.trim().is_empty()) else {
                return Response::json(400, json!({"error": "missing required parameter: id"}));
            };
            return match crate::handoff::remove_custom(root, id) {
                Ok(removed) => Response::json(200, json!({"removed": removed})),
                Err(error) => Response::json(400, json!({"error": error})),
            };
        }
        return Response::json(405, json!({"error": "method not allowed"}));
    }
    if path == "/api/handoff/chats" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let Some(harness) = query.get("harness").filter(|harness| !harness.trim().is_empty()) else {
            return Response::json(400, json!({"error": "missing required parameter: harness"}));
        };
        let hours = match int_param(&query, "hours", crate::handoff::DEFAULT_HOURS, 1, crate::handoff::MAX_HOURS) {
            Some(hours) => hours,
            None => return Response::json(400, json!({"error": "hours out of range"})),
        };
        let limit = match int_param(
            &query,
            "limit",
            crate::handoff::MAX_GROUP_CHATS as i64,
            1,
            crate::handoff::MAX_CHATS as i64,
        ) {
            Some(limit) => limit as usize,
            None => return Response::json(400, json!({"error": "limit out of range"})),
        };
        return match crate::handoff::active_chats(root, harness, hours, limit) {
            Ok(chats) => Response::json(200, json!({"chats": chats})),
            Err(error) => Response::json(400, json!({"error": error})),
        };
    }
    if path == "/api/handoff" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(chat_id) = body.get("chat").and_then(Value::as_str) else {
            return Response::json(400, json!({"error": "chat must be a string"}));
        };
        let Some(target) = body.get("target").and_then(Value::as_str) else {
            return Response::json(400, json!({"error": "target must be a string"}));
        };
        let mode = body.get("mode").and_then(Value::as_str).unwrap_or("auto");
        let launch = body.get("launch").and_then(Value::as_bool).unwrap_or(true);
        let chat = match crate::handoff::open_chat(root, chat_id) {
            Ok(chat) => chat,
            Err(error) => return Response::json(400, json!({"error": error})),
        };
        let config = config_for(root);
        let out_dir = crate::handoff::handoff_dir(root);
        return match crate::handoff::build(root, &chat, target, mode, &out_dir, &config) {
            Ok(mut result) => {
                let prompt = result.get("prompt").and_then(Value::as_str).unwrap_or("").to_owned();
                let cwd = result.get("cwd").and_then(Value::as_str).unwrap_or("").to_owned();
                let delivery = if launch {
                    crate::handoff::launch(root, target, &prompt, &cwd)
                } else {
                    json!({"delivered": "file"})
                };
                if let Some(object) = result.as_object_mut() {
                    if let Some(delivery) = delivery.as_object() {
                        for (key, value) in delivery {
                            object.insert(key.clone(), value.clone());
                        }
                    }
                }
                Response::json(200, result)
            }
            Err(error) => Response::json(400, json!({"error": error})),
        };
    }
    if path == "/api/handoff/switch" {
        if method != "GET" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let query = query_params(target);
        let Some(harness) = query.get("harness").filter(|harness| !harness.trim().is_empty()) else {
            return Response::json(400, json!({"error": "missing required parameter: harness"}));
        };
        let out_dir = crate::handoff::handoff_dir(root);
        return match crate::handoff::switch_status(root, harness, &out_dir) {
            Ok(value) => Response::json(200, value),
            Err(error) => Response::json(400, json!({"error": error})),
        };
    }
    if path == "/api/handoff/switch/prepare" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(harness) = body.get("harness").and_then(Value::as_str) else {
            return Response::json(400, json!({"error": "harness must be a string"}));
        };
        let chats: Vec<String> = body.get("chats").and_then(Value::as_array).map(|items| {
            items.iter().filter_map(Value::as_str).map(str::to_owned).collect()
        }).unwrap_or_default();
        let mode = body.get("mode").and_then(Value::as_str).unwrap_or("auto");
        let config = config_for(root);
        let out_dir = crate::handoff::handoff_dir(root);
        return match crate::handoff::prepare_switch(root, harness, &chats, mode, &out_dir, &config) {
            Ok(value) => Response::json(200, value),
            Err(error) => Response::json(400, json!({"error": error})),
        };
    }
    if path == "/api/handoff/switch/resume" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(harness) = body.get("harness").and_then(Value::as_str) else {
            return Response::json(400, json!({"error": "harness must be a string"}));
        };
        let launch = body.get("launch").and_then(Value::as_bool).unwrap_or(true);
        let out_dir = crate::handoff::handoff_dir(root);
        return match crate::handoff::resume_switch(root, harness, launch, &out_dir) {
            Ok(value) => Response::json(200, value),
            Err(error) => Response::json(400, json!({"error": error})),
        };
    }
    if path == "/api/handoff/switch/cancel" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(harness) = body.get("harness").and_then(Value::as_str) else {
            return Response::json(400, json!({"error": "harness must be a string"}));
        };
        let out_dir = crate::handoff::handoff_dir(root);
        return match crate::handoff::cancel_switch(harness, &out_dir) {
            Ok(removed) => Response::json(200, json!({"removed": removed})),
            Err(error) => Response::json(400, json!({"error": error})),
        };
    }
    if path == "/api/handoff/launch" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(target) = body.get("target").and_then(Value::as_str).filter(|target| crate::handoff::find(root, target).is_some()) else {
            return Response::json(400, json!({"error": "target: неизвестный харнесс"}));
        };
        let items = body.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        if items.is_empty() || items.len() > crate::handoff::MAX_CHATS {
            return Response::json(400, json!({"error": "items: 1-40 переносов"}));
        }
        let out = match crate::handoff::handoff_dir(root).canonicalize() {
            Ok(path) => path,
            Err(_) => return Response::json(400, json!({"error": "handoffs not found"})),
        };
        let mut checked = Vec::new();
        for item in items {
            let path = item.get("path").and_then(Value::as_str).unwrap_or("");
            let item_prompt = item.get("prompt").and_then(Value::as_str).unwrap_or("");
            let cwd = item.get("cwd").and_then(Value::as_str).unwrap_or("");
            let Ok(canonical) = std::fs::canonicalize(path) else {
                return Response::json(400, json!({"error": "path вне каталога переносов"}));
            };
            if !canonical.starts_with(&out) {
                return Response::json(400, json!({"error": "path вне каталога переносов"}));
            }
            // Проверка пути что-то значит, только если промпт ведёт именно на этот
            // файл переноса, а cwd — существующая папка: иначе через «проверенный»
            // запрос запускался бы произвольный промпт в произвольной папке.
            if !item_prompt.contains(path) {
                return Response::json(400, json!({"error": "prompt не ссылается на файл переноса"}));
            }
            if !cwd.is_empty() && !Path::new(cwd).is_dir() {
                return Response::json(400, json!({"error": "cwd: папки нет"}));
            }
            checked.push((item_prompt.to_owned(), cwd.to_owned()));
        }
        // `prompt` не пуст, только если чаты ушли одной общей сессией: по нему
        // панель пишет «в общей сессии приложения».
        let (deliveries, prompt) = crate::handoff::launch_many(root, target, &checked);
        return Response::json(200, json!({"items": deliveries, "prompt": prompt}));
    }
    if path == "/api/open-url" {
        if method != "POST" {
            return Response::json(405, json!({"error": "method not allowed"}));
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Response::json(400, json!({"error": "invalid JSON"}));
        };
        let Some(url) = body.get("url").and_then(Value::as_str) else {
            return Response::json(400, json!({"error": "url is not allowed"}));
        };
        if !matches!(url, "https://github.com/AndreyVerse" | "https://github.com/AndreyVerse/BrepsiBrain") {
            return Response::json(400, json!({"error": "url is not allowed"}));
        }
        let opened = crate::proc::shell_open(url);
        return Response::json(200, json!({"opened": opened}));
    }
    if path.starts_with("/api/") {
        return Response::json(404, json!({"error": "not found"}));
    }
    if method != "GET" {
        return Response::text(405, "text/plain; charset=utf-8", "method not allowed");
    }
    serve_static(root, path)
}

fn ui_file(root: &Path) -> PathBuf {
    root.join("data").join("ui.json")
}

fn open_db(root: &Path) -> Result<Database, String> {
    let path = crate::config::db_path_from_env()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("data").join("memory.db"));
    Database::open(path).map_err(|error| error.to_string())
}

/// Конфиг именно этого корня данных: env `BREPSI_BRAIN_CONFIG` по-прежнему
/// важнее, иначе `root/config.toml`.
fn config_for(root: &Path) -> crate::config::Config {
    config_result_for(root).unwrap_or_default()
}

fn config_result_for(root: &Path) -> Result<crate::config::Config, crate::config::ConfigError> {
    crate::config::load_config_with_key(Some(&crate::settings_file::path_for(root)), crate::config::api_key_from_env())
}

/// `with_knowledge = false` — для трея: он опрашивает раз в минуту, а разбор
/// всех дистиллятов на большой базе не укладывался в его 10-секундный таймаут.
fn stats_value(db: &Database, root: &Path, with_knowledge: bool) -> Value {
    use crate::db::{now_ts, CARD_KIND_DISTILLED};

    let now = now_ts();
    let active_cutoff = now - 7 * 86_400;
    let project_cutoff = now - 30 * 86_400;
    let index = db.unit_index().unwrap_or_default();
    let states = db.unit_states().unwrap_or_default();
    let mut sources: HashMap<String, (i64, i64)> = HashMap::new();
    for info in index.values() {
        let entry = sources.entry(info.source.clone()).or_insert((0, 0));
        entry.0 += 1;
        if info.card_id != 0 {
            entry.1 += 1;
        }
    }
    let mut source_rows: Vec<Value> = sources
        .into_iter()
        .map(|(source, (records, cards))| json!({"source": source, "records": records, "cards": cards}))
        .collect();
    source_rows.sort_by(|left, right| {
        right["records"].as_i64().unwrap_or(0).cmp(&left["records"].as_i64().unwrap_or(0)).then_with(|| {
            left["source"].as_str().unwrap_or("").cmp(right["source"].as_str().unwrap_or(""))
        })
    });

    let mut projects: HashMap<String, i64> = HashMap::new();
    for info in index.values() {
        let entry = projects.entry(info.project.clone()).or_default();
        *entry = (*entry).max(info.last_ts);
    }
    let projects_total = projects.len() as i64;
    let projects_active_30d = projects.values().filter(|last_ts| **last_ts >= project_cutoff).count() as i64;

    let mut knowledge: HashMap<String, i64> = [("факт", 0), ("решение", 0), ("ошибка", 0), ("команда", 0)]
        .into_iter()
        .map(|(kind, count)| (kind.to_owned(), count))
        .collect();
    if let Some(Ok(mut stmt)) = with_knowledge.then(|| db.connection().prepare("SELECT body FROM cards WHERE kind=?")) {
        if let Ok(rows) = stmt.query_map([CARD_KIND_DISTILLED], |row| row.get::<_, String>(0)) {
            for body in rows.flatten() {
                let sections = split_card_sections(&body);
                for line in sections.atoms.lines().filter(|line| !line.trim().is_empty()) {
                    let line = line.trim().trim_start_matches(['-', '*', '•']).trim();
                    let kind = line.split_once(':').map(|(head, _)| head.trim()).filter(|head| {
                        matches!(*head, "факт" | "решение" | "ошибка" | "команда")
                    }).unwrap_or("факт");
                    *knowledge.entry(kind.to_owned()).or_default() += 1;
                }
            }
        }
    }
    let knowledge_total: i64 = knowledge.values().sum();
    let knowledge_json = json!({
        "факт": knowledge["факт"], "решение": knowledge["решение"], "ошибка": knowledge["ошибка"],
        "команда": knowledge["команда"], "total": knowledge_total,
    });

    let config = config_for(root);
    let distill_route = config.route_for(crate::config::Task::Distill, false);
    let configured = distill_route.is_some();
    let local_route_available = distill_route.as_ref().is_some_and(|route| route.local)
        || config.local_route(crate::config::Task::Distill).is_some();
    let (mut done, mut cards, mut pending, mut waiting, mut held, mut failed) = (0, 0, 0, 0, 0, 0);
    for state in &states {
        if state.covered_id >= state.max_id {
            done += 1;
        } else if !local_route_available && state.held_id >= state.max_id {
            held += 1;
        } else if state.attempts >= 3 && state.attempt_id >= state.max_id {
            failed += 1;
        } else if now < unit_quiet_until(state) {
            waiting += 1;
        } else {
            pending += 1;
        }
        if state.covered_id > 0 {
            cards += 1;
        }
    }
    let records_total = states.len() as i64;
    let fragments_total = states.iter().map(|state| state.fragments).sum::<i64>();
    let by_source: Value = {
        let mut map = serde_json::Map::new();
        if let Ok(mut stmt) = db.connection().prepare("SELECT source, COUNT(*) FROM cards GROUP BY source ORDER BY source") {
            if let Ok(rows) = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))) {
                for row in rows.flatten() {
                    map.insert(row.0, json!(row.1));
                }
            }
        }
        Value::Object(map)
    };
    let distilled = db
        .connection()
        .query_row("SELECT COUNT(*) FROM cards WHERE kind=?", [CARD_KIND_DISTILLED], |row| row.get::<_, i64>(0))
        .unwrap_or(0);
    let raw_pending = pending + waiting;
    let db_bytes = std::fs::metadata(db.path()).map(|meta| meta.len() as i64).unwrap_or(0);
    let last_sync_ts = last_sync_ts(Some(db), root);
    let next_sync_ts = config.next_sync_ts(last_sync_ts).filter(|next| *next > now - 60);
    let local_model = distill_route
        .as_ref()
        .filter(|route| route.local)
        .map(|route| route.model.clone())
        .or_else(|| config.local.model.clone());
    json!({
        "records_7d": index.values().filter(|info| info.last_ts >= active_cutoff).count() as i64,
        "projects_active_30d": projects_active_30d,
        "sources": source_rows,
        "knowledge": knowledge_json,
        "last_sync_ts": last_sync_ts,
        "next_sync_ts": next_sync_ts,
        "sync": {"mode": config.sync.mode.as_str(), "interval_minutes": config.sync.interval_minutes},
        "local": {
            "route_configured": config.local.configured(),
            "offline_only": config.local_ai.offline_only,
            "model": local_model.unwrap_or_default(),
            "running": crate::local::status_running(),
        },
        "server_ts": now,
        "total": db.count_cards().unwrap_or(0),
        "records_total": records_total,
        "fragments_total": fragments_total,
        "by_source": by_source,
        "raw_pending": raw_pending,
        "distilled": distilled,
        "queue": {
            "configured": configured,
            "records": records_total,
            "cards": cards,
            "done": done,
            "pending": pending,
            "waiting": waiting,
            "held_local": held,
            "failed": failed,
            "fragments": fragments_total,
        },
        "active_7d": db.connection().query_row(
            "SELECT COUNT(*) FROM cards WHERE ts>=?", [active_cutoff], |row| row.get::<_, i64>(0)
        ).unwrap_or(0),
        "projects_total": projects_total,
        "last_sync": last_sync_ts,
        "db_bytes": db_bytes,
    })
}

/// Последняя успешная синхронизация: журнал планировщика и состояние,
/// которое пишет ручной `POST /api/sync` (он в журнал не ходит).
fn last_sync_ts(db: Option<&Database>, root: &Path) -> Option<i64> {
    let mut last = db
        .and_then(|db| db.get_state("web:last_sync").ok().flatten())
        .and_then(|value| value.parse::<i64>().ok());
    let path = root.join("data").join("sync.log");
    // Журнал растёт без ограничения, строка на прогон: сначала хвост, весь файл —
    // только если в хвосте нет ни одного успешного прогона.
    for limit in [Some(SYNC_LOG_TAIL_BYTES), None] {
        let Some(text) = read_log(&path, limit) else { break };
        let mut found = None;
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else { continue };
            if value.get("status").and_then(Value::as_str) == Some("ok") {
                if let Some(ts) = value.get("ts").and_then(Value::as_i64) {
                    found = Some(found.map_or(ts, |current: i64| current.max(ts)));
                }
            }
        }
        if let Some(ts) = found {
            last = Some(last.map_or(ts, |current| current.max(ts)));
            break;
        }
        if limit.is_some_and(|limit| std::fs::metadata(&path).is_ok_and(|meta| meta.len() <= limit)) {
            break;
        }
    }
    last
}

const SYNC_LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Весь файл или его хвост с границы строки.
fn read_log(path: &Path, tail: Option<u64>) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = tail.map_or(0, |tail| len.saturating_sub(tail));
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(if start > 0 { text.split_once('\n').map_or_else(String::new, |(_, rest)| rest.to_owned()) } else { text })
}

fn tray_value(db: &Database, root: &Path, lang: &str) -> Value {
    let stats = stats_value(db, root, false);
    let queue = &stats["queue"];
    let records = queue["records"].as_i64().unwrap_or(0);
    let waiting = queue["pending"].as_i64().unwrap_or(0) + queue["waiting"].as_i64().unwrap_or(0);
    let failed = queue["failed"].as_i64().unwrap_or(0);
    let last = stats["last_sync_ts"].as_i64();
    let status = if last.is_none() {
        if lang == "en" { "Not synced yet".to_owned() } else { "Ещё не синхронизировано".to_owned() }
    } else if waiting > 0 {
        if lang == "en" {
            format!("Waiting for summaries: {waiting}")
        } else {
            format!("В очереди пересказа: {waiting}")
        }
    } else if lang == "en" {
        "Everything is synced".to_owned()
    } else {
        "Всё синхронизировано".to_owned()
    };
    let summary = if lang == "en" {
        format!("{records} records · {} sources", stats["sources"].as_array().map(Vec::len).unwrap_or(0))
    } else {
        format!("{records} записей · {} источников", stats["sources"].as_array().map(Vec::len).unwrap_or(0))
    };
    let names = if lang == "en" {
        [("claude", "Claude Code"), ("codex", "Codex"), ("kimi", "Kimi Code"), ("git", "Git"),
         ("github", "GitHub"), ("obsidian", "Obsidian"), ("chatgpt", "ChatGPT"), ("gmail", "Gmail"),
         ("yandex", "Yandex Mail"), ("notion", "Notion")].into_iter().collect::<HashMap<_, _>>()
    } else {
        [("claude", "Claude Code"), ("codex", "Codex"), ("kimi", "Kimi Code"), ("git", "Git"),
         ("github", "GitHub"), ("obsidian", "Obsidian"), ("chatgpt", "ChatGPT"), ("gmail", "Gmail"),
         ("yandex", "Яндекс.Почта"), ("notion", "Notion")].into_iter().collect::<HashMap<_, _>>()
    };
    let sources: Vec<Value> = stats["sources"].as_array().cloned().unwrap_or_default().into_iter().map(|row| {
        let source = row["source"].as_str().unwrap_or("");
        json!({
            "name": names.get(source).copied().unwrap_or(source),
            "records": row["records"], "cards": row["cards"],
        })
    }).collect();
    let mut recent: Vec<Value> = db.unit_index().unwrap_or_default().into_values().map(|info| {
        json!({"id": info.node_id(), "title": info.title, "last_ts": info.last_ts, "max_id": info.max_id})
    }).collect();
    recent.sort_by(|left, right| {
        right["last_ts"].as_i64().unwrap_or(0).cmp(&left["last_ts"].as_i64().unwrap_or(0))
            .then(right["max_id"].as_i64().unwrap_or(0).cmp(&left["max_id"].as_i64().unwrap_or(0)))
    });
    let recent: Vec<Value> = recent.into_iter().take(8).map(|value| json!({"id": value["id"], "title": value["title"]})).collect();
    let config = config_for(root);
    let schedule = match config.sync.mode {
        crate::config::SyncMode::Manual => {
            if lang == "en" { "Manual sync only".to_owned() } else { "Только вручную".to_owned() }
        }
        crate::config::SyncMode::Interval => {
            if lang == "en" {
                format!("Every {} min", config.sync.interval_minutes)
            } else {
                format!("Каждые {} мин", config.sync.interval_minutes)
            }
        }
    };
    json!({
        "status": status,
        "ok": last.is_some() && waiting == 0 && failed == 0,
        "summary": summary,
        "schedule": schedule,
        "local": {"running": crate::local::status_running()},
        "recent": recent,
        "sources": sources,
    })
}

fn settings_value(root: &Path) -> Value {
    let path = crate::settings_file::path_for(root);
    let loaded = crate::config::load_config_with_key(Some(&path), crate::config::api_key_from_env());
    let (llm, tasks, local_route, sync, config_error) = match loaded {
        Ok(config) => {
            let local_route = json!({
                "configured": config.local.configured(),
                "model": config.local.model.clone().unwrap_or_default(),
                "installed": crate::local::installed_tags(&config),
                "collect_interval_minutes": config.local_ai.collect_interval_minutes,
            });
            (config.llm, config.tasks, local_route, config.sync, false)
        }
        Err(_) => (
            crate::config::LlmSettings::default(),
            crate::config::TaskModels::default(),
            json!({"configured": false, "model": "", "installed": [], "collect_interval_minutes": null}),
            crate::config::SyncSettings::default(),
            true,
        ),
    };
    let base_url = llm.base_url.clone().unwrap_or_default();
    let local = crate::config::url_hostname(&base_url).is_some_and(|host| {
        matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
    });
    json!({
        "providers": [
            {"id": "timeweb", "name": "Timeweb AI Gateway", "base_url": "https://api.timeweb.ai/v1"},
            {"id": "openrouter", "name": "OpenRouter", "base_url": "https://openrouter.ai/api/v1"},
            {"id": "openai", "name": "OpenAI", "base_url": "https://api.openai.com/v1"},
            {"id": "ollama", "name": "Ollama (локально)", "base_url": "http://127.0.0.1:11434/v1"},
            {"id": "lmstudio", "name": "LM Studio (локально)", "base_url": "http://127.0.0.1:1234/v1"},
        ],
        "base_url": base_url,
        "model": llm.model.clone().unwrap_or_default(),
        "fallback_model": llm.fallback_model.clone().unwrap_or_default(),
        "has_key": llm.api_key.as_deref().is_some_and(|key| !key.is_empty()),
        "key_from_env": crate::config::api_key_from_env().is_some(),
        "json_mode": llm.json_mode,
        "is_local": local,
        "config_error": config_error,
        "config_path": path.display().to_string(),
        "tasks": crate::config::Task::ALL.into_iter().map(|task| {
            (task.as_str().to_owned(), json!(tasks.get(task).unwrap_or("")))
        }).collect::<serde_json::Map<String, Value>>(),
        "local_route": local_route,
        "sync": {"mode": sync.mode.as_str(), "interval_minutes": sync.interval_minutes},
    })
}

/// Модель `[llm.local]`. Этот маршрут обслуживает только свой движок (`job_guard`
/// запускает его и ищет тег), поэтому выбор ограничен установленными моделями.
fn save_local_route(body: &[u8], root: &Path) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, json!({"error": "invalid JSON"}));
    };
    let Some(model) = value.get("model").and_then(Value::as_str).map(str::trim) else {
        return Response::json(400, json!({"error": "model must be a string"}));
    };
    let config = config_for(root);
    if !model.is_empty() && !crate::local::installed_tags(&config).iter().any(|tag| tag == model) {
        return Response::json(400, json!({"error": format!("модель {model} не установлена: откройте «Модели и железо»")}));
    }
    let base_url = config.local.base_url.clone().unwrap_or_else(crate::local::default_base_url);
    let path = crate::settings_file::path_for(root);
    if let Err(error) = crate::settings_file::set_local_route(&path, &base_url, model) {
        return Response::json(500, json!({"error": error}));
    }
    Response::json(200, settings_value(root))
}

/// Применить настройки движка под это железо и поставить рекомендованную
/// модель. Контекст пишется до установки: `create_derived` запекает num_ctx в
/// производную модель, поэтому пересоздание получит уже рекомендованное окно.
fn apply_recommended(root: &Path) -> Response {
    let hardware = crate::hardware::detect();
    let model = crate::catalog::recommended_for(&hardware);
    let path = crate::settings_file::path_for(root);
    if let Err(error) = crate::settings_file::set_local_ai(
        &path,
        None,
        Some("on_demand"),
        Some(120),
        Some(crate::local::recommended_context(&hardware) as u64),
        None,
    ) {
        return Response::json(500, json!({"error": error}));
    }
    // Конфиг перечитывается после записи: install_model берёт context_tokens
    // из него, а не из тела запроса.
    let config = config_for(root);
    match crate::local::start_install(&config, model.id) {
        Ok(()) => Response::json(202, crate::local::status(&config)),
        Err(error) if error.contains("уже идёт") => Response::json(409, json!({"error": error})),
        Err(error) => Response::json(500, json!({"error": error})),
    }
}

/// Настройки локального AI: папка, режим, простой, контекст, офлайн.
fn save_local_settings(body: &[u8], root: &Path) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, json!({"error": "invalid JSON"}));
    };
    let config = config_for(root);
    let mut models_root: Option<String> = None;
    if let Some(field) = value.get("models_root") {
        let Some(text) = field.as_str() else {
            return Response::json(400, json!({"error": "models_root must be a string"}));
        };
        let text = text.trim();
        if !text.is_empty() {
            if text.chars().count() > 512 || text.contains(['\0', '\r', '\n']) {
                return Response::json(400, json!({"error": "models_root is not a valid path"}));
            }
            let candidate = std::path::PathBuf::from(text);
            if !candidate.is_absolute() {
                return Response::json(400, json!({"error": "models_root must be an absolute path"}));
            }
            if candidate.exists() && !candidate.is_dir() {
                return Response::json(400, json!({"error": "models_root is a file"}));
            }
            if let Err(error) = fs::create_dir_all(&candidate) {
                return Response::json(400, json!({"error": format!("cannot create models_root: {error}")}));
            }
        }
        models_root = Some(text.to_owned());
    }
    let mode = match value.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(mode)) if crate::config::LocalMode::parse(mode).is_some() => Some(mode.as_str()),
        Some(_) => return Response::json(400, json!({"error": "mode must be on_demand or always"})),
    };
    let idle = match value.get("idle_timeout_seconds") {
        None | Some(Value::Null) => None,
        Some(number) => match number.as_u64().filter(|value| (10..=3600).contains(value)) {
            Some(value) => Some(value),
            None => return Response::json(400, json!({"error": "idle_timeout_seconds must be 10..3600"})),
        },
    };
    let context = match value.get("context_tokens") {
        None | Some(Value::Null) => None,
        Some(number) => match number.as_u64().filter(|value| (2048..=131_072).contains(value) && value % 1024 == 0) {
            Some(value) => Some(value),
            None => return Response::json(400, json!({"error": "context_tokens must be 2048..131072 and a multiple of 1024"})),
        },
    };
    let offline_only = match value.get("offline_only") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(flag)) => Some(*flag),
        Some(_) => return Response::json(400, json!({"error": "offline_only must be a bool"})),
    };
    let root_changed = models_root
        .as_deref()
        .filter(|text| !text.is_empty())
        .is_some_and(|text| std::path::Path::new(text) != config.models_root());
    if root_changed {
        // Запущенный serve держит старый OLLAMA_MODELS: его надо погасить.
        let _ = crate::local::stop();
    }
    let path = crate::settings_file::path_for(root);
    if let Err(error) = crate::settings_file::set_local_ai(&path, models_root.as_deref(), mode, idle, context, offline_only) {
        return Response::json(500, json!({"error": error}));
    }
    Response::json(200, crate::local::status(&config_for(root)))
}

fn model_stats_value(days: i64, stats: &crate::db::ModelStats) -> Value {
    let sessions: Vec<Value> = stats
        .sessions
        .iter()
        .map(|session| {
            json!({
                "started_ts": session.started_ts,
                "ended_ts": session.ended_ts,
                "seconds": session.seconds(),
                "models": session.models,
                "trigger": session.trigger,
            })
        })
        .collect();
    json!({
        "days": days,
        "hours": {
            "today": stats.seconds_today as f64 / 3600.0,
            "week": stats.seconds_week as f64 / 3600.0,
            "total": stats.seconds_total as f64 / 3600.0,
        },
        "sessions": sessions,
        "tasks": stats.tasks.iter().map(|task| json!({
            "task": task.task, "runs": task.runs,
            "prompt_tokens": task.prompt_tokens, "completion_tokens": task.completion_tokens,
            "cost_rub": task.cost_rub,
        })).collect::<Vec<_>>(),
        "models": stats.models.iter().map(|model| json!({
            "model": model.model, "runs": model.runs,
            "prompt_tokens": model.prompt_tokens, "completion_tokens": model.completion_tokens,
            "cost_rub": model.cost_rub,
        })).collect::<Vec<_>>(),
        "bench": stats.bench.iter().map(|bench| json!({
            "model": bench.model, "ts": bench.ts, "tokens": bench.tokens,
            "load_ms": bench.load_ms, "seconds": bench.seconds, "tps": bench.tps,
        })).collect::<Vec<_>>(),
        "totals": {
            "runs": stats.runs, "prompt_tokens": stats.prompt_tokens,
            "completion_tokens": stats.completion_tokens, "cost_rub": stats.cost_rub,
        },
    })
}

fn sync_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Записи, исчерпавшие попытки пересказа: причина видна в «Журнале обработки».
fn queue_failed_value(db: &Database, root: &Path) -> Value {
    let config = config_for(root);
    let route = config.route_for(crate::config::Task::Distill, false);
    let local_available = route.as_ref().is_some_and(|route| route.local)
        || config.local_route(crate::config::Task::Distill).is_some();
    let now = crate::db::now_ts();
    let index = db.unit_index().unwrap_or_default();
    let mut failed: Vec<Value> = db.unit_states().unwrap_or_default().into_iter()
        .filter(|state| crate::llm::unit_status(state, now, local_available) == crate::llm::UnitStatus::Failed)
        .map(|state| {
            let info = index.get(&state.unit);
            json!({
                "unit": state.unit,
                "title": info.map(|info| info.title.clone()).unwrap_or_default(),
                "source": state.source,
                "project": info.map(|info| info.project.clone()).unwrap_or_default(),
                "last_ts": state.max_ts,
                "attempts": state.attempts,
                "last_error": state.last_error,
            })
        })
        .collect();
    failed.sort_by(|left, right| right["last_ts"].as_i64().unwrap_or(0).cmp(&left["last_ts"].as_i64().unwrap_or(0)));
    json!({"failed": failed})
}

/// Повтор пересказа: существующий `requeue_unit` сбрасывает попытки и аренду.
/// Пересказ пойдёт при следующей синхронизации — автоматически её не запускаем.
fn queue_retry_value(body: &[u8], root: &Path) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, json!({"error": "invalid JSON"}));
    };
    let db = match open_db(root) {
        Ok(db) => db,
        Err(error) => return Response::json(500, json!({"error": error})),
    };
    let config = config_for(root);
    let route = config.route_for(crate::config::Task::Distill, false);
    let local_available = route.as_ref().is_some_and(|route| route.local)
        || config.local_route(crate::config::Task::Distill).is_some();
    let now = crate::db::now_ts();
    let failed: HashSet<String> = db.unit_states().unwrap_or_default().into_iter()
        .filter(|state| crate::llm::unit_status(state, now, local_available) == crate::llm::UnitStatus::Failed)
        .map(|state| state.unit)
        .collect();
    let units: Vec<String> = if value.get("all").and_then(Value::as_bool).unwrap_or(false) {
        failed.into_iter().collect()
    } else {
        let Some(unit) = value.get("unit").and_then(Value::as_str).filter(|unit| !unit.trim().is_empty()) else {
            return Response::json(400, json!({"error": "unit must be a string"}));
        };
        if !failed.contains(unit) {
            return Response::json(400, json!({"error": "запись не в статусе failed"}));
        }
        vec![unit.to_owned()]
    };
    let mut requeued = 0;
    for unit in &units {
        if db.requeue_unit(unit).is_ok() {
            requeued += 1;
        }
    }
    Response::json(200, json!({"requeued": requeued}))
}

const LOG_FILES: [&str; 2] = ["sync.log", "ollama-serve.log"];

/// Хвост журнала для просмотра в GUI. Имя — из белого списка, не путь.
fn log_value(target: &str, root: &Path) -> Response {
    let query = query_params(target);
    let Some(name) = query.get("name").filter(|name| LOG_FILES.contains(&name.as_str())) else {
        return Response::json(400, json!({"error": "name must be sync.log or ollama-serve.log"}));
    };
    let tail_kb = match int_param(&query, "tail_kb", 64, 1, 256) {
        Some(value) => value as u64,
        None => return Response::json(400, json!({"error": "tail_kb must be 1..256"})),
    };
    let path = root.join("data").join(name);
    let tail = tail_kb * 1024;
    let Some(text) = read_log(&path, Some(tail)) else {
        return Response::json(404, json!({"error": "log not found"}));
    };
    let len = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    Response::json(200, json!({"name": name, "text": text, "bytes": text.len(), "truncated": len > tail}))
}

/// Реальное состояние задачи планировщика (только чтение) плюс расчётный интервал.
fn schedule_state_value(root: &Path) -> Value {
    let config = config_for(root);
    let info = crate::schedule::query(crate::schedule::TASK_NAME);
    let db = open_db(root).ok();
    json!({
        "mode": config.sync.mode.as_str(),
        "interval_minutes": config.sync.interval_minutes,
        "task": info.state != "absent",
        "state": info.state,
        "next_run_ts": info.next_run_ts,
        "last_run_ts": info.last_run_ts,
        "last_result": info.last_result,
        "error": info.error,
        "next_sync_ts": config.next_sync_ts(last_sync_ts(db.as_ref(), root)),
    })
}

fn sync_value(root: &Path) -> Response {
    let Ok(_guard) = sync_lock().try_lock() else {
        return Response::json(409, json!({"error": "sync already running"}));
    };
    let db = match open_db(root) {
        Ok(db) => db,
        Err(error) => return Response::json(500, json!({"error": error})),
    };
    let config = match config_result_for(root) {
        Ok(config) => config,
        Err(error) => {
            return Response::json(500, json!({
                "status": "error",
                "error": format!("cannot load config: {error}"),
            }));
        }
    };
    let counts = match crate::ingest::ingest_all(&db, &crate::ingest::sources_for(&config), &config) {
        Ok(counts) => counts,
        Err(error) => return Response::json(500, json!({"status": "error", "error": error.to_string()})),
    };
    let extra = crate::sources_extra::ingest_extra(&db, &config);
    // Кнопку и трей нажимает человек: интервал локальной модели их не задерживает.
    let stage = crate::collect::run(&db, &config, true);
    let new_cards = counts.kimi + counts.claude + counts.codex + counts.git + counts.obsidian
        + extra.values().sum::<usize>();
    let _ = db.set_state("web:last_sync", &crate::db::now_ts().to_string());
    Response::json(200, json!({
        "status": "ok",
        "counts": {
            "kimi": counts.kimi,
            "claude": counts.claude,
            "codex": counts.codex,
            "git": counts.git,
            "obsidian": counts.obsidian,
            "chatgpt": extra.get("chatgpt").copied().unwrap_or(0),
            "gmail": extra.get("gmail").copied().unwrap_or(0),
            "yandex": extra.get("yandex").copied().unwrap_or(0),
            "github": extra.get("github").copied().unwrap_or(0),
            "notion": extra.get("notion").copied().unwrap_or(0),
        },
        "new_cards": new_cards,
        "llm": stage.llm,
        "projects": stage.projects,
    }))
}

fn sync_schedule_value(body: &[u8], root: &Path) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, json!({"error": "invalid JSON"}));
    };
    let current = config_for(root);
    let Some(mode) = value.get("mode").and_then(Value::as_str) else {
        return Response::json(400, json!({"error": "mode is required"}));
    };
    let Some(mode) = crate::config::SyncMode::parse(mode) else {
        return Response::json(400, json!({"error": "mode must be interval or manual"}));
    };
    let interval = match value.get("interval_minutes") {
        None | Some(Value::Null) => current.sync.interval_minutes,
        Some(number) => match number.as_u64().filter(|value| (5..=1439).contains(value)) {
            Some(value) => value as u32,
            None => return Response::json(400, json!({"error": "interval_minutes must be 5..1439"})),
        },
    };
    let collect_interval = match value.get("local_interval_minutes") {
        None | Some(Value::Null) => None,
        Some(number) => match number.as_u64().filter(|value| (5..=1440).contains(value)) {
            Some(value) => Some(value),
            None => return Response::json(400, json!({"error": "local_interval_minutes must be 5..1440"})),
        },
    };
    // Планировщик будит приложение раз в interval: чаще сбор идти не сможет,
    // и молча показывать недостижимое значение нельзя.
    if mode == crate::config::SyncMode::Interval && collect_interval.is_some_and(|value| value < u64::from(interval)) {
        return Response::json(400, json!({"error": "local_interval_minutes must not be below interval_minutes"}));
    }
    let path = crate::settings_file::path_for(root);
    if let Err(error) = crate::settings_file::set_sync(&path, mode.as_str(), interval as u64, collect_interval) {
        return Response::json(500, json!({"error": error}));
    }
    let config = config_for(root);
    let warning = match apply_schedule(&config.sync) {
        Ok(warning) => warning,
        Err(error) => Some(format!("расписание не применено: {error}")),
    };
    let db = open_db(root).ok();
    Response::json(200, json!({
        "mode": config.sync.mode.as_str(),
        "interval_minutes": config.sync.interval_minutes,
        "local_interval_minutes": config.local_ai.collect_interval_minutes,
        "task": task_exists(),
        "next_sync_ts": config.next_sync_ts(last_sync_ts(db.as_ref(), root)),
        "warning": warning,
    }))
}

/// Планировщик трогаем только из настоящего приложения: unit-тест не должен
/// создавать задачи в системе.
#[cfg(not(test))]
fn task_exists() -> bool {
    crate::schedule::exists(crate::schedule::TASK_NAME)
}

#[cfg(test)]
fn task_exists() -> bool {
    false
}

#[cfg(not(test))]
fn apply_schedule(settings: &crate::config::SyncSettings) -> Result<Option<String>, String> {
    crate::schedule::apply(settings, std::env::current_exe().ok().as_deref())
}

#[cfg(test)]
fn apply_schedule(_settings: &crate::config::SyncSettings) -> Result<Option<String>, String> {
    Ok(Some("планировщик отключён в тестах".into()))
}

fn endpoint(value: Option<&str>) -> Option<String> {
    let value = value?.trim().trim_end_matches('/');
    if value.is_empty() || value.contains(['\r', '\n']) {
        return None;
    }
    let (scheme, rest) = value.split_once("://")?;
    if !matches!(scheme, "http" | "https") || rest.is_empty() {
        return None;
    }
    Some(value.to_owned())
}

fn save_settings(body: &[u8], root: &Path) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, json!({"error": "invalid JSON"}));
    };
    let Some(base_url) = endpoint(body.get("base_url").and_then(Value::as_str)) else {
        return Response::json(400, json!({"error": "base_url must be an http(s) URL"}));
    };
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::trim).filter(|model| {
        !model.is_empty() && !model.contains(['\r', '\n'])
    }) else {
        return Response::json(400, json!({"error": "model must be a non-empty single line"}));
    };
    let api_key = match body.get("api_key") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.contains(['\r', '\n']) => Some(value.trim().to_owned()),
        Some(_) => return Response::json(400, json!({"error": "api_key must be a single line"})),
    };
    let json_mode = body.get("json_mode").and_then(Value::as_bool).unwrap_or(false);
    let fallback_model = match body.get("fallback_model") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.contains(['\r', '\n']) => {
            let value = value.trim();
            if value.chars().count() > 200 {
                return Response::json(400, json!({"error": "fallback_model is too long"}));
            }
            Some(value.to_owned())
        }
        Some(_) => return Response::json(400, json!({"error": "fallback_model must be a single line"})),
    };
    let path = crate::settings_file::path_for(root);
    if let Err(error) = crate::settings_file::set_llm_provider(&path, &base_url, model, api_key.as_deref(), json_mode, fallback_model.as_deref()) {
        return Response::json(502, json!({"error": error}));
    }
    Response::json(200, settings_value(root))
}

fn models_value(body: &[u8], root: &Path) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(body) else {
        return Response::json(400, json!({"error": "invalid JSON"}));
    };
    let Some(base_url) = endpoint(body.get("base_url").and_then(Value::as_str)) else {
        return Response::json(400, json!({"error": "base_url must be an http(s) URL"}));
    };
    if config_for(root).local_ai.offline_only && !crate::llm::is_local_endpoint(&base_url) {
        return Response::json(409, json!({"error": "включён режим «только локальные»: запрос к внешнему провайдеру заблокирован"}));
    }
    let body_key = body.get("api_key").and_then(Value::as_str).filter(|key| !key.trim().is_empty());
    let key = match body_key {
        Some(key) => Some(key.trim().to_owned()),
        None => {
            let config = config_for(root);
            crate::config::same_host(config.llm.base_url.as_deref(), Some(&base_url))
                .then_some(config.llm.api_key)
                .flatten()
        }
    };
    match crate::llm::list_models(&base_url, key.as_deref(), 20.0) {
        Ok(models) => Response::json(200, json!({"models": models})),
        Err(error) => Response::json(502, json!({"error": error.to_string()})),
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month_index = (month + 9) % 12;
    let doy = (153 * month_index + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_index = (5 * doy + 2) / 153;
    let day = doy - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn monday(ts: i64) -> i64 {
    let days = ts.div_euclid(86_400);
    days - (days + 3).rem_euclid(7)
}

fn iso_day(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn date_label(days: i64) -> String {
    const MONTHS: [&str; 12] = ["янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"];
    let (_, month, day) = civil_from_days(days);
    format!("{day} {}", MONTHS[(month - 1) as usize])
}

fn timeline_value(db: &Database, weeks: i64) -> Value {
    let now = crate::db::now_ts();
    let index = db.unit_index().unwrap_or_default();
    let cutoff = (weeks > 0).then(|| monday(now) - (weeks - 1) * 7);
    let mut buckets: BTreeMap<i64, HashMap<String, i64>> = BTreeMap::new();
    for info in index.values() {
        let week = monday(info.last_ts);
        if cutoff.is_none_or(|cutoff| week >= cutoff) {
            *buckets.entry(week).or_default().entry(info.source.clone()).or_default() += 1;
        }
    }
    if buckets.is_empty() {
        return json!({"weeks": [], "sources": [], "total": 0, "by_source": {}});
    }
    // Границы — от текущей недели, а не от данных: кривая дата коммита в 9999 году
    // растила цикл до сотен тысяч строк. Потолок «за всё время» — 60 лет.
    let last = monday(now);
    let first = cutoff.unwrap_or_else(|| *buckets.keys().next().unwrap()).max(last - 3120 * 7);
    let mut rows = Vec::new();
    let mut totals: BTreeMap<String, i64> = BTreeMap::new();
    let mut day = first;
    while day <= last {
        let counts = buckets.remove(&day).unwrap_or_default();
        let total: i64 = counts.values().sum();
        for (source, count) in &counts {
            *totals.entry(source.clone()).or_default() += count;
        }
        rows.push(json!({
            "key": iso_day(day),
            "label": date_label(day),
            "end_label": date_label(day + 6),
            "total": total,
            "by_source": counts,
        }));
        day += 7;
    }
    let total: i64 = totals.values().sum();
    json!({"weeks": rows, "sources": totals.keys().cloned().collect::<Vec<_>>(), "total": total, "by_source": totals})
}

fn projects_value(db: &Database, limit: usize) -> Value {
    let now = crate::db::now_ts();
    let this_week = monday(now);
    let mut groups: HashMap<String, Vec<UnitInfo>> = HashMap::new();
    for info in db.unit_index().unwrap_or_default().into_values() {
        groups.entry(info.project.clone()).or_default().push(info);
    }
    let overrides = db.project_overrides().unwrap_or_default();
    let mut projects: Vec<Value> = groups.into_iter().map(|(project, units)| {
        let mut sources: HashMap<String, i64> = HashMap::new();
        let mut keywords: HashMap<String, usize> = HashMap::new();
        let mut spark = [0i64; 12];
        let mut fragments = 0;
        let mut newest = &units[0];
        for info in &units {
            *sources.entry(info.source.clone()).or_default() += 1;
            fragments += info.fragments;
            if info.last_ts > newest.last_ts {
                newest = info;
            }
            for word in info.keywords.split(',').map(str::trim).filter(|word| !word.is_empty()) {
                *keywords.entry(word.to_owned()).or_default() += 1;
            }
            let weeks_ago = (this_week - monday(info.last_ts)) / 7;
            if (0..12).contains(&weeks_ago) {
                spark[(11 - weeks_ago) as usize] += 1;
            }
        }
        let source = sources.iter().max_by_key(|(_, count)| *count).map(|(source, _)| source.clone()).unwrap_or_default();
        let mut keyword_rows: Vec<(String, usize)> = keywords.into_iter().collect();
        keyword_rows.sort_by(|(left_word, left_count), (right_word, right_count)| right_count.cmp(left_count).then(left_word.cmp(right_word)));
        // Ключ переопределения — исходное имя; после применения `project` уже
        // отображаемое, а сброс ищет запись по отображаемому имени.
        let key = overrides
            .iter()
            .find(|(_, value)| value.display_name == project)
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| project.clone());
        json!({
            "project": project,
            "key": key,
            "overridden": key != project,
            "name": project.rsplit('/').next().unwrap_or(""),
            "path": newest.project_path,
            "source": source,
            "sources": sources,
            "count": fragments,
            "records": units.len(),
            "last_ts": newest.last_ts,
            "spark": spark,
            "keywords": keyword_rows.into_iter().take(5).map(|(word, _)| word).collect::<Vec<_>>(),
        })
    }).collect();
    projects.sort_by(|left, right| {
        right["records"].as_u64().unwrap_or(0).cmp(&left["records"].as_u64().unwrap_or(0))
            .then_with(|| left["project"].as_str().unwrap_or("").cmp(right["project"].as_str().unwrap_or("")))
    });
    projects.truncate(limit);
    json!({"projects": projects})
}

fn graph_value(db: &Database, seed: Card, limit: usize, k: usize) -> Value {
    let corpus = db.recent(36_500, limit as i64, None, &Filters::default()).unwrap_or_default();
    let mut nodes = vec![seed.clone()];
    for card in corpus {
        if card.id != seed.id {
            nodes.push(card);
        }
    }
    let token_sets: Vec<HashSet<String>> = nodes
        .iter()
        .map(|card| crate::similarity::token_set(&format!("{}\n{}", card.title, card.body)))
        .collect();
    let weights = crate::similarity::idf_weights(&token_sets);
    let vectors: Vec<HashMap<String, f64>> = token_sets
        .iter()
        .map(|tokens| crate::similarity::vectorize(tokens, &weights))
        .collect();
    let sims: Vec<f64> = std::iter::once(1.0)
        .chain(vectors.iter().skip(1).map(|vector| crate::similarity::cosine(&vectors[0], vector)))
        .collect();
    let mut order: Vec<usize> = (1..nodes.len()).collect();
    order.sort_by(|left, right| {
        sims[*right].partial_cmp(&sims[*left]).unwrap_or(std::cmp::Ordering::Equal)
            .then(nodes[*left].id.cmp(&nodes[*right].id))
    });
    let mut ordered_nodes = vec![nodes[0].clone()];
    let mut ordered_vectors = vec![vectors[0].clone()];
    let mut ordered_sims = vec![1.0];
    for index in order {
        ordered_nodes.push(nodes[index].clone());
        ordered_vectors.push(vectors[index].clone());
        ordered_sims.push((sims[index] * 1_000_000.0).round() / 1_000_000.0);
    }
    let node_json: Vec<Value> = ordered_nodes
        .iter()
        .zip(&ordered_sims)
        .map(|(card, rank)| json!({
            "id": card.id, "title": card.title, "source": card.source, "ts": card.ts, "rank": rank,
        }))
        .collect();
    let mut links = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for index in 0..ordered_vectors.len() {
        for (other, weight) in crate::similarity::nearest(&ordered_vectors, index, k, 0.05) {
            let pair = (index.min(other), index.max(other));
            if !seen.insert(pair) {
                continue;
            }
            links.push(json!([
                ordered_nodes[pair.0].id,
                ordered_nodes[pair.1].id,
                (weight * 1_000_000.0).round() / 1_000_000.0
            ]));
        }
    }
    json!({"nodes": node_json, "links": links})
}

/// Ближайшие по темам записи — тот же механизм, что в графе (`graph_value`):
/// корпус свежих карточек, seed первым, косинусная близость с порогом графа.
fn similar_cards(card: &Card, index: &HashMap<String, UnitInfo>, db: &Database) -> Vec<Value> {
    let corpus = db.recent(36_500, 200, None, &Filters::default()).unwrap_or_default();
    let mut nodes = vec![card.clone()];
    for other in corpus {
        if other.id != card.id {
            nodes.push(other);
        }
    }
    if nodes.len() < 2 {
        return Vec::new();
    }
    let token_sets: Vec<HashSet<String>> = nodes
        .iter()
        .map(|card| crate::similarity::token_set(&format!("{}\n{}", card.title, card.body)))
        .collect();
    let weights = crate::similarity::idf_weights(&token_sets);
    let vectors: Vec<HashMap<String, f64>> = token_sets
        .iter()
        .map(|tokens| crate::similarity::vectorize(tokens, &weights))
        .collect();
    crate::similarity::nearest(&vectors, 0, 5, 0.05)
        .into_iter()
        .map(|(other, weight)| {
            let neighbor = &nodes[other];
            json!({
                "id": neighbor.id,
                "title": neighbor.title,
                "source": neighbor.source,
                "project": index.get(&neighbor.unit).map(|info| info.project.clone()),
                "weight": (weight * 1_000_000.0).round() / 1_000_000.0,
            })
        })
        .collect()
}

fn unit_quiet_until(state: &crate::db::UnitState) -> i64 {
    if state.source == "git" || state.source == "github" {
        let day = state.unit.rsplit(':').next().unwrap_or("");
        if let Some(start) = parse_utc_day(day) {
            return (start + 86_400).max(state.max_ts);
        }
    }
    state.max_ts + 30 * 60
}

fn parse_utc_day(value: &str) -> Option<i64> {
    let mut parts = value.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400)
}

fn int_param(params: &HashMap<String, String>, name: &str, default: i64, minimum: i64, maximum: i64) -> Option<i64> {
    let value = match params.get(name) {
        None => default,
        Some(raw) => raw.parse::<i64>().ok()?,
    };
    (minimum..=maximum).contains(&value).then_some(value)
}

fn optional_int_param(params: &HashMap<String, String>, name: &str, minimum: i64, maximum: i64) -> Result<Option<i64>, ()> {
    match params.get(name) {
        None => Ok(None),
        Some(raw) => {
            let value = raw.parse::<i64>().map_err(|_| ())?;
            (minimum..=maximum).contains(&value).then_some(Some(value)).ok_or(())
        }
    }
}

fn snippet(body: &str, terms: &[String]) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flat.to_lowercase();
    let hit = terms.iter().filter_map(|term| lower.find(&term.to_lowercase())).min();
    let chars: Vec<char> = flat.chars().collect();
    let Some(hit) = hit else {
        if chars.len() <= 160 {
            return flat;
        }
        return format!("{}…", chars.iter().take(159).collect::<String>().trim_end());
    };
    // Смещение найдено в `lower`: при смене регистра длина строки меняется
    // (U+0130 → два символа), поэтому считать символы надо в ней же, а не в `flat`.
    let hit_char = lower[..hit].chars().count().min(chars.len());
    let start = hit_char.saturating_sub(40);
    let end = (start + 160).min(chars.len());
    let mut piece: String = chars[start..end].iter().collect::<String>().trim().to_owned();
    if start > 0 {
        piece.insert(0, '…');
    }
    if end < chars.len() {
        piece.push('…');
    }
    piece
}

fn card_brief(card: &Card, terms: &[String], index: &HashMap<String, UnitInfo>, db: &Database) -> Value {
    let mut text = card.body.clone();
    if !terms.is_empty() && card.kind == crate::db::CARD_KIND_DISTILLED && !terms.iter().any(|term| text.to_lowercase().contains(term)) {
        if let Ok(fragments) = db.unit_fragments(&card.unit) {
            if let Some(best) = fragments.into_iter().max_by_key(|fragment| {
                terms.iter().filter(|term| fragment.body.to_lowercase().contains(*term)).count()
            }) {
                text = best.body;
            }
        }
    }
    json!({
        "project": index.get(&card.unit).map(|info| info.project.clone()),
        "id": card.id,
        "source": card.source,
        "ts": card.ts,
        "date": format_date(card.ts),
        "title": card.title,
        "snippet": snippet(&text, terms),
        "rank": card.score,
        "kind": card.kind,
    })
}

/// Строка «Все записи». Осиротевшая ссылка unit→card (карточка удалена между
/// снимком индекса и чтением) не должна вести на `/api/card?id=undefined`:
/// строка остаётся видимой с названием записи, но без кликабельного id.
fn record_row(info: &UnitInfo, shown: Option<&Card>, index: &HashMap<String, UnitInfo>, db: &Database) -> Value {
    let mut brief = shown
        .map(|card| card_brief(card, &[], index, db))
        .unwrap_or_else(|| json!({"id": Value::Null, "title": info.title}));
    if let Some(object) = brief.as_object_mut() {
        object.insert("ts".into(), json!(info.last_ts));
        object.insert("date".into(), json!(format_date(info.last_ts)));
        object.insert("source".into(), json!(info.source));
        object.insert("unit".into(), json!(info.unit));
        object.insert("fragments".into(), json!(info.fragments));
        object.insert("project".into(), json!(info.project));
        object.insert("has_card".into(), json!(info.card_id != 0));
        object.insert("covered".into(), json!(info.covered_id >= info.max_id));
    }
    brief
}

fn card_items(section: &str) -> Vec<String> {
    section
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim().trim_start_matches(['-', '*', '•']).trim().to_owned())
        .collect()
}

fn parse_atom(line: &str) -> (String, String) {
    match line.split_once(':') {
        Some((head, rest))
            if matches!(head.trim(), "факт" | "решение" | "ошибка" | "команда") && !rest.trim().is_empty() =>
        {
            (head.trim().to_owned(), rest.trim().to_owned())
        }
        _ => ("факт".to_owned(), line.trim().to_owned()),
    }
}

fn card_full(card: &Card, index: &HashMap<String, UnitInfo>, db: &Database) -> Value {
    let info = index.get(&card.unit);
    let sections = if card.kind == crate::db::CARD_KIND_DISTILLED {
        Some(split_card_sections(&card.body))
    } else {
        None
    };
    let fragments = if card.kind == crate::db::CARD_KIND_DISTILLED && !card.unit.is_empty()
        && card.source_ref == format!("llm:{}", card.unit)
    {
        db.unit_fragments(&card.unit).unwrap_or_default()
    } else {
        Vec::new()
    };
    let parent = if card.kind == crate::db::CARD_KIND_RAW && !card.unit.is_empty() {
        db.get_card_by_ref(&format!("llm:{}", card.unit)).ok().flatten()
    } else {
        None
    };
    let transcript = fragments.iter().map(|fragment| fragment.body.clone()).collect::<Vec<_>>().join("\n\n");
    let keywords: Vec<String> = card.keywords.split(',').map(str::trim).filter(|word| !word.is_empty()).map(str::to_owned).collect();
    let atoms: Vec<Value> = sections
        .as_ref()
        .map(|sections| card_items(&sections.atoms).into_iter().map(|line| {
            let (kind, text) = parse_atom(&line);
            json!({"type": kind, "text": text})
        }).collect())
        .unwrap_or_default();
    let record = info.map(|info| json!({
        "started_ts": info.first_ts,
        "last_ts": info.last_ts,
        "fragments": info.fragments,
        "project": info.project,
        "project_name": info.project.rsplit('/').next().unwrap_or(""),
        "project_path": info.project_path,
        "file": info.file,
        "node_id": info.node_id(),
        "has_card": info.card_id != 0,
    }));
    json!({
        "id": card.id,
        "source": card.source,
        "source_ref": card.source_ref,
        "ts": card.ts,
        "date": format_date(card.ts),
        "title": card.title,
        "body": card.body,
        "kind": card.kind,
        "unit": card.unit,
        "fragments": fragments.iter().map(|fragment| json!({
            "id": fragment.id, "title": fragment.title, "date": format_date(fragment.ts)
        })).collect::<Vec<_>>(),
        "unit_card": parent.map(|parent| json!({"id": parent.id, "title": parent.title})),
        "record": record,
        "summary": sections.as_ref().map(|sections| sections.summary.clone()).unwrap_or_default(),
        "questions": sections.as_ref().map(|sections| card_items(&sections.questions)).unwrap_or_default(),
        "atoms": atoms,
        "keywords": keywords,
        "similar": similar_cards(card, index, db),
        "transcript": transcript.chars().take(120_000).collect::<String>(),
        "transcript_chars": transcript.chars().count(),
    })
}

fn query_params(target: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    let Some(query) = target.split_once('?').map(|(_, query)| query) else {
        return params;
    };
    for pair in query.split('&') {
        // Фронт собирает query через URLSearchParams: пробел приходит как '+'.
        let pair = pair.replace('+', " ");
        let (key, value) = pair.split_once('=').unwrap_or((&pair, ""));
        if let Some(key) = percent_decode(key).filter(|key| !key.is_empty()) {
            let value = percent_decode(value).unwrap_or_default();
            params.insert(key, value);
        }
    }
    params
}

fn ui_lang(root: &Path) -> &'static str {
    let Ok(text) = fs::read_to_string(ui_file(root)) else {
        return "auto";
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return "auto";
    };
    match value.get("lang").and_then(Value::as_str) {
        Some(lang) if UI_LANGS.contains(&lang) => match lang {
            "ru" => "ru",
            "en" => "en",
            _ => "auto",
        },
        _ => "auto",
    }
}

fn save_ui_lang(root: &Path, body: &[u8]) -> Response {
    let lang = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("lang").and_then(Value::as_str).map(str::to_owned));
    let Some(lang) = lang.filter(|lang| UI_LANGS.contains(&lang.as_str())) else {
        return Response::json(400, json!({"error": "lang: auto, ru or en"}));
    };
    let path = ui_file(root);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return Response::json(500, json!({"error": "cannot create data dir"}));
        }
    }
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, serde_json::to_vec(&json!({"lang": lang})).unwrap_or_default()).is_err()
        || fs::rename(&tmp, &path).is_err()
    {
        return Response::json(500, json!({"error": "cannot save ui"}));
    }
    Response::json(200, json!({"lang": lang}))
}

fn host_allowed(host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    let name = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
    matches!(name, "127.0.0.1" | "localhost")
}

fn origin_allowed(headers: &HashMap<String, String>, _root: &Path) -> bool {
    let Some(origin) = headers.get("origin") else { return true };
    let Some((scheme, rest)) = origin.split_once("://") else { return false };
    if scheme != "http" {
        return false;
    }
    // Свой origin совпадает с Host вплоть до порта; страница с другого loopback-порта
    // (dev-сервер, чужой инструмент) не должна писать настройки и уводить base_url.
    let authority = rest.split('/').next().unwrap_or("");
    headers.get("host").is_some_and(|host| host.eq_ignore_ascii_case(authority))
}

/// `CON`, `COM1`, `CONIN$` и т. п. на Windows открывают устройство, а не файл:
/// `<img src=".../CON">` с любой страницы навсегда вешал бы поток сервера на чтении.
fn is_dos_device(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ').to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
}

fn serve_static(root: &Path, path: &str) -> Response {
    let decoded = match percent_decode(path) {
        Some(decoded) => decoded,
        None => return Response::text(400, "text/plain; charset=utf-8", "bad path"),
    };
    let trimmed = decoded.trim_start_matches('/');
    let relative = if trimmed.is_empty() { "index.html" } else { trimmed };
    // ':' — дисковый префикс: на Windows join("C:/…") подменяет корень static.
    if relative.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        || relative.contains(['\\', ':'])
        || relative.split('/').any(is_dos_device)
    {
        return Response::text(404, "text/plain; charset=utf-8", "not found");
    }
    let file = root.join("static").join(relative);
    let bytes = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(_) => match crate::web_assets::get(relative) {
            Some(bytes) => bytes.to_vec(),
            None => return Response::text(404, "text/plain; charset=utf-8", "not found"),
        },
    };
    Response::text(200, content_type(relative), bytes)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            output.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    fn exchange(request: &str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().to_path_buf();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = super::handle_connection(stream, &root_path);
        });
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        let _ = client.read_to_string(&mut reply);
        server.join().unwrap();
        reply
    }

    #[test]
    fn oversized_or_chunked_requests_get_a_status_not_a_reset() {
        let big = exchange("POST /api/sync HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nContent-Length: 999999999\r\n\r\n");
        assert!(big.starts_with("HTTP/1.1 413"), "{big}");
        let chunked = exchange("POST /api/sync HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n");
        assert!(chunked.starts_with("HTTP/1.1 411"), "{chunked}");
        let bad = exchange("POST /api/sync HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nContent-Length: abc\r\n\r\n");
        assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
    }

    #[test]
    fn dos_device_names_are_not_files() {
        for name in ["CON", "con.txt", "COM1", "lpt9.html", "CONIN$", "nul"] {
            assert!(super::is_dos_device(name), "{name}");
        }
        for name in ["app.js", "console.css", "COM", "COMX", "index.html"] {
            assert!(!super::is_dos_device(name), "{name}");
        }
    }

    use super::*;

    fn options() -> (tempfile::TempDir, PathBuf, HashMap<String, String>) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("static")).unwrap();
        fs::write(dir.path().join("static/index.html"), "<h1>dash</h1>").unwrap();
        let root = dir.path().to_owned();
        let mut headers = HashMap::new();
        headers.insert("host".to_owned(), "127.0.0.1:8765".to_owned());
        (dir, root, headers)
    }

    #[test]
    fn serves_static_and_rejects_traversal() {
        let (_dir, root, headers) = options();
        let response = route("GET", "/", &headers, &[], &root);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"<h1>dash</h1>");
        let response = route("GET", "/../data/memory.db", &headers, &[], &root);
        assert_eq!(response.status, 404);
        let response = route("GET", "/%2e%2e/secret", &headers, &[], &root);
        assert_eq!(response.status, 404);
        let response = route("GET", "/C:/Windows/win.ini", &headers, &[], &root);
        assert_eq!(response.status, 404);
        let response = route("GET", "/C%3A/Windows/win.ini", &headers, &[], &root);
        assert_eq!(response.status, 404);
    }

    #[test]
    fn query_plus_is_space_like_url_search_params() {
        let params = query_params("/api/search?q=two+words&p=a%2Bb");
        assert_eq!(params["q"], "two words");
        assert_eq!(params["p"], "a+b");
    }

    #[test]
    fn snippet_survives_lowercase_that_changes_length() {
        // U+0130 в нижнем регистре даёт два символа: байтовое смещение совпадения
        // в lowercase-строке не совпадает с границей символа в исходной.
        let terms = vec!["привет".to_owned()];
        let piece = snippet("İпривет мир", &terms);
        assert!(piece.contains("привет"), "{piece}");
        assert_eq!(snippet("İ mir", &terms), "İ mir");
    }

    #[test]
    fn ui_language_roundtrip_and_validation() {
        let (_dir, root, headers) = options();
        let response = route("GET", "/api/ui", &headers, &[], &root);
        assert_eq!(response.status, 200);
        assert_eq!(serde_json::from_slice::<Value>(&response.body).unwrap()["lang"], "auto");
        let response = route("POST", "/api/ui", &headers, br#"{"lang":"ru"}"#, &root);
        assert_eq!(response.status, 200);
        let response = route("GET", "/api/ui", &headers, &[], &root);
        assert_eq!(serde_json::from_slice::<Value>(&response.body).unwrap()["lang"], "ru");
        let response = route("POST", "/api/ui", &headers, br#"{"lang":"de"}"#, &root);
        assert_eq!(response.status, 400);
    }

    #[test]
    fn host_and_origin_are_checked() {
        let (_dir, root, mut headers) = options();
        headers.insert("host".to_owned(), "evil.example".to_owned());
        assert_eq!(route("GET", "/api/ui", &headers, &[], &root).status, 403);
        let mut headers = HashMap::new();
        headers.insert("host".to_owned(), "127.0.0.1:8765".to_owned());
        headers.insert("origin".to_owned(), "https://evil.example".to_owned());
        assert_eq!(route("POST", "/api/ui", &headers, br#"{"lang":"ru"}"#, &root).status, 403);
        headers.insert("origin".to_owned(), "http://127.0.0.1:9999".to_owned());
        assert_eq!(route("POST", "/api/ui", &headers, br#"{"lang":"ru"}"#, &root).status, 403);
        headers.insert("origin".to_owned(), "http://127.0.0.1:8765".to_owned());
        assert_eq!(route("POST", "/api/ui", &headers, br#"{"lang":"ru"}"#, &root).status, 200);
    }

    #[test]
    fn every_mutating_route_rejects_foreign_origin() {
        let (_dir, root, headers) = options();
        let mut foreign = headers.clone();
        foreign.insert("origin".to_owned(), "https://evil.example".to_owned());
        for (method, path, body) in [
            ("POST", "/api/sync", &[][..]),
            ("POST", "/api/settings", br#"{"base_url":"http://127.0.0.1:1/v1","model":"m"}"#),
            ("POST", "/api/handoff", br#"{"chat":"x","target":"claude"}"#),
            ("POST", "/api/sync/schedule", br#"{"mode":"manual"}"#),
        ] {
            assert_eq!(route(method, path, &foreign, body, &root).status, 403, "{method} {path}");
        }
        // Без Origin запрос не браузерный и проходит дальше по маршруту, но
        // невалидный режим останавливает его до записи и планировщика.
        let after = route("POST", "/api/sync/schedule", &headers, br#"{"mode":"nope"}"#, &root);
        assert_eq!(after.status, 400);
    }

    #[test]
    fn unknown_api_is_json_404() {
        let (_dir, root, headers) = options();
        let response = route("GET", "/api/nope", &headers, &[], &root);
        assert_eq!(response.status, 404);
        assert_eq!(serde_json::from_slice::<Value>(&response.body).unwrap()["error"], "not found");
    }

    #[test]
    fn local_status_does_not_start_the_engine() {
        let (_dir, root, headers) = options();
        let response = route("GET", "/api/local/status", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert!(value["hardware"]["tier"].is_string());
        assert_eq!(value["runtime"]["installed"], false);
        assert_eq!(value["route"]["kind"], "none");
        assert!(value["catalog"].as_array().is_some_and(|models| !models.is_empty()));
    }

    #[test]
    fn local_route_accepts_only_installed_models() {
        let (_dir, root, headers) = options();
        let store = root.join("models").join("ollama").join("manifests-v2").join("ollama.com").join("brepsi");
        fs::create_dir_all(store.join("qwen25-7b")).unwrap();

        let unknown = route("POST", "/api/local/route", &headers, br#"{"model":"brepsi/other"}"#, &root);
        assert_eq!(unknown.status, 400);

        let saved = route("POST", "/api/local/route", &headers, br#"{"model":"brepsi/qwen25-7b"}"#, &root);
        assert_eq!(saved.status, 200);
        let value: Value = serde_json::from_slice(&saved.body).unwrap();
        assert_eq!(value["local_route"]["model"], "brepsi/qwen25-7b");
        assert_eq!(value["local_route"]["installed"][0], "brepsi/qwen25-7b");
        assert_eq!(value["local_route"]["configured"], true);

        let cleared = route("POST", "/api/local/route", &headers, br#"{"model":""}"#, &root);
        let value: Value = serde_json::from_slice(&cleared.body).unwrap();
        assert_eq!(value["local_route"]["configured"], false);
    }

    #[test]
    fn local_interval_cannot_beat_the_scheduler() {
        let (_dir, root, headers) = options();
        let post = |body: &[u8]| route("POST", "/api/sync/schedule", &headers, body, &root);
        assert_eq!(post(br#"{"mode":"interval","interval_minutes":30,"local_interval_minutes":15}"#).status, 400);
        assert_eq!(post(br#"{"mode":"interval","interval_minutes":30,"local_interval_minutes":2}"#).status, 400);
        let saved = post(br#"{"mode":"manual","interval_minutes":30,"local_interval_minutes":60}"#);
        assert_eq!(saved.status, 200);
        let value: Value = serde_json::from_slice(&saved.body).unwrap();
        assert_eq!(value["local_interval_minutes"], 60);
        // Без поля значение сохраняется: частичное сохранение его не стирает.
        let kept = post(br#"{"mode":"manual","interval_minutes":45}"#);
        let value: Value = serde_json::from_slice(&kept.body).unwrap();
        assert_eq!(value["local_interval_minutes"], 60);
    }

    #[test]
    fn local_settings_validation_rejects_bad_values() {
        let (_dir, root, headers) = options();
        for body in [
            br#"{"models_root":"relative/path"}"#.as_slice(),
            br#"{"context_tokens":3000}"#.as_slice(),
            br#"{"idle_timeout_seconds":2}"#.as_slice(),
            br#"{"mode":"sometimes"}"#.as_slice(),
            br#"{"offline_only":"yes"}"#.as_slice(),
        ] {
            let response = route("POST", "/api/local/settings", &headers, body, &root);
            assert_eq!(response.status, 400, "{}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn offline_only_can_be_set_before_any_model_and_blocks_the_remote_provider() {
        let (_dir, root, headers) = options();
        fs::write(root.join("config.toml"), "[llm]\nbase_url = \"https://provider.example/v1\"\nmodel = \"remote\"\n").unwrap();
        let before: Value = serde_json::from_slice(&route("GET", "/api/local/status", &headers, &[], &root).body).unwrap();
        assert_eq!(before["route"]["kind"], "remote");

        let response = route("POST", "/api/local/settings", &headers, br#"{"offline_only":true}"#, &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["settings"]["offline_only"], true);
        assert_eq!(value["route"]["kind"], "none", "без локальной модели наружу не идём");

        let listing = route("POST", "/api/models", &headers, br#"{"base_url":"https://provider.example/v1"}"#, &root);
        assert_eq!(listing.status, 409);
    }

    #[test]
    fn install_rejects_unknown_model_and_busy_is_not_started() {
        let (_dir, root, headers) = options();
        let response = route("POST", "/api/local/install", &headers, br#"{"id":"no-such-model"}"#, &root);
        assert_eq!(response.status, 400);
    }

    #[test]
    fn models_stats_endpoint_serves_empty_aggregates() {
        let (_dir, root, headers) = options();
        let response = route("GET", "/api/models/stats?days=30", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["hours"]["total"], 0.0);
        assert!(value["sessions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn manual_sync_state_is_reported_as_last_sync() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        db.set_state("web:last_sync", "4242").unwrap();
        drop(db);
        let response = route("GET", "/api/stats", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["last_sync_ts"], 4242);
    }

    #[test]
    fn project_proposals_require_a_model() {
        let (_dir, root, headers) = options();
        let response = route("POST", "/api/projects/propose", &headers, br#"{"limit":5}"#, &root);
        assert_eq!(response.status, 400);
        assert!(serde_json::from_slice::<Value>(&response.body).unwrap()["error"].is_string());
    }

    #[test]
    fn sync_schedule_rejects_invalid_interval_without_touching_scheduler() {
        let (_dir, root, headers) = options();
        assert_eq!(route("POST", "/api/sync/schedule", &headers, br#"{"mode":"hourly"}"#, &root).status, 400);
        assert_eq!(
            route("POST", "/api/sync/schedule", &headers, br#"{"mode":"interval","interval_minutes":2}"#, &root).status,
            400
        );
        // Режим обязателен: пустое тело не должно ни писать конфиг, ни трогать
        // планировщик (именно этот случай когда-то создал реальную задачу из теста).
        assert_eq!(route("POST", "/api/sync/schedule", &headers, br#"{}"#, &root).status, 400);
    }

    #[test]
    fn task_models_are_saved_into_the_request_root() {
        let (_dir, root, headers) = options();
        let response = route(
            "POST",
            "/api/settings/tasks",
            &headers,
            br#"{"models":{"distill":"brepsi/qwen25-7b","answer":""}}"#,
            &root,
        );
        assert_eq!(response.status, 200);
        let text = fs::read_to_string(root.join("config.toml")).unwrap();
        let value: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(value["llm"]["tasks"]["distill"].as_str(), Some("brepsi/qwen25-7b"));
    }

    #[test]
    fn landscape_endpoint_reads_the_database() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        db.insert_card(&crate::db::CardDraft::new(
            "git",
            "git:repo:a1",
            crate::db::now_ts(),
            "коммит",
            "архитектура памяти",
        ))
        .unwrap();
        drop(db);
        let response = route("GET", "/api/landscape?days=0", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(value["nodes"][0]["project"], "repo");
    }

    #[test]
    fn stats_endpoint_counts_records_and_sources() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        db.insert_card(&crate::db::CardDraft::new(
            "git",
            "git:repo:a1",
            crate::db::now_ts(),
            "коммит",
            "архитектура памяти",
        ))
        .unwrap();
        drop(db);
        let response = route("GET", "/api/stats", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["total"], 1);
        assert_eq!(value["records_total"], 1);
        assert_eq!(value["sources"].as_array().unwrap()[0]["source"], "git");
        assert_eq!(value["knowledge"]["total"], 0);
    }

    #[test]
    fn search_records_and_card_are_served() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        db.insert_card(&crate::db::CardDraft::new(
            "git",
            "git:repo:a1",
            crate::db::now_ts(),
            "GitHub release",
            "Published the GitHub release with the exe asset",
        ))
        .unwrap();
        drop(db);

        let response = route("GET", "/api/search?q=GitHub&limit=5", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["cards"].as_array().unwrap().len(), 1);
        let id = value["cards"][0]["id"].as_i64().unwrap();

        let response = route("GET", &format!("/api/card?id={id}"), &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["title"], "GitHub release");

        let response = route("GET", "/api/records?limit=5", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["total"], 1);
        assert!(value["records"][0]["unit"].as_str().unwrap().starts_with("git:repo:"));
    }

    #[test]
    fn timeline_and_projects_are_served() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        db.insert_card(&crate::db::CardDraft::new(
            "git",
            "git:repo:a1",
            crate::db::now_ts(),
            "коммит",
            "архитектура памяти",
        ))
        .unwrap();
        drop(db);
        let response = route("GET", "/api/timeline?weeks=2", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["total"], 1);
        assert_eq!(value["weeks"].as_array().unwrap().len(), 2);
        let response = route("GET", "/api/projects?limit=5", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["projects"][0]["project"], "repo");
        // Страница «Проекты» просит 1000: этот лимит обязан проходить, а
        // 1001 — по-прежнему 400.
        let response = route("GET", "/api/projects?limit=1000", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let response = route("GET", "/api/projects?limit=1001", &headers, &[], &root);
        assert_eq!(response.status, 400);
    }

    #[test]
    fn graph_is_built_from_a_seed() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        let seed = crate::db::CardDraft::new(
            "git", "git:repo:a1", crate::db::now_ts(), "seed", "architecture memory",
        );
        db.insert_card(&seed).unwrap();
        db.insert_card(&crate::db::CardDraft::new(
            "git", "git:repo:a2", crate::db::now_ts() - 10, "near", "architecture memory notes",
        ))
        .unwrap();
        drop(db);
        let response = route("GET", "/api/graph?seed=1&limit=5&k=2", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["nodes"][0]["id"], 1);
        assert!(value["links"].as_array().is_some());
    }

    #[test]
    fn card_similar_lists_nearest_cards() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        db.insert_card(&crate::db::CardDraft::new(
            "git", "git:repo:a1", crate::db::now_ts(), "seed", "architecture memory storage",
        )).unwrap();
        db.insert_card(&crate::db::CardDraft::new(
            "git", "git:repo:a2", crate::db::now_ts() - 10, "near", "architecture memory storage notes",
        )).unwrap();
        db.insert_card(&crate::db::CardDraft::new(
            "git", "git:repo:a3", crate::db::now_ts() - 20, "far", "completely unrelated cooking recipes",
        )).unwrap();
        drop(db);
        let response = route("GET", "/api/card?id=1", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        let similar = value["similar"].as_array().unwrap();
        assert_eq!(similar.len(), 1, "{value}");
        assert_eq!(similar[0]["id"], 2);
        assert_eq!(similar[0]["title"], "near");
        assert_eq!(similar[0]["source"], "git");
        assert_eq!(similar[0]["project"], "repo");
        assert!(similar[0]["weight"].as_f64().unwrap() > 0.05);
    }

    #[test]
    fn record_row_orphan_has_null_id_and_title() {
        let (_dir, root, _headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        let info = crate::db::UnitInfo {
            unit: "claude:s1".into(), source: "claude".into(), first_ts: 1, last_ts: 2,
            fragments: 1, max_id: 1, covered_id: 0, project: "proj".into(), project_path: String::new(),
            file: String::new(), card_id: 9, title: "заголовок записи".into(), keywords: String::new(),
        };
        let index = HashMap::new();
        // Осиротевшая ссылка unit→card: карточка не читается, строка не должна
        // вести на /api/card?id=undefined.
        let row = record_row(&info, None, &index, &db);
        assert!(row["id"].is_null(), "{row}");
        assert_eq!(row["title"], "заголовок записи");
        assert_eq!(row["unit"], "claude:s1");
        assert_eq!(row["source"], "claude");
        assert_eq!(row["project"], "proj");
        assert_eq!(row["has_card"], true);
    }

    #[test]
    fn queue_failed_list_and_retry() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        let mut draft = crate::db::CardDraft::new("claude", "claude:s1", crate::db::now_ts(), "t1", "body");
        draft.unit = "claude:s1".into();
        db.insert_card(&draft).unwrap();
        for _ in 0..crate::llm::MAX_UNIT_ATTEMPTS {
            db.mark_unit_failed("claude:s1", 1, "boom").unwrap();
        }
        drop(db);

        let response = route("GET", "/api/queue/failed", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        let failed = value["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 1, "{value}");
        assert_eq!(failed[0]["unit"], "claude:s1");
        assert_eq!(failed[0]["title"], "t1");
        assert_eq!(failed[0]["attempts"], crate::llm::MAX_UNIT_ATTEMPTS);
        assert_eq!(failed[0]["last_error"], "boom");

        let response = route("POST", "/api/queue/retry", &headers, br#"{"unit":"claude:s1"}"#, &root);
        assert_eq!(response.status, 200);
        assert_eq!(serde_json::from_slice::<Value>(&response.body).unwrap()["requeued"], 1);
        let value: Value = serde_json::from_slice(&route("GET", "/api/queue/failed", &headers, &[], &root).body).unwrap();
        assert_eq!(value["failed"].as_array().unwrap().len(), 0);
        // Повторный повтор уже не-failed записи — ошибка, а не молчаливый успех.
        assert_eq!(route("POST", "/api/queue/retry", &headers, br#"{"unit":"claude:s1"}"#, &root).status, 400);

        let mut foreign = headers.clone();
        foreign.insert("origin".to_owned(), "https://evil.example".to_owned());
        assert_eq!(route("POST", "/api/queue/retry", &foreign, br#"{"all":true}"#, &root).status, 403);
    }

    #[test]
    fn queue_retry_all_requeues_every_failed() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        let db = Database::open(root.join("data/memory.db")).unwrap();
        for (n, unit) in ["claude:s1", "claude:s2"].iter().enumerate() {
            let mut draft = crate::db::CardDraft::new("claude", unit, crate::db::now_ts(), "t", "body");
            draft.unit = (*unit).into();
            db.insert_card(&draft).unwrap();
            for _ in 0..crate::llm::MAX_UNIT_ATTEMPTS {
                db.mark_unit_failed(unit, (n + 1) as i64, "boom").unwrap();
            }
        }
        drop(db);
        let response = route("POST", "/api/queue/retry", &headers, br#"{"all":true}"#, &root);
        assert_eq!(response.status, 200);
        assert_eq!(serde_json::from_slice::<Value>(&response.body).unwrap()["requeued"], 2);
        let value: Value = serde_json::from_slice(&route("GET", "/api/queue/failed", &headers, &[], &root).body).unwrap();
        assert_eq!(value["failed"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn log_endpoint_whitelist_tail_and_missing() {
        let (_dir, root, headers) = options();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(root.join("data/sync.log"), "FIRST\nmiddle\nlast\n").unwrap();
        let response = route("GET", "/api/log?name=sync.log", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["name"], "sync.log");
        assert!(value["text"].as_str().unwrap().contains("last"));
        assert_eq!(value["truncated"], false);

        assert_eq!(route("GET", "/api/log?name=../config.toml", &headers, &[], &root).status, 400);
        assert_eq!(route("GET", "/api/log?name=memory.db", &headers, &[], &root).status, 400);
        assert_eq!(route("GET", "/api/log?name=ollama-serve.log", &headers, &[], &root).status, 404);
        assert_eq!(route("GET", "/api/log?name=sync.log&tail_kb=0", &headers, &[], &root).status, 400);

        let big = format!("FIRST{}\nlast\n", "x".repeat(4096));
        fs::write(root.join("data/sync.log"), big).unwrap();
        let value: Value = serde_json::from_slice(&route("GET", "/api/log?name=sync.log&tail_kb=1", &headers, &[], &root).body).unwrap();
        assert_eq!(value["truncated"], true);
        let text = value["text"].as_str().unwrap();
        assert!(text.contains("last"), "{text}");
        assert!(!text.contains("FIRST"), "{text}");
    }

    #[test]
    fn schedule_get_is_read_only_state() {
        let (_dir, root, headers) = options();
        let response = route("GET", "/api/sync/schedule", &headers, &[], &root);
        assert_eq!(response.status, 200);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["mode"], "manual");
        assert_eq!(value["state"], "absent");
        assert_eq!(value["task"], false);
        assert!(value["next_run_ts"].is_null());
        assert!(value.get("last_result").is_some());
    }

    #[test]
    fn settings_fallback_model_roundtrip() {
        let (_dir, root, headers) = options();
        let save = |body: &[u8]| route("POST", "/api/settings", &headers, body, &root);
        let get = || serde_json::from_slice::<Value>(&route("GET", "/api/settings", &headers, &[], &root).body).unwrap();

        let response = save(br#"{"base_url":"https://api.example/v1","model":"m","fallback_model":"fb"}"#);
        assert_eq!(response.status, 200);
        assert_eq!(serde_json::from_slice::<Value>(&response.body).unwrap()["fallback_model"], "fb");
        assert_eq!(get()["fallback_model"], "fb");
        // Поле не прислали — существующее значение не стирается.
        assert_eq!(save(br#"{"base_url":"https://api.example/v1","model":"m"}"#).status, 200);
        assert_eq!(get()["fallback_model"], "fb");
        // Пустая строка снимает запасную модель.
        assert_eq!(save(br#"{"base_url":"https://api.example/v1","model":"m","fallback_model":""}"#).status, 200);
        assert_eq!(get()["fallback_model"], "");
        assert_eq!(save(br#"{"base_url":"https://api.example/v1","model":"m","fallback_model":"a\nb"}"#).status, 400);
        let long = "x".repeat(201);
        assert_eq!(save(format!(r#"{{"base_url":"https://api.example/v1","model":"m","fallback_model":"{long}"}}"#).as_bytes()).status, 400);
    }

    #[test]
    fn embedded_static_works_without_a_static_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut headers = HashMap::new();
        headers.insert("host".to_owned(), "127.0.0.1:8765".to_owned());
        let response = route("GET", "/", &headers, &[], dir.path());
        assert_eq!(response.status, 200);
        assert!(String::from_utf8_lossy(&response.body).contains("Brepsi Brain"));
    }
}
