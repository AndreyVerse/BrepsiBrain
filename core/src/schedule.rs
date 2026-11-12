//! Расписание синхронизации через Планировщик заданий Windows.
//!
//! Приложение владеет одной задачей `Brepsi Brain sync`, которая запускает
//! `BrepsiBrain.exe sync` раз в N минут. Ручной режим удаляет её. Устаревшую
//! задачу прежнего имени убираем, только если её действие — наш же exe, чтобы
//! не снести чужую работу пользователя.

use std::path::Path;
use std::process::Stdio;

use crate::config::{SyncMode, SyncSettings};

pub const TASK_NAME: &str = "Brepsi Brain sync";
pub const LEGACY_TASK_NAME: &str = "memory-base-sync";

/// Аргументы `schtasks /Create` для минутного интервала. `/TR` — одна строка
/// `"<путь с пробелами>" sync`: кавычки внутри значения, а не вокруг аргумента.
pub fn create_args(exe: &Path, interval_minutes: u32) -> Vec<String> {
    vec![
        "/Create".into(),
        "/SC".into(),
        "MINUTE".into(),
        "/MO".into(),
        interval_minutes.to_string(),
        "/TN".into(),
        TASK_NAME.into(),
        "/TR".into(),
        format!("\"{}\" sync", exe.display()),
        "/F".into(),
    ]
}

pub fn delete_args(name: &str) -> Vec<String> {
    vec!["/Delete".into(), "/TN".into(), name.into(), "/F".into()]
}

fn schtasks(args: &[String]) -> std::io::Result<std::process::Output> {
    crate::proc::quiet("schtasks")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
}

fn output_text(output: &std::process::Output) -> String {
    // schtasks пишет в консольной кодировке, а `/XML` — UTF-16LE: для поиска
    // ASCII-маркеров убираем нулевые байты и декодируем lossy.
    let mut bytes = output.stdout.clone();
    bytes.extend_from_slice(&output.stderr);
    String::from_utf8_lossy(&bytes).replace('\0', "")
}

/// Есть ли задача с таким именем.
pub fn exists(name: &str) -> bool {
    schtasks(&["/Query".into(), "/TN".into(), name.into()]).is_ok_and(|output| output.status.success())
}

/// Действие задачи указывает на exe Brepsi Brain (текущее или прежнее имя).
pub fn action_is_ours(xml: &str) -> bool {
    let lowered = xml.replace('\0', "").to_lowercase();
    lowered.contains("brepsibrain.exe") || lowered.contains("second-brain.exe")
}

/// XML задачи, если её удалось прочитать.
fn task_xml(name: &str) -> Option<String> {
    let output = schtasks(&["/Query".into(), "/TN".into(), name.into(), "/XML".into()]).ok()?;
    output.status.success().then(|| output_text(&output))
}

/// Создать задачу интервала. Ошибка содержит ответ планировщика.
pub fn create(exe: &Path, interval_minutes: u32) -> Result<(), String> {
    let output = schtasks(&create_args(exe, interval_minutes)).map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(output_text(&output).trim().to_owned())
    }
}

/// Удалить задачу; отсутствие задачи — не ошибка.
pub fn delete(name: &str) -> Result<(), String> {
    let output = schtasks(&delete_args(name)).map_err(|error| error.to_string())?;
    if output.status.success() || output_text(&output).to_lowercase().contains("не найдена") || output_text(&output).to_lowercase().contains("cannot find") {
        Ok(())
    } else {
        Err(output_text(&output).trim().to_owned())
    }
}

/// Привести планировщик к настройке. Возвращает предупреждение, если задача
/// осталась или не создалась.
pub fn apply(settings: &SyncSettings, exe: Option<&Path>) -> Result<Option<String>, String> {
    match settings.mode {
        SyncMode::Interval => {
            let Some(exe) = exe else { return Ok(Some("не удалось определить путь приложения: расписание не создано".into())) };
            create(exe, settings.interval_minutes)?;
            // Прежняя задача того же приложения: после переименования она
            // запускала бы второй sync.
            if exists(LEGACY_TASK_NAME) {
                if task_xml(LEGACY_TASK_NAME).as_deref().is_some_and(action_is_ours) {
                    let _ = delete(LEGACY_TASK_NAME);
                } else {
                    return Ok(Some(format!(
                        "найдена чужая задача {LEGACY_TASK_NAME}: её не трогаем, но синхронизация может идти дважды"
                    )));
                }
            }
            Ok(None)
        }
        SyncMode::Manual => {
            let _ = delete(TASK_NAME);
            if task_xml(LEGACY_TASK_NAME).as_deref().is_some_and(action_is_ours) {
                let _ = delete(LEGACY_TASK_NAME);
            }
            Ok(None)
        }
    }
}

/// Рантайм задачи планировщика для панели «Настройки». Только чтение.
pub struct TaskInfo {
    pub state: String,
    pub next_run_ts: Option<i64>,
    pub last_run_ts: Option<i64>,
    pub last_result: Option<i64>,
    pub error: Option<String>,
}

impl TaskInfo {
    fn absent() -> Self {
        Self { state: "absent".into(), next_run_ts: None, last_run_ts: None, last_result: None, error: None }
    }
}

