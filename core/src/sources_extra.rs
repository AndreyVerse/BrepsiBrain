//! Внешние источники: экспорт ChatGPT. Остальные коннекторы (Gmail/Яндекс,
//! GitHub, Notion) переносятся следующими шагами; пока они возвращают 0.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::db::{now_ts, CardDraft, Database};
use crate::ingest::{mask_secrets, parse_iso, truncate};

const TITLE_MAX_CHARS: usize = 80;
const BODY_MAX_CHARS: usize = 4_000;
const GITHUB_API: &str = "https://api.github.com";
const GITHUB_PAGE_SIZE: usize = 100;
const GITHUB_MAX_REPOS: usize = 200;
const GITHUB_MAX_COMMITS: usize = 300;
const NOTION_API: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";
const NOTION_MAX_RESULTS: usize = 200;
const NOTION_BLOCK_BATCH: usize = 50;
const NOTION_SNIPPET_CHARS: usize = 1_500;

fn epoch(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_f64().map(|value| value as i64),
        Value::String(text) if !text.trim().is_empty() => text.trim().parse::<f64>().ok().map(|value| value as i64),
        _ => None,
    }
}

fn messages(conversation: &Value) -> Vec<(String, String, Option<i64>)> {
    let Some(mapping) = conversation.get("mapping").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut node_id = conversation.get("current_node").and_then(Value::as_str).map(str::to_owned);
    while let Some(id) = node_id {
        if !mapping.contains_key(&id) || !seen.insert(id.clone()) {
            break;
        }
        chain.push(id.clone());
        node_id = mapping.get(&id).and_then(|node| node.get("parent")).and_then(Value::as_str).map(str::to_owned);
    }
    chain.reverse();
    let mut result = Vec::new();
    for id in chain {
        let Some(node) = mapping.get(&id).and_then(Value::as_object) else { continue };
        let Some(message) = node.get("message").and_then(Value::as_object) else { continue };
        let role = message.get("author").and_then(|author| author.get("role")).and_then(Value::as_str);
        if !matches!(role, Some("user" | "assistant")) {
            continue;
        }
        let Some(parts) = message.get("content").and_then(|content| content.get("parts")).and_then(Value::as_array) else {
            continue;
        };
        let text = parts.iter().filter_map(Value::as_str).filter(|part| !part.trim().is_empty()).collect::<Vec<_>>().join("\n");
        if text.trim().is_empty() {
            continue;
        }
        result.push((role.unwrap_or("user").to_owned(), text.trim().to_owned(), epoch(message.get("create_time"))));
    }
    result
}

fn finalize(text: &str, limit: usize) -> String {
    truncate(&mask_secrets(text), limit)
}

