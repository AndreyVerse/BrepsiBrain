//! История поиска и память, которую она наращивает — порт `search_memory.py`.
//!
//! Каждый вопрос из «Спросить память» попадает в search_history (одна строка на
//! вопрос, повтор поднимает её наверх) и становится карточкой 'llm:search:<key>'
//! с ответом, вопросом и карточками-источниками. Карточка — kind 'distilled':
//! пересказ обходит только записи с сырыми фрагментами и не тратит на неё токены.
//! Приватность: scope записи карточки перечисляет scope и ref источников, поэтому
//! карточка не уйдёт внешнему провайдеру, если не ушёл бы любой источник.

use rusqlite::{params, OptionalExtension};
use sha1_smol::Sha1;

use crate::db::{now_ts, Card, CardDraft, Database, Result, CARD_KIND_DISTILLED, SCOPE_KEY_PREFIX};
use crate::pystr;
use crate::text::DISTILLED_PREFIX;

pub const SEARCH_SOURCE: &str = "search";
pub const SOURCES_LISTED: usize = 8;
pub const SUMMARY_MAX_CHARS: usize = 2400;
pub const DEFAULT_HISTORY_LIMIT: i64 = 30;

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    pub q: String,
    pub ts: i64,
    pub answered: bool,
    pub cards: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryItem {
    pub id: i64,
    pub q: String,
    pub ts: i64,
    pub answer: Option<String>,
    pub memory_ref: String,
}

/// Один и тот же вопрос независимо от регистра и пробелов.
pub fn question_key(q: &str) -> String {
    let normalized = pystr::split_whitespace(&q.to_lowercase()).collect::<Vec<_>>().join(" ");
    let hex = Sha1::from(normalized.as_bytes()).digest().to_string();
    hex[..16].to_string()
}

pub fn memory_ref(q: &str) -> String {
    format!("{DISTILLED_PREFIX}{SEARCH_SOURCE}:{}", question_key(q))
}

fn summary(answer: Option<&str>, cards: &[Card]) -> String {
    if let Some(answer) = answer.filter(|a| !a.is_empty()) {
        return pystr::take_chars(pystr::strip(answer), SUMMARY_MAX_CHARS).to_string();
    }
    let titles = cards.iter().take(SOURCES_LISTED).map(|c| c.title.as_str()).collect::<Vec<_>>().join("; ");
    pystr::strip(&format!("По запросу нашлось записей: {}. {titles}", cards.len())).to_string()
}

fn body(q: &str, answer: Option<&str>, cards: &[Card]) -> String {
    let sources = cards
        .iter()
        .take(SOURCES_LISTED)
        .map(|c| format!("- #{} {}", c.id, c.title))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "### Кратко\n{}\n\n### Вопросы\n- {}\n\n### Источники\n{sources}",
        summary(answer, cards),
        pystr::strip(q)
    )
}

/// json.dumps(list[int]) Python: «[1, 2]» с пробелом — Python читает и так, но
/// одинаковый текст в столбце упрощает сверку двух реализаций.
fn ids_json(cards: &[Card]) -> String {
    format!("[{}]", cards.iter().map(|c| c.id.to_string()).collect::<Vec<_>>().join(", "))
}

