//! Этап модели в сборе данных: карточки записей и актуализация названий проектов.
//!
//! Индексация дешёвая и идёт каждый запуск, а локальная модель занимает VRAM,
//! поэтому у неё свой интервал `[local_ai] collect_interval_minutes`. Кнопка и
//! трей (`force`) запускают этап сразу. Метка прошлого сбора лежит в базе: её
//! читают разные процессы — GUI и `BrepsiBrain.exe sync` от планировщика.

use serde_json::{json, Value};

use crate::config::{Config, LocalRoute, Task};
use crate::db::Database;

const LAST_COLLECT_KEY: &str = "local:last_collect";
const EMBED_BUDGET: usize = 400;

#[derive(Debug, Clone, PartialEq)]
pub struct ModelStage {
    pub llm: Value,
    pub projects: Value,
    /// Карточки не обработаны из-за ошибки базы. Сбой названий сюда не входит:
    /// они подождут следующего сбора и запуск ошибочным не считается.
    pub failed: bool,
}

fn distill(db: &Database, config: &Config) -> (Value, bool) {
    match crate::llm::distill_pending(db, config, None, None, 1, None) {
        Ok(stats) => (stats.record(), false),
        Err(error) => (json!({"configured": config.llm.configured(), "error": error.to_string()}), true),
    }
}

/// Исполняется в потоке запроса `/api/sync` или в процессе `sync` планировщика.
pub fn run(db: &Database, config: &Config, force: bool) -> ModelStage {
    let now = crate::db::now_ts();
    let last = db.get_state(LAST_COLLECT_KEY).ok().flatten().and_then(|value| value.parse().ok());
    let due = force || config.local_collect_due(last, now);
    let main_local = config.route_for(Task::Distill, false).is_some_and(|route| route.local);
    let (llm, failed) = if due {
        distill(db, config)
    } else if main_local {
        (json!({"configured": true, "skipped": "interval"}), false)
    } else {
        // Карточки пишет удалённый провайдер, интервал локальной модели его не
        // касается. Без `[llm.local]` движок не поднимается, а приватные записи ждут.
        let mut remote_only = config.clone();
        remote_only.local = LocalRoute::default();
        distill(db, &remote_only)
    };
    let projects = if due {
        match crate::projects::refresh(db, config) {
            Ok(stats) => stats.record(),
            Err(error) => json!({"error": error}),
        }
    } else {
        json!({"skipped": "interval"})
    };
    if due {
        let _ = db.set_state(LAST_COLLECT_KEY, &now.to_string());
    }
    let mut llm = llm;
    if config.search.embed_model.is_some() {
        // Векторы для смыслового поиска: ограниченная порция за прогон, остаток — в следующий.
        let embedded = crate::insights::index_embeddings(db, config, EMBED_BUDGET);
        if let Some(object) = llm.as_object_mut() {
            object.insert("embeddings".into(), match embedded {
                Ok(count) => json!(count),
                Err(error) => json!({"error": error}),
            });
        }
    }
    ModelStage { llm, projects, failed }
}

const CHECKS_LEASE_KEY: &str = "lease:checks";
const CHECKS_LEASE_TTL: i64 = 15 * 60;
/// Бюджет проверки устаревших ссылок за прогон; остаток — в следующую синхронизацию.
const STALE_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
pub const CHECKS_STATE_KEY: &str = "checks:last";

/// Фоновые проверки после синхронизации: кэш проектов, устаревшие ссылки и
/// противоречия решений. Исполняется в отдельном потоке процесса панели (своё
/// соединение SQLite) или в конце процесса `sync` планировщика. Аренда в базе не
/// даёт двум процессам гонять git и платные запросы одновременно. Итог со всеми
/// ошибками git/LLM/IO пишется в state `checks:last`, вызывающий кладёт его в `sync.log`.
pub fn background_checks(db: &Database, config: &Config) -> Value {
    let started = std::time::Instant::now();
    let now = crate::db::now_ts();
    let owner = format!("checks-{}", std::process::id());
    match db.claim_lease(CHECKS_LEASE_KEY, &owner, CHECKS_LEASE_TTL, None) {
        Ok(true) => {}
        Ok(false) => return json!({"ts": now, "task": "memory-base-checks", "skipped": "busy"}),
        Err(error) => return json!({"ts": now, "task": "memory-base-checks", "error": error.to_string()}),
    }
    let mut record = json!({"ts": now, "task": "memory-base-checks"});
    let git_root = crate::ingest::sources_for(config).git_root;
    match db.unit_index() {
        Ok(index) => {
            record["roots"] = match crate::stale::refresh_unit_roots(db, &index, &git_root) {
                Ok(count) => json!(count),
                Err(error) => json!({"error": error}),
            };
            record["stale"] = crate::stale::check(db, now, std::time::Instant::now() + STALE_BUDGET);
            let projects: std::collections::HashMap<String, String> =
                index.into_values().map(|info| (info.unit, info.project)).collect();
            record["conflicts"] = crate::conflicts::run(db, config, &projects, None);
        }
        Err(error) => record["error"] = json!(format!("индекс записей: {error}")),
    }
    record["ms"] = json!(started.elapsed().as_millis() as u64);
    if let Err(error) = db.set_state(CHECKS_STATE_KEY, &record.to_string()) {
        record["state_error"] = json!(error.to_string());
    }
    let _ = db.release_lease(CHECKS_LEASE_KEY, &owner);
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_skips_model_until_due_and_force_overrides_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        let mut config = Config::default();
        // Основной маршрут — свой loopback: сеть в тесте не нужна, очередь пуста.
        config.llm.base_url = Some("http://127.0.0.1:9/v1".into());
        config.llm.model = Some("m".into());
        config.local_ai.collect_interval_minutes = Some(60);

        assert!(run(&db, &config, false).llm.get("skipped").is_none(), "первый сбор идёт сразу");
        assert_eq!(run(&db, &config, false).llm["skipped"], "interval");
        assert_eq!(run(&db, &config, false).projects["skipped"], "interval");
        assert!(run(&db, &config, true).llm.get("skipped").is_none(), "кнопка не ждёт интервала");
    }

    #[test]
    fn remote_cards_do_not_wait_for_local_interval() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("memory.db")).unwrap();
        let mut config = Config::default();
        config.llm.base_url = Some("https://cloud.example/v1".into());
        config.llm.model = Some("cloud".into());
        config.local_ai.collect_interval_minutes = Some(60);
        db.set_state(LAST_COLLECT_KEY, &crate::db::now_ts().to_string()).unwrap();

        let stage = run(&db, &config, false);
        assert!(stage.llm.get("skipped").is_none(), "{}", stage.llm);
        assert_eq!(stage.projects["skipped"], "interval");
    }

    #[test]
    fn due_tolerates_scheduler_jitter() {
        let mut config = Config::default();
        assert!(config.local_collect_due(Some(1_000), 1_000), "без интервала — каждый запуск");
        config.local_ai.collect_interval_minutes = Some(15);
        assert!(config.local_collect_due(None, 1_000));
        assert!(config.local_collect_due(Some(1_000), 1_000 + 899));
        assert!(!config.local_collect_due(Some(1_000), 1_000 + 600));
    }
}
