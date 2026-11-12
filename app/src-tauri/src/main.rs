// Десктопная оболочка Brepsi Brain (Tauri 2).
//
// Оборачивает локальный dashboard memory-base:
//   - находит корень dashboard (BREPSI_BRAIN_ROOT, иначе папка с data/config.toml)
//   - открывает главное окно на встроенном экране запуска (../dist/index.html)
//   - запускает `BrepsiBrain.exe serve --port 8765` (Rust-сервер) в рабочем потоке,
//     если порт свободен; повторяет с backoff, если сервер не поднялся
//   - переводит окно на http://127.0.0.1:8765, как только сервер ответил
//   - системный трей: статус sync, последние записи, источники, настройки, выход;
//     меню пересобирается из GET /api/tray, когда меняется его содержимое
//   - закрытие окна прячет его в трей вместо выхода; размер, позиция
//     и состояние «развёрнуто» переживают перезапуск (tauri-plugin-window-state)
//   - `BrepsiBrain.exe mcp` запускает Rust MCP-сервер на stdio вместо окна
//
// Один экземпляр на сессию пользователя: повторный запуск только показывает работающее окно.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod i18n;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::menu::{
    CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, Url, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

use i18n::{t, Lang};

const ADDR: &str = "127.0.0.1:8765";
const PORT: u16 = 8765;
const WINDOW_LABEL: &str = "main";
const TRAY_ID: &str = "tray-main";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(300);
const DASHBOARD_URL: &str = "http://127.0.0.1:8765/";
// Достаточно, чтобы прочитать экран запуска, когда сервер уже работает.
const SPLASH_MIN_VISIBLE: Duration = Duration::from_millis(700);
const SPLASH_FADE: Duration = Duration::from_millis(180);
const RETRY_BACKOFF_FIRST: Duration = Duration::from_secs(2);
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);
const TRAY_REFRESH: Duration = Duration::from_secs(60);
const SERVER_CHECK: Duration = Duration::from_secs(5);
const AUTOSTART_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const AUTOSTART_VALUE: &str = "BrepsiBrain";
const LEGACY_AUTOSTART_VALUE: &str = "SecondBrain";
const WINDOW_STATE: StateFlags = StateFlags::SIZE.union(StateFlags::POSITION).union(StateFlags::MAXIMIZED);

/// Дескриптор процесса dashboard, который запустили мы сами. `None`, если
/// порт уже обслуживает чужой процесс, который убивать нельзя.
/// Mutex, потому что заполняет его поток запуска, а забирает главный поток при
/// выходе; child кладётся сразу после spawn, поэтому ранний выход его подберёт.
struct ServerState {
    child: Mutex<Option<Child>>,
    /// Выход начат: надзор за сервером не должен поднимать его снова.
    stopping: AtomicBool,
}

fn stopping(app: &AppHandle) -> bool {
    app.state::<ServerState>().inner().stopping.load(Ordering::SeqCst)
}

fn lock_child(app: &AppHandle) -> MutexGuard<'_, Option<Child>> {
    app.state::<ServerState>()
        .inner()
        .child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn resolve_root() -> PathBuf {
    if let Some(root) = memory_core::config::root_from_env() {
        return PathBuf::from(root);
    }
    // Портативная копия: config.toml и data лежат рядом с exe. Рабочая папка
    // проверяется первой — ярлык может запускать exe из папки сборки, где
    // данных нет, а рядом с самой программой они есть.
    // Для рабочей папки голой `data\` мало: харнесс запускает `mcp` из папки
    // проекта, где своя `data\` — частое дело, и там молча создавалась пустая база.
    let looks_like_root = |dir: &Path| dir.join("data").is_dir() || dir.join("config.toml").is_file();
    let cwd_is_root = |dir: &Path| dir.join("data").join("memory.db").is_file() || dir.join("config.toml").is_file();
    let cwd = std::env::current_dir().ok();
    if let Some(dir) = cwd.as_deref().filter(|dir| cwd_is_root(dir)) {
        return dir.to_path_buf();
    }
    let exe_dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
    if let Some(dir) = exe_dir.as_deref().filter(|dir| looks_like_root(dir)) {
        return dir.to_path_buf();
    }
    exe_dir.or(cwd).unwrap_or_default()
}

fn port_open() -> bool {
    TcpStream::connect(ADDR).is_ok()
}

fn spawn_server(root: &Path) -> std::io::Result<Child> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .args(["serve", "--port"])
        .arg(PORT.to_string())
        .env("BREPSI_BRAIN_ROOT", root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // Отвязываемся от консоли, чтобы при запуске не мигало окно.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()
}