/// Записать вопрос и его карточку памяти. None — ничего не нашлось: пустой поиск
/// ничему не учит и только засорял бы историю.
pub fn remember_search(db: &Database, q: &str, answer: Option<&str>, cards: &[Card], now: Option<i64>) -> Result<Option<String>> {
    let card_ref = memory_ref(q);
    // Карточку прошлого поиска по тому же вопросу не цитируем: память ссылалась бы сама на себя.
    let cards: Vec<Card> = cards.iter().filter(|c| c.source_ref != card_ref).cloned().collect();
    if cards.is_empty() {
        return Ok(None);
    }
    let now = now.unwrap_or_else(now_ts);
    let unit = card_ref.strip_prefix(DISTILLED_PREFIX).unwrap_or(&card_ref).to_string();
    let title = pystr::take_chars(pystr::strip(q), 200).to_string();
    let text = body(q, answer, &cards);
    db.atomic(|| {
        match db.get_card_by_ref(&card_ref)? {
            None => {
                let mut draft = CardDraft::new(&format!("{DISTILLED_PREFIX}{SEARCH_SOURCE}"), &card_ref, now, &title, &text);
                draft.kind = CARD_KIND_DISTILLED.into();
                draft.unit = unit.clone();
                db.insert_card(&draft)?;
            }
            Some(existing) => {
                db.update_card(existing.id, now, &title, &text, None, false)?;
            }
        }
        let mut scopes: Vec<String> = Vec::new();
        for card in &cards {
            let scope = if card.unit.is_empty() { None } else { db.get_state(&format!("{SCOPE_KEY_PREFIX}{}", card.unit))? };
            for part in [scope.unwrap_or_default(), card.source_ref.clone()] {
                // dict.fromkeys в Python: порядок первого появления, без повторов.
                if !part.is_empty() && !scopes.contains(&part) {
                    scopes.push(part);
                }
            }
        }
        db.set_state(&format!("{SCOPE_KEY_PREFIX}{unit}"), &scopes.join("\n"))?;
        db.connection().execute(
            "INSERT INTO search_history(qkey,q,ts,answer,card_ids,memory_ref) VALUES(?,?,?,?,?,?) \
             ON CONFLICT(qkey) DO UPDATE SET q=excluded.q, ts=excluded.ts, answer=excluded.answer, \
             card_ids=excluded.card_ids, memory_ref=excluded.memory_ref",
            params![question_key(q), pystr::strip(q), now, answer.unwrap_or(""), ids_json(&cards), card_ref],
        )?;
        Ok(())
    })?;
    Ok(Some(card_ref))
}

fn parse_ids(text: &str) -> Vec<i64> {
    serde_json::from_str::<Vec<i64>>(text).unwrap_or_default()
}

pub fn history(db: &Database, limit: i64) -> Result<Vec<HistoryEntry>> {
    let mut stmt = db
        .connection()
        .prepare("SELECT id, q, ts, answer, card_ids FROM search_history ORDER BY ts DESC, id DESC LIMIT ?")?;
    let rows = stmt.query_map([limit], |r| {
        let answer: String = r.get(3)?;
        let ids: String = r.get(4)?;
        Ok(HistoryEntry { id: r.get(0)?, q: r.get(1)?, ts: r.get(2)?, answered: !answer.is_empty(), cards: parse_ids(&ids).len() })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Прошлый вопрос с ответом и теми карточками, что ещё существуют.
pub fn history_item(db: &Database, item_id: i64) -> Result<Option<(HistoryItem, Vec<Card>)>> {
    let row = db
        .connection()
        .query_row(
            "SELECT id, q, ts, answer, card_ids, memory_ref FROM search_history WHERE id=?",
            [item_id],
            |r| {
                let answer: String = r.get(3)?;
                let ids: String = r.get(4)?;
                Ok((
                    HistoryItem {
                        id: r.get(0)?,
                        q: r.get(1)?,
                        ts: r.get(2)?,
                        answer: (!answer.is_empty()).then_some(answer),
                        memory_ref: r.get(5)?,
                    },
                    ids,
                ))
            },
        )
        .optional()?;
    let Some((item, ids)) = row else {
        return Ok(None);
    };
    let mut cards = Vec::new();
    for id in parse_ids(&ids) {
        if let Some(card) = db.get_card(id)? {
            cards.push(card);
        }
    }
    Ok(Some((item, cards)))
}

/// Убрать вопрос из истории; его карточка памяти остаётся.
pub fn forget_history_item(db: &Database, item_id: i64) -> Result<bool> {
    Ok(db.connection().execute("DELETE FROM search_history WHERE id=?", [item_id])? > 0)
}
