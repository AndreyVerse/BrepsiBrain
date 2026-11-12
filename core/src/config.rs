//! config.toml — порт той части `config.py`, что нужна перенесённым модулям:
//! [llm], [privacy] и [obsidian]. Внешние источники ([sources]) придут с этапом 8.

use std::path::{Path, PathBuf};

pub const ENV_CONFIG: &str = "BREPSI_BRAIN_CONFIG";
pub const ENV_API_KEY: &str = "BREPSI_BRAIN_API_KEY";
/// Прежние имена (приложение называлось Second Brain): читаются, если новых
/// переменных нет, чтобы старые ярлыки и MCP-регистрации не ломались.
pub const ENV_CONFIG_LEGACY: &str = "MEMORY_BASE_CONFIG";
pub const ENV_API_KEY_LEGACY: &str = "MEMORY_BASE_API_KEY";
pub const ENV_ROOT: &str = "BREPSI_BRAIN_ROOT";
pub const ENV_ROOT_LEGACY: &str = "SECOND_BRAIN_ROOT";
pub const ENV_DB: &str = "BREPSI_BRAIN_DB";
pub const ENV_DB_LEGACY: &str = "MEMORY_BASE_DB";
/// Корень локальных моделей важнее `[local_ai].models_root`: так склад можно
/// перенести, не переписывая конфиг.
pub const ENV_MODELS_ROOT: &str = "BREPSI_BRAIN_MODELS_ROOT";
pub const ENV_GMAIL_APP_PASSWORD: &str = "GMAIL_APP_PASSWORD";
pub const ENV_YANDEX_APP_PASSWORD: &str = "YANDEX_APP_PASSWORD";
pub const ENV_GITHUB_TOKEN: &str = "GITHUB_TOKEN";
pub const ENV_NOTION_TOKEN: &str = "NOTION_TOKEN";
pub const DEFAULT_OBSIDIAN_EXCLUDE_DIRS: [&str; 3] = [".obsidian", ".trash", "node_modules"];
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;
pub const DEFAULT_BATCH_SIZE: usize = 20;
/// Приватность: по умолчанию пусто, закрытые проекты перечисляются в config.toml.
pub const DEFAULT_LOCAL_ONLY_PATTERNS: [&str; 0] = [];

fn env_or_legacy(name: &str, legacy: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty()).or_else(|| std::env::var(legacy).ok().filter(|value| !value.is_empty()))
}

pub fn api_key_from_env() -> Option<String> {
    env_or_legacy(ENV_API_KEY, ENV_API_KEY_LEGACY)
}

pub fn db_path_from_env() -> Option<PathBuf> {
    env_or_legacy(ENV_DB, ENV_DB_LEGACY).map(PathBuf::from)
}

pub fn root_from_env() -> Option<PathBuf> {
    env_or_legacy(ENV_ROOT, ENV_ROOT_LEGACY).map(PathBuf::from)
}

/// OpenAI-совместимый chat-completions; None — не задано.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmSettings {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub timeout_seconds: f64,
    pub batch_size: usize,
    pub json_mode: bool,
    /// Тот же endpoint, другая модель: один раз, когда основная отвергла запись.
    pub fallback_model: Option<String>,
}

impl Default for LlmSettings {
    fn default() -> Self {
        LlmSettings {
            base_url: None,
            model: None,
            api_key: None,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            batch_size: DEFAULT_BATCH_SIZE,
            json_mode: false,
            fallback_model: None,
        }
    }
}

impl LlmSettings {
    pub fn configured(&self) -> bool {
        self.base_url.as_deref().is_some_and(|s| !s.is_empty()) && self.model.as_deref().is_some_and(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrivacySettings {
    /// Регулярные выражения без учёта регистра: совпавшее уходит только локальной модели.
    pub local_only_patterns: Vec<String>,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        PrivacySettings { local_only_patterns: DEFAULT_LOCAL_ONLY_PATTERNS.map(String::from).to_vec() }
    }
}

/// Тип создаваемых данных: своя модель на каждый (пусто — модель маршрута).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Distill,
    Answer,
    Handoff,
    Projects,
}

impl Task {
    pub const ALL: [Task; 4] = [Task::Distill, Task::Answer, Task::Handoff, Task::Projects];

    pub fn as_str(self) -> &'static str {
        match self {
            Task::Distill => "distill",
            Task::Answer => "answer",
            Task::Handoff => "handoff",
            Task::Projects => "projects",
        }
    }