/// Поднимает dashboard при необходимости и ждёт ответа на ADDR.
/// Блокирующий; идёт в потоке запуска. По таймауту сервер останавливается,
/// чтобы повтор не оставил два процесса, дерущихся за порт.
fn ensure_server(app: &AppHandle, root: &Path) -> Result<(), String> {
    if port_open() {
        return Ok(());
    }
    let lang = i18n::current(root);
    let child = spawn_server(root).map_err(|e| t(lang, "server_failed").replace("{e}", &e.to_string()))?;
    {
        // Флаг проверяется под тем же мьютексом, под которым kill_server забирает
        // child: иначе запущенный в момент выхода сервер остался бы сиротой.
        let mut guard = lock_child(app);
        if stopping(app) {
            drop(guard);
            stop_child(child);
            return Err(t(lang, "quitting").to_string());
        }
        *guard = Some(child);
    }
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if port_open() {
            return Ok(());
        }
        {
            let mut guard = lock_child(app);
            let Some(child) = guard.as_mut() else {
                return Err(t(lang, "quitting").to_string());
            };
            if let Ok(Some(status)) = child.try_wait() {
                *guard = None;
                return Err(t(lang, "server_exited").replace("{status}", &status.to_string()));
            }
        }
        if Instant::now() >= deadline {
            if let Some(child) = lock_child(app).take() {
                stop_child(child);
            }
            return Err(t(lang, "port_timeout").to_string());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Вызывает функцию splash.* экрана запуска, если страница уже загружена.
fn splash_call(app: &AppHandle, script: &str) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.eval(script);
    }
}

fn open_dashboard(app: &AppHandle) -> Result<(), String> {
    let url = Url::parse(DASHBOARD_URL).map_err(|e| e.to_string())?;
    let window = app
        .get_webview_window(WINDOW_LABEL)
        .ok_or_else(|| t(Lang::En, "no_window").to_string())?;
    window.navigate(url).map_err(|e| e.to_string())
}

/// Точка входа потока: поднимает сервер, повторяя с backoff и объясняя
/// экрану запуска причину, затем меняет экран на dashboard и наполняет трей.
fn run_startup(app: AppHandle, root: PathBuf, last: Arc<Mutex<Option<Value>>>) {
    let started = Instant::now();
    let mut backoff = RETRY_BACKOFF_FIRST;
    loop {
        let result = ensure_server(&app, &root).and_then(|()| {
            thread::sleep(SPLASH_MIN_VISIBLE.saturating_sub(started.elapsed()));
            splash_call(&app, "window.splashLeaving && window.splashLeaving()");
            thread::sleep(SPLASH_FADE);
            open_dashboard(&app)
        });
        let message = match result {
            Ok(()) => break,
            Err(message) => message,
        };
        // Экранирование JSON-строки годится и для JavaScript-строки.
        let quoted = serde_json::to_string(&message).unwrap_or_else(|_| "\"\"".to_string());
        splash_call(&app, &format!("window.splashFailed && window.splashFailed({quoted})"));
        thread::sleep(backoff);
        backoff = (backoff * 2).min(RETRY_BACKOFF_MAX);
        splash_call(&app, "window.splashRetrying && window.splashRetrying()");
    }
    refresh_tray(&app, &root, &last, false);
    supervise_server(&app, &root);
}

/// Продолжение потока запуска. Сервер — отдельный процесс с panic=abort и
/// stderr=null: упав после старта, он оставлял окно на «connection refused» до
/// перезапуска приложения. Поднимаем его снова и перезагружаем страницу.
fn supervise_server(app: &AppHandle, root: &Path) {
    let mut backoff = RETRY_BACKOFF_FIRST;
    loop {
        thread::sleep(SERVER_CHECK);
        if stopping(app) {
            return;
        }
        if let Some(child) = lock_child(app).as_mut() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                continue;
            }
        }
        if port_open() {
            continue;
        }
        *lock_child(app) = None;
        match ensure_server(app, root).and_then(|()| open_dashboard(app)) {
            Ok(()) => backoff = RETRY_BACKOFF_FIRST,
            Err(_) if stopping(app) => return,
            Err(_) => {
                thread::sleep(backoff);
                backoff = (backoff * 2).min(RETRY_BACKOFF_MAX);
            }
        }
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Показывает окно и переводит dashboard на маршрут вроде `record/12`.
/// Идёт в том потоке, что доставил событие меню; eval только ставит скрипт в очередь.
fn open_route(app: &AppHandle, route: &str) {
    show_main_window(app);
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.eval(format!("location.hash = '{route}'"));
    }
}

fn set_tray_tooltip(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(text));
    }
}

const SYNC_STILL_RUNNING: &str = "still running";

/// Крошечный HTTP POST на голой std: возвращает строку статуса ответа, тело не
/// читается. Путь передаётся с ведущим слэшем.
fn post_endpoint(path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(ADDR).map_err(|e| e.to_string())?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    // Цикл может идти долго; долгое ожидание — норма, а не сбой.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).map_err(|e| format!("request write failed: {e}"))?;
    let mut buf = [0u8; 512];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        // Цикл дольше таймаута — не провал: сервер доводит его и пишет базу сам
        Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {
            return Ok(SYNC_STILL_RUNNING.to_string());
        }
        Err(e) => return Err(format!("response read failed: {e}")),
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    Ok(head.lines().next().unwrap_or("empty response").to_string())
}

