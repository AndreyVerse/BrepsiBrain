//! Локальный AI: портативный Ollama, модели по требованию, установка и статистика.
//!
//! Движок ставится в `<models_root>/runtime/ollama`, хранилище Ollama — в
//! `<models_root>/ollama`, скачанные GGUF — в `<models_root>/gguf`. Каждая
//! установка создаёт производную модель `brepsi/<id>` с фиксированным окном
//! контекста: каталог не зависит от того, как модель названа в реестре.
//!
//! Жизненный цикл: по умолчанию `serve` поднимается под задачу, после простоя
//! гасится watchdog'ом; счётчик `in_flight` не даёт погасить его посреди запроса.
//! Старт в двух процессах (GUI и `sync`) разводит lease в базе и проверка порта.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::catalog::{self, Source};
use crate::config::{Config, LocalMode, Task};
use crate::db::{now_ts, BenchResult, Database};
use crate::hardware::{self, Backend, Tier, Vendor};

/// Версия портативного Ollama.
pub const OLLAMA_VERSION: &str = "v0.40.3";
const OLLAMA_ZIP_URL: &str =
    "https://github.com/ollama/ollama/releases/download/{version}/ollama-windows-amd64.zip";
/// SHA-256 архива `OLLAMA_VERSION` из `sha256sum.txt` релиза (снято 2026-10-17).
const OLLAMA_ZIP_SHA256: &str = "e5ecff02dfc7cae8ddf6d6b7a7ac5f727ffeedf4491f44ae75ff5b0820dbbbd5";
const OLLAMA_ZIP_SIZE: u64 = 1_468_053_171;
/// Тот же ассет через API: `releases/download` отдавал 504 (08.11.2026), пока API
/// и release-assets работали. Подлинность держит SHA-256, а не источник.
const OLLAMA_ASSET_API_URL: &str = "https://api.github.com/repos/ollama/ollama/releases/assets/629762437";
const DEFAULT_PORT: u16 = 11434;
const PORT_SCAN: u16 = 20;
const READY_TIMEOUT: Duration = Duration::from_secs(90);
const RUNTIME_RESERVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const LEASE_KEY: &str = "local:runtime";
const LEASE_TTL_SECONDS: i64 = 180;

#[derive(Default)]
struct RuntimeState {
    child: Option<Child>,
    /// pid `ollama serve`, в том числе принятого у прошлого запуска: по нему
    /// движок можно погасить, даже если `Child` не наш.
    pid: Option<u32>,
    port: u16,
    session_id: Option<i64>,
    db_path: Option<PathBuf>,
    in_flight: usize,
    last_activity: i64,
}

static RUNTIME: Mutex<RuntimeState> = Mutex::new(RuntimeState {
    child: None,
    pid: None,
    port: 0,
    session_id: None,
    db_path: None,
    in_flight: 0,
    last_activity: 0,
});

#[derive(Debug, Clone, Default)]
pub struct InstallProgress {
    /// install | runtime | bench
    pub kind: String,
    pub id: String,
    /// runtime | unpack | download | import | verify | done | error
    pub phase: String,
    pub status: String,
    pub done: u64,
    pub total: u64,
    pub speed_bps: u64,
    pub eta_seconds: Option<u64>,
    meter: RateMeter,
    pub active: bool,
    pub error: Option<String>,
}

/// Скорость по реально переданным байтам: докачанное с прошлого раза и слои,
/// уже лежащие на диске, её не завышают.
#[derive(Debug, Clone, Default)]
struct RateMeter {
    anchor: Option<(Instant, u64)>,
    bps: f64,
}

impl RateMeter {
    fn sample(&mut self, now: Instant, moved: u64) -> f64 {
        let Some((at, from)) = self.anchor else {
            self.anchor = Some((now, moved));
            return self.bps;
        };
        if moved < from {
            self.anchor = Some((now, moved));
            self.bps = 0.0;
            return 0.0;
        }
        let elapsed = now.duration_since(at).as_secs_f64();
        if elapsed < 1.0 {
            return self.bps;
        }
        let rate = (moved - from) as f64 / elapsed;
        self.bps = if self.bps == 0.0 { rate } else { self.bps * 0.6 + rate * 0.4 };
        if self.bps < 1.0 {
            self.bps = 0.0;
        }
        self.anchor = Some((now, moved));
        self.bps
    }
}

static JOB: Mutex<Option<InstallProgress>> = Mutex::new(None);
static CANCEL: AtomicBool = AtomicBool::new(false);

fn boot_id() -> &'static str {
    static BOOT: OnceLock<String> = OnceLock::new();
    BOOT.get_or_init(|| format!("{}-{}", std::process::id(), now_ts()))
}

// -- пути ---------------------------------------------------------------------------

fn runtime_dir(models_root: &Path) -> PathBuf {
    models_root.join("runtime").join("ollama")
}

fn runtime_exe(models_root: &Path) -> PathBuf {
    runtime_dir(models_root).join("ollama.exe")
}

fn store_dir(models_root: &Path) -> PathBuf {
    models_root.join("ollama")
}

fn gguf_dir(models_root: &Path) -> PathBuf {
    models_root.join("gguf")
}

fn db_path(config: &Config) -> PathBuf {
    crate::config::db_path_from_env().unwrap_or_else(|| config.root().join("data").join("memory.db"))
}

fn open_db(config: &Config) -> Option<Database> {
    Database::open(db_path(config)).ok()
}

const SERVE_PID_KEY: &str = "local:serve_pid";

/// Запомненный pid/порт движка: после падения процесса его можно принять
/// обратно и погасить, а не оставлять держать VRAM.
fn stored_serve(config: &Config) -> Option<(u32, u16)> {
    let db = open_db(config)?;
    let text = db.get_state(SERVE_PID_KEY).ok().flatten()?;
    let (pid, port) = text.split_once(':')?;
    let (pid, port) = (pid.parse().ok()?, port.parse().ok()?);
    pid_is_our_runtime(pid, &config.models_root()).then_some((pid, port))
}

/// Pid всё ещё принадлежит нашему портативному движку. Windows переиспользует
/// pid, а `stop()` гасит дерево через `taskkill /T /F`: принять чужой pid за свой
/// значит убить процесс владельца.
fn pid_is_our_runtime(pid: u32, models_root: &Path) -> bool {
    #[cfg(windows)]
    {
        use windows::core::PWSTR;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let expected = runtime_exe(models_root).to_string_lossy().to_ascii_lowercase();
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
        let Ok(handle) = handle else { return false };
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let queried =
            unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut size) };
        let _ = unsafe { CloseHandle(handle) };
        queried.is_ok() && String::from_utf16_lossy(&buffer[..size as usize]).to_ascii_lowercase() == expected
    }
    #[cfg(not(windows))]
    {
        let _ = (pid, models_root);
        false
    }
}

fn remember_serve(config: &Config, pid: u32, port: u16) {
    if let Some(db) = open_db(config) {
        let _ = db.set_state(SERVE_PID_KEY, &format!("{pid}:{port}"));
    }
}

fn forget_serve(db_path: Option<&Path>) {
    if let Some(path) = db_path {
        if let Ok(db) = Database::open(path) {
            let _ = db.set_state(SERVE_PID_KEY, "");
        }
    }
}

fn configured_port(config: &Config) -> u16 {
    config
        .local
        .base_url
        .as_deref()
        .and_then(|url| {
            let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
            let host = rest.split('/').next().unwrap_or(rest);
            host.rsplit_once(':').and_then(|(_, port)| port.parse::<u16>().ok())
        })
        .unwrap_or(DEFAULT_PORT)
}

fn local_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// Записать модель в текущую сессию движка (для статистики «какие модели»).
fn note_model(config: &Config, model: &str) {
    let session = RUNTIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).session_id;
    if let (Some(id), Some(db)) = (session, open_db(config)) {
        let _ = db.touch_local_session(id, model, now_ts());
    }
}

// -- HTTP ---------------------------------------------------------------------------

fn agent(timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .tls_config(tls)
        .build()
        .into()
}

fn api_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

fn api_json(port: u16, path: &str, body: &Value, timeout: Duration) -> Result<Value, String> {
    let response = agent(timeout)
        .post(&api_url(port, path))
        .header("Content-Type", "application/json")
        .send(body.to_string())
        .map_err(|error| format!("локальный движок недоступен: {error}"))?;
    let status = response.status().as_u16();
    let mut response = response;
    let raw = response.body_mut().read_to_string().map_err(|error| error.to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("локальный движок ответил HTTP {status}: {raw}"));
    }
    serde_json::from_str(&raw).map_err(|error| format!("ответ движка не JSON: {error}"))
}

