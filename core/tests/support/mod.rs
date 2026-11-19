//! Подменный OpenAI-совместимый провайдер: настоящий HTTP на 127.0.0.1 с
//! эфемерным портом, ответы по сценарию, все запросы записываются.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use memory_core::db::{CardDraft, Database};

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Имена в нижнем регистре.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("request body is JSON")
    }
}

type Handler = Box<dyn Fn(&Recorded, usize) -> (u16, String) + Send + Sync>;

pub struct FakeProvider {
    pub base: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeProvider {
    /// handler(запрос, номер запроса с нуля) → (HTTP-статус, тело).
    /// Поток сервера живёт до конца процесса тестов: каждый тест берёт свой порт.
    pub fn start(handler: impl Fn(&Recorded, usize) -> (u16, String) + Send + Sync + 'static) -> FakeProvider {
        Self::start_at("127.0.0.1", handler)
    }

    /// На другом адресе loopback (127.0.0.2): для гейта приватности это нелокальный хост.
    pub fn start_at(ip: &str, handler: impl Fn(&Recorded, usize) -> (u16, String) + Send + Sync + 'static) -> FakeProvider {
        let listener = TcpListener::bind((ip, 0)).unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let handler: Handler = Box::new(handler);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(request) = read_request(&mut stream) else { continue };
                let index = {
                    let mut log = log.lock().unwrap();
                    log.push(request.clone());
                    log.len() - 1
                };
                let (status, body) = handler(&request, index);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body.as_bytes()));
            }
        });
        FakeProvider { base, requests }
    }

    pub fn reply(content: &str) -> FakeProvider {
        let body = completion(content);
        FakeProvider::start(move |_, _| (200, body.clone()))
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Recorded> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (k, v) = header.split_once(':')?;
        headers.push((k.trim().to_lowercase(), v.trim().to_string()));
    }
    let length: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Recorded { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

pub fn completion(content: &str) -> String {
    serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string()
}

pub fn rejection(message: &str) -> String {
    serde_json::json!({"error": {"message": message, "code": "400"}}).to_string()
}

/// Порт, на котором никто не слушает: сбой соединения.
pub fn closed_port_base() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/v1")
}

pub const DAY: i64 = 86_400;
/// Часы фикстур пересказа: все записи давно затихли.
pub const NOW: i64 = 1_790_000_000;

/// Как фикстуры conftest после ingest_all: 8 фрагментов = разговоры kimi,
/// claude, codex по два хода и два рабочих дня git.
pub fn seed_sources(db: &Database) {
    let t = NOW - 5 * DAY;
    let drafts = [
        CardDraft::new("kimi", "kimi:wd1:s1:a1:0", t, "как устроен второй мозг", "Вопрос: как устроен второй мозг?\nОтвет: SQLite и FTS5."),
        CardDraft::new("kimi", "kimi:wd1:s1:a1:1", t + 60, "А секреты?", "Вопрос: А секреты?\nОтвет: маскируются."),
        CardDraft::new("claude", "claude:c1:0", t + 120, "индексация claude", "Вопрос: индексация?\nОтвет: по ходам."),
        CardDraft::new("claude", "claude:c1:1", t + 180, "водяные знаки", "Вопрос: водяные знаки?\nОтвет: по файлу."),
        CardDraft::new("codex", "codex:x1:0", t + 240, "сессии codex", "Вопрос: codex?\nОтвет: rollout-файлы."),
        CardDraft::new("codex", "codex:x1:1", t + 300, "guardian", "Вопрос: guardian?\nОтвет: пропускаем."),
        CardDraft::new("git", "git:demo-repo:aaa111", t, "feat: поиск", "feat: поиск"),
        CardDraft::new("git", "git:demo-repo:bbb222", t + DAY, "fix: свежесть", "fix: свежесть\n\nполураспад 120 дней"),
    ];
    for draft in &drafts {
        assert!(db.insert_card(draft).unwrap());
    }
}