/// Попросить панель выполнить один цикл синхронизации.
fn trigger_sync(lang: Lang) -> Result<String, String> {
    post_endpoint("/api/sync").map_err(|error| t(lang, "server_down").replace("{e}", &error))
}

/// Остановить локальный движок из трея и обновить меню.
fn stop_local_model(app: AppHandle, root: PathBuf, last: Arc<Mutex<Option<Value>>>) {
    thread::spawn(move || {
        let status = post_endpoint("/api/local/stop");
        let lang = i18n::current(&root);
        let text = match status {
            Ok(line) if line.contains("200") => t(lang, "local_stop").to_string(),
            Ok(line) => t(lang, "error").replace("{e}", &line),
            Err(error) => t(lang, "error").replace("{e}", &error),
        };
        set_tray_tooltip(&app, &text);
        refresh_tray(&app, &root, &last, false);
    });
}

/// Крошечный HTTP GET на голой std для документа трея. None, если сервер лежит
/// или отвечает чем-то другим: тогда меню так и пишет вместо старых чисел.
/// Блокирующий; сначала идёт в потоке setup, потом в потоке обновления.
fn fetch_tray(lang: Lang) -> Option<Value> {
    let mut stream = TcpStream::connect(ADDR).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let request = format!(
        "GET /api/tray?lang={} HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nConnection: close\r\n\r\n",
        lang.code()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n")?;
    if !head.lines().next()?.contains(" 200 ") {
        return None;
    }
    serde_json::from_str(body).ok()
}

/// Просит чужой `serve` на порту завершиться и ждёт, пока порт освободится.
fn request_shutdown() {
    if let Ok(mut stream) = TcpStream::connect(ADDR) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let request = "POST /api/shutdown HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        if stream.write_all(request.as_bytes()).is_ok() {
            let _ = stream.read_to_end(&mut Vec::new());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while port_open() && Instant::now() < deadline {
        thread::sleep(POLL_INTERVAL);
    }
}

fn no_window(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Путь exe из записи Run, если она есть.
/// Значение читается через RegGetValueW, а не разбором `reg query`: в pipe он
/// пишет в OEM-кодировке, и кириллический путь exe превращался в `�` — галка
/// не горела никогда, а клик по ней снова включал автозапуск.
#[cfg(windows)]
fn read_run_value(name: &str) -> Option<String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    let subkey = HSTRING::from(AUTOSTART_KEY.trim_start_matches(r"HKCU\"));
    let name = HSTRING::from(name);
    let mut bytes = 0u32;
    // SAFETY: первый вызов только узнаёт размер; буфер второго — ровно этого
    // размера в байтах, и RegGetValueW сам завершает строку нулём.
    unsafe {
        if RegGetValueW(HKEY_CURRENT_USER, &subkey, &name, RRF_RT_REG_SZ, None, None, Some(&mut bytes)) != ERROR_SUCCESS {
            return None;
        }
        let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
        if RegGetValueW(HKEY_CURRENT_USER, &subkey, &name, RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut bytes)) != ERROR_SUCCESS {
            return None;
        }
        let len = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]).trim().trim_matches('"').to_string())
    }
}

#[cfg(windows)]
fn autostart_target() -> Option<String> {
    read_run_value(AUTOSTART_VALUE)
}

#[cfg(not(windows))]
fn autostart_target() -> Option<String> {
    None
}

/// Галка «автозапуск» правдива, только если запись ведёт на этот exe: после переноса
/// папки старая запись оставалась, галка горела, а Windows ничего не поднимала.
fn autostart_enabled() -> bool {
    let current = std::env::current_exe().ok();
    autostart_target().is_some_and(|target| {
        current.as_deref().is_some_and(|exe| exe.as_os_str().eq_ignore_ascii_case(target.as_str()))
    })
}

/// Запись Run есть, но ведёт на прежнее место exe: владелец автозапуск включал —
/// переписываем путь на текущий.
fn repair_autostart_path() {
    if autostart_target().is_some() && !autostart_enabled() {
        let _ = set_autostart(true);
    }
}

/// Добавляет или убирает запись Run для пользователя; только по клику владельца.
fn set_autostart(enabled: bool) -> Result<(), String> {
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        reg_add_run(AUTOSTART_VALUE, &format!("\"{}\"", exe.display()))
    } else {
        // Снимаем и прежнее имя продукта: иначе запись осталась бы в автозагрузке.
        let _ = reg_delete_run(LEGACY_AUTOSTART_VALUE);
        reg_delete_run(AUTOSTART_VALUE)
    }
}

fn reg_add_run(name: &str, data: &str) -> Result<(), String> {
    let mut command = Command::new("reg");
    command.args(["add", AUTOSTART_KEY, "/v", name, "/t", "REG_SZ", "/f", "/d", data]);
    run_reg(&mut command)
}