/// Состояние задачи: `schtasks /XML` даёт только определение, а следующий/последний
/// запуск и результат — `Get-ScheduledTaskInfo`. Имена свойств и epoch-даты
/// locale-независимы, в отличие от `schtasks /V` с русскими заголовками.
#[cfg(not(test))]
pub fn query(name: &str) -> TaskInfo {
    if !exists(name) {
        return TaskInfo::absent();
    }
    let name = name.replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='Stop'; $t=Get-ScheduledTask -TaskName '{name}'; $i=$t|Get-ScheduledTaskInfo; \
         function Epoch($d){{ if($d){{ [DateTimeOffset]::new($d).ToUnixTimeSeconds() }} else {{ $null }} }}; \
         [pscustomobject]@{{ state=$t.State.ToString(); next_run_ts=(Epoch $i.NextRunTime); last_run_ts=(Epoch $i.LastRunTime); last_result=$i.LastTaskResult }} | ConvertTo-Json -Compress"
    );
    let output = crate::proc::quiet("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            parse_info(&text).unwrap_or_else(|error| TaskInfo { state: "unknown".into(), error: Some(error), ..TaskInfo::absent() })
        }
        Ok(output) => {
            let error = output_text(&output).trim().to_owned();
            // Определение читаемо и без PowerShell: Disabled виден в XML.
            let state = task_xml(&name)
                .filter(|xml| xml.contains("<Enabled>false</Enabled>"))
                .map(|_| "disabled".to_owned())
                .unwrap_or_else(|| "unknown".to_owned());
            TaskInfo { state, error: Some(error), ..TaskInfo::absent() }
        }
        Err(error) => TaskInfo { state: "unknown".into(), error: Some(error.to_string()), ..TaskInfo::absent() },
    }
}

#[cfg(test)]
pub fn query(_name: &str) -> TaskInfo {
    TaskInfo::absent()
}

fn parse_info(text: &str) -> Result<TaskInfo, String> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).map_err(|error| format!("task info JSON: {error}"))?;
    let state = match value["state"].as_str().unwrap_or("").to_lowercase().as_str() {
        "ready" | "queued" => "ready",
        "disabled" => "disabled",
        "running" => "running",
        _ => "unknown",
    };
    Ok(TaskInfo {
        state: state.into(),
        next_run_ts: value["next_run_ts"].as_i64(),
        last_run_ts: value["last_run_ts"].as_i64(),
        last_result: value["last_result"].as_i64(),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_arguments_quote_executable_with_spaces() {
        let args = create_args(Path::new(r"C:\Program Files\Brepsi\BrepsiBrain.exe"), 30);
        assert_eq!(args[0], "/Create");
        assert_eq!(args[4], "30");
        assert_eq!(args[6], TASK_NAME);
        assert_eq!(args[8], r#""C:\Program Files\Brepsi\BrepsiBrain.exe" sync"#);
    }

    #[test]
    fn delete_arguments_name_the_task() {
        assert_eq!(delete_args(TASK_NAME), vec!["/Delete", "/TN", TASK_NAME, "/F"]);
    }

    #[test]
    fn task_action_is_ours_only_for_our_exe() {
        assert!(action_is_ours(r#"<Command>"C:\Apps\BrepsiBrain.exe"</Command>"#));
        assert!(action_is_ours(r#"<Command>"C:\old\second-brain.exe"</Command>"#));
        // UTF-16LE из schtasks /XML после снятия нулей.
        assert!(action_is_ours("B\0r\0e\0p\0s\0i\0B\0r\0a\0i\0n\0.\0e\0x\0e\0"));
        assert!(!action_is_ours(r#"<Command>C:\Tools\backup.exe</Command>"#));
    }

    #[test]
    fn interval_bounds_are_validated_by_config_layer() {
        // Сам модуль принимает любое число; границы проверяет config/web.
        // 1439 — потолок `schtasks /SC MINUTE /MO`.
        assert_eq!(create_args(Path::new("x"), 5)[4], "5");
        assert_eq!(create_args(Path::new("x"), 1439)[4], "1439");
    }

    #[test]
    fn parse_task_info_maps_state_and_epochs() {
        let info = parse_info(r#"{"state":"Ready","next_run_ts":1762900000,"last_run_ts":1762899000,"last_result":0}"#).unwrap();
        assert_eq!(info.state, "ready");
        assert_eq!(info.next_run_ts, Some(1762900000));
        assert_eq!(info.last_run_ts, Some(1762899000));
        assert_eq!(info.last_result, Some(0));
        let disabled = parse_info(r#"{"state":"Disabled","next_run_ts":null,"last_run_ts":null,"last_result":267011}"#).unwrap();
        assert_eq!(disabled.state, "disabled");
        assert!(disabled.next_run_ts.is_none());
        assert_eq!(disabled.last_result, Some(267011));
        assert_eq!(parse_info(r#"{"state":"Queued"}"#).unwrap().state, "ready");
        assert_eq!(parse_info(r#"{"state":"Running"}"#).unwrap().state, "running");
        assert_eq!(parse_info(r#"{"state":"Bogus"}"#).unwrap().state, "unknown");
        assert!(parse_info("not json").is_err());
    }
}