fn tags_at(port: u16) -> Result<Vec<String>, String> {
    let mut response = agent(Duration::from_secs(2))
        .get(&api_url(port, "/api/tags"))
        .call()
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let raw = response.body_mut().read_to_string().map_err(|error| error.to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let value: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
    Ok(value
        .get("models")
        .and_then(Value::as_array)
        .map(|models| models.iter().filter_map(|model| model.get("name").and_then(Value::as_str)).map(str::to_owned).collect())
        .unwrap_or_default())
}

fn tag_installed(port: u16, tag: &str) -> bool {
    match tags_at(port) {
        Ok(names) => names.iter().any(|name| name == tag || name == &format!("{tag}:latest")),
        Err(_) => false,
    }
}

// -- установка рантайма ---------------------------------------------------------------

fn runtime_installed(models_root: &Path) -> bool {
    // Маркер пишется последним: exe без него — оборванная распаковка.
    runtime_exe(models_root).is_file() && runtime_marker(models_root).is_some()
}

fn runtime_marker(models_root: &Path) -> Option<String> {
    fs::read_to_string(runtime_dir(models_root).join(".version")).ok().map(|text| text.trim().to_owned())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn unzip_to(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(zip_path).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| format!("архив движка повреждён: {error}"))?;
    let mut total = 0u64;
    for index in 0..archive.len() {
        total += archive.by_index(index).map_err(|error| error.to_string())?.size();
    }
    let mut done = 0u64;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut last_update = Instant::now();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let Some(name) = entry.enclosed_name() else { continue };
        let out = dest.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|error| error.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut target = File::create(&out).map_err(|error| error.to_string())?;
        loop {
            if CANCEL.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            let read = entry.read(&mut buffer).map_err(|error| format!("архив движка повреждён: {error}"))?;
            if read == 0 {
                break;
            }
            target.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
            done += read as u64;
            if last_update.elapsed() >= PROGRESS_INTERVAL {
                progress_bytes(done, total, done, "распаковка движка…");
                last_update = Instant::now();
            }
        }
    }
    Ok(())
}

fn install_runtime(config: &Config) -> Result<(), String> {
    let models_root = config.models_root();
    let exe = runtime_exe(&models_root);
    if exe.is_file() && runtime_marker(&models_root).as_deref() == Some(OLLAMA_VERSION) {
        return Ok(());
    }
    if runtime_exe(&models_root).is_file() && runtime_marker(&models_root).is_none() {
        // Установка могла оборваться на распаковке: битый exe не запускается.
        let _ = fs::remove_file(&exe);
    }
    let runtime_root = models_root.join("runtime");
    fs::create_dir_all(&runtime_root).map_err(|error| error.to_string())?;
    let zip_path = runtime_root.join("ollama.zip");
    // Имя недокачки привязано к версии: после обновления OLLAMA_VERSION Range
    // дописал бы новый хвост к старому началу.
    let part = runtime_root.join(format!("ollama-{OLLAMA_VERSION}.zip.part"));
    let _ = fs::remove_file(runtime_root.join("ollama.zip.part"));
    // Распаковка прервалась, а архив цел: второй раз 1,4 ГБ качать незачем.
    let zip_ready = zip_path.is_file() && sha256_file(&zip_path).is_ok_and(|actual| actual.eq_ignore_ascii_case(OLLAMA_ZIP_SHA256));
    if !zip_ready {
        if hardware::free_space_bytes(&models_root).is_some_and(|free| free < RUNTIME_RESERVE_BYTES) {
            return Err("на диске меньше 4 ГБ: движку нужно место и для архива, и для распаковки".into());
        }
        begin_stage("runtime", "загрузка движка Ollama…");
        let direct = DownloadSource::direct(OLLAMA_ZIP_URL.replace("{version}", OLLAMA_VERSION));
        let via_api = DownloadSource { url: OLLAMA_ASSET_API_URL.into(), accept: Some("application/octet-stream") };
        download_from(&[direct, via_api], &part, OLLAMA_ZIP_SIZE, "загрузка движка", &DOWNLOAD_RETRY)?;
        begin_stage("runtime", "проверка контрольной суммы движка…");
        let actual = sha256_file(&part)?;
        if !actual.eq_ignore_ascii_case(OLLAMA_ZIP_SHA256) {
            let _ = fs::remove_file(&part);
            return Err("контрольная сумма архива движка не совпала: скачивание повреждено".into());
        }
        fs::rename(&part, &zip_path).map_err(|error| error.to_string())?;
    }
    begin_stage("unpack", "распаковка движка…");
    let dest = runtime_dir(&models_root);
    // Работающий serve держит ollama.exe и DLL: удаление прошло бы наполовину
    // (вместе с .version), а запись exe упала бы с access denied.
    if !stop_if(|state| state.in_flight == 0) {
        return Err("движок занят задачей: обновите его после её окончания".into());
    }
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|error| format!("не удалось заменить старый движок: {error}"))?;
    }
    fs::create_dir_all(&dest).map_err(|error| error.to_string())?;
    unzip_to(&zip_path, &dest)?;
    let _ = fs::remove_file(&zip_path);
    if !exe.is_file() {
        return Err("в архиве движка не нашлось ollama.exe".into());
    }
    fs::write(runtime_dir(&models_root).join(".version"), OLLAMA_VERSION).map_err(|error| error.to_string())?;
    Ok(())
}

// -- запуск и остановка ----------------------------------------------------------------