    pub fn from_str(value: &str) -> Option<Task> {
        Task::ALL.into_iter().find(|task| task.as_str() == value)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskModels {
    pub distill: Option<String>,
    pub answer: Option<String>,
    pub handoff: Option<String>,
    pub projects: Option<String>,
}

impl TaskModels {
    pub fn get(&self, task: Task) -> Option<&str> {
        match task {
            Task::Distill => self.distill.as_deref(),
            Task::Answer => self.answer.as_deref(),
            Task::Handoff => self.handoff.as_deref(),
            Task::Projects => self.projects.as_deref(),
        }
        .filter(|model| !model.is_empty())
    }

    pub fn set(&mut self, task: Task, model: Option<String>) {
        let slot = match task {
            Task::Distill => &mut self.distill,
            Task::Answer => &mut self.answer,
            Task::Handoff => &mut self.handoff,
            Task::Projects => &mut self.projects,
        };
        *slot = model.filter(|value| !value.is_empty());
    }
}

/// Локальный маршрут `[llm.local]`: адрес и модель ставит установщик, но можно
/// указать и свой loopback-сервер.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalRoute {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub json_mode: bool,
}

impl LocalRoute {
    pub fn configured(&self) -> bool {
        self.base_url.as_deref().is_some_and(|url| !url.is_empty())
            && self.model.as_deref().is_some_and(|model| !model.is_empty())
    }
}

/// Как живёт локальный движок: только под задачу или постоянно.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalMode {
    #[default]
    OnDemand,
    Always,
}

