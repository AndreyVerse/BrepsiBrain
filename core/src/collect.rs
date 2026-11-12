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
    ModelStage { llm, projects, failed }
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