fn spawn_server(config: &Config, port: u16) -> Result<Child, String> {
    let models_root = config.models_root();
    let exe = runtime_exe(&models_root);
    if !exe.is_file() {
        return Err("локальный движок не установлен".into());
    }
    let store = store_dir(&models_root);
    fs::create_dir_all(&store).map_err(|error| error.to_string())?;
    let home = config.root().join("data").join("ollama");
    fs::create_dir_all(&home).map_err(|error| error.to_string())?;
    let log_path = config.root().join("data").join("ollama-serve.log");
    let log = OpenOptions::new().create(true).append(true).open(&log_path).ok();
    let mut command = crate::proc::quiet(&exe.to_string_lossy());
    command
        .arg("serve")
        .env("OLLAMA_HOST", format!("127.0.0.1:{port}"))
        .env("OLLAMA_MODELS", &store)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        // Без этого serve на каждом старте удаляет недокачанные слои.
        .env("OLLAMA_NOPRUNE", "1")
        // 8 ГБ VRAM: две модели разом не помещаются, параллельные запросы
        // умножают KV-кэш.
        .env("OLLAMA_MAX_LOADED_MODELS", "1")
        .env("OLLAMA_NUM_PARALLEL", "1")
        .env("OLLAMA_KEEP_ALIVE", "5m")
        .stdin(Stdio::null());
    match log {
        Some(file) => {
            let err = file.try_clone().ok();
            command.stdout(Stdio::from(file));
            match err {
                Some(err) => {
                    command.stderr(Stdio::from(err));
                }
                None => {
                    command.stderr(Stdio::null());
                }
            }
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    if matches!(hardware::detect().vendor, Vendor::Amd | Vendor::Intel) {
        command.env("OLLAMA_VULKAN", "1");
    }
    command.spawn().map_err(|error| format!("не удалось запустить локальный движок: {error}"))
}

fn wait_ready(port: u16, child: &mut Child) -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if tags_at(port).is_ok() {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("локальный движок завершился с кодом {status}: смотрите data/ollama-serve.log"));
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err("локальный движок не ответил за 90 секунд: смотрите data/ollama-serve.log".into())
}

fn kill_child(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = crate::proc::quiet("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Погасить дерево процессов по pid: движок мог быть принят у прошлого запуска,
/// и `Child` для него уже нет.
fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        let _ = crate::proc::quiet("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
    }
}

fn lock_runtime() -> std::sync::MutexGuard<'static, RuntimeState> {
    RUNTIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Сериализует старт движка внутри процесса. Отдельно от `RUNTIME`, чтобы скан
/// портов, ожидание аренды и `wait_ready` (до минуты с лишним) не вешали
/// `status()`, «Остановить» и watchdog.
static STARTING: Mutex<()> = Mutex::new(());

/// Поднять движок (или переиспользовать свой) и вернуть порт. Второй параметр —
/// запустили ли его мы: сессия и остановка касаются только своих процессов.
fn ensure_server(config: &Config, trigger: &str) -> Result<(u16, bool), String> {
    let _starting = STARTING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let models_root = config.models_root();
    {
        let mut state = lock_runtime();
        if let Some(child) = state.child.as_mut() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                state.last_activity = now_ts();
                return Ok((state.port, false));
            }
            // Свой serve упал (OOM, сброс драйвера): иначе все задачи получали бы
            // мёртвый порт и ложное «модель не установлена».
            let (session, db_path) = (state.session_id.take(), state.db_path.take());
            state.child = None;
            state.pid = None;
            state.port = 0;
            drop(state);
            if let (Some(id), Some(path)) = (session, db_path.as_ref()) {
                if let Ok(db) = Database::open(path) {
                    let _ = db.end_local_session(id, now_ts());
                }
            }
            forget_serve(db_path.as_deref());
        }
    }
    // Движок, переживший прошлый запуск приложения: принимаем по записанному
    // pid, чтобы его можно было погасить и чтобы VRAM не держалась зря.
    if let Some((pid, port)) = stored_serve(config) {
        if tags_at(port).is_ok() {
            let mut state = lock_runtime();
            state.port = port;
            state.pid = Some(pid);
            state.db_path = Some(db_path(config));
            state.last_activity = now_ts();
            return Ok((port, false));
        }
    }
    let preferred = configured_port(config);
    let mut last_error = String::new();
    for port in preferred..preferred.saturating_add(PORT_SCAN) {
        match tags_at(port) {
            Ok(names) => {
                // Чужой Ollama годится, только если в нём уже лежат наши теги:
                // иначе скачанное в наш склад он не увидит.
                if names.iter().any(|name| name.starts_with("brepsi/")) {
                    let pid = stored_serve(config).filter(|(_, stored_port)| *stored_port == port).map(|(pid, _)| pid);
                    let mut state = lock_runtime();
                    state.port = port;
                    state.pid = pid;
                    state.db_path = Some(db_path(config));
                    state.last_activity = now_ts();
                    return Ok((port, false));
                }
                last_error = "порт занят чужим Ollama без моделей Brepsi Brain".into();
            }
            Err(_) => {
                // Свободный порт: поднимаем свой движок.
                if !runtime_installed(&models_root) {
                    return Err("локальный движок не установлен: откройте «Модели и железо»".into());
                }
                let lease_owner = format!("{}-{}", std::process::id(), port);
                let db = open_db(config);
                let claim = || {
                    db.as_ref()
                        .is_none_or(|db| db.claim_lease(LEASE_KEY, &lease_owner, LEASE_TTL_SECONDS, None).unwrap_or(true))
                };
                if !claim() {
                    // Другой процесс (sync) уже поднимает движок на этом порту. Ждём
                    // дольше его `wait_ready` и не уходим на следующий порт: иначе
                    // после снятия его аренды поднялся бы второй serve.
                    let deadline = Instant::now() + READY_TIMEOUT + Duration::from_secs(30);
                    loop {
                        thread::sleep(Duration::from_millis(500));
                        if tags_at(port).is_ok() {
                            let mut state = lock_runtime();
                            state.port = port;
                            state.last_activity = now_ts();
                            return Ok((port, false));
                        }
                        // Владелец аренды сдался: порт по-прежнему пуст, поднимаем сами.
                        if claim() {
                            break;
                        }
                        if Instant::now() >= deadline {
                            return Err("локальный движок поднимает другой процесс и пока не готов: повторите позже".into());
                        }
                    }
                }
                let mut child = match spawn_server(config, port) {
                    Ok(child) => child,
                    Err(error) => {
                        drop_lease(&db, &lease_owner);
                        return Err(error);
                    }
                };
                if let Err(error) = wait_ready(port, &mut child) {
                    kill_child(&mut child);
                    drop_lease(&db, &lease_owner);
                    return Err(error);
                }
                // Пока ждали готовности, порт мог занять другой процесс, а наш
                // serve — выйти: тогда движок чужой, но рабочий.
                if child.try_wait().ok().flatten().is_some() {
                    let mut state = lock_runtime();
                    state.port = port;
                    state.last_activity = now_ts();
                    drop(state);
                    drop_lease(&db, &lease_owner);
                    return Ok((port, false));
                }
                let session_id = db
                    .as_ref()
                    .and_then(|db| db.start_local_session(boot_id(), "", trigger, child.id(), now_ts()).ok());
                let pid = child.id();
                {
                    let mut state = lock_runtime();
                    state.child = Some(child);
                    state.pid = Some(pid);
                    state.port = port;
                    state.session_id = session_id;
                    state.db_path = db.as_ref().map(|db| db.path().to_path_buf());
                    state.last_activity = now_ts();
                }
                if port != preferred {
                    persist_local_route(config, port);
                }
                remember_serve(config, pid, port);
                drop_lease(&db, &lease_owner);
                return Ok((port, true));
            }
        }
    }
    Err(format!("не удалось поднять локальный движок: {last_error}"))
}

/// Аренда «движок поднимаю я» живёт до TTL, поэтому снимать её надо и на отказе:
/// иначе другой процесс ждёт порт, которого уже нет.
fn drop_lease(db: &Option<Database>, owner: &str) {
    if let Some(db) = db {
        let _ = db.release_lease(LEASE_KEY, owner);
    }
}

fn persist_local_route(config: &Config, port: u16) {
    let path = crate::settings_file::path_for(&config.root());
    let current = config.local.model.clone().unwrap_or_default();
    let _ = crate::settings_file::set_local_route(&path, &local_base_url(port), &current);
}

/// Остановить движок и закрыть сессию. Безопасно вызывать из любого потока.
pub fn stop() -> Result<(), String> {
    stop_if(|_| true);
    Ok(())
}

/// Условие проверяется под тем же захватом, где забирается движок: иначе между
/// проверкой watchdog'а и остановкой `job_guard` успевал взять запрос.
fn stop_if(should_stop: impl FnOnce(&RuntimeState) -> bool) -> bool {
    let (child, pid, session, db_path) = {
        let mut state = lock_runtime();
        if !should_stop(&state) {
            return false;
        }
        let values = (state.child.take(), state.pid.take(), state.session_id.take(), state.db_path.take());
        state.port = 0;
        values
    };
    if let Some(mut child) = child {
        kill_child(&mut child);
    } else if let Some(pid) = pid {
        kill_pid(pid);
    }
    if let (Some(id), Some(path)) = (session, db_path.as_ref()) {
        if let Ok(db) = Database::open(path) {
            let _ = db.end_local_session(id, now_ts());
        }
    }
    forget_serve(db_path.as_deref());
    true
}

/// Работает ли движок, который мы запустили или приняли.
pub fn status_running() -> bool {
    let state = RUNTIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.child.is_some() || state.pid.is_some()
}

/// Поднять движок без задачи (кнопка «Запустить»).
pub fn start(config: &Config) -> Result<u16, String> {
    ensure_server(config, "manual").map(|(port, _)| port)
}

/// Удаление гасит движок: посреди пересказа, установки или замера это оборвало бы
/// запрос, поэтому сначала проверяем занятость.
fn refuse_if_busy() -> Result<(), String> {
    let job_active = JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().is_some_and(|progress| progress.active);
    if job_active || lock_runtime().in_flight > 0 {
        return Err("локальная модель сейчас занята: дождитесь окончания задачи".into());
    }
    Ok(())
}

/// Удалить портативный движок; модели остаются в хранилище.
pub fn remove_runtime(config: &Config) -> Result<(), String> {
    refuse_if_busy()?;
    stop()?;
    let dir = runtime_dir(&config.models_root());
    fs::remove_dir_all(&dir).map_err(|error| format!("не удалось удалить движок: {error}"))
}

/// Закрыть сессии прошлых запусков: их `ended_ts` остался пустым при аварии.
pub fn sweep_stale(config: &Config) {
    if let Some(db) = open_db(config) {
        let _ = db.sweep_local_sessions(boot_id());
    }
}

/// Watchdog: гасит движок после простоя в режиме `on_demand`.
pub fn start_watchdog(root: PathBuf) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(5));
        let config = crate::config::load_config_with_key(
            Some(&crate::settings_file::path_for(&root)),
            crate::config::api_key_from_env(),
        )
        .unwrap_or_default();
        if config.local_ai.mode == LocalMode::Always {
            continue;
        }
        let idle = config.local_ai.idle_timeout_seconds as i64;
        // Активная установка/бенч держит движок даже без запросов к модели.
        let job_active = JOB
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|progress| progress.active);
        if job_active {
            continue;
        }
        stop_if(|state| {
            // Движок, принятый у прошлого запуска, живёт без `child`: иначе он
            // держал бы видеопамять до ручного «Остановить».
            let running = state.child.is_some() || state.pid.is_some();
            running && state.in_flight == 0 && now_ts() - state.last_activity >= idle
        });
    });
}

// -- область запроса -------------------------------------------------------------------

