//! Черновик записи журнала проекта по карточкам: «Симптом → Причина →
//! Изменённые файлы и контракты → Проверки → Вне скоупа».
//!
//! Исполняется в потоке HTTP-запроса панели (`POST /api/journal/draft`). В файлы
//! журналов ничего не пишет: результат уходит в окно с кнопкой «Копировать».
//! С провайдером черновик пишет модель (с учётом шаблонов приватности), без него
//! или при его сбое — детерминированная сборка из атомов.

use serde_json::{json, Value};

use crate::config::{Config, Task};
use crate::db::{Card, Database};
use crate::llm::{card_is_local_only, record_reply_usage, ChatClient, LocalOnly};
use crate::text::{format_date, split_card_sections};

pub const MAX_CARDS: usize = 15;
const MAX_TOKENS: u32 = 1_500;
const TODO: &str = "— уточнить";

const SYSTEM: &str = "Ты пишешь черновик записи инженерного журнала проекта по карточкам памяти. \
Формат строго такой (markdown, по-русски):\n\
**Симптом.** …\n**Причина.** …\n**Изменённые файлы и контракты.** …\n**Проверки.** … (пропущенное или \
непройденное помечай `degraded: <причина> → ручной чек: <что проверить>`)\n**Вне скоупа.** …\n\
Используй только факты из карточек, ничего не выдумывай; чего нет в карточках — пиши «— уточнить». \
Пути и имена символов — в обратных кавычках. Без вступления и заключения.";

const CAUSE_MARKERS: &[&str] = &["потому", "из-за", "причин", "because", "caused", "root cause"];
const CHECK_MARKERS: &[&str] = &["тест", "test", "провер", "зелён", "зелен", "degraded", "smoke", "cargo check"];
const SCOPE_MARKERS: &[&str] = &["вне скоуп", "осталось", "не делал", "позже", "todo", "out of scope", "отложен"];

fn has_marker(text: &str, markers: &[&str]) -> bool {
    let lower = text.to_lowercase();
    markers.iter().any(|marker| lower.contains(marker))
}

fn bullets(items: &[String]) -> String {
    if items.is_empty() {
        format!(" {TODO}")
    } else {
        items.iter().map(|item| format!("\n- {item}")).collect()
    }
}

fn heading(cards: &[Card], title: &str) -> String {
    let date = cards.iter().map(|card| card.ts).max().map(format_date).unwrap_or_default();
    let ids = cards.iter().map(|card| format!("#{}", card.id)).collect::<Vec<_>>().join(", ");
    format!("## {date} — {title}\n\n_Черновик по записям Brepsi Brain: {ids}._")
}

/// Сборка без модели: атомы раскладываются по разделам по типу и маркерам.
pub fn from_atoms(cards: &[Card], title: &str) -> String {
    let (mut symptoms, mut causes, mut changes, mut checks, mut scope) = (vec![], vec![], vec![], vec![], vec![]);
    let mut refs: Vec<String> = Vec::new();
    for card in cards {
        let atoms = crate::insights::atoms(card);
        for (kind, text) in &atoms {
            let line = text.clone();
            match kind.as_str() {
                "ошибка" => symptoms.push(line),
                "решение" => changes.push(line),
                "команда" => checks.push(format!("`{line}`")),
                _ if has_marker(text, SCOPE_MARKERS) => scope.push(line),
                _ if has_marker(text, CHECK_MARKERS) => checks.push(line),
                _ => causes.push(line),
            }
        }
        if atoms.iter().all(|(kind, _)| kind != "ошибка") {
            let summary = split_card_sections(&card.body).summary;
            let first = summary.split_terminator(['.', '\n']).next().unwrap_or("").trim().to_owned();
            if !first.is_empty() {
                symptoms.push(first);
            }
        }
        let found = crate::stale::extract_refs(&card.body);
        refs.extend(found.paths.into_iter().chain(found.idents).map(|item| format!("`{item}`")));
    }
    // Причинами считаются факты с явной причинной связкой; остальные факты — контекст.
    let (explicit, context): (Vec<String>, Vec<String>) = causes.into_iter().partition(|text| has_marker(text, CAUSE_MARKERS));
    let causes = if explicit.is_empty() { context } else { explicit.into_iter().chain(context).collect() };
    refs.sort();
    refs.dedup();
    if !refs.is_empty() {
        changes.push(format!("Упомянуто: {}", refs.join(", ")));
    }
    if !checks.iter().any(|line| line.to_lowercase().contains("degraded")) {
        checks.push("degraded: — уточнить → ручной чек: — уточнить".into());
    }
    format!(
        "{}\n\n**Симптом.**{}\n\n**Причина.**{}\n\n**Изменённые файлы и контракты.**{}\n\n**Проверки.**{}\n\n**Вне скоупа.**{}\n",
        heading(cards, title),
        bullets(&symptoms),
        bullets(&causes),
        bullets(&changes),
        bullets(&checks),
        bullets(&scope),
    )
}

