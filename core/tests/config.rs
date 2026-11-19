//! Новые секции конфига и выбор маршрута: `[llm.tasks]`, `[llm.local]`,
//! `[local_ai]`, `[sync]`.

use std::path::Path;

use memory_core::config::{
    load_config_with_key, Config, LlmSettings, LocalAiSettings, LocalMode, LocalRoute, SyncMode, Task,
};

fn write(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

#[test]
fn parses_local_sections() {
    let (_dir, path) = write(
        r#"
[llm]
base_url = "https://api.example/v1"
model = "cloud"

[llm.tasks]
distill = "brepsi/qwen25-7b"
answer = ""
handoff = "brepsi/qwen25-14b"

[llm.local]
base_url = "http://127.0.0.1:11500/v1"
model = "brepsi/qwen25-7b"

[local_ai]
models_root = "D:/Models/Brepsi"
mode = "always"
idle_timeout_seconds = 300
context_tokens = 10000
timeout_seconds = 600
batch_size = 3
offline_only = true

[sync]
mode = "manual"
interval_minutes = 60
"#,
    );
    let config = load_config_with_key(Some(&path), None).unwrap();
    assert_eq!(config.tasks.get(Task::Distill), Some("brepsi/qwen25-7b"));
    assert_eq!(config.tasks.get(Task::Answer), None);
    assert_eq!(config.tasks.get(Task::Handoff), Some("brepsi/qwen25-14b"));
    assert_eq!(config.tasks.get(Task::Projects), None);
    assert!(config.local.configured());
    assert_eq!(config.local_ai.models_root.as_deref(), Some(Path::new(r"D:\Models\Brepsi")));
    assert_eq!(config.local_ai.mode, LocalMode::Always);
    assert_eq!(config.local_ai.idle_timeout_seconds, 300);
    // Окно выравнивается по 1024 вниз.
    assert_eq!(config.local_ai.context_tokens, 9216);
    assert_eq!(config.local_ai.batch_size, 3);
    assert!(config.local_ai.offline_only);
    assert_eq!(config.sync.mode, SyncMode::Manual);
    assert_eq!(config.sync.interval_minutes, 60);
    assert_eq!(config.root(), path.parent().unwrap());
}

#[test]
fn old_config_keeps_old_behavior() {
    let (_dir, path) = write("[llm]\nbase_url = \"https://api.example/v1\"\nmodel = \"cloud\"\n");
    let config = load_config_with_key(Some(&path), None).unwrap();
    assert_eq!(config.tasks, Default::default());
    assert!(!config.local.configured());
    assert_eq!(config.local_ai.mode, LocalMode::OnDemand);
    // Решение владельца 08.11.2026: автосинк выключен, пока его не включат явно.
    assert_eq!(config.sync.mode, SyncMode::Manual);
    assert_eq!(config.sync.interval_minutes, 15);
    let route = config.route_for(Task::Answer, false).unwrap();
    assert_eq!((route.model.as_str(), route.local), ("cloud", false));
}

#[test]
fn invalid_enum_and_types_are_errors() {
    for text in [
        "[llm]\n[llm.tasks]\ndistill = 3\n",
        "[llm]\n[llm.local]\nbase_url = 3\n",
        "[local_ai]\nmode = \"sometimes\"\n",
        "[sync]\nmode = \"hourly\"\n",
        "[local_ai]\nidle_timeout_seconds = -5\n",
    ] {
        let (_dir, path) = write(text);
        assert!(load_config_with_key(Some(&path), None).is_err(), "{text}");
    }
}

#[test]
fn route_precedence_and_offline_mode() {
    let mut config = Config {
        llm: LlmSettings { base_url: Some("https://api.example/v1".into()), model: Some("cloud".into()), ..LlmSettings::default() },
        local: LocalRoute {
            base_url: Some("http://127.0.0.1:11500/v1".into()),
            model: Some("brepsi/qwen25-7b".into()),
            ..LocalRoute::default()
        },
        ..Config::default()
    };
    // Переопределение задачи применяется к активному маршруту.
    config.tasks.set(Task::Answer, Some("cloud-mini".into()));
    assert_eq!(config.route_for(Task::Answer, false).unwrap().model, "cloud-mini");
    assert_eq!(config.route_for(Task::Distill, false).unwrap().model, "cloud");
    // Приватный запрос уходит локально, и облачная модель задачи туда не попадает:
    // в локальном движке её нет.
    let local = config.route_for(Task::Answer, true).unwrap();
    assert!(local.local && local.model == "brepsi/qwen25-7b", "{}", local.model);
    // Свой тег — наоборот, только локально, а не облачному провайдеру.
    config.tasks.set(Task::Distill, Some("brepsi/qwen25-14b".into()));
    assert_eq!(config.route_for(Task::Distill, false).unwrap().model, "cloud");
    assert_eq!(config.route_for(Task::Distill, true).unwrap().model, "brepsi/qwen25-14b");
    config.tasks.set(Task::Distill, None);
    // Без провайдера — локальный маршрут.
    config.llm = LlmSettings::default();
    assert!(config.route_for(Task::Distill, false).unwrap().local);
    // Офлайн-режим игнорирует провайдера.
    config.llm = LlmSettings { base_url: Some("https://api.example/v1".into()), model: Some("cloud".into()), ..LlmSettings::default() };
    config.local_ai = LocalAiSettings { offline_only: true, ..LocalAiSettings::default() };
    assert!(config.route_for(Task::Distill, false).unwrap().local);
    // Локальный адрес не-loopback не считается локальным.
    config.local_ai = LocalAiSettings::default();
    config.local.base_url = Some("http://192.168.1.5:11434/v1".into());
    assert!(config.local_route(Task::Distill).is_none());
    assert_eq!(config.route_for(Task::Distill, false).unwrap().model, "cloud");
}

#[test]
fn no_route_means_no_generation() {
    let config = Config::default();
    assert!(config.route_for(Task::Distill, false).is_none());
    assert!(config.route_for(Task::Distill, true).is_none());
}

#[test]
fn next_sync_follows_schedule() {
    let mut config = Config::default();
    // Новый дефолт — manual; интервал включается явно.
    assert_eq!(config.next_sync_ts(Some(1000)), None);
    config.sync.mode = SyncMode::Interval;
    assert_eq!(config.next_sync_ts(Some(1000)), Some(1000 + 15 * 60));
    config.sync.mode = SyncMode::Manual;
    assert_eq!(config.next_sync_ts(Some(1000)), None);
    config.sync.mode = SyncMode::Interval;
    config.sync.interval_minutes = 30;
    assert_eq!(config.next_sync_ts(None), None);
    assert_eq!(config.next_sync_ts(Some(0)), Some(1800));
}