fn ingest_chatgpt(db: &Database, config: &Config) -> usize {
    let Some(configured) = config.extra_sources.chatgpt_export.path.as_deref() else {
        return 0;
    };
    let file = if configured.is_file() {
        configured.to_path_buf()
    } else {
        let candidate = configured.join("conversations.json");
        if !candidate.is_file() {
            return 0;
        }
        candidate
    };
    let Ok(text) = std::fs::read_to_string(&file) else { return 0 };
    let Ok(Value::Array(conversations)) = serde_json::from_str::<Value>(&text) else { return 0 };
    let key = format!("chatgpt:watermark:{}", file.canonicalize().unwrap_or(file.clone()).display());
    let watermark = db.get_state(&key).ok().flatten().and_then(|value| value.parse::<f64>().ok()).unwrap_or(0.0);
    let mtime = std::fs::metadata(&file).ok().and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_else(crate::db::now_ts);
    let mut inserted = 0;
    let mut max_update = watermark;
    for conversation in conversations {
        let Some(id) = conversation.get("id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else {
            continue;
        };
        let update = epoch(conversation.get("update_time")).unwrap_or(0);
        if update > 0 && (update as f64) < watermark {
            continue;
        }
        max_update = max_update.max(update as f64);
        let messages = messages(&conversation);
        if messages.is_empty() {
            continue;
        }
        let title_source = conversation.get("title").and_then(Value::as_str).filter(|title| !title.trim().is_empty())
            .unwrap_or(&messages[0].1);
        let ts = epoch(conversation.get("create_time")).or(Some(update).filter(|value| *value > 0))
            .or_else(|| messages.iter().find_map(|(_, _, ts)| *ts)).unwrap_or(mtime);
        let body = messages.iter().map(|(role, text, _)| format!("{role}: {text}")).collect::<Vec<_>>().join("\n\n");
        let draft = CardDraft::new(
            "chatgpt",
            &format!("chatgpt:{id}"),
            ts,
            &finalize(title_source.trim(), TITLE_MAX_CHARS),
            &finalize(&body, BODY_MAX_CHARS),
        );
        match db.get_card_by_ref(&draft.source_ref).ok().flatten() {
            None => {
                if db.insert_card(&draft).unwrap_or(false) {
                    inserted += 1;
                }
            }
            Some(existing) if existing.title != draft.title || existing.body != draft.body => {
                let _ = db.update_card(existing.id, draft.ts, &draft.title, &draft.body, None, true);
            }
            Some(_) => {}
        }
    }
    let _ = db.set_state(&key, &max_update.to_string());
    inserted
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

fn iso_utc(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let seconds = ts.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn github_agent() -> ureq::Agent {
    // Дефолт RootCerts::WebPki без фичи native-tls-webpki-roots — panic!, а в release panic = abort.
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn github_get(agent: &ureq::Agent, token: &str, path: &str) -> Option<Value> {
    let response = agent
        .get(format!("{GITHUB_API}{path}"))
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "memory-base")
        .call()
        .ok()?;
    let mut response = response;
    let body = response.body_mut().read_to_vec().ok()?;
    serde_json::from_slice(&body).ok()
}

fn github_pages(agent: &ureq::Agent, token: &str, path: &str, limit: usize) -> Option<Vec<Value>> {
    let mut items = Vec::new();
    let mut page = 1;
    let separator = if path.contains('?') { '&' } else { '?' };
    while items.len() < limit {
        let data = github_get(agent, token, &format!("{path}{separator}per_page={GITHUB_PAGE_SIZE}&page={page}"))?;
        let rows = data.as_array()?;
        items.extend(rows.iter().filter(|row| row.is_object()).cloned());
        if rows.len() < GITHUB_PAGE_SIZE {
            break;
        }
        page += 1;
    }
    items.truncate(limit);
    Some(items)
}

fn local_commit_exists(db: &Database, sha: &str) -> bool {
    db.connection()
        .query_row(
            "SELECT 1 FROM cards WHERE source='git' AND source_ref LIKE ? LIMIT 1",
            [format!("git:%:{sha}")],
            |_| Ok(()),
        )
        .is_ok()
}

const GITHUB_SINCE_MARGIN_SECONDS: i64 = 7 * 86_400;

fn ingest_github(db: &Database, config: &Config) -> usize {
    let Some(token) = config.extra_sources.github.token.as_deref().filter(|token| !token.is_empty()) else {
        return 0;
    };
    let agent = github_agent();
    let Some(user) = github_get(&agent, token, "/user") else { return 0 };
    let Some(login) = user.get("login").and_then(Value::as_str).filter(|login| !login.is_empty()) else {
        return 0;
    };
    let Some(repos) = github_pages(&agent, token, "/user/repos?affiliation=owner&sort=pushed", GITHUB_MAX_REPOS) else {
        return 0;
    };
    let mut inserted = 0;
    for repo in repos {
        let Some(name) = repo.get("full_name").and_then(Value::as_str).filter(|name| !name.is_empty()) else {
            continue;
        };
        let since_key = format!("github:since:{name}");
        let since = db.get_state(&since_key).ok().flatten().filter(|value| !value.is_empty());
        let mut path = format!("/repos/{name}/commits?author={login}");
        if let Some(since) = &since {
            path.push_str(&format!("&since={since}"));
        }
        let Some(commits) = github_pages(&agent, token, &path, GITHUB_MAX_COMMITS) else {
            continue;
        };
        let mut newest_commit: Option<i64> = None;
        for item in &commits {
            let Some(sha) = item.get("sha").and_then(Value::as_str) else { continue };
            let Some(commit) = item.get("commit").and_then(Value::as_object) else { continue };
            if local_commit_exists(db, sha) {
                continue;
            }
            let message = commit.get("message").and_then(Value::as_str).unwrap_or("").trim();
            let message = if message.is_empty() { "(no subject)" } else { message };
            let ts = commit
                .get("author")
                .and_then(|author| author.get("date"))
                .and_then(Value::as_str)
                .and_then(parse_iso)
                .unwrap_or_else(now_ts);
            let committed = commit
                .get("committer")
                .and_then(|committer| committer.get("date"))
                .and_then(Value::as_str)
                .and_then(parse_iso)
                .unwrap_or(ts);
            newest_commit = Some(newest_commit.map_or(committed, |newest| newest.max(committed)));
            let title = message.lines().next().unwrap_or(message);
            let draft = CardDraft::new(
                "github",
                &format!("github:{name}:{sha}"),
                ts,
                &finalize(title, TITLE_MAX_CHARS),
                &finalize(message, BODY_MAX_CHARS),
            );
            if db.insert_card(&draft).unwrap_or(false) {
                inserted += 1;
            }
        }
        // `since` сравнивается с датой коммита, а не push: от времени синхронизации
        // терялся бы коммит, сделанный раньше и запушенный позже. Берём самый
        // свежий коммит минус неделю запаса; повторы отсекает UNIQUE(source_ref).
        if let Some(newest) = newest_commit {
            let _ = db.set_state(&since_key, &iso_utc(newest - GITHUB_SINCE_MARGIN_SECONDS));
        }
    }
    inserted
}

fn notion_request(agent: &ureq::Agent, token: &str, method: &str, path: &str, payload: Option<&Value>) -> Option<Value> {
    let url = format!("{NOTION_API}{path}");
    let result = if method == "POST" {
        agent
            .post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Notion-Version", NOTION_VERSION)
            .header("Content-Type", "application/json")
            .send(payload.cloned().unwrap_or(Value::Null).to_string())
    } else {
        agent
            .get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Notion-Version", NOTION_VERSION)
            .call()
    };
    let mut response = match result {
        Ok(response) => response,
        Err(_) => return None,
    };
    let body = response.body_mut().read_to_vec().ok()?;
    serde_json::from_slice(&body).ok().filter(Value::is_object)
}

fn notion_page_title(page: &Value) -> String {
    let Some(properties) = page.get("properties").and_then(Value::as_object) else { return String::new() };
    for property in properties.values() {
        if property.get("type").and_then(Value::as_str) != Some("title") {
            continue;
        }
        let Some(title) = property.get("title").and_then(Value::as_array) else { continue };
        let text = title.iter().filter_map(|part| part.get("plain_text").and_then(Value::as_str)).collect::<String>();
        if !text.trim().is_empty() {
            return text.trim().to_owned();
        }
    }
    String::new()
}

/// `None` — запрос не удался (429, 5xx): страницу надо перечитать в следующий раз.
fn notion_page_text(agent: &ureq::Agent, token: &str, page_id: &str) -> Option<String> {
    let object = notion_request(agent, token, "GET", &format!("/blocks/{page_id}/children?page_size={NOTION_BLOCK_BATCH}"), None)?;
    let mut lines = Vec::new();
    for block in object.get("results").and_then(Value::as_array).into_iter().flatten().take(NOTION_BLOCK_BATCH) {
        let Some(kind) = block.get("type").and_then(Value::as_str) else { continue };
        let Some(data) = block.get(kind).and_then(Value::as_object) else { continue };
        let Some(rich) = data.get("rich_text").and_then(Value::as_array) else { continue };
        let text = rich.iter().filter_map(|part| part.get("plain_text").and_then(Value::as_str)).collect::<String>().trim().to_owned();
        if text.is_empty() {
            continue;
        }
        let prefix = match kind {
            "heading_1" => "# ",
            "heading_2" => "## ",
            "heading_3" => "### ",
            "quote" => "> ",
            _ => "",
        };
        lines.push(format!("{prefix}{text}"));
    }
    Some(lines.join("\n").chars().take(NOTION_SNIPPET_CHARS).collect())
}

fn ingest_notion(db: &Database, config: &Config) -> usize {
    let Some(token) = config.extra_sources.notion.token.as_deref().filter(|token| !token.is_empty()) else {
        return 0;
    };
    let agent = github_agent();
    let watermark = db.get_state("notion:watermark").ok().flatten().unwrap_or_default();
    let mut newest = watermark.clone();
    let mut inserted = 0;
    let mut scanned = 0;
    let mut capped = false;
    // Сбой на любой странице поиска или блоков не даёт сдвинуть метку: иначе
    // необработанные страницы новее прежней метки больше никогда не читались.
    let mut failed = false;
    let mut reached_old = false;
    let mut cursor: Option<String> = None;
    while !capped && !reached_old {
        let mut payload = serde_json::json!({
            "page_size": 100,
            "filter": {"property": "object", "value": "page"},
            "sort": {"direction": "descending", "timestamp": "last_edited_time"},
        });
        if !config.extra_sources.notion.query.is_empty() {
            payload["query"] = Value::String(config.extra_sources.notion.query.clone());
        }
        if let Some(cursor) = &cursor {
            payload["start_cursor"] = Value::String(cursor.clone());
        }
        let Some(object) = notion_request(&agent, token, "POST", "/search", Some(&payload)) else {
            failed = true;
            break;
        };
        let Some(results) = object.get("results").and_then(Value::as_array) else {
            failed = true;
            break;
        };
        for page in results {
            if page.get("object").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let Some(page_id) = page.get("id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else {
                continue;
            };
            let edited = page.get("last_edited_time").and_then(Value::as_str).unwrap_or("");
            // Строго меньше: `last_edited_time` округлён до минуты, и страница,
            // правленная в ту же минуту, что и метка, иначе терялась. Уже
            // прочитанную отсекает `edited_key` ниже.
            if !watermark.is_empty() && !edited.is_empty() && edited < watermark.as_str() {
                reached_old = true;
                break;
            }
            let source_ref = format!("notion:{page_id}");
            let edited_key = format!("notion:edited:{page_id}");
            let existing = db.get_card_by_ref(&source_ref).ok().flatten();
            if existing.is_some() && !edited.is_empty() && db.get_state(&edited_key).ok().flatten().as_deref() == Some(edited) {
                continue;
            }
            if scanned >= NOTION_MAX_RESULTS {
                capped = true;
                break;
            }
            scanned += 1;
            let url = page.get("url").and_then(Value::as_str).unwrap_or("");
            let title = {
                let title = notion_page_title(page);
                if title.is_empty() { "(untitled)".to_owned() } else { title }
            };
            let ts = parse_iso(edited)
                .or_else(|| page.get("created_time").and_then(Value::as_str).and_then(parse_iso))
                .unwrap_or_else(now_ts);
            let Some(text) = notion_page_text(&agent, token, page_id) else {
                failed = true;
                continue;
            };
            if edited > newest.as_str() {
                newest = edited.to_owned();
            }
            let body = if url.is_empty() { text } else { format!("{url}\n\n{text}") };
            let draft = CardDraft::new(
                "notion",
                &source_ref,
                ts,
                &finalize(&title, TITLE_MAX_CHARS),
                &finalize(&body, BODY_MAX_CHARS),
            );
            match &existing {
                None => {
                    if db.insert_card(&draft).unwrap_or(false) {
                        inserted += 1;
                    }
                }
                Some(existing) => {
                    let _ = db.update_card(existing.id, draft.ts, &draft.title, &draft.body, None, true);
                }
            }
            if !edited.is_empty() {
                let _ = db.set_state(&edited_key, edited);
            }
        }
        if capped || reached_old || object.get("has_more") != Some(&Value::Bool(true)) {
            break;
        }
        cursor = object.get("next_cursor").and_then(Value::as_str).filter(|cursor| !cursor.is_empty()).map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    if !newest.is_empty() && !capped && !failed {
        let _ = db.set_state("notion:watermark", &newest);
    }
    inserted
}

/// Все внешние источники; не настроенные пропускаются.
pub fn ingest_extra(db: &Database, config: &Config) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    counts.insert("chatgpt".to_owned(), ingest_chatgpt(db, config));
    counts.insert(
        "gmail".to_owned(),
        crate::imap::ingest_mail(db, "gmail", "imap.gmail.com", &config.extra_sources.gmail),
    );
    counts.insert(
        "yandex".to_owned(),
        crate::imap::ingest_mail(db, "yandex", "imap.yandex.ru", &config.extra_sources.yandex),
    );
    counts.insert("github".to_owned(), ingest_github(db, config));
    counts.insert("notion".to_owned(), ingest_notion(db, config));
    counts
}