fn reg_delete_run(name: &str) -> Result<(), String> {
    let mut command = Command::new("reg");
    command.args(["delete", AUTOSTART_KEY, "/v", name, "/f"]);
    run_reg(&mut command)
}

fn run_reg(command: &mut Command) -> Result<(), String> {
    let status = no_window(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("reg exited with {status}"))
    }
}

/// Решение миграции имени автозапуска: старую запись удаляем только после
/// успешного переноса, чтобы сбой не потерял автозапуск владельца.
#[derive(Debug, PartialEq, Eq)]
enum Migration {
    None,
    Rename,
    DropLegacy,
}

fn migration_action(new_present: bool, legacy_present: bool) -> Migration {
    match (new_present, legacy_present) {
        (_, false) => Migration::None,
        (true, true) => Migration::DropLegacy,
        (false, true) => Migration::Rename,
    }
}

#[cfg(windows)]
fn migrate_autostart_name() {
    let legacy = read_run_value(LEGACY_AUTOSTART_VALUE);
    let new = read_run_value(AUTOSTART_VALUE);
    match migration_action(new.is_some(), legacy.is_some()) {
        Migration::None => {}
        Migration::DropLegacy => {
            let _ = reg_delete_run(LEGACY_AUTOSTART_VALUE);
        }
        Migration::Rename => {
            if let Some(target) = legacy {
                if reg_add_run(AUTOSTART_VALUE, &target).is_ok() {
                    let _ = reg_delete_run(LEGACY_AUTOSTART_VALUE);
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn migrate_autostart_name() {}

fn plain_item(app: &AppHandle, text: &str) -> tauri::Result<tauri::menu::MenuItem<tauri::Wry>> {
    MenuItemBuilder::new(text).enabled(false).build(app)
}

/// Собирает всё меню из одного документа трея. Нативные меню создаются
/// только в главном потоке, поэтому поток обновления передаёт это ему.
fn build_menu(app: &AppHandle, root: &Path, doc: Option<&Value>) -> tauri::Result<Menu<tauri::Wry>> {
    let lang = i18n::current(root);
    let text = |key: &str| doc.and_then(|d| d[key].as_str()).unwrap_or("").to_string();
    let status = match doc {
        Some(d) if d["ok"].as_bool() == Some(true) => format!("✓ {}", text("status")),
        Some(_) => text("status"),
        None => t(lang, "db_unavailable").to_string(),
    };
    let mut menu = MenuBuilder::new(app).item(&plain_item(app, &status)?);
    if doc.is_some() {
        menu = menu.item(&plain_item(app, &text("summary"))?);
        let schedule = text("schedule");
        if !schedule.is_empty() {
            menu = menu.item(&plain_item(app, &schedule)?);
        }
    }

    let empty = Vec::new();
    let recent = doc.and_then(|d| d["recent"].as_array()).unwrap_or(&empty);
    let mut recent_menu = SubmenuBuilder::new(app, t(lang, "recent"));
    for item in recent {
        let (Some(id), Some(title)) = (item["id"].as_i64(), item["title"].as_str()) else {
            continue;
        };
        let label: String = title.chars().take(64).collect();
        recent_menu = recent_menu.item(&MenuItemBuilder::with_id(format!("record:{id}"), label).build(app)?);
    }
    if recent.is_empty() {
        recent_menu = recent_menu.item(&plain_item(app, t(lang, "empty"))?);
    }

    let sources = doc.and_then(|d| d["sources"].as_array()).unwrap_or(&empty);
    let mut sources_menu = SubmenuBuilder::new(app, t(lang, "sources").replace("{n}", &sources.len().to_string()));
    for row in sources {
        let line = t(lang, "source_line")
            .replace("{name}", row["name"].as_str().unwrap_or(""))
            .replace("{records}", &row["records"].as_i64().unwrap_or(0).to_string())
            .replace("{cards}", &row["cards"].as_i64().unwrap_or(0).to_string());
        sources_menu = sources_menu.item(&plain_item(app, &line)?);
    }
    if sources.is_empty() {
        sources_menu = sources_menu.item(&plain_item(app, t(lang, "empty"))?);
    }

    let choice = i18n::saved_choice(root);
    let lang_item = |id: &str, label: &str, value: &str| {
        CheckMenuItemBuilder::with_id(id, label).checked(choice == value).build(app)
    };
    // Названия языков — на самих языках: их ищут, когда интерфейс непонятен.
    let language_menu = SubmenuBuilder::new(app, t(lang, "language"))
        .item(&lang_item("lang:auto", t(lang, "lang_auto"), "auto")?)
        .item(&lang_item("lang:ru", "Русский", "ru")?)
        .item(&lang_item("lang:en", "English", "en")?)
        .build()?;
    let settings_menu = SubmenuBuilder::new(app, t(lang, "settings"))
        .item(&MenuItemBuilder::with_id("settings", t(lang, "model")).build(app)?)
        .item(&MenuItemBuilder::with_id("models", t(lang, "models")).build(app)?)
        .item(&language_menu)
        .item(
            &CheckMenuItemBuilder::with_id("autostart", t(lang, "autostart"))
                .checked(autostart_enabled())
                .build(app)?,
        )
        .item(&MenuItemBuilder::with_id("log", t(lang, "log")).build(app)?)
        .item(&MenuItemBuilder::with_id("folder", t(lang, "folder")).build(app)?)
        .build()?;

    let local_running = doc.and_then(|doc| doc["local"]["running"].as_bool()).unwrap_or(false);
    menu = menu
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&MenuItemBuilder::with_id("open", t(lang, "open")).build(app)?)
        .item(&MenuItemBuilder::with_id("sync", t(lang, "sync")).build(app)?);
    if local_running {
        menu = menu.item(&MenuItemBuilder::with_id("local-stop", t(lang, "local_stop")).build(app)?);
    }
    menu
        .item(&recent_menu.build()?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&sources_menu.build()?)
        .item(&settings_menu)
        .item(&MenuItemBuilder::with_id("about", t(lang, "about")).build(app)?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&MenuItemBuilder::with_id("quit", t(lang, "quit")).build(app)?)
        .build()
}

/// Пересобирает меню при изменении документа: подменить открытое меню значит
/// закрыть его под курсором, поэтому обновление с тем же содержимым пропускается.
/// `force` — для изменений, которых в документе нет (проверка автозапуска).
/// Блокирующая загрузка; вызывать из рабочего потока.
fn refresh_tray(app: &AppHandle, root: &Path, last: &Arc<Mutex<Option<Value>>>, force: bool) {
    let doc = fetch_tray(i18n::current(root));
    {
        let mut guard = last.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !force && *guard == doc {
            return;
        }
        guard.clone_from(&doc);
    }
    let handle = app.clone();
    let root = root.to_path_buf();
    let _ = app.run_on_main_thread(move || {
        if let (Some(tray), Ok(menu)) = (handle.tray_by_id(TRAY_ID), build_menu(&handle, &root, doc.as_ref())) {
            let _ = tray.set_menu(Some(menu));
        }
    });
}

/// Выполняет запрос sync вне UI-потока и сообщает итог во всплывающей подсказке.
fn start_sync(app: AppHandle, root: PathBuf, last: Arc<Mutex<Option<Value>>>) {
    let lang = i18n::current(&root);
    set_tray_tooltip(&app, t(lang, "syncing"));
    thread::spawn(move || {
        let tooltip = match trigger_sync(lang) {
            Ok(status) if status == SYNC_STILL_RUNNING => t(lang, "sync_long").to_string(),
            Ok(status) if status.contains("200") => t(lang, "sync_done").replace("{status}", &status),
            Ok(status) => t(lang, "sync_failed").replace("{status}", &status),
            Err(e) => t(lang, "error").replace("{e}", &e),
        };
        set_tray_tooltip(&app, &tooltip);
        refresh_tray(&app, &root, &last, false);
    });
}

fn open_folder(root: &Path) {
    let _ = Command::new("explorer").arg(root).spawn();
}

fn build_tray(app: &AppHandle, root: &Path) -> tauri::Result<Arc<Mutex<Option<Value>>>> {
    // значок трея с id "main" создаётся сам из app.trayIcon в tauri.conf.json
    // ещё до запуска setup. Меню и обработчиков у него нет, поэтому прячем его
    // и собираем полностью связанный значок вместо двух значков в трее.
    if let Some(auto_tray) = app.tray_by_id("main") {
        let _ = auto_tray.set_visible(false);
    }

    // Сервер ещё не поднят: run_startup наполнит меню, когда поднимется.
    let menu = build_menu(app, root, None)?;
    let last = Arc::new(Mutex::new(None));

    // Сырой RGBA вкомпилирован: в tauri 2.12 нет Image::from_bytes без
    // необязательной фичи image-png, поэтому поставляем уже разобранные пиксели.
    let icon = tauri::image::Image::new(&include_bytes!("../icons/icon.rgba")[..], 64, 64);
    // Тот же значок на окне и в панели задач, а не только в трее.
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_icon(icon.clone());
    }

    let menu_root = root.to_path_buf();
    let menu_last = last.clone();
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip("Brepsi Brain")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
            "sync" => start_sync(app.clone(), menu_root.clone(), menu_last.clone()),
            "about" => open_route(app, "about"),
            "folder" => open_folder(&menu_root),
            "log" => open_folder(&menu_root.join("data").join("sync.log")),
            "settings" => open_route(app, "settings"),
            "models" => open_route(app, "models"),
            "local-stop" => stop_local_model(app.clone(), menu_root.clone(), menu_last.clone()),
            "autostart" => {
                let wanted = !autostart_enabled();
                if let Err(e) = set_autostart(wanted) {
                    let lang = i18n::current(&menu_root);
                    set_tray_tooltip(app, &t(lang, "autostart_failed").replace("{e}", &e));
                }
                let (app, root, last) = (app.clone(), menu_root.clone(), menu_last.clone());
                thread::spawn(move || refresh_tray(&app, &root, &last, true));
            }
            id if id.starts_with("lang:") => {
                let _ = i18n::save_choice(&menu_root, &id["lang:".len()..]);
                // Панель берёт язык при загрузке страницы.
                if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
                    let _ = window.eval("location.reload()");
                }
                let (app, root, last) = (app.clone(), menu_root.clone(), menu_last.clone());
                thread::spawn(move || refresh_tray(&app, &root, &last, true));
            }
            id if id.starts_with("record:") => {
                if let Ok(record) = id["record:".len()..].parse::<i64>() {
                    open_route(app, &format!("record/{record}"));
                }
            }
            "quit" => {
                // std::process::exit(0) обходит цикл событий Tauri, поэтому RunEvent::Exit
                // (с очисткой сервера и сохранением состояния плагином) уже не сработает —
                // сохраняем состояние окна и гасим сервер здесь.
                let _ = app.save_window_state(WINDOW_STATE);
                kill_server(app);
                std::process::exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => show_main_window(tray.app_handle()),
            _ => {}
        })
        .build(app)?;

    let refresher = app.clone();
    let refresher_root = root.to_path_buf();
    let refresher_last = last.clone();
    thread::spawn(move || {
        // Язык могли сменить в панели: тогда меню пересобирается, даже если документ тот же.
        let mut lang = i18n::current(&refresher_root);
        loop {
            thread::sleep(TRAY_REFRESH);
            let now = i18n::current(&refresher_root);
            refresh_tray(&refresher, &refresher_root, &refresher_last, now != lang);
            lang = now;
        }
    });
    Ok(last)
}

/// Останавливает сервер, который запустили мы; taskkill /T снимает дерево процессов,
/// чтобы сервер не оставил порт 8765 занятым. В Windows нет SIGTERM.
fn stop_child(mut child: Child) {
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let _ = no_window(Command::new("taskkill").args(["/PID", &pid, "/T", "/F"]))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Идёт в главном потоке из RunEvent::Exit и из пункта выхода в трее.
fn kill_server(app: &AppHandle) {
    app.state::<ServerState>().inner().stopping.store(true, Ordering::SeqCst);
    let child = lock_child(app).take();
    if let Some(child) = child {
        stop_child(child);
    }
}

/// `BrepsiBrain.exe mcp`: MCP по stdio без окна и без single-instance — его
/// запускает харнесс, параллельно с открытым приложением. GUI-подсистема не мешает:
/// stdin/stdout — унаследованные от харнесса пайпы, консоль для них не нужна.
fn run_mcp(root: &Path) -> i32 {
    let path = memory_core::config::db_path_from_env()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("data").join("memory.db"));
    let db = match memory_core::db::Database::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("memory-base: cannot open {}: {e}", path.display());
            return 1;
        }
    };
    let stdin = std::io::stdin();
    match memory_core::mcp::serve(&db, None, std::io::BufReader::new(stdin.lock()), std::io::stdout().lock()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("memory-base: stdio: {e}");
            1
        }
    }
}