pub struct JobGuard(#[allow(dead_code)] BusyGuard);

/// Занятость без запроса к модели: установка, удаление и замер тоже не должны
/// дать watchdog'у погасить движок посреди скачивания или импорта.
struct BusyGuard;

impl BusyGuard {
    fn acquire() -> Self {
        let mut state = RUNTIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.in_flight += 1;
        state.last_activity = now_ts();
        BusyGuard
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        let mut state = RUNTIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.in_flight = state.in_flight.saturating_sub(1);
        state.last_activity = now_ts();
    }
}

/// Поднять движок под задачу и отметить занятость. `None` — маршрут не локальный.
pub fn job_guard(config: &Config, task: Task) -> Result<Option<JobGuard>, String> {
    // Управляем только маршрутом `[llm.local]`: чужой loopback-сервер в `[llm]`
    // (свой Ollama/LM Studio) приложение не запускает и не гасит.
    let target = config.local_route(task);
    let Some(route) = target else { return Ok(None) };
    // Занятость до старта: между `ensure_server` и отметкой watchdog мог погасить
    // только что поднятый движок.
    let busy = BusyGuard::acquire();
    let (port, _) = ensure_server(config, task.as_str())?;
    if !tag_installed(port, &route.model) {
        return Err(format!("модель {} не установлена: откройте «Модели и железо»", route.model));
    }
    note_model(config, &route.model);
    Ok(Some(JobGuard(busy)))
}

// -- статус ----------------------------------------------------------------------------

fn installed_ids(config: &Config) -> Vec<String> {
    let mut ids = Vec::new();
    let store = store_dir(&config.models_root());
    // Ollama 0.40 пишет `manifests-v2/ollama.com/<ns>/<model>/<tag>`, старые
    // сборки — `manifests/registry.ollama.ai/<ns>/...`. Читаем оба.
    for manifests in [
        store.join("manifests-v2").join("ollama.com").join("brepsi"),
        store.join("manifests").join("registry.ollama.ai").join("brepsi"),
    ] {
        if let Ok(entries) = fs::read_dir(manifests) {
            for entry in entries.flatten() {
                if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    if let Some(name) = entry.file_name().to_str() {
                        ids.push(name.to_owned());
                    }
                }
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Теги установленных моделей: только их умеет запускать `[llm.local]`.
pub fn installed_tags(config: &Config) -> Vec<String> {
    installed_ids(config).iter().map(|id| catalog::tag(id)).collect()
}

/// Адрес своего движка, пока он ни разу не запускался на другом порту.
pub fn default_base_url() -> String {
    local_base_url(DEFAULT_PORT)
}

fn progress_value() -> Option<Value> {
    let progress = JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()?;
    Some(json!({
        "kind": progress.kind,
        "id": progress.id,
        "phase": progress.phase,
        "status": progress.status,
        "done": progress.done,
        "total": progress.total,
        "speed_bps": progress.speed_bps,
        "eta_seconds": progress.eta_seconds,
        "active": progress.active,
        "error": progress.error,
    }))
}

/// Рекомендуемое окно контекста под это железо: KV-кэш растёт вместе с окном,
/// поэтому минимальному и низкому тиру хватает 8k, среднему — 16k, а 32k —
/// только с 24 ГБ VRAM: на 16 ГБ оно вытесняет 14B-модель из видеопамяти.
pub fn recommended_context(hardware: &hardware::Hardware) -> u32 {
    match hardware.tier {
        Tier::Minimal | Tier::Low => 8_192,
        Tier::Mid => 16_384,
        Tier::High if hardware.vram_mb.unwrap_or(0) >= 24_000 => 32_768,
        Tier::High => 16_384,
    }
}

/// Документ для `/api/local/status` и страницы «Модели и железо».
pub fn status(config: &Config) -> Value {
    let hardware = hardware::detect();
    let models_root = config.models_root();
    let installed = installed_ids(config);
    let (child_running, pid, port, in_flight, idle_seconds) = {
        let state = RUNTIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            state.child.is_some(),
            state.pid,
            state.port,
            state.in_flight,
            if state.child.is_some() || state.pid.is_some() { now_ts() - state.last_activity } else { 0 },
        )
    };
    // Принятый движок живёт без `Child`: признак работы — ответ на порту.
    let running = child_running || (pid.is_some() && port != 0 && tags_at(port).is_ok());
    let recommended = catalog::recommended_for(&hardware);
    let recommended_id = recommended.id;
    let recommended_installed = installed.iter().any(|id| id == recommended_id);
    let catalog: Vec<Value> = catalog::models()
        .iter()
        .map(|model| {
            let is_installed = installed.contains(&model.id.to_owned());
            json!({
                "id": model.id,
                "name": model.name,
                "params": model.params,
                "quant": model.quant,
                "size_bytes": model.size_bytes,
                "min_vram_mb": model.min_vram_mb,
                "min_tier": model.min_tier.as_str(),
                "fits": catalog::fits(model, &hardware),
                "roles": model.roles.iter().map(|task| task.as_str()).collect::<Vec<_>>(),
                "languages": model.languages,
                "summary_ru": model.summary_ru,
                "summary_en": model.summary_en,
                "source": match model.source { Source::Ollama(_) => "ollama", Source::Hf { .. } => "huggingface" },
                "installed": is_installed,
                "recommended": model.id == recommended_id,
            })
        })
        .collect();
    let route = config.route_for(Task::Distill, false);
    let tasks: Value = Task::ALL
        .into_iter()
        .map(|task| (task.as_str().to_owned(), Value::String(config.tasks.get(task).unwrap_or("").to_owned())))
        .collect::<serde_json::Map<String, Value>>()
        .into();
    json!({
        "hardware": {
            "vendor": match hardware.vendor {
                Vendor::Nvidia => "nvidia", Vendor::Amd => "amd", Vendor::Intel => "intel", Vendor::Unknown => "unknown",
            },
            "name": hardware.name,
            "vram_mb": hardware.vram_mb,
            "ram_mb": hardware.ram_mb,
            "backend": match hardware.backend {
                Backend::Cuda => "cuda", Backend::Vulkan => "vulkan", Backend::Cpu => "cpu",
            },
            "tier": hardware.tier.as_str(),
            "notes": hardware.notes,
        },
        "runtime": {
            "installed": runtime_installed(&models_root),
            "version": runtime_marker(&models_root),
            "running": running,
            "port": port,
            "in_flight": in_flight,
            "idle_seconds": idle_seconds,
            "path": runtime_dir(&models_root),
            "log": config.root().join("data").join("ollama-serve.log"),
        },
        "models_root": models_root,
        "free_bytes": hardware::free_space_bytes(&models_root),
        "installed": installed,
        "catalog": catalog,
        // Что поставить и как настроить движок под это железо: страница
        // «Модели и железо» показывает это кнопкой, сервер применяет.
        "recommended": {
            "id": recommended_id,
            "context_tokens": recommended_context(&hardware),
            "mode": "on_demand",
            "idle_timeout_seconds": 120,
            "installed": recommended_installed,
        },
        "settings": {
            "mode": config.local_ai.mode.as_str(),
            "idle_timeout_seconds": config.local_ai.idle_timeout_seconds,
            "context_tokens": config.local_ai.context_tokens,
            "offline_only": config.local_ai.offline_only,
        },
        "tasks": tasks,
        "route": {
            "kind": match &route { Some(route) if route.local => "local", Some(_) => "remote", None => "none" },
            "base_url": route.as_ref().map(|route| route.base_url.clone()).unwrap_or_default(),
            "model": route.as_ref().map(|route| route.model.clone()).unwrap_or_default(),
            "local_model": config.local.model,
        },
        "install": progress_value(),
    })
}

// -- скачивание ------------------------------------------------------------------------

fn set_progress(update: impl FnOnce(&mut InstallProgress)) {
    let mut slot = JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(progress) = slot.as_mut() {
        update(progress);
    }
}

/// Новый этап установки: прежние байты и скорость к нему не относятся.
fn begin_stage(phase: &str, status: &str) {
    set_progress(|progress| {
        progress.phase = phase.to_owned();
        progress.status = status.to_owned();
        progress.done = 0;
        progress.total = 0;
        progress.speed_bps = 0;
        progress.eta_seconds = None;
        progress.meter = RateMeter::default();
    });
}

/// `moved` — сколько байт передано в этой попытке: по нему считается скорость,
/// а `done` включает и то, что уже лежало на диске.
fn progress_bytes(done: u64, total: u64, moved: u64, status: &str) {
    set_progress(|progress| {
        let bps = progress.meter.sample(Instant::now(), moved);
        progress.done = done;
        progress.total = total;
        progress.status = status.to_owned();
        progress.speed_bps = bps as u64;
        progress.eta_seconds = (bps >= 1.0 && total > done).then(|| ((total - done) as f64 / bps) as u64);
    });
}

const CANCELLED: &str = "отменено";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// Как часто поток задания просыпается проверить отмену и зависание.
const FETCH_TICK: Duration = Duration::from_millis(200);
/// Предел жизни соединения: зависшее чтение не должно держать поток вечно.
const FETCH_LIFETIME: Duration = Duration::from_secs(3 * 3600);
const FETCH_QUEUE_CHUNKS: usize = 16;
const FETCH_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
struct DownloadSource {
    url: String,
    accept: Option<&'static str>,
}

impl DownloadSource {
    fn direct(url: String) -> Self {
        DownloadSource { url, accept: None }
    }

    fn host(&self) -> &str {
        let rest = self.url.split_once("://").map_or(self.url.as_str(), |(_, rest)| rest);
        rest.split('/').next().unwrap_or(rest)
    }
}

struct RetryPolicy {
    /// Паузы между кругами по источникам; их число — сколько кругов подряд
    /// без единого нового байта терпим.
    delays: &'static [Duration],
    /// Сколько ждать хоть каких-то данных от источника.
    stall: Duration,
}

const DOWNLOAD_RETRY: RetryPolicy = RetryPolicy {
    delays: &[
        Duration::from_secs(2),
        Duration::from_secs(5),
        Duration::from_secs(10),
        Duration::from_secs(30),
        Duration::from_secs(60),
        // Сбой GitHub длится минутами: установку запускают и уходят.
        Duration::from_secs(120),
        Duration::from_secs(300),
        Duration::from_secs(300),
    ],
    stall: Duration::from_secs(60),
};

enum FetchError {
    Cancelled,
    /// Диск, а не сеть: другой источник не поможет.
    Fatal(String),
    /// 4xx, кроме 408/429: этот источник файл не отдаст (gated-репо, переименован),
    /// повторять его 14 минут бессмысленно.
    Gone(String),
    Retry(String),
}

enum Fetched {
    Head { status: u16, length: Option<u64> },
    Chunk(Vec<u8>),
    End,
    Failed(String),
}

/// Поток-читатель: блокирующие connect и read из ureq не прервать, поэтому
/// они живут отдельно, а поток задания может бросить их при отмене или зависании.
/// Ограниченный sync_channel — backpressure, если диск медленнее сети. Брошенный
/// поток выходит на следующем send или по FETCH_LIFETIME.
fn spawn_fetch(source: DownloadSource, offset: u64) -> mpsc::Receiver<Fetched> {
    let (sender, receiver) = mpsc::sync_channel(FETCH_QUEUE_CHUNKS);
    thread::spawn(move || {
        let mut request = agent(FETCH_LIFETIME).get(&source.url);
        if offset > 0 {
            request = request.header("Range", &format!("bytes={offset}-"));
        }
        if let Some(accept) = source.accept {
            request = request.header("Accept", accept);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(error) => {
                let _ = sender.send(Fetched::Failed(error.to_string()));
                return;
            }
        };
        let status = response.status().as_u16();
        let length = response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if sender.send(Fetched::Head { status, length }).is_err() || !(200..300).contains(&status) {
            return;
        }
        let mut reader = response.into_body().into_reader();
        loop {
            let mut chunk = vec![0u8; FETCH_CHUNK_BYTES];
            match reader.read(&mut chunk) {
                Ok(0) => {
                    let _ = sender.send(Fetched::End);
                    return;
                }
                Ok(read) => {
                    chunk.truncate(read);
                    if sender.send(Fetched::Chunk(chunk)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Fetched::Failed(error.to_string()));
                    return;
                }
            }
        }
    });
    receiver
}

/// Одна попытка у одного источника. Всё принятое остаётся в `part`: следующая
/// попытка, в том числе у другого источника, продолжит с этого места.
fn fetch_once(source: &DownloadSource, part: &Path, total_hint: u64, label: &str, stall: Duration) -> Result<u64, FetchError> {
    let offset = fs::metadata(part).map(|meta| meta.len()).unwrap_or(0);
    if total_hint > 0 && offset == total_hint {
        return Ok(offset);
    }
    let receiver = spawn_fetch(source.clone(), offset);
    let status_text = format!("{label}…");
    let mut file: Option<File> = None;
    let (mut start, mut done, mut total) = (offset, offset, total_hint);
    let mut last_data = Instant::now();
    let mut last_update = Instant::now();
    loop {
        if CANCEL.load(Ordering::Relaxed) {
            return Err(FetchError::Cancelled);
        }
        match receiver.recv_timeout(FETCH_TICK) {
            Ok(Fetched::Head { status, length }) => {
                if status == 416 {
                    return Ok(offset);
                }
                if (400..500).contains(&status) && status != 408 && status != 429 {
                    return Err(FetchError::Gone(format!("HTTP {status}")));
                }
                if !(200..300).contains(&status) {
                    return Err(FetchError::Retry(format!("HTTP {status}")));
                }
                // 200 вместо 206: сервер игнорирует Range, файл начинается заново.
                let resume = status == 206 && offset > 0;
                start = if resume { offset } else { 0 };
                done = start;
                total = length.map(|length| length + start).filter(|total| *total > 0).unwrap_or(total_hint);
                let opened = OpenOptions::new().create(true).write(true).append(resume).truncate(!resume).open(part);
                file = Some(opened.map_err(|error| FetchError::Fatal(error.to_string()))?);
                progress_bytes(done, total, 0, &status_text);
                last_data = Instant::now();
            }
            Ok(Fetched::Chunk(chunk)) => {
                let Some(target) = file.as_mut() else {
                    return Err(FetchError::Retry("данные раньше заголовков".into()));
                };
                target.write_all(&chunk).map_err(|error| FetchError::Fatal(error.to_string()))?;
                done += chunk.len() as u64;
                last_data = Instant::now();
                if last_update.elapsed() >= PROGRESS_INTERVAL {
                    progress_bytes(done, total, done - start, &status_text);
                    last_update = Instant::now();
                }
            }
            Ok(Fetched::End) => {
                if total > 0 && done < total {
                    return Err(FetchError::Retry("соединение оборвалось".into()));
                }
                if let Some(target) = file.as_mut() {
                    target.flush().map_err(|error| FetchError::Fatal(error.to_string()))?;
                }
                progress_bytes(done, total, done - start, &format!("{label}: готово"));
                return Ok(done);
            }
            Ok(Fetched::Failed(error)) => return Err(FetchError::Retry(error)),
            Err(RecvTimeoutError::Timeout) => {
                if last_data.elapsed() >= stall {
                    return Err(FetchError::Retry(format!("нет данных {} с", stall.as_secs())));
                }
                if file.is_some() {
                    // Без данных скорость должна падать, а не застывать на старой.
                    progress_bytes(done, total, done - start, &status_text);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err(FetchError::Retry("соединение закрыто".into())),
        }
    }
}

/// Пауза перед повтором с обратным отсчётом; отмена прерывает её сразу.
fn wait_before_retry(delay: Duration, reason: &str) -> Result<(), String> {
    let deadline = Instant::now() + delay;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if CANCEL.load(Ordering::Relaxed) {
            return Err(CANCELLED.into());
        }
        set_progress(|progress| {
            progress.speed_bps = 0;
            progress.eta_seconds = None;
            progress.status = format!("{reason}. Повтор через {} с", left.as_secs() + 1);
        });
        thread::sleep(FETCH_TICK.min(left));
    }
    Ok(())
}

/// Скачать файл с докачкой, перебирая источники по кругу. `part` — и огрызок,
/// и готовый файл до `rename`. Счётчик неудач обнуляется, пока прибывают новые
/// байты: медленная, но живая сеть докачает любой размер. Исполняется в потоке
/// фонового задания.
fn download_from(sources: &[DownloadSource], part: &Path, total_hint: u64, label: &str, policy: &RetryPolicy) -> Result<u64, String> {
    if let Some(parent) = part.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut failed_rounds = 0;
    let mut gone = vec![None; sources.len()];
    loop {
        let before = fs::metadata(part).map(|meta| meta.len()).unwrap_or(0);
        let mut failures = Vec::new();
        for (index, source) in sources.iter().enumerate() {
            if gone[index].is_some() {
                continue;
            }
            match fetch_once(source, part, total_hint, label, policy.stall) {
                Ok(size) => return Ok(size),
                Err(FetchError::Cancelled) => return Err(CANCELLED.into()),
                Err(FetchError::Fatal(error)) => return Err(format!("{label}: {error}")),
                Err(FetchError::Gone(error)) => gone[index] = Some(format!("{}: {error}", source.host())),
                Err(FetchError::Retry(error)) => failures.push(format!("{}: {error}", source.host())),
            }
        }
        if gone.iter().all(Option::is_some) {
            let summary = gone.into_iter().flatten().collect::<Vec<_>>().join("; ");
            return Err(format!("{label}: источники отказали ({summary})"));
        }
        let summary = failures.join("; ");
        if fs::metadata(part).map(|meta| meta.len()).unwrap_or(0) > before {
            failed_rounds = 0;
        }
        let Some(delay) = policy.delays.get(failed_rounds) else {
            return Err(format!("{label}: источники недоступны ({summary})"));
        };
        failed_rounds += 1;
        wait_before_retry(*delay, &format!("{label}: {summary}"))?;
    }
}

/// Скачать GGUF из Hugging Face и проверить SHA-256.
fn download_hf(config: &Config, model: &catalog::Model, repo: &str, file: &str, sha256: &str, size: u64) -> Result<PathBuf, String> {
    let endpoint = std::env::var("HF_ENDPOINT").ok().filter(|value| !value.is_empty()).unwrap_or_else(|| "https://huggingface.co".into());
    let url = format!("{}/{}/resolve/main/{}?download=true", endpoint.trim_end_matches('/'), repo, file);
    let dir = gguf_dir(&config.models_root());
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join(format!("{}.gguf", model.id));
    if path.is_file() && sha256_file(&path).is_ok_and(|actual| actual.eq_ignore_ascii_case(sha256)) {
        return Ok(path);
    }
    // Префикс хэша в имени: при смене файла в каталоге старая недокачка не продолжается.
    let part = dir.join(format!("{}.{}.gguf.part", model.id, sha256.get(..12).unwrap_or(sha256)));
    let _ = fs::remove_file(dir.join(format!("{}.gguf.part", model.id)));
    // Импорт копирует GGUF в хранилище Ollama: нужно вдвое больше места.
    if hardware::free_space_bytes(&dir).is_some_and(|free| free < size.saturating_mul(2).saturating_add(512 * 1024 * 1024)) {
        return Err("на диске мало места: для импорта GGUF нужно вдвое больше размера модели".into());
    }
    begin_stage("download", &format!("скачивание {}…", model.name));
    download_from(&[DownloadSource::direct(url)], &part, size, &format!("скачивание {}", model.name), &DOWNLOAD_RETRY)?;
    begin_stage("verify", "проверка контрольной суммы…");
    let actual = sha256_file(&part)?;
    if !actual.eq_ignore_ascii_case(sha256) {
        let _ = fs::remove_file(&part);
        return Err("контрольная сумма GGUF не совпала: скачивание повреждено".into());
    }
    fs::rename(&part, &path).map_err(|error| error.to_string())?;
    Ok(path)
}

// -- установка модели ------------------------------------------------------------------

/// Сумма по слоям: `/api/pull` шлёт `completed/total` каждого слоя отдельно,
/// и без суммы индикатор прыгал бы между ними. Базовая линия слоя отделяет уже
/// лежащее на диске от переданного сейчас.
#[derive(Default)]
struct PullProgress {
    layers: HashMap<String, (u64, u64, u64)>,
}

impl PullProgress {
    fn update(&mut self, digest: &str, completed: u64, total: u64) {
        let layer = self.layers.entry(digest.to_owned()).or_insert((completed, completed, total));
        layer.1 = completed.max(layer.1);
        layer.2 = total;
    }

    /// (получено, всего, передано за эту попытку)
    fn totals(&self) -> (u64, u64, u64) {
        self.layers.values().fold((0, 0, 0), |(done, total, moved), (base, completed, size)| {
            (done + completed, total + size, moved + (completed - base))
        })
    }
}

/// Тишина дольше этого — зависший слой. С запасом: проверка SHA-256 большого
/// blob идёт без строк прогресса несколько минут.
const PULL_STALL: Duration = Duration::from_secs(10 * 60);

enum PullError {
    Cancelled,
    /// Реестр ответил отказом (нет такой модели, 4xx): повтор не поможет.
    Fatal(String),
    Retry(String),
}

/// `received` — сколько байт слоёв уже на месте: по нему повтор понимает, что
/// загрузка движется, и не тратит попытки на медленную, но живую сеть.
fn pull(port: u16, tag: &str, label: &str, received: &mut u64) -> Result<(), PullError> {
    let response = agent(FETCH_LIFETIME)
        .post(&api_url(port, "/api/pull"))
        .header("Content-Type", "application/json")
        .send(json!({"model": tag, "stream": true}).to_string())
        .map_err(|error| PullError::Retry(format!("реестр недоступен: {error}")))?;
    let status = response.status().as_u16();
    let mut response = response;
    if !(200..300).contains(&status) {
        let raw = response.body_mut().read_to_string().unwrap_or_default();
        let message = format!("реестр ответил HTTP {status}: {raw}");
        return Err(if (400..500).contains(&status) && status != 408 && status != 429 {
            PullError::Fatal(message)
        } else {
            PullError::Retry(message)
        });
    }
    // Строки читает отдельный поток: блокирующий `lines()` не давал проверить
    // отмену и зависание, пока Ollama молчит (до FETCH_LIFETIME = 3 ч). Поток
    // завершится сам, когда Ollama закроет поток ответа.
    let (sender, receiver) = mpsc::sync_channel::<std::io::Result<String>>(64);
    let reader = BufReader::new(response.into_body().into_reader());
    thread::spawn(move || {
        for line in reader.lines() {
            let failed = line.is_err();
            if sender.send(line).is_err() || failed {
                return;
            }
        }
    });
    let mut layers = PullProgress::default();
    let mut last_line = Instant::now();
    loop {
        if CANCEL.load(Ordering::Relaxed) {
            return Err(PullError::Cancelled);
        }
        let line = match receiver.recv_timeout(FETCH_TICK) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                if last_line.elapsed() >= PULL_STALL {
                    return Err(PullError::Retry(format!("нет данных {} мин", PULL_STALL.as_secs() / 60)));
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        last_line = Instant::now();
        // Обрыв посреди потока — это не конец загрузки: иначе модель считалась
        // бы скачанной после закрытия соединения на середине.
        let line = line.map_err(|error| PullError::Retry(format!("скачивание прервано: {error}")))?;
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            // «file does not exist», «pull model manifest: 404» — модели нет в реестре.
            let lower = error.to_ascii_lowercase();
            let message = format!("реестр: {error}");
            return Err(if lower.contains("does not exist") || lower.contains("not found") || lower.contains("404") {
                PullError::Fatal(message)
            } else {
                PullError::Retry(message)
            });
        }
        let status = value.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "success" {
            return Ok(());
        }
        let total = value.get("total").and_then(Value::as_u64).unwrap_or(0);
        if let Some(digest) = value.get("digest").and_then(Value::as_str).filter(|_| total > 0) {
            layers.update(digest, value.get("completed").and_then(Value::as_u64).unwrap_or(0), total);
        }
        let (done, total, moved) = layers.totals();
        *received = (*received).max(done);
        let text = match status {
            "verifying sha256 digest" => "проверка контрольной суммы…".to_owned(),
            "writing manifest" => "запись манифеста…".to_owned(),
            "pulling manifest" => "получение манифеста…".to_owned(),
            _ => format!("{label}…"),
        };
        progress_bytes(done, total, moved, &text);
    }
    Err(PullError::Retry("реестр закрыл поток до завершения загрузки модели".into()))
}

/// Ollama докачивает слои сама, поэтому повтор продолжит, а не начнёт заново.
/// Паузы обнуляются, пока байты прибывают, как в `download_from`.
fn pull_with_retry(port: u16, tag: &str, label: &str, policy: &RetryPolicy) -> Result<(), String> {
    let mut failed_rounds = 0;
    let mut received = 0;
    loop {
        let before = received;
        match pull(port, tag, label, &mut received) {
            Ok(()) => return Ok(()),
            Err(PullError::Cancelled) => return Err(CANCELLED.into()),
            Err(PullError::Fatal(error)) => return Err(error),
            Err(PullError::Retry(error)) => {
                if received > before {
                    failed_rounds = 0;
                }
                let Some(delay) = policy.delays.get(failed_rounds) else { return Err(error) };
                failed_rounds += 1;
                wait_before_retry(*delay, &format!("{label}: {error}"))?;
            }
        }
    }
}

fn create_derived(config: &Config, port: u16, id: &str, from: &str) -> Result<(), String> {
    let models_root = config.models_root();
    let dir = gguf_dir(&models_root);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let modelfile = dir.join(format!("{id}.Modelfile"));
    let body = format!(
        "FROM \"{from}\"\nPARAMETER num_ctx {}\nPARAMETER temperature 0.2\n",
        config.local_ai.context_tokens
    );
    fs::write(&modelfile, body).map_err(|error| error.to_string())?;
    begin_stage("import", "создание модели в Ollama…");
    let mut command = crate::proc::quiet(&runtime_exe(&models_root).to_string_lossy());
    command
        .args(["create", &catalog::tag(id), "-f"])
        .arg(&modelfile)
        .env("OLLAMA_HOST", format!("127.0.0.1:{port}"))
        .env("OLLAMA_MODELS", store_dir(&models_root))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    // `create` печатает прогресс непрерывно: без дренажа ребёнок встаёт на записи
    // в переполненный pipe, и мы ставим ему диагноз «не завершилось за 30 минут».
    let stderr = child.stderr.take().map(drain_pipe);
    let _progress = child.stdout.take().map(drain_pipe);
    let deadline = Instant::now() + Duration::from_secs(1800);
    let outcome: Result<(), String> = loop {
        if CANCEL.load(Ordering::Relaxed) {
            kill_child(&mut child);
            break Err(CANCELLED.into());
        }
        // Ошибка try_wait не выходит через `?`: ребёнок остался бы жив, а Modelfile — на диске.
        let polled = match child.try_wait() {
            Ok(polled) => polled,
            Err(error) => {
                kill_child(&mut child);
                break Err(error.to_string());
            }
        };
        match polled {
            Some(status) if status.success() => break Ok(()),
            Some(status) => {
                let _ = child.wait();
                let output = stderr.as_ref().map_or_else(String::new, drained_text);
                break Err(format!("не удалось создать модель (код {status}): {output}"));
            }
            None if Instant::now() >= deadline => {
                kill_child(&mut child);
                break Err("создание модели не завершилось за 30 минут".into());
            }
            None => thread::sleep(Duration::from_millis(500)),
        }
    };
    let _ = fs::remove_file(&modelfile);
    outcome?;
    if !tag_installed(port, &catalog::tag(id)) {
        return Err("модель создана, но не появилась в списке Ollama".into());
    }
    Ok(())
}

/// Сколько байт вывода чужого процесса держим для диагноза: pipe конечен, а весь
/// прогресс импорта модели не нужен — достаточно последних строк.
const DRAIN_KEEP_BYTES: usize = 8 * 1024;

fn drain_pipe<R: Read + Send + 'static>(mut pipe: R) -> Arc<Mutex<Vec<u8>>> {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let buffer = Arc::clone(&sink);
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let mut sink = buffer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    sink.extend_from_slice(&chunk[..read]);
                    if sink.len() > DRAIN_KEEP_BYTES {
                        let drop = sink.len() - DRAIN_KEEP_BYTES;
                        sink.drain(..drop);
                    }
                }
            }
        }
    });
    sink
}