impl LocalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LocalMode::OnDemand => "on_demand",
            LocalMode::Always => "always",
        }
    }

    pub fn parse(value: &str) -> Option<LocalMode> {
        match value {
            "on_demand" => Some(LocalMode::OnDemand),
            "always" => Some(LocalMode::Always),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalAiSettings {
    pub models_root: Option<PathBuf>,
    pub mode: LocalMode,
    pub idle_timeout_seconds: u64,
    pub context_tokens: u32,
    pub timeout_seconds: f64,
    pub batch_size: usize,
    pub offline_only: bool,
    /// Как часто плановый запуск будит локальную модель; None — каждый запуск.
    pub collect_interval_minutes: Option<u32>,
}

impl Default for LocalAiSettings {
    fn default() -> Self {
        LocalAiSettings {
            models_root: None,
            mode: LocalMode::OnDemand,
            idle_timeout_seconds: 120,
            context_tokens: 16384,
            timeout_seconds: 300.0,
            batch_size: 6,
            offline_only: false,
            collect_interval_minutes: None,
        }
    }
}

/// Расписание синхронизации: задача планировщика или только ручная кнопка.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    #[default]
    Interval,
    Manual,
}

impl SyncMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncMode::Interval => "interval",
            SyncMode::Manual => "manual",
        }
    }

    pub fn parse(value: &str) -> Option<SyncMode> {
        match value {
            "interval" => Some(SyncMode::Interval),
            "manual" => Some(SyncMode::Manual),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncSettings {
    pub mode: SyncMode,
    pub interval_minutes: u32,
}

impl Default for SyncSettings {
    fn default() -> Self {
        // Автосинк выключен по умолчанию: решение владельца 08.11.2026.
        // Задачу создаёт только явное сохранение режима interval.
        SyncSettings { mode: SyncMode::Manual, interval_minutes: 15 }
    }
}

/// Маршрут одного запроса: куда идти и какой моделью.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub timeout_seconds: f64,
    pub json_mode: bool,
    pub local: bool,
    pub task: Task,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObsidianSettings {
    /// Пусто — коннектор берёт папку `Obsidian` рядом с данными приложения.
    pub vaults: Vec<PathBuf>,
    pub exclude_dirs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatGptExportSettings {
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MailSettings {
    pub enabled: bool,
    pub email: Option<String>,
    pub app_password: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitHubSettings {
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NotionSettings {
    pub token: Option<String>,
    pub query: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtraSourcesSettings {
    pub chatgpt_export: ChatGptExportSettings,
    pub gmail: MailSettings,
    pub yandex: MailSettings,
    pub github: GitHubSettings,
    pub notion: NotionSettings,
}

impl Default for ObsidianSettings {
    fn default() -> Self {
        ObsidianSettings { vaults: Vec::new(), exclude_dirs: DEFAULT_OBSIDIAN_EXCLUDE_DIRS.map(String::from).to_vec() }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    pub llm: LlmSettings,
    pub privacy: PrivacySettings,
    pub obsidian: ObsidianSettings,
    pub extra_sources: ExtraSourcesSettings,
    pub tasks: TaskModels,
    pub local: LocalRoute,
    pub local_ai: LocalAiSettings,
    pub sync: SyncSettings,
    /// Корень для индексации git-репозиториев; None — папка данных приложения.
    pub git_root: Option<PathBuf>,
    /// None — файла нет, всё по умолчанию.
    pub path: Option<PathBuf>,
}

impl Config {
    /// Корень данных: папка `config.toml`, иначе — вычисленный для exe.
    pub fn root(&self) -> PathBuf {
        self.path
            .as_ref()
            .and_then(|path| path.parent())
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(project_root)
    }

    /// Где лежат движок, хранилище Ollama и GGUF.
    pub fn models_root(&self) -> PathBuf {
        std::env::var_os(ENV_MODELS_ROOT)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| self.local_ai.models_root.clone())
            .unwrap_or_else(|| self.root().join("models"))
    }

    /// Маршрут задачи. `need_local` — запрос не имеет права уходить наружу.
    pub fn route_for(&self, task: Task, need_local: bool) -> Option<Route> {
        if need_local || self.local_ai.offline_only {
            return self.local_route(task);
        }
        self.default_route(task).or_else(|| self.local_route(task))
    }

    fn default_route(&self, task: Task) -> Option<Route> {
        let base_url = self.llm.base_url.as_deref().filter(|url| !url.is_empty())?;
        // Свой тег `brepsi/…` облачному провайдеру не отправляем: его там нет.
        let remote = !crate::llm::is_local_endpoint(base_url);
        let task_model = self.tasks.get(task).filter(|model| !(remote && is_own_tag(model)));
        let model = task_model.or(self.llm.model.as_deref()).filter(|model| !model.is_empty())?;
        Some(Route {
            base_url: base_url.to_owned(),
            model: model.to_owned(),
            api_key: self.llm.api_key.clone(),
            timeout_seconds: self.llm.timeout_seconds,
            json_mode: self.llm.json_mode,
            local: crate::llm::is_local_endpoint(base_url),
            task,
        })
    }

    /// Локальный маршрут; не-loopback адрес локальным не считается — иначе
    /// приватность и режим «только локально» обещали бы не то, что делают.
    pub fn local_route(&self, task: Task) -> Option<Route> {
        if !self.local.configured() {
            return None;
        }
        let base_url = self.local.base_url.as_deref()?;
        if !crate::llm::is_local_endpoint(base_url) {
            return None;
        }
        // Модель задачи, выбранная для облачного провайдера, в локальном движке не
        // установлена: приватный запрос падал с «модель не установлена».
        let remote_configured = self.llm.base_url.as_deref().is_some_and(|url| !url.is_empty() && !crate::llm::is_local_endpoint(url));
        let task_model = self.tasks.get(task).filter(|model| is_own_tag(model) || !remote_configured);
        let model = task_model.or(self.local.model.as_deref()).filter(|model| !model.is_empty())?;
        Some(Route {
            base_url: base_url.to_owned(),
            model: model.to_owned(),
            api_key: self.local.api_key.clone(),
            timeout_seconds: self.local_ai.timeout_seconds,
            json_mode: self.local.json_mode,
            local: true,
            task,
        })
    }

    /// Часовой шаг расписания; в ручном режиме нечего показывать.
    pub fn next_sync_ts(&self, last_sync_ts: Option<i64>) -> Option<i64> {
        if self.sync.mode != SyncMode::Interval {
            return None;
        }
        last_sync_ts.map(|last| last + i64::from(self.sync.interval_minutes) * 60)
    }

    /// Пора ли плановому запуску будить локальную модель. Планировщик срабатывает
    /// с дрожанием в секунды: без запаса запуск на 899-й секунде из 900 пропускал
    /// бы цикл и удваивал интервал.
    pub fn local_collect_due(&self, last_collect_ts: Option<i64>, now: i64) -> bool {
        let Some(minutes) = self.local_ai.collect_interval_minutes else { return true };
        let Some(last) = last_collect_ts else { return true };
        now - last >= i64::from(minutes) * 60 - 60
    }
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Корень данных: рядом лежат config.toml и data/. Портативная копия держит
/// Тег модели из своего хранилища (`catalog::tag`).
fn is_own_tag(model: &str) -> bool {
    model.starts_with("brepsi/")
}

/// их рядом с exe; при запуске из исходников — текущая папка.
pub fn project_root() -> PathBuf {
    if let Some(root) = root_from_env() {
        return root;
    }
    // Для рабочей папки голой `data/` мало: харнесс запускает `mcp` из папки
    // проекта, где своя `data/` — частое дело, и там молча создавалась пустая база.
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

pub fn active_config_path() -> PathBuf {
    config_path_from_env().unwrap_or_else(|| project_root().join("config.toml"))
}

/// Явный путь конфига из окружения: настройки пишутся туда же, откуда читаются.
pub fn config_path_from_env() -> Option<PathBuf> {
    env_or_legacy(ENV_CONFIG, ENV_CONFIG_LEGACY).map(PathBuf::from)
}

/// Windows-путь из строки конфига, как Path() в Python: прямые слэши становятся обратными.
pub fn windows_path(value: &str) -> PathBuf {
    PathBuf::from(if cfg!(windows) { value.replace('/', "\\") } else { value.to_string() })
}

fn string_list(table: &toml::Table, key: &str, name: &str) -> Result<Option<Vec<String>>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| ConfigError(format!("{name} must be a list of strings"))))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(ConfigError(format!("{name} must be a list of strings"))),
    }
}

fn section<'a>(raw: &'a toml::Table, name: &str, empty: &'a toml::Table) -> Result<&'a toml::Table, ConfigError> {
    match raw.get(name) {
        None => Ok(empty),
        Some(toml::Value::Table(t)) => Ok(t),
        Some(_) => Err(ConfigError(format!("[{name}] must be a table"))),
    }
}

/// Пустая строка — как отсутствие, так же делает `or None` в Python.
fn opt_string(table: &toml::Table, key: &str) -> Result<Option<String>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone()).filter(|s| !s.is_empty())),
        Some(_) => Err(ConfigError(format!("llm.{key} must be a string"))),
    }
}

fn opt_string_named(table: &toml::Table, key: &str, name: &str) -> Result<Option<String>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone()).filter(|s| !s.is_empty())),
        Some(_) => Err(ConfigError(format!("{name} must be a string"))),
    }
}