/// `BrepsiBrain.exe serve [--port N]`: HTTP-панель без окна Tauri.
/// Переходный режим: полный API ещё портируется, статика и `/api/ui` уже здесь.
fn run_serve(root: &Path) -> i32 {
    let mut port = 8765u16;
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--port" {
            if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                port = value;
            }
        }
    }
    match memory_core::web::serve(root.to_path_buf(), &format!("127.0.0.1:{port}")) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("memory-base: serve: {error}");
            1
        }
    }
}

/// `BrepsiBrain.exe sync`: один цикл для планировщика, без окна и HTTP.
fn run_sync(root: &Path) -> i32 {
    let db_path = memory_core::config::db_path_from_env()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("data").join("memory.db"));
    let log_path = db_path.parent().unwrap_or(root).join("sync.log");
    let started = memory_core::db::now_ts();
    let mut record = serde_json::json!({"ts": started, "task": "memory-base-sync"});
    let mut code = 0;
    match memory_core::db::Database::open(&db_path) {
        Ok(db) => {
            let config = memory_core::config::load_config_with_key(
                None,
                memory_core::config::api_key_from_env(),
            );
            match config {
                Ok(config) => match memory_core::ingest::ingest_all(
                    &db,
                    &memory_core::ingest::sources_for(&config),
                    &config,
                ) {
                    Ok(counts) => {
                        let extra = memory_core::sources_extra::ingest_extra(&db, &config);
                        let new_cards = counts.kimi + counts.claude + counts.codex + counts.git + counts.obsidian
                            + extra.values().sum::<usize>();
                        record["status"] = serde_json::json!("ok");
                        record["counts"] = serde_json::json!({
                            "kimi": counts.kimi, "claude": counts.claude, "codex": counts.codex,
                            "git": counts.git, "obsidian": counts.obsidian,
                            "chatgpt": extra.get("chatgpt").copied().unwrap_or(0),
                            "gmail": extra.get("gmail").copied().unwrap_or(0),
                            "yandex": extra.get("yandex").copied().unwrap_or(0),
                            "github": extra.get("github").copied().unwrap_or(0),
                            "notion": extra.get("notion").copied().unwrap_or(0),
                        });
                        record["new_cards"] = serde_json::json!(new_cards);
                        let stage = memory_core::collect::run(&db, &config, false);
                        record["llm"] = stage.llm;
                        record["projects"] = stage.projects;
                        if stage.failed {
                            code = 1;
                        }
                        let _ = db.set_state("web:last_sync", &started.to_string());
                    }
                    Err(error) => {
                        record["status"] = serde_json::json!("error");
                        record["error"] = serde_json::json!(error.to_string());
                        code = 1;
                    }
                },
                Err(error) => {
                    record["status"] = serde_json::json!("error");
                    record["error"] = serde_json::json!(error.to_string());
                    code = 1;
                }
            }
        }
        Err(error) => {
            record["status"] = serde_json::json!("error");
            record["error"] = serde_json::json!(error.to_string());
            code = 1;
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        use std::io::Write;
        let _ = writeln!(file, "{}", record);
    }
    code
}