fn drained_text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = buffer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

fn delete_tag(port: u16, tag: &str) {
    let _ = agent(Duration::from_secs(30))
        .delete(&api_url(port, "/api/delete"))
        .header("Content-Type", "application/json")
        .force_send_body()
        .send(json!({"model": tag}).to_string());
}

fn run_install(config: Config, id: String) {
    let result = install_model(&config, &id);
    set_progress(|progress| {
        progress.active = false;
        progress.speed_bps = 0;
        progress.eta_seconds = None;
        match &result {
            Ok(()) => {
                progress.phase = "done".into();
                progress.status = "готово".into();
            }
            Err(error) => {
                progress.phase = "error".into();
                progress.error = Some(error.clone());
                progress.status = error.clone();
            }
        }
    });
}

fn install_model(config: &Config, id: &str) -> Result<(), String> {
    let model = catalog::find(id).ok_or_else(|| format!("неизвестная модель: {id}"))?;
    install_runtime(config)?;
    begin_stage("runtime", "запуск движка…");
    let (port, _) = ensure_server(config, "install")?;
    let _busy = BusyGuard::acquire();
    let label = format!("скачивание {}", model.name);
    begin_stage("download", &format!("{label}…"));
    match model.source {
        Source::Ollama(tag) => {
            let store = store_dir(&config.models_root());
            fs::create_dir_all(&store).map_err(|error| error.to_string())?;
            if hardware::free_space_bytes(&store).is_some_and(|free| free < model.size_bytes.saturating_add(512 * 1024 * 1024)) {
                return Err("на диске мало места для этой модели".into());
            }
            let existed = tag_installed(port, tag);
            pull_with_retry(port, tag, &label, &DOWNLOAD_RETRY)?;
            create_derived(config, port, id, tag)?;
            if !existed {
                // Базовый тег остаётся в списке пользователя, а слои уже
                // используются производной моделью — убираем только манифест.
                delete_tag(port, tag);
            }
        }
        Source::Hf { repo, file, sha256, size } => {
            let path = download_hf(config, model, repo, file, sha256, size)?;
            create_derived(config, port, id, &path.to_string_lossy())?;
            let _ = fs::remove_file(&path);
        }
    }
    begin_stage("verify", "проверка установки…");
    if !tag_installed(port, &catalog::tag(id)) {
        return Err("после установки модель не найдена в Ollama".into());
    }
    note_model(config, &catalog::tag(id));
    // Первая установка сама подключает локальный маршрут; основной провайдер
    // не трогаем — он мог быть настроен раньше.
    let fresh = crate::config::load_config_with_key(
        Some(&crate::settings_file::path_for(&config.root())),
        crate::config::api_key_from_env(),
    )
    .unwrap_or_default();
    if fresh.local.base_url.is_none() || fresh.local.model.is_none() {
        let _ = crate::settings_file::set_local_route(
            &crate::settings_file::path_for(&config.root()),
            &local_base_url(port),
            &catalog::tag(id),
        );
    }
    Ok(())
}