fn opt_uint(table: &toml::Table, key: &str, name: &str) -> Result<Option<u64>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Integer(value)) if *value >= 0 => Ok(Some(*value as u64)),
        Some(_) => Err(ConfigError(format!("{name} must be a non-negative integer"))),
    }
}

fn opt_float(table: &toml::Table, key: &str, name: &str) -> Result<Option<f64>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Float(value)) => Ok(Some(*value)),
        Some(toml::Value::Integer(value)) => Ok(Some(*value as f64)),
        Some(_) => Err(ConfigError(format!("{name} must be a number"))),
    }
}

fn opt_bool(table: &toml::Table, key: &str, name: &str) -> Result<Option<bool>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Boolean(value)) => Ok(Some(*value)),
        Some(_) => Err(ConfigError(format!("{name} must be a bool"))),
    }
}

fn parse_mail(table: &toml::Table, password_env: &str) -> Result<MailSettings, ConfigError> {
    Ok(MailSettings {
        enabled: opt_bool(table, "enabled", "sources.mail.enabled")?.unwrap_or(false),
        email: opt_string(table, "email")?,
        app_password: std::env::var(password_env).ok().filter(|value| !value.is_empty())
            .or(opt_string(table, "app_password")?),
    })
}

fn parse_llm(table: &toml::Table, env_api_key: Option<String>) -> Result<LlmSettings, ConfigError> {
    let mut llm = LlmSettings {
        base_url: opt_string(table, "base_url")?,
        model: opt_string(table, "model")?,
        api_key: env_api_key.filter(|k| !k.is_empty()).or(opt_string(table, "api_key")?),
        fallback_model: opt_string(table, "fallback_model")?,
        ..LlmSettings::default()
    };
    match table.get("timeout_seconds") {
        None => {}
        Some(toml::Value::Float(v)) => llm.timeout_seconds = *v,
        Some(toml::Value::Integer(v)) => llm.timeout_seconds = *v as f64,
        Some(_) => return Err(ConfigError("llm.timeout_seconds must be a number".into())),
    }
    match table.get("batch_size") {
        None => {}
        Some(toml::Value::Integer(v)) if *v >= 0 => llm.batch_size = *v as usize,
        Some(_) => return Err(ConfigError("llm.batch_size must be a non-negative integer".into())),
    }
    match table.get("json_mode") {
        None => {}
        Some(toml::Value::Boolean(v)) => llm.json_mode = *v,
        Some(_) => return Err(ConfigError("llm.json_mode must be a bool".into())),
    }
    Ok(llm)
}