/// `BrepsiBrain.exe search <q> [--limit N]`: то же, что скилл memory-search
/// раньше получал от Python CLI.
fn run_search(root: &Path, mut args: std::iter::Skip<std::env::Args>) -> i32 {
    let mut query = String::new();
    let mut limit = 8i64;
    while let Some(arg) = args.next() {
        if arg == "--limit" {
            if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                limit = value;
            }
        } else if query.is_empty() {
            query = arg;
        } else {
            query.push(' ');
            query.push_str(&arg);
        }
    }
    if query.trim().is_empty() {
        eprintln!("usage: BrepsiBrain.exe search <query> [--limit N]");
        return 2;
    }
    let db_path = memory_core::config::db_path_from_env()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("data").join("memory.db"));
    let db = match memory_core::db::Database::open(&db_path) {
        Ok(db) => db,
        Err(error) => {
            eprintln!("memory-base: database: {error}");
            return 1;
        }
    };
    match db.search(&query, limit.clamp(1, 1000), None, &memory_core::db::Filters::default()) {
        Ok(cards) => {
            for card in cards {
                let flat = card.body.split_whitespace().collect::<Vec<_>>().join(" ");
                let snippet: String = flat.chars().take(160).collect();
                println!("[{}] ({}, {}) {} — {}", card.id, memory_core::text::format_date(card.ts), card.source, card.title, snippet);
            }
            0
        }
        Err(error) => {
            eprintln!("memory-base: search: {error}");
            1
        }
    }
}