pub fn start_install(config: &Config, id: &str) -> Result<(), String> {
    if catalog::find(id).is_none() {
        return Err(format!("неизвестная модель: {id}"));
    }
    let mut slot = JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.as_ref().is_some_and(|progress| progress.active) {
        return Err("установка уже идёт".into());
    }
    *slot = Some(InstallProgress {
        kind: "install".into(),
        id: id.to_owned(),
        phase: "runtime".into(),
        status: "подготовка…".into(),
        active: true,
        ..InstallProgress::default()
    });
    CANCEL.store(false, Ordering::Relaxed);
    let config = config.clone();
    let id = id.to_owned();
    thread::spawn(move || run_install(config, id));
    Ok(())
}

pub fn install_runtime_only(config: &Config) -> Result<(), String> {
    let mut slot = JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.as_ref().is_some_and(|progress| progress.active) {
        return Err("задача уже идёт".into());
    }
    *slot = Some(InstallProgress {
        kind: "runtime".into(),
        phase: "runtime".into(),
        status: "подготовка…".into(),
        active: true,
        ..InstallProgress::default()
    });
    CANCEL.store(false, Ordering::Relaxed);
    let config = config.clone();
    thread::spawn(move || {
        let result = install_runtime(&config);
        set_progress(|progress| {
            progress.active = false;
            progress.speed_bps = 0;
            progress.eta_seconds = None;
            match &result {
                Ok(()) => {
                    progress.phase = "done".into();
                    progress.status = "движок установлен".into();
                }
                Err(error) => {
                    progress.phase = "error".into();
                    progress.error = Some(error.clone());
                    progress.status = error.clone();
                }
            }
        });
    });
    Ok(())
}