fn prompt(cards: &[Card], project: &str) -> String {
    let mut out = format!("Проект: {}\n", if project.is_empty() { "неизвестен" } else { project });
    for card in cards {
        let sections = split_card_sections(&card.body);
        out.push_str(&format!(
            "\n### #{} {} ({})\nКратко: {}\nАтомы:\n{}\n",
            card.id,
            card.title,
            format_date(card.ts),
            sections.summary.trim(),
            sections.atoms.trim()
        ));
    }
    out
}

/// Черновик и откуда он: `llm` или `atoms` (+ причина, если модель не участвовала).
pub fn draft(db: &Database, config: &Config, cards: &[Card], title: &str, project: &str) -> Value {
    let fallback = |note: String| json!({"markdown": from_atoms(cards, title), "mode": "atoms", "note": note});
    if cards.is_empty() {
        return json!({"markdown": "", "mode": "atoms", "note": "нет карточек знаний за выбранный период"});
    }
    let Some(mut route) = config.route_for(Task::Handoff, false) else {
        return fallback("провайдер не настроен — собрано из атомов без модели".into());
    };
    if !route.local {
        let gate = LocalOnly::new(&config.privacy.local_only_patterns);
        // Ошибка проверки приватности считается совпадением: наружу не уходит.
        if cards.iter().any(|card| card_is_local_only(db, card, &gate).unwrap_or(true)) {
            match config.local_route(Task::Handoff) {
                Some(local) => route = local,
                None => return fallback("записи под шаблоном приватности, локальной модели нет — собрано из атомов".into()),
            }
        }
    }
    let _guard = if route.local {
        match crate::local::job_guard(config, Task::Handoff) {
            Ok(guard) => guard,
            Err(error) => return fallback(format!("локальная модель недоступна: {error}")),
        }
    } else {
        None
    };
    let client = match crate::llm::client_for(&route, MAX_TOKENS) {
        Ok(client) => client,
        Err(error) => return fallback(format!("провайдер: {error}")),
    };
    with_client(db, &client, cards, title, project).unwrap_or_else(fallback)
}

/// Запрос к модели; Err — причина, по которой черновик собран из атомов.
pub fn with_client(db: &Database, client: &dyn ChatClient, cards: &[Card], title: &str, project: &str) -> Result<Value, String> {
    let reply = client.chat(SYSTEM, &prompt(cards, project)).map_err(|error| format!("модель не ответила: {error}"))?;
    let _ = record_reply_usage(db, client, &reply, cards.len() as i64, Task::Handoff);
    let body = reply.text.trim();
    if !body.contains("**Симптом") {
        return Err("ответ модели не в формате журнала — собрано из атомов".into());
    }
    Ok(json!({
        "markdown": format!("{}\n\n{body}\n", heading(cards, title)),
        "mode": "llm",
        "note": format!("модель {}", client.model()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{now_ts, CardDraft};

    fn card(db: &Database, unit: &str, atoms: &[&str]) -> Card {
        let body = crate::llm::render_card_body("Починили падение синхронизации. Детали ниже", &[], &atoms.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let mut draft = CardDraft::new("claude", &format!("llm:{unit}"), now_ts(), "Синхронизация", &body).distilled();
        draft.unit = unit.to_owned();
        db.insert_card(&draft).unwrap();
        db.get_card_by_ref(&format!("llm:{unit}")).unwrap().unwrap()
    }

    #[test]
    fn draft_without_provider_is_built_from_atoms() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("m.db")).unwrap();
        let card = card(&db, "u1", &[
            "ошибка: SQLITE_BUSY при параллельной записи",
            "факт: блокировка держалась, потому что транзакция не закрывалась",
            "решение: правка в `core/src/db.rs`, функция `claim_lease`",
            "команда: cargo test --offline",
            "факт: перенос на Linux вне скоупа",
        ]);
        let value = draft(&db, &Config::default(), std::slice::from_ref(&card), "Синхронизация", "p");
        assert_eq!(value["mode"], "atoms");
        let text = value["markdown"].as_str().unwrap();
        let order = ["**Симптом.**", "**Причина.**", "**Изменённые файлы и контракты.**", "**Проверки.**", "**Вне скоупа.**"];
        let positions: Vec<usize> = order.iter().map(|head| text.find(head).unwrap()).collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert!(text.contains("- SQLITE_BUSY при параллельной записи"));
        assert!(text.contains("- блокировка держалась, потому что"));
        assert!(text.contains("`core/src/db.rs`") && text.contains("`claim_lease`"));
        assert!(text.contains("- `cargo test --offline`") && text.contains("degraded: — уточнить"));
        assert!(text.contains("- перенос на Linux вне скоупа"));
        assert!(text.contains(&format!("#{}", card.id)));
    }
}