/// Прежний identifier приложения (Second Brain). Состояние окна и профиль
/// WebView2 лежат в папках с ним; переносим один раз, иначе после
/// переименования потерялись бы размер окна и локальные настройки панели.
fn migrate_legacy_appdata() {
    let pairs = [("APPDATA", "com.secondbrain.app", "com.brepsibrain.app"), ("LOCALAPPDATA", "com.secondbrain.app", "com.brepsibrain.app")];
    for (env, old_name, new_name) in pairs {
        let Some(base) = std::env::var_os(env).map(PathBuf::from) else { continue };
        let old = base.join(old_name);
        let new = base.join(new_name);
        if old.is_dir() && !new.exists() {
            let _ = std::fs::rename(&old, &new);
        }
    }
}

/// Нативный выбор папки для склада моделей. Панель вызывает команду через
/// `window.__TAURI__.core.invoke`; в обычном браузере её нет и путь вводят руками.
#[tauri::command]
fn pick_folder() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("Папка для локальных моделей")
        .pick_folder()
        .map(|path| path.display().to_string())
}

fn update_config() -> tauri_selfupdate::Config {
    tauri_selfupdate::Config {
        // Токен вшивается только в сборки для своих; без него — открытый репозиторий
        // на GitHub: туда же ведут README и «О программе».
        source: match option_env!("SELFUPDATE_GITHUB_TOKEN") {
            Some(token) => tauri_selfupdate::Source::GitHubPrivate {
                repo: "AndreyVerse/releases".into(),
                token: token.into(),
                tag_prefix: "BrepsiBrain".into(),
            },
            None => tauri_selfupdate::Source::GitHub { repo: "AndreyVerse/BrepsiBrain".into() },
        },
        asset: "BrepsiBrain.exe".into(),
        // Публичный ключ общий для программ владельца; приватный лежит вне репозитория.
        public_key: include_str!("../updater.pub").trim().into(),
        user_agent: "BrepsiBrain".into(),
    }
}