pub fn remove_model(config: &Config, id: &str) -> Result<(), String> {
    let tag = catalog::tag(id);
    refuse_if_busy()?;
    stop()?;
    let (port, _) = ensure_server(config, "remove")?;
    let _busy = BusyGuard::acquire();
    let response = agent(Duration::from_secs(60))
        .delete(&api_url(port, "/api/delete"))
        .header("Content-Type", "application/json")
        .force_send_body()
        .send(json!({"model": tag}).to_string())
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(format!("Ollama не удалила модель: HTTP {status}"))
    }
}

pub fn cancel() -> bool {
    let active = JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().is_some_and(|progress| progress.active);
    if active {
        CANCEL.store(true, Ordering::Relaxed);
    }
    active
}

// -- бенчмарк ----------------------------------------------------------------------------

fn run_bench(config: Config, id: String) {
    let result = bench_model(&config, &id);
    set_progress(|progress| {
        progress.active = false;
        progress.speed_bps = 0;
        progress.eta_seconds = None;
        match &result {
            Ok(()) => {
                progress.phase = "done".into();
                progress.status = "замер готов".into();
            }
            Err(error) => {
                progress.phase = "error".into();
                progress.error = Some(error.clone());
                progress.status = error.clone();
            }
        }
    });
}

fn bench_model(config: &Config, id: &str) -> Result<(), String> {
    let (port, _) = ensure_server(config, "bench")?;
    let _busy = BusyGuard::acquire();
    let tag = catalog::tag(id);
    if !tag_installed(port, &tag) {
        return Err("модель не установлена".into());
    }
    note_model(config, &tag);
    set_progress(|progress| progress.status = "замер скорости…".into());
    let value = api_json(
        port,
        "/api/generate",
        &json!({
            "model": tag,
            "prompt": "Одним коротким предложением объясни, что такое локальная база знаний.",
            "stream": false,
            "options": {"num_predict": 64, "temperature": 0.2}
        }),
        Duration::from_secs(900),
    )?;
    let tokens = value.get("eval_count").and_then(Value::as_i64).unwrap_or(0);
    let eval_ns = value.get("eval_duration").and_then(Value::as_f64).unwrap_or(0.0);
    let total_ns = value.get("total_duration").and_then(Value::as_f64).unwrap_or(0.0);
    let load_ns = value.get("load_duration").and_then(Value::as_f64).unwrap_or(0.0);
    let tps = if eval_ns > 0.0 { tokens as f64 / (eval_ns / 1e9) } else { 0.0 };
    if let Some(db) = open_db(config) {
        let _ = db.record_bench(&BenchResult {
            model: tag,
            ts: now_ts(),
            tokens,
            load_ms: (load_ns / 1e6) as i64,
            seconds: total_ns / 1e9,
            tps,
        });
    }
    Ok(())
}

