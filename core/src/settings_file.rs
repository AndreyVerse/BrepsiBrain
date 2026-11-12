//! Точечная запись `config.toml`.
//!
//! Приложение меняет только свои ключи, а комментарии, порядок и чужие таблицы
//! остаются как были: файл правит человек, и настройки не должны его переписывать.
//! Все записи идут через один мьютекс и замену файла целиком, чтобы параллельные
//! сохранения не оставили половину.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::same_host;

fn write_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

/// Путь конфига: env `BREPSI_BRAIN_CONFIG` (и прежнее имя) важнее папки данных.
pub fn path_for(root: &Path) -> PathBuf {
    crate::config::config_path_from_env().unwrap_or_else(|| root.join("config.toml"))
}

/// Прочитать документ; нет файла или пусто — новый.
fn read(path: &Path) -> Result<DocumentMut, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("cannot read config: {error}")),
    };
    if text.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    text.parse::<DocumentMut>().map_err(|error| format!("cannot parse config: {error}"))
}

/// Записать документ атомарно: временный файл рядом и `rename`.
fn write(path: &Path, doc: &DocumentMut) -> Result<(), String> {
    let updated = doc.to_string();
    // Проверяем до записи: сломанный TOML не должен попасть на диск.
    toml::from_str::<toml::Value>(&updated).map_err(|error| format!("config check failed: {error}"))?;
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, updated).map_err(|error| error.to_string())?;
    std::fs::rename(&tmp, path).map_err(|error| error.to_string())
}

/// Изменить документ под мьютексом. Ошибка `change` оставляет файл нетронутым.
pub fn edit(path: &Path, change: impl FnOnce(&mut DocumentMut) -> Result<(), String>) -> Result<(), String> {
    let _guard = write_lock().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut doc = read(path)?;
    change(&mut doc)?;
    write(path, &doc)
}

fn table<'a>(parent: &'a mut Table, name: &str) -> &'a mut Table {
    if !parent.contains_key(name) || !parent[name].is_table() {
        // Инлайн-таблицу (`tasks = { distill = "a" }`) переводим в обычную, а не
        // заменяем пустой: иначе пропадали ключи пользователя и api_key.
        let replacement = parent.get(name).and_then(Item::as_inline_table).map(|inline| inline.clone().into_table()).unwrap_or_default();
        parent.insert(name, Item::Table(replacement));
    }
    parent[name].as_table_mut().expect("table inserted above")
}

fn document_table<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut Table {
    table(doc.as_table_mut(), name)
}

fn set_string(target: &mut Table, key: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            target.insert(key, Item::Value(Value::from(value)));
        }
        None => {
            target.remove(key);
        }
    }
}

fn set_uint(target: &mut Table, key: &str, value: Option<u64>) {
    match value {
        Some(value) => {
            target.insert(key, Item::Value(Value::from(value as i64)));
        }
        None => {
            target.remove(key);
        }
    }
}

fn set_bool(target: &mut Table, key: &str, value: Option<bool>) {
    match value {
        Some(value) => {
            target.insert(key, Item::Value(Value::from(value)));
        }
        None => {
            target.remove(key);
        }
    }
}

