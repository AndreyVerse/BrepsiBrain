// Строки оболочки (трей, подсказки, ошибки запуска) на русском и английском.
//
// Выбор языка общий с панелью: data/ui.json {"lang": "auto"|"ru"|"en"} пишут и
// пункт трея, и настройки панели; "auto" — по локали Windows. Файл читается при
// каждом построении меню: это раз в минуту, а не горячий путь.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Ru,
    En,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "en",
        }
    }
}

pub fn system_lang() -> Lang {
    match sys_locale::get_locale() {
        Some(locale) if locale.to_lowercase().starts_with("ru") => Lang::Ru,
        _ => Lang::En,
    }
}

fn ui_file(root: &Path) -> PathBuf {
    root.join("data").join("ui.json")
}

/// Сохранённый выбор: "auto", "ru" или "en"; всё прочее считается "auto".
pub fn saved_choice(root: &Path) -> String {
    std::fs::read_to_string(ui_file(root))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|doc| doc["lang"].as_str().map(str::to_owned))
        .filter(|lang| lang == "ru" || lang == "en")
        .unwrap_or_else(|| "auto".to_owned())
}

pub fn current(root: &Path) -> Lang {
    match saved_choice(root).as_str() {
        "ru" => Lang::Ru,
        "en" => Lang::En,
        _ => system_lang(),
    }
}

/// Запись через временный файл и rename: панель читает тот же файл из другого процесса.
pub fn save_choice(root: &Path, choice: &str) -> std::io::Result<()> {
    let path = ui_file(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json!({ "lang": choice }).to_string())?;
    std::fs::rename(tmp, path)
}

pub fn t(lang: Lang, key: &str) -> &'static str {
    let (ru, en) = match key {
        "db_unavailable" => ("База недоступна", "Database unavailable"),
        "recent" => ("Последние карточки", "Recent cards"),
        "empty" => ("Пока пусто", "Nothing yet"),
        "sources" => ("Источники · {n}", "Sources · {n}"),
        "source_line" => ("{name} — записей {records}, карточек {cards}", "{name} — {records} records, {cards} cards"),
        "settings" => ("Настройки", "Settings"),
        "model" => ("Открыть настройки…", "Open settings…"),
        "models" => ("Модели и железо…", "Models and hardware…"),
        "local_stop" => ("Остановить локальную модель", "Stop the local model"),
        "autostart" => ("Запускать при входе", "Start at sign-in"),
        "log" => ("Открыть журнал", "Open log"),
        "folder" => ("Открыть папку базы", "Open data folder"),
        "language" => ("Язык", "Language"),
        "lang_auto" => ("Как в системе", "System default"),
        "open" => ("Открыть", "Open"),
        "sync" => ("Синхронизировать сейчас", "Sync now"),
        "about" => ("О программе", "About"),
        "quit" => ("Выход", "Exit"),
        "syncing" => ("Brepsi Brain — синхронизация…", "Brepsi Brain — syncing…"),
        "sync_done" => ("Brepsi Brain — синхронизация завершена ({status})", "Brepsi Brain — sync finished ({status})"),
        "sync_long" => (
            "Brepsi Brain — синхронизация идёт дольше обычного, результат появится в меню",
            "Brepsi Brain — sync is taking longer than usual; results will appear in the menu",
        ),
        "sync_failed" => ("Brepsi Brain — ошибка синхронизации ({status})", "Brepsi Brain — sync failed ({status})"),
        "error" => ("Brepsi Brain — ошибка: {e}", "Brepsi Brain — error: {e}"),
        "autostart_failed" => ("Brepsi Brain — автозапуск не изменён: {e}", "Brepsi Brain — autostart unchanged: {e}"),
        "server_down" => ("сервер недоступен: {e}", "server unreachable: {e}"),
        "server_failed" => ("не удалось запустить сервер: {e}", "could not start the server: {e}"),
        "quitting" => ("приложение завершается", "the app is quitting"),
        "server_exited" => ("процесс сервера завершился: {status}", "the server process exited: {status}"),
        "port_timeout" => ("порт 8765 не открылся за 60 секунд", "port 8765 did not open within 60 seconds"),
        "no_window" => ("окно не найдено", "window not found"),
        _ => ("", ""),
    };
    match lang {
        Lang::Ru => ru,
        Lang::En => en,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_round_trip_and_fallback() {
        let dir = std::env::temp_dir().join(format!("sb-i18n-{}", std::process::id()));
        assert_eq!(saved_choice(&dir), "auto");
        save_choice(&dir, "en").unwrap();
        assert_eq!(current(&dir), Lang::En);
        std::fs::write(ui_file(&dir), "{\"lang\": \"de\"}").unwrap();
        assert_eq!(saved_choice(&dir), "auto");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn every_key_has_both_languages() {
        for key in [
            "db_unavailable", "recent", "empty", "sources", "source_line", "settings", "model", "autostart", "log",
            "models", "local_stop", "folder", "language", "lang_auto", "open", "sync", "about", "quit", "syncing", "sync_done", "sync_long",
            "sync_failed", "error", "autostart_failed", "server_down", "server_failed", "quitting", "server_exited",
            "port_timeout", "no_window",
        ] {
            assert!(!t(Lang::Ru, key).is_empty() && !t(Lang::En, key).is_empty(), "{key}");
        }
    }
}