pub fn start_bench(config: &Config, id: &str) -> Result<(), String> {
    if catalog::find(id).is_none() {
        return Err(format!("неизвестная модель: {id}"));
    }
    let mut slot = JOB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.as_ref().is_some_and(|progress| progress.active) {
        return Err("задача уже идёт".into());
    }
    *slot = Some(InstallProgress {
        kind: "bench".into(),
        id: id.to_owned(),
        phase: "bench".into(),
        status: "подготовка…".into(),
        active: true,
        ..InstallProgress::default()
    });
    CANCEL.store(false, Ordering::Relaxed);
    let config = config.clone();
    let id = id.to_owned();
    thread::spawn(move || run_bench(config, id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    #[test]
    fn a_reused_pid_is_not_accepted_as_our_runtime() {
        // Windows переиспользует pid: записанный прошлой сессией номер годится,
        // только пока процесс по нему — наш портативный движок.
        let models_root = Path::new("models");
        assert!(!pid_is_our_runtime(std::process::id(), models_root));
        assert!(!pid_is_our_runtime(0, models_root));
    }

    const FAST_RETRY: RetryPolicy = RetryPolicy {
        delays: &[Duration::from_millis(1), Duration::from_millis(1)],
        stall: Duration::from_secs(5),
    };

    /// HTTP-сервер по сценарию: обработчик получает запрос и пишет ответ в сокет
    /// как есть — так проще изобразить обрыв и зависание. Запросы записываются.
    fn serve(handler: impl Fn(usize, &str, &mut TcpStream) + Send + Sync + 'static) -> (DownloadSource, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let handler = Arc::new(handler);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let handler = handler.clone();
                let seen = seen.clone();
                thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    let index = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(head.clone());
                        seen.len() - 1
                    };
                    handler(index, &head, &mut stream);
                });
            }
        });
        (DownloadSource::direct(url), requests)
    }

    fn respond(stream: &mut TcpStream, status: &str, body: &[u8]) {
        let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
    }

    fn gateway_timeout() -> (DownloadSource, Arc<Mutex<Vec<String>>>) {
        serve(|_, _, stream| respond(stream, "504 Gateway Time-out", b"oops"))
    }

    #[test]
    fn gateway_error_falls_through_to_the_next_source_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.part");
        fs::write(&part, b"hello ").unwrap();
        let (broken, broken_seen) = gateway_timeout();
        let (working, working_seen) = serve(|_, _, stream| respond(stream, "206 Partial Content", b"world"));
        let size = download_from(&[broken, working], &part, 11, "тест", &FAST_RETRY).unwrap();
        assert_eq!(size, 11);
        assert_eq!(fs::read(&part).unwrap(), b"hello world");
        assert_eq!(broken_seen.lock().unwrap().len(), 1);
        assert!(working_seen.lock().unwrap()[0].contains("range: bytes=6-"));
    }

    #[test]
    fn a_broken_body_keeps_its_bytes_and_the_next_round_continues() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.part");
        let (source, seen) = serve(|index, _, stream| {
            if index == 0 {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello ").unwrap();
                stream.flush().unwrap();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            } else {
                respond(stream, "206 Partial Content", b"world");
            }
        });
        download_from(&[source], &part, 0, "тест", &FAST_RETRY).unwrap();
        assert_eq!(fs::read(&part).unwrap(), b"hello world");
        assert!(seen.lock().unwrap()[1].contains("range: bytes=6-"));
    }

    #[test]
    fn a_server_ignoring_range_restarts_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.part");
        fs::write(&part, b"stale").unwrap();
        let (source, _) = serve(|_, _, stream| respond(stream, "200 OK", b"fresh body"));
        let size = download_from(&[source], &part, 0, "тест", &FAST_RETRY).unwrap();
        assert_eq!(size, 10);
        assert_eq!(fs::read(&part).unwrap(), b"fresh body");
    }

    #[test]
    fn a_stalled_source_is_abandoned_for_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.part");
        let (stalled, _) = serve(|_, _, _| thread::sleep(Duration::from_secs(30)));
        let (working, _) = serve(|_, _, stream| respond(stream, "200 OK", b"data"));
        const ONE_PAUSE: &[Duration] = &[Duration::from_millis(1)];
        let policy = RetryPolicy { delays: ONE_PAUSE, stall: Duration::from_millis(300) };
        download_from(&[stalled, working], &part, 0, "тест", &policy).unwrap();
        assert_eq!(fs::read(&part).unwrap(), b"data");
    }

    #[test]
    fn exhausted_sources_name_the_host_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.part");
        let (broken, seen) = gateway_timeout();
        let host = broken.host().to_owned();
        let error = download_from(&[broken], &part, 0, "загрузка", &FAST_RETRY).unwrap_err();
        assert!(error.contains(&host) && error.contains("HTTP 504"), "{error}");
        // Первый круг и по одному на каждую паузу политики.
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert!(!part.exists(), "ответ с ошибкой не должен создавать файл");
    }

    #[test]
    fn rate_meter_ignores_bytes_that_were_already_on_disk() {
        let start = Instant::now();
        let mut meter = RateMeter::default();
        assert_eq!(meter.sample(start, 0), 0.0);
        assert_eq!(meter.sample(start + Duration::from_millis(100), 500), 0.0, "меньше секунды — ещё рано");
        assert_eq!(meter.sample(start + Duration::from_secs(2), 2_000), 1_000.0);
        // Сглаживание: 0.6 * 1000 + 0.4 * 3000.
        assert_eq!(meter.sample(start + Duration::from_secs(3), 5_000), 1_800.0);
        assert_eq!(meter.sample(start + Duration::from_secs(4), 1_000), 0.0, "новая попытка начинает отсчёт заново");
    }

    #[test]
    fn pull_progress_sums_layers_and_separates_what_was_already_there() {
        let mut layers = PullProgress::default();
        layers.update("sha256:a", 100, 100);
        layers.update("sha256:b", 0, 400);
        layers.update("sha256:b", 150, 400);
        assert_eq!(layers.totals(), (250, 500, 150));
        // Повтор строки с меньшим completed не откатывает индикатор.
        layers.update("sha256:b", 100, 400);
        assert_eq!(layers.totals(), (250, 500, 150));
    }

    #[test]
    fn port_comes_from_local_url_or_default() {
        let mut config = Config::default();
        assert_eq!(configured_port(&config), DEFAULT_PORT);
        config.local.base_url = Some("http://127.0.0.1:11500/v1".into());
        assert_eq!(configured_port(&config), 11500);
        config.local.base_url = Some("http://127.0.0.1/v1".into());
        assert_eq!(configured_port(&config), DEFAULT_PORT);
    }

    #[test]
    fn local_url_is_openai_compatible() {
        assert_eq!(local_base_url(11434), "http://127.0.0.1:11434/v1");
    }

    #[test]
    fn tag_names_are_namespaced() {
        assert_eq!(catalog::tag("qwen25-7b"), "brepsi/qwen25-7b");
    }

    #[test]
    fn recommended_context_follows_tier_and_vram() {
        let low = hardware::build(Vendor::Nvidia, "GTX 1070".into(), 8_000, 32_000);
        assert_eq!(recommended_context(&low), 8_192);
        let mid = hardware::build(Vendor::Nvidia, "RTX 3060".into(), 12_000, 32_000);
        assert_eq!(recommended_context(&mid), 16_384);
        // 16 ГБ — «высокий» тир, но 32k вытеснит 14B-модель из видеопамяти.
        let high16 = hardware::build(Vendor::Nvidia, "RTX 5080".into(), 16_000, 64_000);
        assert_eq!(recommended_context(&high16), 16_384);
        let high24 = hardware::build(Vendor::Nvidia, "RTX 4090".into(), 24_000, 64_000);
        assert_eq!(recommended_context(&high24), 32_768);
    }
}