fn parse_tasks(llm: &toml::Table) -> Result<TaskModels, ConfigError> {
    let Some(value) = llm.get("tasks") else { return Ok(TaskModels::default()) };
    let table = value.as_table().ok_or_else(|| ConfigError("llm.tasks must be a table".into()))?;
    let mut tasks = TaskModels::default();
    for task in Task::ALL {
        let model = opt_string_named(table, task.as_str(), &format!("llm.tasks.{}", task.as_str()))?;
        tasks.set(task, model);
    }
    Ok(tasks)
}

fn parse_local_route(llm: &toml::Table) -> Result<LocalRoute, ConfigError> {
    let Some(value) = llm.get("local") else { return Ok(LocalRoute::default()) };
    let table = value.as_table().ok_or_else(|| ConfigError("llm.local must be a table".into()))?;
    Ok(LocalRoute {
        base_url: opt_string_named(table, "base_url", "llm.local.base_url")?,
        model: opt_string_named(table, "model", "llm.local.model")?,
        api_key: opt_string_named(table, "api_key", "llm.local.api_key")?,
        json_mode: opt_bool(table, "json_mode", "llm.local.json_mode")?.unwrap_or(false),
    })
}

fn parse_local_ai(raw: &toml::Table) -> Result<LocalAiSettings, ConfigError> {
    let mut settings = LocalAiSettings::default();
    if let Some(root) = opt_string_named(raw, "models_root", "local_ai.models_root")? {
        settings.models_root = Some(windows_path(&root));
    }
    if let Some(mode) = opt_string_named(raw, "mode", "local_ai.mode")? {
        settings.mode = LocalMode::parse(&mode)
            .ok_or_else(|| ConfigError("local_ai.mode must be on_demand or always".into()))?;
    }
    if let Some(value) = opt_uint(raw, "idle_timeout_seconds", "local_ai.idle_timeout_seconds")? {
        settings.idle_timeout_seconds = value.clamp(10, 3600);
    }
    if let Some(value) = opt_uint(raw, "context_tokens", "local_ai.context_tokens")? {
        // Окно выравниваем по 1024: в Modelfile уходят целые шаги.
        settings.context_tokens = (value.clamp(2048, 131_072) / 1024 * 1024) as u32;
    }
    if let Some(value) = opt_float(raw, "timeout_seconds", "local_ai.timeout_seconds")? {
        settings.timeout_seconds = value.clamp(10.0, 3600.0);
    }
    if let Some(value) = opt_uint(raw, "batch_size", "local_ai.batch_size")? {
        settings.batch_size = value.clamp(1, 64) as usize;
    }
    if let Some(value) = opt_bool(raw, "offline_only", "local_ai.offline_only")? {
        settings.offline_only = value;
    }
    if let Some(value) = opt_uint(raw, "collect_interval_minutes", "local_ai.collect_interval_minutes")? {
        settings.collect_interval_minutes = Some(value.clamp(5, 1440) as u32);
    }
    Ok(settings)
}

fn parse_sync(raw: &toml::Table) -> Result<SyncSettings, ConfigError> {
    let mut settings = SyncSettings::default();
    if let Some(mode) = opt_string_named(raw, "mode", "sync.mode")? {
        settings.mode = SyncMode::parse(&mode)
            .ok_or_else(|| ConfigError("sync.mode must be interval or manual".into()))?;
    }
    if let Some(value) = opt_uint(raw, "interval_minutes", "sync.interval_minutes")? {
        // 1439 — потолок `schtasks /SC MINUTE /MO`: 1440 планировщик отвергает,
        // и расписание молча не создаётся.
        settings.interval_minutes = value.clamp(5, 1439) as u32;
    }
    Ok(settings)
}

/// Нет файла — значения по умолчанию; битый файл — ошибка. Ключ API из
/// BREPSI_BRAIN_API_KEY важнее файла.
pub fn load_config(path: Option<&Path>) -> Result<Config, ConfigError> {
    load_config_with_key(path, api_key_from_env())
}