fn non_empty(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Настройки основного провайдера. Ключ переезжает только на тот же хост —
/// как раньше: чужому адресу сохранённый ключ не передаётся.
pub fn set_llm_provider(
    path: &Path,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    json_mode: bool,
    fallback_model: Option<&str>,
) -> Result<(), String> {
    edit(path, |doc| {
        let llm = document_table(doc, "llm");
        let stored_base = llm.get("base_url").and_then(Item::as_str).unwrap_or("").to_owned();
        let stored_key = llm.get("api_key").and_then(Item::as_str).map(str::to_owned);
        let key = match api_key {
            Some("") => None,
            Some(key) => non_empty(key).map(str::to_owned),
            None if same_host(Some(&stored_base), Some(base_url)) => stored_key,
            None => None,
        };
        llm.insert("base_url", Item::Value(Value::from(base_url)));
        llm.insert("model", Item::Value(Value::from(model)));
        llm.insert("json_mode", Item::Value(Value::from(json_mode)));
        set_string(llm, "api_key", key.as_deref());
        // None — «не трогать»: частичное сохранение не стирает запасную модель.
        if let Some(value) = fallback_model {
            set_string(llm, "fallback_model", non_empty(value));
        }
        Ok(())
    })
}

/// Модель одной задачи: пусто — ключ удаляется, задача наследует модель маршрута.
pub fn set_task_model(path: &Path, task: &str, model: Option<&str>) -> Result<(), String> {
    edit(path, |doc| {
        let llm = document_table(doc, "llm");
        let tasks = table(llm, "tasks");
        set_string(tasks, task, model.filter(|value| !value.is_empty()));
        Ok(())
    })
}

/// Локальный маршрут: адрес и модель пишет установщик, но их можно задать руками.
pub fn set_local_route(path: &Path, base_url: &str, model: &str) -> Result<(), String> {
    edit(path, |doc| {
        let llm = document_table(doc, "llm");
        let local = table(llm, "local");
        set_string(local, "base_url", non_empty(base_url));
        set_string(local, "model", non_empty(model));
        Ok(())
    })
}

/// Поля `[local_ai]`; None — ключ не трогать.
pub fn set_local_ai(
    path: &Path,
    models_root: Option<&str>,
    mode: Option<&str>,
    idle_timeout_seconds: Option<u64>,
    context_tokens: Option<u64>,
    offline_only: Option<bool>,
) -> Result<(), String> {
    edit(path, |doc| {
        let local = document_table(doc, "local_ai");
        if let Some(models_root) = models_root {
            set_string(local, "models_root", non_empty(&models_root.replace('\\', "/")));
        }
        if let Some(mode) = mode {
            set_string(local, "mode", non_empty(mode));
        }
        // None — «не трогать»: частичное сохранение не должно стирать соседние
        // ключи, иначе режим «только локально» сбрасывал бы простой и контекст.
        if let Some(value) = idle_timeout_seconds {
            set_uint(local, "idle_timeout_seconds", Some(value));
        }
        if let Some(value) = context_tokens {
            set_uint(local, "context_tokens", Some(value));
        }
        if let Some(value) = offline_only {
            set_bool(local, "offline_only", Some(value));
        }
        Ok(())
    })
}

/// Расписание синхронизации и сбора локальной моделью. `collect_interval_minutes`:
/// None — ключ не трогать.
pub fn set_sync(path: &Path, mode: &str, interval_minutes: u64, collect_interval_minutes: Option<u64>) -> Result<(), String> {
    edit(path, |doc| {
        let sync = document_table(doc, "sync");
        sync.insert("mode", Item::Value(Value::from(mode)));
        sync.insert("interval_minutes", Item::Value(Value::from(interval_minutes as i64)));
        if let Some(value) = collect_interval_minutes {
            set_uint(document_table(doc, "local_ai"), "collect_interval_minutes", Some(value));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        (dir, path)
    }

    #[test]
    fn keeps_comments_and_unknown_tables() {
        let (_dir, path) = temp();
        std::fs::write(
            &path,
            "# мой конфиг\n[llm]\nmodel = \"old\"\napi_key = \"secret\"\n\n# чужая таблица\n[[custom]]\nname = \"x\"\n",
        )
        .unwrap();
        set_llm_provider(&path, "https://api.example/v1", "new-model", Some("key2"), true, None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# мой конфиг"), "{text}");
        assert!(text.contains("# чужая таблица"), "{text}");
        assert!(text.contains("[[custom]]"), "{text}");
        assert!(text.contains("model = \"new-model\""), "{text}");
        assert!(text.contains("api_key = \"key2\""), "{text}");
    }

    #[test]
    fn inline_tables_keep_user_keys() {
        let (_dir, path) = temp();
        std::fs::write(&path, "llm = { base_url = \"https://api.example/v1\", api_key = \"k\", tasks = { distill = \"a\" } }\n").unwrap();
        set_task_model(&path, "answer", Some("b")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_key = \"k\""), "{text}");
        assert!(text.contains("distill = \"a\""), "{text}");
        assert!(text.contains("answer = \"b\""), "{text}");
    }

    #[test]
    fn nested_task_table_is_written_under_llm() {
        let (_dir, path) = temp();
        set_task_model(&path, "distill", Some("qwen2.5:7b")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[llm.tasks]"), "{text}");
        let value: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(value["llm"]["tasks"]["distill"].as_str(), Some("qwen2.5:7b"));
        // Пустое значение удаляет ключ, но не ломает таблицу.
        set_task_model(&path, "distill", None).unwrap();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value["llm"]["tasks"].get("distill").is_none());
    }

    #[test]
    fn foreign_host_drops_stored_key() {
        let (_dir, path) = temp();
        std::fs::write(&path, "[llm]\nbase_url = \"https://a.example/v1\"\napi_key = \"old\"\n").unwrap();
        set_llm_provider(&path, "https://b.example/v1", "m", None, false, None).unwrap();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value["llm"].get("api_key").is_none(), "{value:#?}");
    }

    #[test]
    fn same_host_keeps_stored_key() {
        let (_dir, path) = temp();
        std::fs::write(&path, "[llm]\nbase_url = \"https://a.example/v1\"\napi_key = \"old\"\n").unwrap();
        set_llm_provider(&path, "https://a.example/v2", "m", None, false, None).unwrap();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["llm"]["api_key"].as_str(), Some("old"));
    }

    #[test]
    fn fallback_model_set_keep_and_clear() {
        let (_dir, path) = temp();
        set_llm_provider(&path, "https://a.example/v1", "m", None, false, Some("fb")).unwrap();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["llm"]["fallback_model"].as_str(), Some("fb"));
        set_llm_provider(&path, "https://a.example/v1", "m", None, false, None).unwrap();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["llm"]["fallback_model"].as_str(), Some("fb"));
        set_llm_provider(&path, "https://a.example/v1", "m", None, false, Some("")).unwrap();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value["llm"].get("fallback_model").is_none(), "{value:#?}");
    }

    #[test]
    fn local_sections_roundtrip() {
        let (_dir, path) = temp();
        set_local_route(&path, "http://127.0.0.1:11434/v1", "brepsi/qwen2.5-7b").unwrap();
        set_local_ai(&path, Some("D:\\Models\\Brepsi"), Some("on_demand"), Some(120), Some(16384), Some(true)).unwrap();
        set_sync(&path, "interval", 30, Some(120)).unwrap();
        set_sync(&path, "interval", 30, None).unwrap();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["local_ai"]["collect_interval_minutes"].as_integer(), Some(120));
        assert_eq!(value["llm"]["local"]["model"].as_str(), Some("brepsi/qwen2.5-7b"));
        assert_eq!(value["local_ai"]["models_root"].as_str(), Some("D:/Models/Brepsi"));
        assert_eq!(value["local_ai"]["offline_only"].as_bool(), Some(true));
        assert_eq!(value["sync"]["interval_minutes"].as_integer(), Some(30));
    }

    #[test]
    fn partial_local_ai_update_keeps_other_keys() {
        let (_dir, path) = temp();
        set_local_ai(&path, Some("D:/Models/Brepsi"), Some("on_demand"), Some(60), Some(8192), Some(false)).unwrap();
        // Включение офлайн-режима не должно стирать простой и контекст.
        set_local_ai(&path, None, None, None, None, Some(true)).unwrap();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["local_ai"]["idle_timeout_seconds"].as_integer(), Some(60));
        assert_eq!(value["local_ai"]["context_tokens"].as_integer(), Some(8192));
        assert_eq!(value["local_ai"]["mode"].as_str(), Some("on_demand"));
        assert_eq!(value["local_ai"]["offline_only"].as_bool(), Some(true));
        assert_eq!(value["local_ai"]["models_root"].as_str(), Some("D:/Models/Brepsi"));
    }
}