/// Проверка новой версии из панели; HTTP блокирующий, поэтому в пуле `spawn_blocking`.
#[tauri::command]
async fn check_update() -> Result<tauri_selfupdate::UpdateInfo, String> {
    tauri::async_runtime::spawn_blocking(|| {
        tauri_selfupdate::check(&update_config(), env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Скачать, проверить подпись, подменить exe и перезапуститься. Прогресс — событие
/// `update-progress`; при успехе процесс завершается и ответа панель не получает.
/// MCP-процессы харнессов остаются на прежнем exe, пока харнесс их не перезапустит.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let config = update_config();
        let info = tauri_selfupdate::check(&config, env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())?;
        let progress = app.clone();
        tauri_selfupdate::install(&config, &info, move |p| {
            let _ = progress.emit("update-progress", p);
        })
        .map_err(|e| e.to_string())?;
        // Сервер панели запущен со старого exe и держит порт: новый экземпляр
        // увидел бы открытый порт и остался со старой панелью.
        kill_server(&app);
        // Порт мог держать сирота-`serve` от упавшего экземпляра: своего child у нас
        // тогда нет, и новый exe подхватил бы старый сервер.
        if port_open() {
            request_shutdown();
        }
        if let Err(error) = tauri_selfupdate::relaunch() {
            // Перезапуск не удался: окно остаётся, надзор поднимет сервер снова.
            app.state::<ServerState>().inner().stopping.store(false, Ordering::SeqCst);
            return Err(error.to_string());
        }
        app.exit(0);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = std::env::args().nth(1);
    if command.as_deref() == Some("serve") {
        std::process::exit(run_serve(&resolve_root()));
    }
    if command.as_deref() == Some("sync") {
        std::process::exit(run_sync(&resolve_root()));
    }
    if command.as_deref() == Some("search") {
        std::process::exit(run_search(&resolve_root(), std::env::args().skip(2)));
    }
    if command.as_deref() == Some("mcp") {
        std::process::exit(run_mcp(&resolve_root()));
    }
    // До single-instance: после обновления ждёт выхода прежнего окна.
    tauri_selfupdate::startup();
    migrate_legacy_appdata();
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![pick_folder, check_update, install_update])
        // Регистрируется первым, как требует плагин: второй процесс выходит раньше,
        // чем что-либо стартует. Колбэк выполняется в первом экземпляре.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_window_state::Builder::default().with_state_flags(WINDOW_STATE).build())
        .setup(|app| {
            let root = resolve_root();
            thread::spawn(|| {
                migrate_autostart_name();
                repair_autostart_path();
            });
            // Окно создаётся здесь, а не из конфига: скрипт инициализации передаёт язык
            // трея заставке и панели при каждой загрузке страницы.
            let config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == WINDOW_LABEL)
                .cloned()
                .ok_or("main window config missing")?;
            // __SB_LANG — для заставки (выбор из трея на момент запуска), __SB_SYSTEM_LANG —
            // для панели, которая сама решает "auto" по data/ui.json при каждой загрузке.
            let system = serde_json::to_string(i18n::system_lang().code())?;
            let current = serde_json::to_string(i18n::current(&root).code())?;
            WebviewWindowBuilder::from_config(app.handle(), &config)?
                .initialization_script(format!("window.__SB_SYSTEM_LANG = {system}; window.__SB_LANG = {current};"))
                .build()?;
            app.manage(ServerState {
                child: Mutex::new(None),
                stopping: AtomicBool::new(false),
            });
            let last = build_tray(app.handle(), &root)?;
            // Не в этом потоке: он UI-поток, и экран запуска не смог бы
            // отрисоваться, пока ждёт сервер.
            let handle = app.handle().clone();
            thread::spawn(move || run_startup(handle, root, last));
            Ok(())
        })
        .on_window_event(|window, event| {
            // Поведение трей-приложения: кнопка закрытия прячет окно;
            // пункт "Выход" в трее — настоящий выход.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                // Плагин пишет состояние только при выходе, а окно в трее может не
                // дожить до него (выключение Windows) — сохраняем сразу.
                let _ = window.app_handle().save_window_state(WINDOW_STATE);
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())?;

    // Идёт в главном потоке; в Windows нет SIGTERM, поэтому RunEvent::Exit —
    // единственная точка очистки при закрытии оконным менеджером и выходе из трея.
    app.run(|app_handle, event| {
        if let RunEvent::Exit = event {
            kill_server(app_handle);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_migration_matrix() {
        assert_eq!(migration_action(false, false), Migration::None);
        assert_eq!(migration_action(true, false), Migration::None);
        assert_eq!(migration_action(true, true), Migration::DropLegacy);
        assert_eq!(migration_action(false, true), Migration::Rename);
    }

    #[test]
    fn autostart_value_renamed_with_legacy_kept() {
        assert_eq!(AUTOSTART_VALUE, "BrepsiBrain");
        assert_eq!(LEGACY_AUTOSTART_VALUE, "SecondBrain");
    }
}