/// `load_config` с явно переданным ключом из окружения: тесты не трогают
/// переменные процесса, общие для параллельных потоков cargo test.
pub fn load_config_with_key(path: Option<&Path>, env_api_key: Option<String>) -> Result<Config, ConfigError> {
    let path = path.map_or_else(active_config_path, Path::to_path_buf);
    let empty = toml::Table::new();
    if !path.is_file() {
        return Ok(Config { llm: parse_llm(&empty, env_api_key)?, path: Some(path), ..Config::default() });
    }
    let text = std::fs::read_to_string(&path).map_err(|e| ConfigError(format!("cannot parse config {}: {e}", path.display())))?;
    let raw: toml::Table = text.parse().map_err(|e| ConfigError(format!("cannot parse config {}: {e}", path.display())))?;

    let privacy_raw = section(&raw, "privacy", &empty)?;
    let mut privacy = PrivacySettings::default();
    if let Some(patterns) = string_list(privacy_raw, "local_only_patterns", "privacy.local_only_patterns")? {
        privacy.local_only_patterns = patterns;
    }

    let obsidian_raw = section(&raw, "obsidian", &empty)?;
    let mut obsidian = ObsidianSettings::default();
    if let Some(vaults) = string_list(obsidian_raw, "vaults", "obsidian.vaults")? {
        obsidian.vaults = vaults.iter().filter(|v| !v.trim().is_empty()).map(|v| windows_path(v)).collect();
    }
    if let Some(dirs) = string_list(obsidian_raw, "exclude_dirs", "obsidian.exclude_dirs")? {
        obsidian.exclude_dirs = dirs;
    }
    // Явный obsidian.local_only_patterns дополняет privacy-гейт (как config.py); без ключа
    // ничего не добавляет — умолчания не возвращают снятые владельцем ограничения.
    if let Some(extra) = string_list(obsidian_raw, "local_only_patterns", "obsidian.local_only_patterns")? {
        for pattern in extra {
            if !privacy.local_only_patterns.contains(&pattern) {
                privacy.local_only_patterns.push(pattern);
            }
        }
    }

    let sources_raw = section(&raw, "sources", &empty)?;
    // Без `is_dir`: временно недоступный корень (сетевой диск) должен означать
    // «нечего индексировать», а не тихий переход на корень данных приложения —
    // обход несуществующего каталога и так безопасен (`iter_git_repos`).
    let git_root = opt_string(sources_raw, "git_root")?.map(|path| windows_path(&path));
    let chatgpt_raw = section(sources_raw, "chatgpt_export", &empty)?;
    let gmail_raw = section(sources_raw, "gmail", &empty)?;
    let yandex_raw = section(sources_raw, "yandex", &empty)?;
    let github_raw = section(sources_raw, "github", &empty)?;
    let notion_raw = section(sources_raw, "notion", &empty)?;
    let extra_sources = ExtraSourcesSettings {
        chatgpt_export: ChatGptExportSettings {
            path: opt_string(chatgpt_raw, "path")?.map(|path| windows_path(&path)),
        },
        gmail: parse_mail(gmail_raw, ENV_GMAIL_APP_PASSWORD)?,
        yandex: parse_mail(yandex_raw, ENV_YANDEX_APP_PASSWORD)?,
        github: GitHubSettings {
            token: std::env::var(ENV_GITHUB_TOKEN).ok().filter(|value| !value.is_empty())
                .or(opt_string(github_raw, "token")?),
        },
        notion: NotionSettings {
            token: std::env::var(ENV_NOTION_TOKEN).ok().filter(|value| !value.is_empty())
                .or(opt_string(notion_raw, "token")?),
            query: opt_string(notion_raw, "query")?.unwrap_or_default(),
        },
    };

    let llm_raw = section(&raw, "llm", &empty)?;
    let llm = parse_llm(llm_raw, env_api_key)?;
    let tasks = parse_tasks(llm_raw)?;
    let local = parse_local_route(llm_raw)?;
    let local_ai = parse_local_ai(section(&raw, "local_ai", &empty)?)?;
    let sync = parse_sync(section(&raw, "sync", &empty)?)?;
    Ok(Config { llm, privacy, obsidian, extra_sources, tasks, local, local_ai, sync, git_root, path: Some(path) })
}

/// Как `_load_config` в obsidian.py: битый конфиг не должен ломать индексацию.
pub fn load_config_or_default() -> Config {
    load_config(None).unwrap_or_default()
}

/// Хост URL без порта и учётных данных, в нижнем регистре, как `urlsplit().hostname`.
pub fn url_hostname(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("//")?;
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    Some(host.to_lowercase()).filter(|h| !h.is_empty())
}

/// Ключ API остаётся у своего провайдера: оба URL указывают на один хост.
pub fn same_host(a: Option<&str>, b: Option<&str>) -> bool {
    match (url_hostname(a.unwrap_or("")), url_hostname(b.unwrap_or(""))) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}
