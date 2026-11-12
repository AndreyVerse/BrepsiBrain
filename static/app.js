/* Brepsi Brain dashboard: одностраничный клиент локального сервера на чистом JS.
 *
 * Всё здесь крутится в event loop браузера. Пересекающиеся запросы
 * упорядочены тикетами (см. latest()), поэтому медленный старый ответ никогда
 * не заменит новый. Без фреймворков и сборки. UI-строки идут через tr(): ключ —
 * русский текст, английский берётся из SB_EN (i18n.js); всё из API
 * экранируется, прежде чем попасть в DOM.
 */
"use strict";

// -- i18n --------------------------------------------------------------------

// Выбор общий с треем (data/ui.json): сервер кладёт его в <html data-lang>,
// а "auto" решается по локали Windows, которую оболочка передаёт в __SB_SYSTEM_LANG.
function resolveLang() {
  const choice = globalThis.document?.documentElement?.dataset?.lang;
  if (choice === "ru" || choice === "en") return choice;
  const system = String(globalThis.__SB_SYSTEM_LANG || globalThis.navigator?.language || "ru").toLowerCase();
  return system.startsWith("ru") ? "ru" : "en";
}
const LANG = resolveLang();
const LOCALE = LANG === "en" ? "en-US" : "ru-RU";
const EN = globalThis.SB_EN || {};

function tr(ru, vars) {
  const text = LANG === "en" ? (EN[ru] ?? ru) : ru;
  return vars ? text.replace(/\{(\w+)\}/g, (m, k) => (k in vars ? vars[k] : m)) : text;
}

const RECORDS_PAGE = 50;
const SEARCH_LIMIT = 30;
const PALETTE_LIMIT = 8;
const ANSWER_LIMIT = 8;
const ANSWER_HISTORY_MAX = 8;
const ASK_PLACEHOLDER = tr("Спросите о прошлой работе: решения, ошибки, команды…");
const PROJECTS_LIMIT = 1000;
const DEBOUNCE_MS = 180;
const STATS_REFRESH_MS = 60_000;
// Просрочка считается от сохранённого интервала, а не от прежних 15 минут.
const syncIntervalSeconds = (stats) => Math.max(5, Number(((stats || {}).sync || {}).interval_minutes) || 15) * 60;

// Проверенная категориальная палитра (dataviz validate_palette, тёмная тема). Задана
// по источнику, никогда по рангу: фильтр не должен перекрашивать остальные.
const SOURCE_COLORS = {
  claude: "#3987e5", codex: "#d95926", git: "#199e70", github: "#199e70",
  kimi: "#c98500", obsidian: "#d55181", gmail: "#008300", yandex: "#008300",
  notion: "#9085e9", chatgpt: "#e66767",
};
const SOURCE_ORDER = ["claude", "codex", "git", "github", "kimi", "obsidian", "gmail", "yandex", "notion", "chatgpt"];
const SOURCE_NAMES = {
  claude: "Claude Code", codex: "Codex", kimi: "Kimi Code", git: "Git", github: "GitHub",
  obsidian: "Obsidian", chatgpt: "ChatGPT", gmail: "Gmail", yandex: tr("Яндекс.Почта"), notion: "Notion",
  search: tr("Поиск"),
};
const DEFAULT_COLOR = "#8f8ca0";
const PAGES = ["answers", "overview", "records", "graph", "projects", "handoff", "activity", "models", "about"];
const PAGE_TITLES = {
  overview: tr("Обзор"), records: tr("Все записи"), graph: tr("Карта памяти"), projects: tr("Проекты"),
  answers: tr("Спросить память"), activity: tr("Журнал обработки"), models: tr("Модели и железо"),
  handoff: tr("Перенос чатов"), about: tr("О программе"),
};

const $ = (id) => document.getElementById(id);

// -- помощники ---------------------------------------------------------------

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

const origin = (source) => String(source || "").replace(/^llm:/, "");
const sourceColor = (source) => SOURCE_COLORS[origin(source)] || DEFAULT_COLOR;
const sourceName = (source) => SOURCE_NAMES[origin(source)] || origin(source);
const sourceRank = (source) => {
  const at = SOURCE_ORDER.indexOf(origin(source));
  return at < 0 ? SOURCE_ORDER.length : at;
};
const leaf = (project) => String(project || "").split("/").pop();
const number = (n) => (Number(n) || 0).toLocaleString(LOCALE);

// Русские формы — ключи словаря: в английском остаются «одна» и «много».
function plural(n, one, few, many) {
  if (LANG === "en") return tr(Math.abs(n) === 1 ? one : many);
  const m = Math.abs(n) % 100;
  const d = m % 10;
  if (m > 10 && m < 20) return many;
  if (d > 1 && d < 5) return few;
  return d === 1 ? one : many;
}
// Подписи недель сервер пишет по-русски («25 мая»); в английском — из ключа недели (понедельник, UTC).
function weekLabel(week, end = false) {
  if (LANG !== "en") return end ? week.end_label : week.label;
  const day = new Date(`${week.key}T00:00:00Z`);
  if (end) day.setUTCDate(day.getUTCDate() + 6);
  return day.toLocaleDateString(LOCALE, { day: "numeric", month: "short", timeZone: "UTC" });
}
const records = (n) => `${number(n)} ${plural(n, "запись", "записи", "записей")}`;

const clock = (ts) => new Date(ts * 1000).toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit" });
const fullDate = (ts) => new Date(ts * 1000).toLocaleString(LOCALE, {
  day: "numeric", month: "long", year: "numeric", hour: "2-digit", minute: "2-digit",
});
const shortDate = (ts) => new Date(ts * 1000).toLocaleDateString(LOCALE, { day: "numeric", month: "short" });
const dayDate = (ts) => new Date(ts * 1000)
  .toLocaleDateString(LOCALE, { day: "numeric", month: "long", year: "numeric" }).replace(/\s*г\.$/, "");

function ago(ts, now = Date.now() / 1000) {
  const minutes = Math.max(0, Math.round((now - ts) / 60));
  if (minutes < 1) return tr("только что");
  if (minutes < 60) return tr("{n} мин назад", { n: minutes });
  const hours = Math.round(minutes / 60);
  if (hours < 24) return tr("{n} ч назад", { n: hours });
  const days = Math.round(hours / 24);
  return tr("{n} {unit} назад", { n: days, unit: plural(days, "день", "дня", "дней") });
}

async function request(url, init) {
  let resp;
  try {
    resp = await fetch(url, init);
  } catch {
    throw new Error(tr("сервер недоступен"));
  }
  let data = null;
  try {
    data = await resp.json();
  } catch {
    /* не JSON: об этом скажет проверка статуса ниже */
  }
  if (!resp.ok) {
    const failure = new Error((data && data.error) || `HTTP ${resp.status}`);
    failure.status = resp.status;
    throw failure;
  }
  return data;
}

function api(path, params) {
  const qs = new URLSearchParams();
  for (const [key, value] of Object.entries(params || {})) {
    if (value !== undefined && value !== null && value !== "") qs.set(key, value);
  }
  const query = qs.toString();
  return request(query ? `${path}?${query}` : path);
}

const postJson = (path, body) => request(path, {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(body),
});

// Возвращает функцию с тикетами: ticket() начинает запрос, а
// ticket.current(t) говорит, всё ли t самый новый.
function latest() {
  let last = 0;
  const ticket = () => ++last;
  ticket.current = (t) => t === last;
  return ticket;
}

let toastTimer = 0;
function toast(message, ms = 4500) {
  const el = $("toast");
  el.textContent = message;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, ms);
}

function setPressed(group, match) {
  group.querySelectorAll("button").forEach((b) => b.setAttribute("aria-pressed", String(match(b))));
}

// Debounce и безопасное для async сохранение мелких настроек вида. Хранилище
// браузера может бросить исключение или вернуть пусто; страница живёт и без него.
function loadPref(key, fallback) {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : JSON.parse(raw);
  } catch {
    return fallback;
  }
}
function savePref(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    /* только удобство */
  }
}

// -- маршрутизация -----------------------------------------------------------
//
// #overview #graph #graph/<id> #projects #records #answers #activity, и две
// команды из трея: #record/<id> открывает запись поверх текущей страницы,
// #settings открывает диалог провайдера.

const state = {
  page: null,
  stats: null,
  records: { q: "", source: "", project: null, items: [], total: 0, loading: false },
  selectedRecord: null,
};

function parseHash() {
  const [page, arg] = (location.hash || "").replace(/^#/, "").split("/");
  return { page, id: Number(arg) || null };
}

function applyRoute() {
  const { page, id } = parseHash();
  if ((page === "record" && id) || page === "settings") {
    if (!state.page) showPage("overview");
    restoreHash();
    if (page === "settings") openSettings();
    else openRecord(id);
    return;
  }
  showPage(PAGES.includes(page) ? page : "overview");
  if (page === "graph" && id) graphFocusWhenReady(id);
}

function restoreHash() {
  const target = `#${state.page || "overview"}`;
  history.replaceState(null, "", target);
}

function go(page) {
  if (location.hash === `#${page}`) showPage(page);
  else location.hash = `#${page}`;
}

function showPage(page) {
  const changed = state.page !== page;
  state.page = page;
  $("breadcrumb-current").textContent = PAGE_TITLES[page];
  document.querySelector(".workspace").dataset.page = page;
  for (const name of PAGES) $(`page-${name}`).hidden = name !== page;
  document.querySelectorAll(".nav-item").forEach((a) => {
    const active = a.dataset.page === page;
    a.classList.toggle("active", active);
    if (active) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
  closePanel();
  if (page !== "graph") graphPause();
  if (page === "overview") loadOverview();
  if (page === "graph") loadGraph();
  if (page === "projects") loadProjects();
  if (page === "records" && (changed || !state.records.items.length)) loadRecords();
  if (page === "answers" && changed) $("answer-q").focus();
  if (page === "activity") loadActivity();
  if (page === "models") loadModels();
  if (page === "handoff") loadHandoff();
  if (page === "about") renderAbout();
  if (changed) scrollTo(0, 0);
}

// -- статистика и сайдбар ----------------------------------------------------

const statsTicket = latest();
let syncRunning = false;

async function loadStats() {
  const t = statsTicket();
  try {
    const stats = await api("/api/stats");
    if (!statsTicket.current(t)) return null;
    const previousSync = state.stats?.last_sync_ts;
    state.stats = stats;
    renderSidebar();
    if (previousSync && stats.last_sync_ts > previousSync && !syncRunning) checkAppUpdate({ auto: true });
    if (state.page === "overview") renderOverviewHead();
    if (state.page === "activity") renderQueue(stats);
    return stats;
  } catch (e) {
    if (statsTicket.current(t)) {
      $("sync-dot").className = "status-dot err";
      $("sync-status").textContent = tr("База недоступна: {e}", { e: e.message });
    }
    return null;
  }
}

function formatBytes(n) {
  n = Number(n) || 0;
  if (n >= 1073741824) return tr("{n} ГБ", { n: (n / 1073741824).toLocaleString(LOCALE, { maximumFractionDigits: 2 }) });
  if (n >= 1048576) return tr("{n} МБ", { n: (n / 1048576).toLocaleString(LOCALE, { maximumFractionDigits: 1 }) });
  return tr("{n} КБ", { n: Math.round(n / 1024) });
}

function queueFacts(stats) {
  const queue = stats.queue || {};
  const waiting = (Number(queue.pending) || 0) + (Number(queue.waiting) || 0);
  return {
    waiting,
    held: Number(queue.held_local) || 0,
    failed: Number(queue.failed) || 0,
    done: Number(queue.done) || 0,
    records: Number(queue.records) || 0,
    configured: Boolean(queue.configured),
  };
}

function renderSidebar() {
  const stats = state.stats;
  if (!stats) return;
  const facts = queueFacts(stats);
  $("nav-graph").textContent = number(facts.records);
  $("nav-graph").hidden = false;
  $("stats").innerHTML =
    `<span class="stat"><i class="status-dot ok"></i>${tr("Локальная база")}</span>` +
    `<span class="stat"><b>${number(facts.records)}</b> ${plural(facts.records, "запись", "записи", "записей")}</span>` +
    `<span class="stat">${esc(formatBytes(stats.db_bytes))}</span>`;
  if (syncRunning) return;
  const last = stats.last_sync_ts;
  $("sync-dot").className = `status-dot ${!last ? "warn" : facts.failed ? "warn" : "ok"}`;
  $("sync-status").textContent = last
    ? tr("Синхронизация: {time}, {ago}", { time: clock(last), ago: ago(last) })
    : tr("Синхронизаций ещё не было");
  // Прогон пишется в лог по завершении, через минуты после срока.
  const manual = (stats.sync || {}).mode === "manual";
  const overdue = !manual && last && stats.server_ts - last > 2 * syncIntervalSeconds(stats);
  $("sync-built").textContent = manual
    ? tr("синхронизация вручную")
    : stats.next_sync_ts
      ? tr("следующая в {time}", { time: clock(stats.next_sync_ts) })
      : (last ? (overdue ? tr("расписание опаздывает") : tr("следующая вот-вот")) : "");
  if (overdue) $("sync-dot").className = "status-dot warn";
}

function renderOverviewHead() {
  const stats = state.stats;
  if (!stats) return;
  renderTiles(stats, queueFacts(stats));
  renderSources(stats.sources || []);
  renderLegend();
}

function tile(label, icon, value, note, extra = "", accent = false) {
  return `<div class="tile"><div class="tile-top"><span>${esc(label)}</span><svg class="icon"><use href="#${icon}"/></svg></div>` +
    `<span class="tile-value${accent ? " is-accent" : ""}">${value}</span><span class="tile-note">${esc(note)}</span>${extra}</div>`;
}

function emptyStateHtml() {
  return `<div class="empty-state">` +
    `<h2>${tr("Память пока пуста")}</h2>` +
    `<p>${tr("Brepsi Brain собирает сессии ваших AI-харнессов и историю git в единую базу. Нажмите «Синхронизировать сейчас» — записи появятся здесь. Пересказы и карточки знаний пишет модель: для поиска она не обязательна.")}</p>` +
    `<button class="btn-cta" type="button" data-sync-now>${tr("Синхронизировать сейчас")}</button>` +
    `<p class="subtle">${tr("Модель настраивается в «Настройках» внизу боковой панели.")}</p>` +
    `</div>`;
}

function renderTiles(stats, facts) {
  if (!facts.records) {
    $("tiles").innerHTML = emptyStateHtml();
    return;
  }
  const knowledge = stats.knowledge || {};
  const kinds = [
    [knowledge["решение"], "решение", "решения", "решений"],
    [knowledge["ошибка"], "ошибка", "ошибки", "ошибок"],
    [knowledge["команда"], "команда", "команды", "команд"],
    [knowledge["факт"], "факт", "факта", "фактов"],
  ].filter(([n]) => n).map(([n, one, few, many]) => `${number(n)} ${plural(n, one, few, many)}`);
  const eligible = Math.max(0, facts.records - facts.held);
  const pct = eligible ? Math.min(100, Math.floor(facts.done / eligible * 100)) : 0;
  const queueNote = [
    facts.waiting ? tr("{n} в очереди", { n: number(facts.waiting) }) : (eligible ? tr("очередь пуста") : tr("записей пока нет")),
    facts.held ? tr("{n} только для локальной модели", { n: number(facts.held) }) : "",
    facts.failed ? tr("{n} с ошибкой", { n: number(facts.failed) }) : "",
  ].filter(Boolean).join(" · ");
  $("tiles").innerHTML =
    tile(tr("Сохранено записей"), "i-list", number(facts.records), tr("{n} активных за 7 дней", { n: number(stats.records_7d) })) +
    tile(tr("Проектов и коллекций"), "i-folder", number(stats.projects_total), tr("{n} живых за 30 дней", { n: number(stats.projects_active_30d) })) +
    tile(tr("Карточек знаний"), "i-chat", number((stats.queue || {}).cards), kinds.join(", ") || tr("Появятся после первой обработки")) +
    tile(tr("Обработано записей"), "i-sync", eligible ? `${pct}%` : "—", queueNote,
      `<div class="progress-track"><i class="is-grad" style="width:${pct}%"></i></div>`, true);
}

function renderSources(rows) {
  const max = Math.max(1, ...rows.map((row) => row.records));
  $("source-breakdown").innerHTML = rows.length ? rows.map((row) =>
    `<button class="source-line" data-source="${esc(row.source)}" type="button">` +
    `<span class="source-name"><i class="dot" style="background:${sourceColor(row.source)}"></i>${esc(sourceName(row.source))}</span>` +
    `<span class="source-track"><i style="width:${(row.records / max * 100).toFixed(1)}%;background:${sourceColor(row.source)}"></i></span>` +
    `<b>${number(row.records)}</b></button>`
  ).join("") : `<div class="state">${tr("После синхронизации здесь появятся источники.")}</div>`;
}

async function runSync() {
  if (syncRunning) return;
  syncRunning = true;
  const button = $("sync-now");
  button.disabled = true;
  button.classList.add("is-syncing");
  $("sync-dot").className = "status-dot busy";
  $("sync-status").textContent = tr("Синхронизация идёт…");
  try {
    const data = await request("/api/sync", { method: "POST" });
    const llm = data.llm || {};
    const parts = [tr("новых фрагментов: {n}", { n: number(data.new_cards) })];
    if (llm.configured) parts.push(tr("карточек: {n} из {total}", { n: number(llm.written), total: number(llm.candidates) }));
    if (llm.skipped_local_only) parts.push(tr("только для локальной модели: {n}", { n: number(llm.skipped_local_only) }));
    const names = data.projects || {};
    if ((names.applied || []).length) parts.push(tr("названий проектов обновлено: {n}", { n: number(names.applied.length) }));
    if (names.error) parts.push(tr("названия проектов не обновились: {e}", { e: names.error }));
    if (llm.error) parts.push(tr("модель: {e}", { e: llm.error }));
    else if (llm.aborted) parts.push(tr("провайдер недоступен"));
    if (llm.failed) parts.push(tr("{n} с ошибкой — см. «Журнал обработки»", { n: number(llm.failed) }));
    const troubled = llm.error || llm.aborted || llm.failed;
    toast(`${troubled ? tr("Готово с ошибками модели") : tr("Готово")}: ${parts.join(", ")}`, troubled ? 12000 : 5000);
    refreshCurrentPage();
    checkAppUpdate({ auto: true });
  } catch (e) {
    toast(e.status === 409 ? tr("Синхронизация уже идёт.") : tr("Синхронизация не удалась: {e}", { e: e.message }), 8000);
  } finally {
    syncRunning = false;
    button.disabled = false;
    button.classList.remove("is-syncing");
    loadStats();
  }
}

function refreshCurrentPage() {
  if (state.page === "overview") loadOverview();
  if (state.page === "graph") loadGraph(true);
  if (state.page === "projects") loadProjects();
  if (state.page === "records") loadRecords();
  if (state.page === "activity") loadActivity();
}

// -- обзор -------------------------------------------------------------------

let timeline = null;
let timelineWeeks = 0;
const timelineTicket = latest();
const overviewTicket = latest();

async function loadOverview() {
  await loadStats();
  loadTimeline();
  const t = overviewTicket();
  try {
    const [projects, fresh] = await Promise.all([
      api("/api/projects", { limit: 6 }),
      api("/api/records", { limit: 6 }),
    ]);
    if (!overviewTicket.current(t)) return;
    renderTopProjects(projects.projects || []);
    renderFresh(fresh.records || []);
  } catch (e) {
    if (!overviewTicket.current(t)) return;
    const failed = `<div class="state state-error">${tr("Не загрузилось: {e}", { e: esc(e.message) })}</div>`;
    $("top-projects").innerHTML = failed;
    $("fresh").innerHTML = failed;
  }
}

function renderTopProjects(projects) {
  const max = Math.max(1, ...projects.map((p) => p.records));
  $("top-projects").innerHTML = projects.length ? projects.map((p) =>
    `<button class="source-line" data-project="${esc(p.project)}" type="button" title="${esc(p.project)}">` +
    `<span class="source-name">${esc(p.name)}</span>` +
    `<span class="source-track"><i style="width:${(p.records / max * 100).toFixed(1)}%;background:var(--accent-strong)"></i></span>` +
    `<b>${number(p.records)}</b></button>`
  ).join("") : `<div class="state">${tr("Проекты появятся после первой синхронизации.")}</div>`;
}

function renderFresh(items) {
  $("fresh").innerHTML = items.length ? items.map((r) =>
    `<button class="preview-row" data-id="${r.id}" type="button"><i class="dot" style="background:${sourceColor(r.source)}"></i>` +
    `<span><strong>${esc(r.title)}</strong>` +
    `<small>${esc(sourceName(r.source))}${r.project && r.project !== origin(r.source) ? " · " + esc(leaf(r.project)) : ""} · ${esc(ago(r.ts))}</small></span>` +
    `<span class="preview-arrow">↗</span></button>`
  ).join("") : `<div class="state">${tr("Записей пока нет.")}</div>`;
}

async function loadTimeline() {
  const t = timelineTicket();
  try {
    const data = await api("/api/timeline", { weeks: timelineWeeks });
    if (!timelineTicket.current(t)) return;
    timeline = data;
    const total = Number(data.total) || 0;
    $("timeline-sub").textContent = tr("{records} по неделям, по дате последней активности", { records: records(total) });
    renderLegend();
    renderTimeline();
    renderTimelineTable();
  } catch (e) {
    if (!timelineTicket.current(t)) return;
    timeline = null;
    $("timeline-sub").textContent = "";
    $("timeline").innerHTML = `<div class="state state-error">${tr("График не загрузился: {e}", { e: esc(e.message) })}</div>`;
  }
}

function renderLegend() {
  if (!timeline) return;
  const sources = (timeline.sources || []).slice().sort((a, b) => sourceRank(a) - sourceRank(b));
  $("timeline-legend").innerHTML = sources.map((src) =>
    `<span><i style="background:${sourceColor(src)}"></i>${esc(sourceName(src))} <b>${number(timeline.by_source[src])}</b></span>`
  ).join("");
}

// Круглая ось: около четырёх интервалов, верхняя линия чуть выше пика.
function niceAxis(peak) {
  const rough = Math.max(1, peak) / 4;
  const pow = 10 ** Math.floor(Math.log10(rough));
  const unit = [1, 2, 2.5, 5, 10].map((k) => k * pow).find((v) => v >= rough && Number.isInteger(v)) || Math.ceil(rough);
  return { step: unit, max: Math.ceil(peak / unit) * unit };
}

// Недельные столбики стопкой: тонкие метки, зазор 2px между сегментами,
// скруглённый верх, приглушённая сетка, одна прямая подпись на пике. Тултип на столбец.
function renderTimeline() {
  const host = $("timeline");
  const data = timeline;
  if (!data || !data.weeks || !data.weeks.length) {
    host.innerHTML = `<div class="state">${tr("Данные появятся после первой синхронизации.")}</div>`;
    return;
  }
  const width = host.clientWidth || 800;
  const height = host.clientHeight || 235;
  const left = 34, right = 4, top = 18, bottom = 24;
  const plotW = width - left - right;
  const plotH = height - top - bottom;
  const weeks = data.weeks;
  const peak = Math.max(1, ...weeks.map((w) => w.total));
  const axis = niceAxis(peak);
  const yMax = axis.max;
  const step = plotW / weeks.length;
  const barW = Math.max(2, Math.min(52, step * 0.86));
  const sources = (data.sources || []).slice().sort((a, b) => sourceRank(a) - sourceRank(b));
  const scale = plotH / yMax;
  let svg = "";
  for (let value = 0; value <= yMax; value += axis.step) {
    const y = top + plotH - value * scale;
    svg += `<line class="tl-grid" x1="${left}" x2="${left + plotW}" y1="${y.toFixed(1)}" y2="${y.toFixed(1)}"/>` +
      `<text class="tl-axis" x="${left - 8}" y="${(y + 4).toFixed(1)}" text-anchor="end">${number(Math.round(value))}</text>`;
  }
  const labelEvery = Math.max(1, Math.ceil(64 / step));
  const peakIndex = weeks.findIndex((w) => w.total === peak);
  weeks.forEach((week, i) => {
    const center = left + step * i + step / 2;
    const x = center - barW / 2;
    let y = top + plotH;
    const present = sources.filter((src) => week.by_source[src]);
    let marks = "";
    present.forEach((src, at) => {
      const raw = week.by_source[src] * scale;
      const isTop = at === present.length - 1;
      const h = Math.max(1.5, raw - (isTop ? 0 : 2));
      y -= h;
      const radius = isTop ? Math.min(3, barW / 2, h) : 0;
      marks += radius
        ? `<path d="M${x.toFixed(1)} ${(y + h).toFixed(1)}V${(y + radius).toFixed(1)}q0 ${-radius} ${radius} ${-radius}h${(barW - 2 * radius).toFixed(1)}q${radius} 0 ${radius} ${radius}V${(y + h).toFixed(1)}Z" fill="${sourceColor(src)}"/>`
        : `<rect x="${x.toFixed(1)}" y="${y.toFixed(1)}" width="${barW.toFixed(1)}" height="${h.toFixed(1)}" fill="${sourceColor(src)}"/>`;
      if (!isTop) y -= 2;
    });
    svg += `<g class="tl-col" data-i="${i}"><rect class="tl-hit" x="${(left + step * i).toFixed(1)}" y="${top}" width="${step.toFixed(1)}" height="${plotH}"/>${marks}</g>`;
    if (i === peakIndex) svg += `<text class="tl-peak" x="${center.toFixed(1)}" y="${(y - 5).toFixed(1)}">${number(peak)}</text>`;
    if (i % labelEvery === 0) {
      svg += `<text class="tl-axis" x="${center.toFixed(1)}" y="${height - 6}" text-anchor="middle">${esc(weekLabel(week))}</text>`;
    }
  });
  host.innerHTML = `<svg viewBox="0 0 ${width} ${height}" preserveAspectRatio="none" aria-hidden="true">${svg}</svg><div id="timeline-tip" class="chart-tip" hidden></div>`;
}

function onTimelineMove(e) {
  const col = e.target.closest && e.target.closest(".tl-col");
  const tip = $("timeline-tip");
  if (!tip) return;
  if (!col || !timeline) {
    tip.hidden = true;
    return;
  }
  const week = timeline.weeks[Number(col.dataset.i)];
  const sources = (timeline.sources || []).filter((src) => week.by_source[src])
    .sort((a, b) => sourceRank(a) - sourceRank(b));
  tip.innerHTML = `<header>${esc(weekLabel(week))} — ${esc(weekLabel(week, true))}</header>` + sources.map((src) =>
    `<div><i style="background:${sourceColor(src)}"></i><b>${number(week.by_source[src])}</b><span>${esc(sourceName(src))}</span></div>`
  ).join("") + `<div class="tip-total"><i></i><b>${number(week.total)}</b><span>${tr("всего")}</span></div>`;
  tip.hidden = false;
  const host = $("timeline").getBoundingClientRect();
  let x = e.clientX - host.left + 14;
  if (x + tip.offsetWidth > host.width) x = e.clientX - host.left - tip.offsetWidth - 14;
  tip.style.left = `${Math.max(0, x)}px`;
  tip.style.top = `${Math.max(0, e.clientY - host.top - 30)}px`;
}

function renderTimelineTable() {
  if (!timeline) return;
  const sources = (timeline.sources || []).slice().sort((a, b) => sourceRank(a) - sourceRank(b));
  $("timeline-table").innerHTML = `<table class="table"><thead><tr><th>${tr("Неделя")}</th>` +
    sources.map((src) => `<th>${esc(sourceName(src))}</th>`).join("") + `<th>${tr("Всего")}</th></tr></thead><tbody>` +
    timeline.weeks.filter((w) => w.total).map((w) =>
      `<tr><td>${esc(weekLabel(w))} — ${esc(weekLabel(w, true))}</td>` +
      sources.map((src) => `<td>${number(w.by_source[src] || 0)}</td>`).join("") +
      `<td>${number(w.total)}</td></tr>`
    ).join("") + `</tbody></table>`;
}

// -- проекты -----------------------------------------------------------------

let projectsData = [];
let projectsSort = "records";
const projectsTicket = latest();
let projectsLoadError = "";

async function loadProjects() {
  const t = projectsTicket();
  if (!projectsData.length) $("projects-grid").innerHTML = `<div class="state">${tr("Загрузка…")}</div>`;
  try {
    const data = await api("/api/projects", { limit: PROJECTS_LIMIT });
    if (!projectsTicket.current(t)) return;
    projectsData = data.projects || [];
    projectsLoadError = "";
    renderProjects();
  } catch (e) {
    if (!projectsTicket.current(t)) return;
    projectsLoadError = e.message;
    $("projects-grid").innerHTML = `<div class="state state-error">${tr("Проекты не загрузились: {e}", { e: esc(e.message) })}</div>`;
  }
}

function renderProjects() {
  const needle = $("projects-filter").value.trim().toLowerCase();
  const list = projectsData
    .filter((p) => !needle || p.project.toLowerCase().includes(needle) ||
      (p.keywords || []).some((word) => word.toLowerCase().includes(needle)))
    .slice()
    .sort((a, b) => (b[projectsSort] - a[projectsSort]) || (b.records - a.records));
  const active = projectsData.filter((p) => p.last_ts >= Date.now() / 1000 - 30 * 86400).length;
  $("projects-lede").textContent = projectsData.length
    ? tr("{n} {unit}, {active} живых за 30 дней. Разговор относится к проекту своей рабочей папки, а разговор из корня рабочей области — к репозиторию, о котором в нём речь.",
      { n: number(projectsData.length), unit: plural(projectsData.length, "проект", "проекта", "проектов"), active: number(active) })
    : "";
  $("projects-grid").innerHTML = list.length ? list.map(projectCard).join("")
    : `<div class="state${projectsLoadError && !projectsData.length ? " state-error" : ""}">${
      projectsData.length ? tr("Ничего не нашлось.")
        : projectsLoadError ? tr("Проекты не загрузились: {e}", { e: esc(projectsLoadError) })
        : tr("Проекты появятся после первой синхронизации.")}</div>`;
  dropSettledProposals();
  renderProjectProposals();
}

function projectCard(p) {
  const peak = Math.max(1, ...p.spark);
  const spark = p.spark.map((n) =>
    n ? `<i style="height:${Math.max(3, n / peak * 18).toFixed(0)}px"></i>` : `<i class="empty"></i>`
  ).join("");
  const mix = Object.entries(p.sources || {}).sort((a, b) => sourceRank(a[0]) - sourceRank(b[0]));
  const total = mix.reduce((sum, [, n]) => sum + n, 0) || 1;
  const bar = mix.map(([src, n]) =>
    `<i style="width:${(n / total * 100).toFixed(2)}%;background:${sourceColor(src)}" title="${esc(sourceName(src))}: ${number(n)}"></i>`
  ).join("");
  const words = (p.keywords || []).map((word) => `<span class="word-chip">${esc(word)}</span>`).join("");
  return `<button class="project-chip" data-project="${esc(p.project)}" type="button">` +
    `<span class="project-top"><i class="dot" style="background:${sourceColor(p.source)}"></i><strong>${esc(p.name)}</strong><b>${number(p.records)}</b></span>` +
    `<span class="project-path">${esc(p.path)}</span>` +
    `<span class="project-meta"><span>${tr("последняя {date}", { date: esc(shortDate(p.last_ts)) })}</span><span class="spark" aria-hidden="true">${spark}</span></span>` +
    `<span class="source-mix">${bar}</span>` +
    (words ? `<span class="word-chips">${words}</span>` : "") + `</button>`;
}

function openProject(project) {
  Object.assign(state.records, { project, source: "", q: "" });
  $("records-q").value = "";
  if (state.page === "records") loadRecords();
  else go("records");
}

// -- названия проектов -------------------------------------------------------

// Ответ модели стоит денег и времени, поэтому переживает перезапуск окна.
const PROPOSALS_KEY = "projects.proposals";
const savedProposals = loadPref(PROPOSALS_KEY, []);
let projectProposals = Array.isArray(savedProposals) ? savedProposals : [];
let projectNotice = "";
let projectProposed = false;
const proposalsTicket = latest();

function setProjectProposals(list) {
  projectProposals = list;
  savePref(PROPOSALS_KEY, list);
}

// Сохранённое предложение могло устареть: имя уже поставили, пока окно было закрыто.
function dropSettledProposals() {
  const open = projectProposals.filter((proposal) => !projectsData.some((project) =>
    project.key === proposal.key && project.name === proposal.name &&
    (!proposal.path || project.path === proposal.path)));
  if (open.length !== projectProposals.length) setProjectProposals(open);
}

function renderProjectProposals() {
  const host = $("projects-proposals");
  const overridden = projectsData.filter((project) => project.overridden);
  if (!projectProposals.length && !overridden.length && !projectNotice) {
    host.hidden = true;
    host.innerHTML = "";
    return;
  }
  const parts = [];
  if (projectNotice) parts.push(`<p class="form-error">${esc(projectNotice)}</p>`);
  if (projectProposals.length) {
    parts.push(`<div class="surface-head"><h3 class="section-title">${tr("Предложения модели")}</h3>` +
      `<span class="subtle">${tr("Проверьте и примените по одному")}</span></div><div class="proposal-list">` +
      projectProposals.map((proposal, i) =>
        `<div class="proposal-row"><p class="proposal-change">${tr("Было: {from}", { from: esc(proposal.key) })} → ` +
        `${tr("стало: {to}", { to: esc(proposal.name) })}</p>` +
        (proposal.path ? `<code class="project-path">${esc(proposal.path)}</code>` : "") +
        (proposal.evidence ? `<p class="subtle">${esc(proposal.evidence)}</p>` : "") +
        `<div class="proposal-actions"><button class="btn" data-proposal-apply="${i}" type="button">${tr("Применить")}</button>` +
        `<button class="sync-button" data-proposal-skip="${i}" type="button">${tr("Пропустить")}</button></div></div>`).join("") +
      `</div>`);
  } else if (!projectNotice && projectProposed) {
    parts.push(`<p class="subtle">${tr("Модель не предложила изменений.")}</p>`);
  }
  if (overridden.length) {
    parts.push(`<div class="surface-head"><h3 class="section-title">${tr("Переименованные")}</h3></div>` +
      `<div class="renamed-list">` + overridden.map((project) =>
        `<div class="renamed-row"><span><b>${esc(project.name)}</b> <code class="project-path">${esc(project.key)}</code></span>` +
        `<button class="sync-button" data-project-reset="${esc(project.project)}" type="button">${tr("Сбросить")}</button></div>`).join("") +
      `</div>`);
  }
  host.hidden = false;
  host.innerHTML = parts.join("");
}

async function proposeProjects() {
  const button = $("projects-propose");
  const t = proposalsTicket();
  button.disabled = true;
  button.textContent = tr("Спрашиваю модель…");
  try {
    const data = await postJson("/api/projects/propose", { limit: 10 });
    if (!proposalsTicket.current(t)) return;
    setProjectProposals(data.proposals || []);
    projectNotice = "";
    projectProposed = true;
    renderProjectProposals();
    if (!projectProposals.length) toast(tr("Модель не предложила изменений."));
  } catch (e) {
    if (!proposalsTicket.current(t)) return;
    projectNotice = tr("Предложения не готовы: {e}", { e: e.message });
    renderProjectProposals();
    toast(projectNotice, 8000);
  } finally {
    button.disabled = false;
    button.textContent = tr("Актуализировать названия");
  }
}

async function applyProjectProposal(index) {
  const proposal = projectProposals[index];
  if (!proposal) return;
  try {
    await postJson("/api/projects/apply", {
      key: proposal.key, name: proposal.name, path: proposal.path || "",
    });
    setProjectProposals(projectProposals.filter((item) => item.key !== proposal.key));
    projectProposed = projectProposals.length > 0;
    projectNotice = "";
    toast(tr("Название проекта обновлено"));
    renderProjectProposals();
    await loadProjects();
  } catch (e) {
    projectNotice = tr("Не применилось: {e}", { e: e.message });
    renderProjectProposals();
    toast(projectNotice, 8000);
  }
}

async function resetProjectName(project) {
  try {
    await postJson("/api/projects/reset", { project });
    projectNotice = "";
    toast(tr("Название сброшено"));
    await loadProjects();
  } catch (e) {
    projectNotice = tr("Не сбросилось: {e}", { e: e.message });
    renderProjectProposals();
    toast(projectNotice, 8000);
  }
}

function onProjectsProposalClick(e) {
  const apply = e.target.closest("[data-proposal-apply]");
  if (apply) {
    applyProjectProposal(Number(apply.dataset.proposalApply));
    return;
  }
  const skip = e.target.closest("[data-proposal-skip]");
  if (skip) {
    const skipped = Number(skip.dataset.proposalSkip);
    setProjectProposals(projectProposals.filter((_, i) => i !== skipped));
    projectProposed = projectProposals.length > 0;
    renderProjectProposals();
    return;
  }
  const reset = e.target.closest("[data-project-reset]");
  if (reset) resetProjectName(reset.dataset.projectReset);
}

// -- записи ------------------------------------------------------------------

const recordsTicket = latest();

function recordsFilterParams() {
  const { source, project } = state.records;
  return { source: source || undefined, project: project || undefined };
}

async function loadRecords(append = false) {
  const rec = state.records;
  const t = recordsTicket();
  rec.loading = true;
  if (!append) {
    rec.items = [];
    $("records-list").innerHTML = `<div class="state">${rec.q ? tr("Ищу…") : tr("Загрузка…")}</div>`;
  }
  renderRecordFilters();
  try {
    if (rec.q) {
      const data = await api("/api/search", { q: rec.q, limit: SEARCH_LIMIT, ...recordsFilterParams() });
      if (!recordsTicket.current(t)) return;
      rec.items = data.cards || [];
      rec.total = rec.items.length;
    } else {
      const data = await api("/api/records", {
        offset: append ? rec.items.length : 0, limit: RECORDS_PAGE, ...recordsFilterParams(),
      });
      if (!recordsTicket.current(t)) return;
      const page = data.records || [];
      if (append) {
        // Синхронизация между страницами сдвигает offset: запись, всплывшая по
        // last_ts, пришла бы второй раз.
        const shown = new Set(rec.items.map((item) => item.id));
        rec.items = rec.items.concat(page.filter((item) => !shown.has(item.id)));
      } else {
        rec.items = page;
      }
      rec.total = Number(data.total) || rec.items.length;
    }
    rec.loading = false;
    renderRecords();
  } catch (e) {
    if (!recordsTicket.current(t)) return;
    rec.loading = false;
    if (append) {
      toast(tr("Не удалось загрузить ещё: {e}", { e: e.message }));
      return;
    }
    $("records-list").innerHTML = `<div class="state state-error">${tr("Ошибка загрузки: {e}", { e: esc(e.message) })}</div>`;
  }
}

function renderRecordFilters() {
  const rec = state.records;
  const rows = (state.stats && state.stats.sources) || [];
  $("records-sources").innerHTML = rows.length
    ? `<button class="chip" data-source="" type="button" aria-pressed="${!rec.source}">${tr("Все источники")}</button>` + rows.map((row) =>
      `<button class="chip" data-source="${esc(row.source)}" type="button" aria-pressed="${rec.source === row.source}">` +
      `<i class="dot" style="background:${sourceColor(row.source)}"></i>${esc(sourceName(row.source))}<b>${number(row.records)}</b></button>`
    ).join("")
    : "";
  $("records-clear").hidden = !(rec.q || rec.source || rec.project);
}

function recordRow(r) {
  const meta = [];
  if (r.project && r.project !== origin(r.source)) meta.push(`<span>${esc(leaf(r.project))}</span>`);
  if (Number(r.fragments) > 1) {
    meta.push(`<span>${number(r.fragments)} ${plural(Number(r.fragments), "фрагмент", "фрагмента", "фрагментов")}</span>`);
  }
  if (r.has_card === false) meta.push(`<span class="tag-wait">${tr("пересказа пока нет")}</span>`);
  // Осиротевшая ссылка unit→card: карточка не читается — строка без кликабельного id.
  const orphan = r.id == null;
  if (orphan) meta.push(`<span class="tag-wait">${tr("карточка недоступна")}</span>`);
  const selected = state.selectedRecord === r.id ? " selected" : "";
  const tag = orphan ? "div" : "button";
  const attrs = orphan ? "" : ` data-id="${r.id}" type="button"`;
  return `<${tag} class="record-row${selected}${orphan ? " is-orphan" : ""}"${attrs}>` +
    `<span class="row-top"><span class="src" style="color:${sourceColor(r.source)}"><i class="dot" style="background:${sourceColor(r.source)}"></i>${esc(sourceName(r.source))}</span>${meta.join("")}` +
    `<time class="row-date">${esc(shortDate(r.ts))}, ${esc(clock(r.ts))}</time></span>` +
    `<strong class="row-title">${esc(r.title)}</strong>` +
    (r.snippet ? `<span class="row-snippet">${esc(r.snippet)}</span>` : "") + `</${tag}>`;
}

function renderRecords() {
  const rec = state.records;
  const scope = [
    rec.project ? tr("проект {name}", { name: leaf(rec.project) }) : "",
    rec.source ? sourceName(rec.source) : "",
  ].filter(Boolean).join(", ");
  const count = rec.q ? rec.items.length : rec.total;
  $("records-lede").textContent = rec.q
    ? `${tr("Поиск: «{q}»", { q: rec.q })}${scope ? ` · ${scope}` : ""} · ${tr("найдено {n}", { n: number(count) })}`
    : `${scope || tr("Все записи")} · ${records(count)}`;
  if (!rec.items.length) {
    $("records-list").innerHTML = !rec.q && !rec.total
      ? emptyStateHtml()
      : `<div class="state">${rec.q ? tr("По запросу «{q}» ничего не нашлось.", { q: esc(rec.q) }) : tr("Записей пока нет.")}</div>`;
    return;
  }
  const rest = rec.total - rec.items.length;
  $("records-list").innerHTML = rec.items.map(recordRow).join("") + (!rec.q && rest > 0
    ? `<button id="records-more" class="records-more" type="button">${tr("Показать ещё · осталось {n}", { n: number(rest) })}</button>`
    : "");
}

// -- панель записи -----------------------------------------------------------

const panelTicket = latest();
let panelReturnFocus = null;

async function openRecord(id) {
  const t = panelTicket();
  state.selectedRecord = id;
  if ($("panel").hidden) panelReturnFocus = document.activeElement;
  $("panel").hidden = false;
  $("scrim").hidden = false;
  markSelectedRow();
  $("panel").innerHTML = `<div class="state">${tr("Загрузка записи…")}</div>`;
  try {
    const card = await api("/api/card", { id });
    if (!panelTicket.current(t)) return;
    renderPanel(card);
    $("panel").scrollTop = 0;
    $("panel").focus({ preventScroll: true });
  } catch (e) {
    if (!panelTicket.current(t)) return;
    $("panel").innerHTML = `<div class="panel-top"><span></span><button class="btn" data-action="close" type="button">${tr("Закрыть")}</button></div>` +
      `<div class="state state-error">${tr("Запись не открылась: {e}", { e: esc(e.message) })}</div>`;
  }
}

function closePanel() {
  panelTicket();
  state.selectedRecord = null;
  $("panel").hidden = true;
  $("scrim").hidden = true;
  markSelectedRow();
  if (panelReturnFocus && panelReturnFocus.isConnected && panelReturnFocus.focus) panelReturnFocus.focus();
  panelReturnFocus = null;
}

function markSelectedRow() {
  document.querySelectorAll(".record-row").forEach((row) =>
    row.classList.toggle("selected", Number(row.dataset.id) === state.selectedRecord));
}

// Текст карточки простой, но модели и старые карточки оставляют в нём лёгкий markdown.
// Сначала экранируем, потом добавляем только эти теги.
function richText(text) {
  const inline = (line) => esc(line)
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/`([^`]+)`/g, "<code>$1</code>");
  const out = [];
  let list = false;
  for (const raw of String(text || "").split("\n")) {
    const line = raw.trim();
    const item = /^[-*•]\s+(.*)$/.exec(line);
    if (list && !item) {
      out.push("</ul>");
      list = false;
    }
    if (item) {
      if (!list) out.push("<ul>");
      list = true;
      out.push(`<li>${inline(item[1])}</li>`);
    } else if (/^#{1,6}\s/.test(line)) {
      out.push(`<h4>${inline(line.replace(/^#+\s*/, ""))}</h4>`);
    } else if (line) {
      out.push(`<p>${inline(line)}</p>`);
    }
  }
  if (list) out.push("</ul>");
  return out.join("");
}

function panelSection(title, html, hint) {
  return html ? `<section class="panel-section"><h3${hint ? ` title="${esc(hint)}"` : ""}>${esc(title)}</h3>${html}</section>` : "";
}

function renderPanel(card) {
  const meta = card.record;
  const facts = [];
  if (meta) {
    facts.push([tr("Начат"), esc(fullDate(meta.started_ts))]);
    facts.push([tr("Последнее сообщение"), esc(fullDate(meta.last_ts))]);
    if (meta.project !== origin(card.source)) {
      facts.push([tr("Проект"), `<button class="text-button" data-project="${esc(meta.project)}" type="button">${esc(meta.project_name)}</button> <span class="path">${esc(meta.project_path)}</span>`]);
    }
    if (meta.file) facts.push([tr("Файл"), `<span class="path">${esc(meta.file)}</span>`]);
    if (meta.fragments > 1) facts.push([tr("Объём"), `${number(meta.fragments)} ${plural(meta.fragments, "фрагмент", "фрагмента", "фрагментов")}`]);
  } else {
    facts.push([tr("Дата"), esc(fullDate(card.ts))]);
  }
  const summary = card.summary || (card.kind === "distilled" ? card.body : "");
  const atoms = (card.atoms || []).map((a) =>
    `<div class="atom"><span class="atom-kind is-${esc(a.type)}">${esc(tr(a.type))}</span><span class="atom-text${a.type === "команда" ? " is-command" : ""}">${esc(a.text)}</span></div>`
  ).join("");
  const similar = (card.similar || []).map((s) =>
    `<button class="similar-row" data-open="${s.id}" type="button"><i class="dot" style="background:${sourceColor(s.source)}"></i><span>${esc(s.title)}</span><small>${esc(leaf(s.project))}</small></button>`
  ).join("");
  const questions = (card.questions || []).map((q) => `<div class="question">${esc(q)}</div>`).join("");
  const words = (card.keywords || []).map((w) => `<span class="word-chip">${esc(w)}</span>`).join("");
  const fragments = (card.fragments || []).map((f) =>
    `<button class="fragment" data-open="${f.id}" type="button"><span>${esc(f.title || "#" + f.id)}</span><small>${esc(f.date)}</small></button>`
  ).join("");
  const transcript = card.transcript || card.body || "";
  const chars = Number(card.transcript_chars) || transcript.length;
  const cut = chars > transcript.length ? ` ${tr("Показано {n}.", { n: number(transcript.length) })}` : "";
  $("panel").innerHTML =
    `<div class="panel-top"><span class="panel-eyebrow">${esc(sourceName(card.source))}, id ${card.id}</span>` +
    `<button class="btn" data-action="close" type="button">${tr("Закрыть")}</button></div>` +
    `<h2 class="panel-title">${esc(card.title)}</h2>` +
    `<dl class="facts">${facts.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${v}</dd>`).join("")}</dl>` +
    `<div class="panel-actions">` +
    (meta ? `<a class="btn" href="#graph/${meta.node_id}">${tr("Показать на графе")}</a>` : "") +
    `<button class="btn" data-action="copy" type="button">${tr("Копировать")}</button>` +
    (card.unit_card ? `<button class="btn" data-open="${card.unit_card.id}" type="button">${tr("К карточке записи")}</button>` : "") +
    `</div>` +
    (summary ? "" : `<p class="notice">${tr("Пересказа пока нет: запись ждёт очередь или локальную модель. Ниже исходный текст.")}</p>`) +
    panelSection(tr("Кратко"), summary ? `<div class="panel-summary rich">${richText(summary)}</div>` : "") +
    panelSection(tr("Похожие записи"), similar) +
    panelSection(tr("Атомы"), atoms, tr("Атомы — отдельные факты, решения, ошибки и команды, извлечённые из разговора.")) +
    panelSection(tr("Как это будет искать"), questions) +
    panelSection(tr("Ключевые слова"), words ? `<div class="word-chips">${words}</div>` : "") +
    (fragments ? `<section class="panel-section"><details class="fold"><summary>${tr("Исходные фрагменты")} · ${number(card.fragments.length)}</summary><div>${fragments}</div></details></section>` : "") +
    `<section class="panel-section"><details class="fold"${summary ? "" : " open"}><summary>${tr("Расшифровка")} · ${number(chars)} ${plural(chars, "символ", "символа", "символов")}.${cut}</summary><pre class="transcript">${esc(transcript)}</pre></details></section>`;
  $("panel")._card = card;
}

// Вставленное вне приложения тело теряет, что это и откуда.
function cardClipboardText(card) {
  const meta = card.record;
  const origin = [sourceName(card.source), fullDate(meta ? meta.last_ts : card.ts), meta && meta.project_name]
    .filter(Boolean).join(" · ");
  const head = [`# ${card.title}`, origin, (meta && meta.file) || card.source_ref].filter(Boolean);
  return `${head.join("\n")}\n\n${card.body || ""}`;
}

async function onPanelClick(e) {
  const actionEl = e.target.closest("[data-action]");
  if (actionEl && actionEl.dataset.action === "close") {
    closePanel();
    return;
  }
  if (actionEl && actionEl.dataset.action === "copy") {
    const card = $("panel")._card;
    try {
      await navigator.clipboard.writeText(card ? cardClipboardText(card) : "");
      toast(tr("Скопировано"));
    } catch {
      toast(tr("Буфер обмена недоступен"));
    }
    return;
  }
  const open = e.target.closest("[data-open]");
  if (open) {
    openRecord(Number(open.dataset.open));
    return;
  }
  const project = e.target.closest("[data-project]");
  if (project) {
    closePanel();
    openProject(project.dataset.project);
    return;
  }
  // Ссылка на граф меняет hash, пока панель висит над старой страницей.
  if (e.target.closest('a[href^="#graph/"]')) closePanel();
}

// -- граф --------------------------------------------------------------------
//
// Каждая запись — точка, каждый проект — колечко (hub), на котором висят его
// записи, как в референсе: force-раскладка, где общие темы тянут записи
// друг к другу поверх проектов. Симуляция доходит до покоя один раз, синхронно,
// из заданного зерна, поэтому одни и те же данные дают одну картину при каждом
// визите и в простое ничего не шевелится. Переходы камеры и акцента идут в одном
// animation frame в event loop браузера; в простое и в скрытых вьюхах всё стоит.

const GRAPH_MIN_SCALE = 0.08;
const GRAPH_MAX_SCALE = 8;
const GRAPH_FOCUS_MAX_SCALE = 2.6;
const GRAPH_FOCUS_MS = 380;
// Замер по референсу (03:23.9-03:24.5, 25 fps): выбор уводит точку
// в середину свободной области и даёт ~1.36x по одной ease-out
// кривой за ~0.55 с; фон гаснет в пределах двух кадров.
const GRAPH_SELECT_MS = 560;
const GRAPH_SELECT_ZOOM = 1.4;
const GRAPH_FRAME_MIN_SPAN = 90; // мировые единицы: минимум, который занимает вписанное
const GRAPH_EMPHASIS_MS = 120;
// Зум колесом вокруг указателя, по референсу на 02:20: ~1.65x за
// ~360 мс на ease-out, то есть два щелчка по 100 px здесь.
const GRAPH_WHEEL_MS = 360;
const GRAPH_WHEEL_RATE = 1.0025;
// Перетаскивание точки разогревает раскладку, как alphaTarget в d3; соседи и
// hub проекта идут за ней на своих пружинах, потом карта остывает за ~2 с.
const GRAPH_DRAG_HEAT = 0.3;
const GRAPH_COOL = 0.0228;
const GRAPH_HEAT_MIN = 0.005;
const GRAPH_HIT_PX = 7;
const GRAPH_TICKS = 300;
const GRAPH_DIM = 0.18;
const HUB_PX = 4.5;
const CELL = 30; // запись толкает записи в пределах двух ячеек сетки
const RECORD_CHARGE = 30;
const HUB_REACH = 24; // толчок hub кончается у края его собственного облака
const HUB_REACH_STEP = 8;
const LABEL_LIMIT = 46;

const graph = {
  data: null, sim: null, ctx: null, size: { w: 0, h: 0 },
  view: { scale: 1, tx: 0, ty: 0 },
  hover: null, selected: null, onlyLinked: false, source: "", days: 0,
  layers: loadPref("graph-layers", { labels: true, rings: true, edges: true }),
  drag: null, nodeDrag: null, suppressClick: false, camera: null, framePending: false, frameId: null, pendingFocus: null,
  returnView: null,
  labelEls: [], signature: "", visualKey: null, emphasisStart: 0, keep: null,
};
const graphTicket = latest();
const graphCardTicket = latest();

function graphPause() {
  graph.camera = null;
  if (graph.framePending) cancelAnimationFrame(graph.frameId);
  graph.framePending = false;
  graph.hover = null;
  graph.drag = null;
  graph.nodeDrag = null;
  graph.sim?.forces.release();
  $("graph-tip").hidden = true;
}

async function loadGraph(force = false) {
  const t = graphTicket();
  if (!graph.sim || force) $("graph-state").textContent = tr("Строю граф…");
  $("graph-state").classList.remove("is-error");
  try {
    const data = await api("/api/landscape", { days: graph.days || undefined });
    if (!graphTicket.current(t)) return;
    graph.data = data;
    renderGraph();
  } catch (e) {
    if (!graphTicket.current(t)) return;
    $("graph-state").textContent = tr("Граф не построился: {e}", { e: e.message });
    $("graph-state").classList.add("is-error");
  }
}

function renderGraph() {
  const data = graph.data;
  const nodes = data.nodes || [];
  const edges = data.edges || [];
  const projects = data.projects || [];
  $("graph-lede").textContent = tr("{records}, {projects} {pu}, {edges} {eu} по темам. Кольца — проекты, точки — записи, линии — общие темы. Выбери точку, и останутся видны только её связи.", {
    records: records(nodes.length),
    projects: number(projects.length), pu: plural(projects.length, "проект", "проекта", "проектов"),
    edges: number(edges.length), eu: plural(edges.length, "связь", "связи", "связей"),
  });
  renderGraphSources(nodes);
  if (!nodes.length) {
    graphPause();
    graph.returnView = null;
    graphClearSelection();
    graph.sim = null;
    graph.signature = "";
    clearGraphCanvas();
    $("graph-state").textContent = graph.days ? tr("За этот период записей нет.") : tr("Записей пока нет.");
    return;
  }
  $("graph-state").textContent = "";
  const signature = JSON.stringify([nodes, edges, graph.days]);
  sizeGraphCanvas();
  if (graph.sim && graph.signature === signature) {
    requestGraphDraw();
  } else {
    graph.signature = signature;
    graph.sim = layoutGraph(nodes, edges);
    graph.visualKey = null;
    graph.selected = graph.sim.byId.has(graph.selected) ? graph.selected : null;
    if (graph.selected == null) hideGraphCard();
    graphFit(false);
  }
  exposeGraphProbe();
  if (graph.pendingFocus != null) {
    const id = graph.pendingFocus;
    graph.pendingFocus = null;
    graphSelect(id);
  }
}

function renderGraphSources(nodes) {
  const counts = new Map();
  for (const n of nodes) counts.set(n.source, (counts.get(n.source) || 0) + 1);
  // Фильтр переживает смену данных: иначе при другом периоде карта гаснет вся,
  // а ни один чип не нажат и причина не видна (выбор сбрасывается так же ниже).
  if (graph.source && !counts.has(graph.source)) graph.source = "";
  const rows = [...counts.entries()].sort((a, b) => b[1] - a[1]);
  $("graph-sources").innerHTML = rows.map(([src, n]) =>
    `<button class="chip" data-source="${esc(src)}" type="button" aria-pressed="${graph.source === src}">` +
    `<i class="dot" style="background:${sourceColor(src)}"></i>${esc(sourceName(src))}<b>${number(n)}</b></button>`
  ).join("");
}

function layoutGraph(nodes, edges) {
  const groups = new Map();
  for (const node of nodes) {
    if (!groups.has(node.project)) groups.set(node.project, { project: node.project, nodes: [], x: 0, y: 0, r: 0 });
    groups.get(node.project).nodes.push(node);
  }
  const list = [...groups.values()].sort((a, b) => b.nodes.length - a.nodes.length || (a.project < b.project ? -1 : 1));
  // Запись, попавшая под свой же источник ("codex" от Codex), остаётся без проекта:
  // она висит свободно, а не на hub.
  const hubs = list.filter((g) => g.nodes.some((n) => leaf(g.project).toLowerCase() !== origin(n.source).toLowerCase()));
  const byId = new Map();
  const simNodes = [];
  let free = 0;
  hubs.forEach((hub, i) => {
    Object.assign(hub, {
      id: `p:${hub.project}`, name: leaf(hub.project), r: 0, alpha: 0,
      ...seedAt(0, 0, 14, i), charge: 60 * Math.sqrt(hub.nodes.length),
    });
    byId.set(hub.id, hub);
  });
  for (const group of list) {
    const hub = byId.get(`p:${group.project}`);
    const inside = group.nodes.slice().sort((a, b) => b.ts - a.ts || a.id - b.id);
    inside.forEach((node, i) => {
      const sim = {
        id: node.id, title: node.title || "", source: node.source, project: node.project, ts: node.ts,
        hasCard: node.has_card, hub: hub || null, alpha: 0,
        ...(hub ? seedAt(hub.x, hub.y, 3, i) : seedAt(0, 0, 14, hubs.length + free++)),
      };
      byId.set(sim.id, sim);
      simNodes.push(sim);
    });
  }
  const adj = new Map(simNodes.map((n) => [n.id, []]));
  for (const hub of hubs) adj.set(hub.id, []);
  const simEdges = [];
  const springs = [];
  for (const n of simNodes) {
    if (!n.hub) continue;
    adj.get(n.hub.id).push({ id: n.id, w: 1 });
    springs.push({ a: n.hub, b: n, len: 24, k: 0.35 });
  }
  for (const [a, b, w] of edges) {
    const na = byId.get(a);
    const nb = byId.get(b);
    if (!na || !nb) continue;
    simEdges.push({ a: na, b: nb, w, alpha: 0 });
    springs.push({ a: na, b: nb, len: 40, k: 0.04 + 0.2 * w });
    adj.get(a).push({ id: b, w });
    adj.get(b).push({ id: a, w });
  }
  const forces = createForces(hubs, simNodes, springs);
  const decay = 1 - 0.001 ** (1 / GRAPH_TICKS);
  for (let tick = 0, alpha = 1; tick < GRAPH_TICKS; tick++) forces.tick(alpha -= alpha * decay);
  forces.sync();
  for (const n of simNodes) {
    if (n.hub) n.hub.r = Math.max(n.hub.r, Math.hypot(n.x - n.hub.x, n.y - n.hub.y));
  }
  return { nodes: simNodes, edges: simEdges, byId, adj, groups: hubs, forces, heat: 0 };
}

// Засев «подсолнухом» делает старт детерминированным и разнесённым, поэтому
// симуляции не приходится растаскивать совпавшие тела.
function seedAt(cx, cy, step, i) {
  const radius = step * Math.sqrt(i + 0.5);
  const angle = i * 2.399963;
  return { x: cx + radius * Math.cos(angle), y: cy + radius * Math.sin(angle) };
}

// Velocity Verlet в манере d3-force: many-body отталкивание (записи
// через сетку с коротким радиусом; hub'ы толкают соседние hub'ы пропорционально
// размеру и тянут за собой свои записи), пружины со смещением по
// степени и центрирующая тяга, сильнее по вертикали, чтобы облако заполняло
// широкую сцену. Состояние в typed arrays, потому что внутренний цикл определяет
// первую отрисовку. Тики идут в event loop браузера: пачка при смене данных,
// затем по одному на кадр, пока перетаскиваемое тело прижато и карта
// остывает. Покой не зависит от alpha, поэтому разогрев двигает только
// то, что потревожил drag.
function createForces(hubs, dots, springs) {
  const bodies = [...hubs, ...dots];
  const n = bodies.length;
  const first = hubs.length;
  const index = new Map(bodies.map((b, i) => [b, i]));
  const x = Float64Array.from(bodies, (b) => b.x);
  const y = Float64Array.from(bodies, (b) => b.y);
  const vx = new Float64Array(n);
  const vy = new Float64Array(n);
  const sa = Int32Array.from(springs, (s) => index.get(s.a));
  const sb = Int32Array.from(springs, (s) => index.get(s.b));
  const degree = new Int32Array(n);
  for (let i = 0; i < springs.length; i++) { degree[sa[i]]++; degree[sb[i]]++; }
  const bias = Float64Array.from(springs, (s, i) => degree[sa[i]] / (degree[sa[i]] + degree[sb[i]]));
  const reach2 = (CELL * 2) ** 2;
  // Hub'ы толкают только пока облака не разошлись: дальше записи сами
  // держат их врозь, поэтому два hub'а без записей между ними не разъезжаются.
  const hubReach = Float64Array.from(hubs, (hub) => HUB_REACH + HUB_REACH_STEP * Math.sqrt(hub.nodes.length));
  const next = new Int32Array(n);
  let pinned = -1;
  let pinX = 0;
  let pinY = 0;
  function tick(alpha) {
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (let i = first; i < n; i++) {
      if (x[i] < minX) minX = x[i];
      if (x[i] > maxX) maxX = x[i];
      if (y[i] < minY) minY = y[i];
      if (y[i] > maxY) maxY = y[i];
    }
    const cols = Math.floor((maxX - minX) / CELL) + 1;
    const rows = Math.floor((maxY - minY) / CELL) + 1;
    const head = new Int32Array(cols * rows).fill(-1);
    for (let i = first; i < n; i++) {
      const c = Math.floor((x[i] - minX) / CELL) + Math.floor((y[i] - minY) / CELL) * cols;
      next[i] = head[c];
      head[c] = i;
    }
    // Каждая пара один раз: ячейка с собой и с 12 ячейками после себя.
    const push = RECORD_CHARGE * alpha;
    for (let c = 0; c < head.length; c++) {
      if (head[c] === -1) continue;
      const gx = c % cols;
      const gy = (c - gx) / cols;
      for (let oy = 0; oy <= 2; oy++) {
        for (let ox = -2; ox <= 2; ox++) {
          if (oy === 0 && ox < 0) continue;
          const cx = gx + ox;
          const cy = gy + oy;
          if (cx < 0 || cx >= cols || cy >= rows) continue;
          const same = ox === 0 && oy === 0;
          for (let i = head[c]; i !== -1; i = next[i]) {
            for (let j = same ? next[i] : head[cx + cy * cols]; j !== -1; j = next[j]) {
              const dx = x[j] - x[i];
              const dy = y[j] - y[i];
              const d2 = dx * dx + dy * dy;
              if (d2 > reach2) continue;
              const k = push / (d2 < 4 ? 4 : d2);
              vx[i] -= dx * k;
              vy[i] -= dy * k;
              vx[j] += dx * k;
              vy[j] += dy * k;
            }
          }
        }
      }
    }
    for (let h = 0; h < first; h++) {
      const k0 = hubs[h].charge * alpha;
      for (let j = 0; j < first; j++) {
        if (j === h) continue;
        const dx = x[j] - x[h];
        const dy = y[j] - y[h];
        const d2 = dx * dx + dy * dy;
        const r = hubReach[h] + hubReach[j];
        if (d2 > r * r) continue;
        const k = k0 / Math.max(d2, 4);
        vx[j] += dx * k;
        vy[j] += dy * k;
      }
    }
    for (let i = 0; i < springs.length; i++) {
      const a = sa[i];
      const b = sb[i];
      const dx = x[b] + vx[b] - x[a] - vx[a];
      const dy = y[b] + vy[b] - y[a] - vy[a];
      const d = Math.hypot(dx, dy) || 1;
      const l = ((d - springs[i].len) / d) * alpha * springs[i].k;
      vx[b] -= dx * l * bias[i];
      vy[b] -= dy * l * bias[i];
      vx[a] += dx * l * (1 - bias[i]);
      vy[a] += dy * l * (1 - bias[i]);
    }
    for (let i = 0; i < n; i++) {
      vx[i] = (vx[i] - x[i] * 0.012 * alpha) * 0.6;
      vy[i] = (vy[i] - y[i] * 0.04 * alpha) * 0.6;
      x[i] += vx[i];
      y[i] += vy[i];
    }
    if (pinned !== -1) {
      x[pinned] = pinX;
      y[pinned] = pinY;
      vx[pinned] = 0;
      vy[pinned] = 0;
    }
  }
  return {
    tick,
    sync() { bodies.forEach((b, i) => { b.x = x[i]; b.y = y[i]; }); },
    // Прижатое тело двигается сразу, поэтому отпускание до следующего кадра
    // всё равно оставляет его там, где бросили.
    pin(body, px, py) {
      pinned = index.get(body);
      x[pinned] = pinX = body.x = px;
      y[pinned] = pinY = body.y = py;
      vx[pinned] = 0;
      vy[pinned] = 0;
    },
    release() { pinned = -1; },
  };
}

function sizeGraphCanvas() {
  const stage = $("graph-stage");
  const w = stage.clientWidth || 800;
  const h = stage.clientHeight || 560;
  const dpr = Math.min(typeof devicePixelRatio === "number" ? devicePixelRatio : 1, 2);
  const canvas = $("graph-canvas");
  canvas.width = Math.max(1, Math.round(w * dpr));
  canvas.height = Math.max(1, Math.round(h * dpr));
  graph.ctx = canvas.getContext("2d");
  graph.size = { w, h, dpr };
}

function clearGraphCanvas() {
  if (graph.ctx) graph.ctx.clearRect(0, 0, $("graph-canvas").width, $("graph-canvas").height);
  $("graph-labels").innerHTML = "";
  graph.labelEls = [];
}

const toScreen = (x, y) => ({
  x: x * graph.view.scale + graph.view.tx,
  y: y * graph.view.scale + graph.view.ty,
});
const toWorld = (x, y) => ({
  x: (x - graph.view.tx) / graph.view.scale,
  y: (y - graph.view.ty) / graph.view.scale,
});

function worldBounds(items, pad = 0) {
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const item of items) {
    const r = (item.r || 0) + pad;
    minX = Math.min(minX, item.x - r);
    minY = Math.min(minY, item.y - r);
    maxX = Math.max(maxX, item.x + r);
    maxY = Math.max(maxY, item.y + r);
  }
  return { minX, minY, maxX, maxY };
}

function overviewView() {
  const b = worldBounds(graph.sim.nodes);
  const pad = 48; // по краям место для имени проекта, центрированного на hub у края
  const scale = Math.max(GRAPH_MIN_SCALE, Math.min(
    (graph.size.w - pad * 4) / Math.max(1, b.maxX - b.minX),
    (graph.size.h - pad * 2) / Math.max(1, b.maxY - b.minY),
    2.2,
  ));
  return {
    scale,
    tx: graph.size.w / 2 - ((b.minX + b.maxX) / 2) * scale,
    ty: graph.size.h / 2 - ((b.minY + b.maxY) / 2) * scale,
  };
}

function graphFit(animate = true) {
  if (!graph.sim) return;
  graph.returnView = null;
  const target = overviewView();
  if (animate) {
    animateView(target);
  } else {
    graph.camera = null;
    Object.assign(graph.view, target);
    requestGraphDraw();
  }
}

const graphMotion = typeof matchMedia === "function"
  ? matchMedia("(prefers-reduced-motion: reduce)") : null;
const motionOk = () => !graphMotion?.matches;
const easeOut = (t) => 1 - (1 - t) ** 3;

// `pin` — мировая точка, идущая к цели по прямой на экране, пока
// меняется масштаб; без неё зум держится под неподвижным указателем.
function animateView(target, duration = GRAPH_FOCUS_MS, pin = null) {
  if (!motionOk() || typeof requestAnimationFrame !== "function") {
    graph.camera = null;
    Object.assign(graph.view, target);
    requestGraphDraw();
    return;
  }
  graph.camera = { from: { ...graph.view }, target, start: performance.now(), duration, pin };
  requestGraphDraw();
}

function advanceGraphCamera(now) {
  const camera = graph.camera;
  if (!camera) return false;
  const { from, target, start, duration, pin } = camera;
  const t = motionOk() ? Math.min(1, Math.max(0, (now - start) / duration)) : 1;
  const k = easeOut(t);
  const scale = from.scale * (target.scale / from.scale) ** k;
  graph.view.scale = scale;
  if (pin) {
    const at = (view, axis, offset) => pin[axis] * view.scale + view[offset];
    graph.view.tx = at(from, "x", "tx") + (at(target, "x", "tx") - at(from, "x", "tx")) * k - pin.x * scale;
    graph.view.ty = at(from, "y", "ty") + (at(target, "y", "ty") - at(from, "y", "ty")) * k - pin.y * scale;
  } else {
    // Тот же прогресс масштаба берём для сдвига, чтобы зум оставался под указателем.
    const pan = Math.abs(target.scale - from.scale) > 1e-9
      ? (scale - from.scale) / (target.scale - from.scale) : k;
    graph.view.tx = from.tx + (target.tx - from.tx) * pan;
    graph.view.ty = from.ty + (target.ty - from.ty) * pan;
  }
  if (t === 1) {
    Object.assign(graph.view, target);
    graph.camera = null;
  }
  return t < 1;
}

// Как в референсе: точка скользит в середину области, которую оставляет
// карточка, пока камера чуть наезжает. Окружение не вписывается
// (его линии могут уйти со сцены), а более близкая камера держит масштаб.
function focusNode(id) {
  const node = graph.sim?.byId.get(id);
  if (!node) return;
  const cardW = $("graph-card").hidden ? 0 : Math.min(340, graph.size.w * 0.44) + 12;
  const availableW = graph.size.w - cardW;
  const current = (graph.camera?.target || graph.view).scale;
  const scale = Math.min(GRAPH_MAX_SCALE, Math.max(current, overviewView().scale * GRAPH_SELECT_ZOOM));
  animateView(
    { scale, tx: availableW / 2 - node.x * scale, ty: graph.size.h / 2 - node.y * scale },
    GRAPH_SELECT_MS, { x: node.x, y: node.y },
  );
}

// Один тик на кадр, пока перетаскиваемое тело прижато (жар держится) и пока
// карта остывает после отпускания; останавливается сама, как камера.
function advanceGraphForces() {
  const sim = graph.sim;
  if (!sim || (!graph.nodeDrag?.moved && sim.heat < GRAPH_HEAT_MIN)) return false;
  if (!graph.nodeDrag?.moved) sim.heat -= sim.heat * GRAPH_COOL;
  sim.forces.tick(sim.heat);
  sim.forces.sync();
  return Boolean(graph.nodeDrag?.moved) || sim.heat >= GRAPH_HEAT_MIN;
}

function requestGraphDraw() {
  if (graph.framePending || state.page !== "graph" || document.hidden) return;
  graph.framePending = true;
  graph.frameId = requestAnimationFrame((now) => {
    graph.framePending = false;
    if (state.page !== "graph" || document.hidden) return;
    const cameraMoving = advanceGraphCamera(now);
    const forcesMoving = advanceGraphForces();
    const emphasisMoving = advanceGraphEmphasis(now);
    drawGraph();
    if (cameraMoving || forcesMoving || emphasisMoving) requestGraphDraw();
  });
}

function visibleSet(id = graph.selected ?? graph.hover) {
  if (id == null || !graph.sim?.adj.has(id)) return null;
  const keep = new Set([id]);
  for (const nb of graph.sim.adj.get(id)) keep.add(nb.id);
  return keep;
}

function advanceGraphEmphasis(now) {
  const sim = graph.sim;
  if (!sim) return false;
  const focus = graph.selected ?? graph.hover;
  const key = `${focus}|${graph.hover}|${graph.source}|${graph.onlyLinked}|${graph.layers.rings}|${graph.layers.edges}`;
  if (key !== graph.visualKey) {
    graph.visualKey = key;
    graph.emphasisStart = performance.now();
    graph.keep = visibleSet(focus);
    const retarget = (item, alpha) => { item.fromAlpha = item.alpha || 0; item.targetAlpha = alpha; };
    for (const n of sim.nodes) {
      const faded = (graph.keep && !graph.keep.has(n.id)) || (graph.source && n.source !== graph.source);
      retarget(n, faded ? (graph.onlyLinked ? 0 : GRAPH_DIM) : (n.hasCard ? 1 : 0.72));
    }
    for (const g of sim.groups) {
      const faded = (graph.keep && !graph.keep.has(g.id)) || graph.source;
      retarget(g, !graph.layers.rings ? 0 : faded ? (graph.onlyLinked ? 0 : GRAPH_DIM) : 0.9);
    }
    // При выборе наведённая точка показывает свои линии на уровне
    // приглушённого окружения: видно, но не спорит с выбором.
    const glance = graph.selected != null && graph.hover !== graph.selected ? graph.hover : null;
    for (const e of sim.edges) {
      const touchesFocus = e.a.id === focus || e.b.id === focus;
      const touchesGlance = glance != null && (e.a.id === glance || e.b.id === glance);
      const visible = (graph.layers.edges || graph.keep) && (!graph.keep || touchesFocus || touchesGlance) &&
        (!graph.source || e.a.source === graph.source || e.b.source === graph.source);
      const alpha = touchesFocus ? 0.82 : touchesGlance ? (graph.onlyLinked ? 0 : GRAPH_DIM) : 0.07 + 0.16 * e.w;
      retarget(e, visible ? alpha : 0);
    }
  }
  const t = motionOk() ? Math.min(1, Math.max(0, (now - graph.emphasisStart) / GRAPH_EMPHASIS_MS)) : 1;
  const k = easeOut(t);
  for (const items of [sim.nodes, sim.edges, sim.groups]) {
    for (const item of items) item.alpha = item.fromAlpha + (item.targetAlpha - item.fromAlpha) * k;
  }
  return t < 1;
}

function drawGraph() {
  const sim = graph.sim;
  if (!sim || !graph.ctx || state.page !== "graph") return;
  const ctx = graph.ctx;
  const { w, h, dpr } = graph.size;
  const { scale, tx, ty } = graph.view;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  ctx.setTransform(dpr * scale, 0, 0, dpr * scale, dpr * tx, dpr * ty);
  const keep = graph.keep;
  const focus = graph.selected ?? graph.hover;
  const selectedNode = graph.selected != null ? sim.byId.get(graph.selected) : null;
  const focusHub = typeof focus === "string" ? sim.byId.get(focus) : null;
  if (focusHub) {
    ctx.strokeStyle = "#cfd2d7";
    ctx.lineWidth = 1.2 / scale;
    for (const nb of sim.adj.get(focusHub.id)) {
      const n = sim.byId.get(nb.id);
      ctx.globalAlpha = Math.min(n.alpha, focusHub.alpha) * 0.7;
      ctx.beginPath();
      ctx.moveTo(focusHub.x, focusHub.y);
      ctx.lineTo(n.x, n.y);
      ctx.stroke();
    }
  }
  // Hub под курсором при другом выборе показывает свои спицы приглушёнными.
  const glanceHub = graph.selected != null && typeof graph.hover === "string" && graph.hover !== focus
    ? sim.byId.get(graph.hover) : null;
  if (glanceHub && !graph.onlyLinked) {
    ctx.strokeStyle = "#9a9da2";
    ctx.lineWidth = 1 / scale;
    ctx.globalAlpha = GRAPH_DIM;
    for (const nb of sim.adj.get(glanceHub.id)) {
      const n = sim.byId.get(nb.id);
      ctx.beginPath();
      ctx.moveTo(glanceHub.x, glanceHub.y);
      ctx.lineTo(n.x, n.y);
      ctx.stroke();
    }
  }

  for (const e of sim.edges) {
    if (e.alpha <= 0) continue;
    const touchesFocus = keep && (e.a.id === focus || e.b.id === focus);
    ctx.globalAlpha = e.alpha;
    if (touchesFocus) {
      ctx.strokeStyle = "#cfd2d7";
      ctx.lineWidth = 1.4 / scale;
    } else {
      ctx.strokeStyle = "#9a9da2";
      ctx.lineWidth = 1 / scale;
    }
    ctx.beginPath();
    ctx.moveTo(e.a.x, e.a.y);
    ctx.lineTo(e.b.x, e.b.y);
    ctx.stroke();
  }

  // В обзоре точки не соприкасаются, а в зуме остаются около 3px.
  const radius = Math.min(2.4, 3.2 / scale);
  for (const n of sim.nodes) {
    if (n.alpha <= 0) continue;
    ctx.globalAlpha = n.alpha;
    ctx.fillStyle = sourceColor(n.source);
    ctx.beginPath();
    ctx.arc(n.x, n.y, radius, 0, Math.PI * 2);
    ctx.fill();
  }
  // Hub'ы держат постоянный размер на экране: кольцо с точкой, как в легенде.
  const hubRadius = HUB_PX / scale;
  ctx.strokeStyle = "#dbdee3";
  ctx.fillStyle = "#dbdee3";
  ctx.lineWidth = 1.3 / scale;
  for (const hub of sim.groups) {
    if (hub.alpha <= 0) continue;
    ctx.globalAlpha = hub.alpha;
    ctx.beginPath();
    ctx.arc(hub.x, hub.y, hubRadius, 0, Math.PI * 2);
    ctx.stroke();
    ctx.beginPath();
    ctx.arc(hub.x, hub.y, 1.4 / scale, 0, Math.PI * 2);
    ctx.fill();
  }
  ctx.globalAlpha = 1;
  // Как в референсе, кольцо и ореол получает только выбранная точка;
  // наведённая держит слабое кольцо, чтобы цель указателя была видна.
  const ring = (n, grow, strength) => {
    if (!n) return;
    const r = typeof n.id === "string" ? hubRadius : radius;
    if (strength === 1) {
      ctx.globalAlpha = n.alpha * 0.16;
      ctx.fillStyle = sourceColor(n.source);
      ctx.beginPath();
      ctx.arc(n.x, n.y, r + (grow + 4) / scale, 0, Math.PI * 2);
      ctx.fill();
    }
    ctx.globalAlpha = n.alpha * strength;
    ctx.strokeStyle = "#eceef1";
    ctx.lineWidth = 1.5 / scale;
    ctx.beginPath();
    ctx.arc(n.x, n.y, r + grow / scale, 0, Math.PI * 2);
    ctx.stroke();
  };
  if (graph.hover !== graph.selected) ring(sim.byId.get(graph.hover), 2.5, 0.35);
  ring(selectedNode, 4, 1);
  ctx.globalAlpha = 1;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  updateGraphLabels(keep);
}

// Подписи в DOM, чтобы оставаться резкими. Имена проектов конкурируют размером:
// жадный проход ставит крупнейшие первыми и пропускает то, что перекрылось бы.
// При выборе подписями становятся заголовки записей его окружения.
function updateGraphLabels(keep) {
  const sim = graph.sim;
  const host = $("graph-labels");
  const { w, h } = graph.size;
  const wanted = [];
  if (keep) {
    for (const id of keep) {
      const node = sim.byId.get(id);
      const at = toScreen(node.x, node.y);
      const text = node.title ?? node.name;
      wanted.push({
        text, x: at.x, y: at.y - 14,
        className: `g-label record${id === graph.selected ? " selected" : ""}`,
        alpha: node.alpha,
        width: Math.min(240, text.length * 6.6 + 16), height: 17,
        priority: id === graph.selected ? 1e9 : id === graph.hover ? 1e8 : 1,
      });
    }
  } else if (graph.layers.labels) {
    for (const group of sim.groups) {
      const at = toScreen(group.x, group.y);
      const size = group.nodes.length;
      if (size < 3 && graph.view.scale < 1.6) continue;
      wanted.push({
        text: group.name, x: at.x, y: at.y - 13,
        className: `g-label${size >= 15 ? " big" : ""}`,
        width: group.name.length * 7.4 + 8, height: 18, priority: size,
      });
    }
  }
  wanted.sort((a, b) => b.priority - a.priority);
  const placed = [];
  for (const item of wanted) {
    if (placed.length >= LABEL_LIMIT) break;
    const left = item.x - item.width / 2;
    const top = item.y - item.height / 2;
    if (left + item.width < 0 || top + item.height < 0 || left > w || top > h) continue;
    const box = { left, top, right: left + item.width, bottom: top + item.height };
    const hit = placed.some((p) => box.left < p.box.right && box.right > p.box.left &&
      box.top < p.box.bottom && box.bottom > p.box.top);
    if (!hit || item.priority >= 1e8) placed.push({ ...item, box });
  }
  while (graph.labelEls.length < placed.length) {
    const el = document.createElement("div");
    host.appendChild(el);
    graph.labelEls.push(el);
  }
  graph.labelEls.forEach((el, i) => {
    const item = placed[i];
    if (!item) {
      el.hidden = true;
      return;
    }
    el.hidden = false;
    el.className = item.className;
    el.textContent = item.text;
    el.style.opacity = item.alpha ?? 1;
    el.style.left = `${Math.round(item.x)}px`;
    el.style.top = `${Math.round(item.y)}px`;
  });
}

function graphHit(sx, sy) {
  const sim = graph.sim;
  if (!sim) return null;
  const p = toWorld(sx, sy);
  let best = null;
  let bestD = GRAPH_HIT_PX / graph.view.scale;
  const keep = visibleSet(graph.selected);
  const hubs = graph.layers.rings ? sim.groups : [];
  for (const n of [...sim.nodes, ...hubs]) {
    if (graph.onlyLinked && keep && !keep.has(n.id)) continue;
    // Hub попадает по всему кольцу, чуть шире точки.
    const reach = typeof n.id === "string" ? (HUB_PX - 2) / graph.view.scale : 0;
    const d = Math.hypot(n.x - p.x, n.y - p.y) - reach;
    if (d <= bestD) {
      bestD = d;
      best = n;
    }
  }
  return best;
}

// Ручка для инструментов и smoke-теста: тот же hit-test и преобразования, что
// у событий указателя, без выдачи наружу объекта состояния.
function exposeGraphProbe() {
  $("graph-canvas").__graph = {
    nodeXY(id) {
      const n = graph.sim && graph.sim.byId.get(id);
      return n ? { x: n.x, y: n.y } : null;
    },
    selected: () => graph.selected,
    view: () => ({ ...graph.view }),
    hit: graphHit,
    toScreen,
    overviewScale: () => overviewView().scale,
    groups: () => graph.sim.groups.map((g) => ({ project: g.project, x: g.x, y: g.y, r: g.r, size: g.nodes.length })),
  };
}

// Клик по карте вписывает то, что точка держит вместе: проект — своё
// облако, запись — себя, свой проект и связанные записи. Выбор из списка,
// поиска или ссылки подводит точку, как в референсе.
function graphSelect(id, { frame = true } = {}) {
  const sim = graph.sim;
  if (!sim || !sim.byId.has(id)) {
    if (!sim) {
      graph.pendingFocus = id;
    } else if (graph.days > 0) {
      // Запись старше выбранного периода: иначе «Показать на графе» молча
      // открывал карту без выбора. Переключаемся на «Всё время» и фокусируем.
      graph.days = 0;
      setPressed($("graph-period"), (b) => Number(b.dataset.days) === 0);
      graph.pendingFocus = id;
      loadGraph(true);
    } else {
      $("graph-find-note").textContent = tr("Записи нет на карте");
    }
    return;
  }
  graph.selected = id;
  if (typeof id === "string") {
    showHubCard(sim.byId.get(id));
    focusGroup(sim.byId.get(id));
  } else {
    showGraphCard(id);
    if (frame) focusNode(id);
    else focusTree(id);
  }
  requestGraphDraw();
  history.replaceState(null, "", typeof id === "string" ? "#graph" : `#graph/${id}`);
}

// Тела вписываются в область, которую оставляет карточка; прежний вид
// запоминается, чтобы снятие выбора вернуло зум к нему. Панорамирование
// сохраняет возврат; ручной зум (колесо, fit) делает текущий масштаб
// своим для читателя, и возврат отбрасывается.
function frameBodies(bodies) {
  const view = graph.camera?.target || graph.view;
  graph.returnView ??= { ...view };
  const b = worldBounds(bodies);
  const cx = (b.minX + b.maxX) / 2;
  const cy = (b.minY + b.maxY) / 2;
  // Иначе одинокая запись заполнила бы сцену на максимальном зуме.
  const spanX = Math.max(GRAPH_FRAME_MIN_SPAN, b.maxX - b.minX);
  const spanY = Math.max(GRAPH_FRAME_MIN_SPAN, b.maxY - b.minY);
  const cardW = Math.min(340, graph.size.w * 0.44) + 12;
  const availableW = graph.size.w - cardW;
  const pad = 48;
  const scale = Math.max(GRAPH_MIN_SCALE, Math.min(
    GRAPH_MAX_SCALE,
    (availableW - pad * 2) / spanX,
    (graph.size.h - pad * 2) / spanY,
  ));
  animateView({ scale, tx: availableW / 2 - cx * scale, ty: graph.size.h / 2 - cy * scale }, GRAPH_SELECT_MS);
}

// Радиусы hub'ов — размеры облаков, поэтому проект вписывает всё своё облако.
function focusGroup(hub) {
  frameBodies([hub, ...hub.nodes.map((n) => graph.sim.byId.get(n.id))]);
}

// Дерево записи: сама запись, hub её проекта как точка и все записи,
// с которыми у неё общие темы.
function focusTree(id) {
  const sim = graph.sim;
  const node = sim.byId.get(id);
  const bodies = [node, ...sim.adj.get(id).map((nb) => sim.byId.get(nb.id))];
  if (node.hub) bodies.push({ x: node.hub.x, y: node.hub.y });
  frameBodies(bodies);
}

function graphFocusWhenReady(id) {
  if (graph.sim && graph.sim.byId.has(id)) graphSelect(id);
  else graph.pendingFocus = id;
}

function graphClearSelection() {
  graphCardTicket();
  graph.selected = null;
  graph.onlyLinked = false;
  hideGraphCard();
  if (graph.returnView && graph.sim) animateView(graph.returnView, GRAPH_SELECT_MS);
  graph.returnView = null;
  requestGraphDraw();
  if (state.page === "graph") history.replaceState(null, "", "#graph");
}

function hideGraphCard() {
  $("graph-card").hidden = true;
  $("graph-card").innerHTML = "";
}

async function showGraphCard(id) {
  const t = graphCardTicket();
  const host = $("graph-card");
  host.hidden = false;
  host.innerHTML = `<div class="state">${tr("Загрузка…")}</div>`;
  try {
    const card = await api("/api/card", { id });
    if (!graphCardTicket.current(t)) return;
    const words = (card.keywords || []).slice(0, 8).map((w) => `<span class="word-chip">${esc(w)}</span>`).join("");
    const similar = (card.similar || []).map((s) =>
      `<button class="similar-row" data-focus="${s.id}" type="button"><i class="dot" style="background:${sourceColor(s.source)}"></i><span>${esc(s.title)}</span><small>${esc(leaf(s.project))}</small></button>`
    ).join("");
    const meta = card.record;
    host.innerHTML =
      `<div class="card-top"><span class="card-eyebrow">${esc(sourceName(card.source))}, ${esc(shortDate(card.ts))}</span>` +
      `<button class="close-x" data-action="close" type="button" aria-label="${tr("Снять выбор")}">×</button></div>` +
      `<h3>${esc(card.title)}</h3>` +
      (meta && meta.project !== origin(card.source) ? `<span class="project-path">${esc(meta.project_name)}</span>` : "") +
      (graph.sim?.byId.get(id)?.hasCard === false ? `<p class="sub">${tr("Карточки пока нет: запись ждёт пересказа.")}</p>` : "") +
      (words ? `<div class="word-chips">${words}</div>` : "") +
      `<div class="card-actions"><button class="btn" data-action="open" type="button">${tr("Открыть карточку")}</button>` +
      `<button class="btn${graph.onlyLinked ? " is-on" : ""}" data-action="linked" type="button" aria-pressed="${graph.onlyLinked}">${tr("Только связанные")}</button></div>` +
      (similar ? `<section><span class="micro">${tr("Похожие по темам")}</span><div>${similar}</div></section>` : `<p class="sub">${tr("Близких записей не нашлось.")}</p>`);
  } catch (e) {
    if (!graphCardTicket.current(t)) return;
    host.innerHTML = `<div class="state state-error">${tr("Не загрузилось: {e}", { e: esc(e.message) })}</div>`;
  }
}

// Карточка hub строится из самого графа: без запроса, свежие записи первыми.
function showHubCard(hub) {
  graphCardTicket();
  const host = $("graph-card");
  const recent = graph.sim.adj.get(hub.id)
    .map((nb) => graph.sim.byId.get(nb.id))
    .sort((a, b) => b.ts - a.ts)
    .slice(0, 8)
    .map((n) => `<button class="similar-row" data-focus="${n.id}" type="button"><i class="dot" style="background:${sourceColor(n.source)}"></i><span>${esc(n.title)}</span><small>${esc(shortDate(n.ts))}</small></button>`)
    .join("");
  host.hidden = false;
  host.innerHTML =
    `<div class="card-top"><span class="card-eyebrow">${tr("Проект, {records}", { records: records(hub.nodes.length) })}</span>` +
    `<button class="close-x" data-action="close" type="button" aria-label="${tr("Снять выбор")}">×</button></div>` +
    `<h3>${esc(hub.name)}</h3>` +
    (hub.name !== hub.project ? `<span class="project-path">${esc(hub.project)}</span>` : "") +
    `<div class="card-actions"><button class="btn" data-action="open" type="button">${tr("Открыть записи")}</button>` +
    `<button class="btn${graph.onlyLinked ? " is-on" : ""}" data-action="linked" type="button" aria-pressed="${graph.onlyLinked}">${tr("Только связанные")}</button></div>` +
    `<section><span class="micro">${tr("Последние записи")}</span><div>${recent}</div></section>`;
}

function openGraphSelection() {
  const id = graph.selected;
  if (typeof id === "string") openProject(graph.sim.byId.get(id).project);
  else if (id != null) openRecord(id);
}

function onGraphCardClick(e) {
  const action = e.target.closest("[data-action]");
  if (action) {
    const what = action.dataset.action;
    if (what === "close") graphClearSelection();
    if (what === "open") openGraphSelection();
    if (what === "linked") {
      graph.onlyLinked = !graph.onlyLinked;
      action.classList.toggle("is-on", graph.onlyLinked);
      action.setAttribute("aria-pressed", String(graph.onlyLinked));
      requestGraphDraw();
    }
    return;
  }
  const focus = e.target.closest("[data-focus]");
  if (focus) graphSelect(Number(focus.dataset.focus));
}

function wireGraph() {
  const canvas = $("graph-canvas");
  canvas.addEventListener("mousemove", (e) => {
    if (graph.drag || graph.nodeDrag) return;
    const n = graphHit(e.offsetX, e.offsetY);
    const id = n ? n.id : null;
    canvas.style.cursor = n ? "pointer" : "";
    if (id !== graph.hover) {
      graph.hover = id;
      requestGraphDraw();
    }
    const tip = $("graph-tip");
    if (!n) {
      tip.hidden = true;
      return;
    }
    tip.innerHTML = typeof n.id === "string"
      ? `<b>${esc(n.name)}</b><small>${tr("проект, {records}", { records: records(n.nodes.length) })}</small>`
      : `<b><i style="background:${sourceColor(n.source)}"></i>${esc(n.title)}</b>` +
        `<small>${esc(sourceName(n.source))}, ${n.hub ? esc(n.hub.name) : tr("без проекта")}, ${esc(dayDate(n.ts))}</small>`;
    tip.hidden = false;
    const pad = 14;
    const left = e.offsetX + pad + tip.offsetWidth > graph.size.w ? e.offsetX - tip.offsetWidth - pad : e.offsetX + pad;
    const top = e.offsetY + pad + tip.offsetHeight > graph.size.h ? e.offsetY - tip.offsetHeight - pad : e.offsetY + pad;
    tip.style.left = `${Math.max(4, left)}px`;
    tip.style.top = `${Math.max(4, top)}px`;
  });
  canvas.addEventListener("mouseleave", () => {
    $("graph-tip").hidden = true;
    if (graph.hover != null) {
      graph.hover = null;
      requestGraphDraw();
    }
  });
  canvas.addEventListener("click", (e) => {
    if (graph.suppressClick) {
      graph.suppressClick = false;
      return;
    }
    const n = graphHit(e.offsetX, e.offsetY);
    if (n && n.id === graph.selected) openGraphSelection();
    else if (n) graphSelect(n.id, { frame: false });
    else if (graph.selected != null) graphClearSelection();
  });
  canvas.addEventListener("dblclick", () => graphFit());
  canvas.addEventListener("wheel", (e) => {
    if (!graph.sim) return;
    e.preventDefault();
    const view = graph.camera?.target || graph.view;
    const delta = e.deltaY * (e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? graph.size.h : 1);
    const next = Math.min(GRAPH_MAX_SCALE, Math.max(GRAPH_MIN_SCALE, view.scale * GRAPH_WHEEL_RATE ** -delta));
    const x = (e.offsetX - view.tx) / view.scale;
    const y = (e.offsetY - view.ty) / view.scale;
    graph.returnView = null;
    animateView({ scale: next, tx: e.offsetX - x * next, ty: e.offsetY - y * next }, GRAPH_WHEEL_MS);
    $("graph-tip").hidden = true;
  }, { passive: false });
  // Event loop браузера. Нажатие на точку тащит эту точку (её связи идут
  // через симуляцию), нажатие на пустом месте двигает камеру.
  canvas.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || !graph.sim) return;
    graph.suppressClick = false;
    const hit = graphHit(e.offsetX, e.offsetY);
    if (hit) {
      const p = toWorld(e.offsetX, e.offsetY);
      graph.nodeDrag = { node: hit, sx: e.offsetX, sy: e.offsetY, ox: hit.x - p.x, oy: hit.y - p.y, moved: false };
    } else {
      graph.camera = null;
      graph.drag = { sx: e.offsetX, sy: e.offsetY, tx0: graph.view.tx, ty0: graph.view.ty, moved: false };
    }
    // Захват падает, если указатель уже ушёл; тогда drag просто
    // обрывается на краю canvas.
    try { canvas.setPointerCapture?.(e.pointerId); } catch { /* указатель уже отпущен */ }
  });
  canvas.addEventListener("pointermove", (e) => {
    const grab = graph.nodeDrag;
    if (grab) {
      if (!grab.moved && Math.hypot(e.offsetX - grab.sx, e.offsetY - grab.sy) > 3) {
        grab.moved = true;
        graph.sim.heat = Math.max(graph.sim.heat, GRAPH_DRAG_HEAT);
        graph.hover = grab.node.id;
        canvas.style.cursor = "grabbing";
        $("graph-tip").hidden = true;
      }
      if (grab.moved) {
        const p = toWorld(e.offsetX, e.offsetY);
        graph.sim.forces.pin(grab.node, p.x + grab.ox, p.y + grab.oy);
        requestGraphDraw();
      }
      return;
    }
    const drag = graph.drag;
    if (!drag) return;
    const dx = e.offsetX - drag.sx;
    const dy = e.offsetY - drag.sy;
    if (!drag.moved && Math.hypot(dx, dy) > 3) drag.moved = true;
    if (drag.moved) {
      graph.camera = null;
      graph.view.tx = drag.tx0 + dx;
      graph.view.ty = drag.ty0 + dy;
      $("graph-tip").hidden = true;
      requestGraphDraw();
    }
  });
  const endPress = () => {
    if (graph.nodeDrag) {
      graph.suppressClick = graph.nodeDrag.moved;
      graph.nodeDrag = null;
      graph.sim?.forces.release();
      canvas.style.cursor = "pointer";
      requestGraphDraw();
      return;
    }
    if (!graph.drag) return;
    graph.suppressClick = graph.drag.moved;
    graph.drag = null;
  };
  canvas.addEventListener("pointerup", endPress);
  canvas.addEventListener("pointercancel", endPress);
  for (const [id, factor] of [["graph-zoom-in", 1.3], ["graph-zoom-out", 1 / 1.3]]) {
    $(id).addEventListener("click", () => {
      if (!graph.sim) return;
      const cx = graph.size.w / 2;
      const cy = graph.size.h / 2;
      const view = graph.camera?.target || graph.view;
      const p = { x: (cx - view.tx) / view.scale, y: (cy - view.ty) / view.scale };
      const scale = Math.max(GRAPH_MIN_SCALE, Math.min(GRAPH_MAX_SCALE, view.scale * factor));
      graph.returnView = null;
      animateView({ scale, tx: cx - p.x * scale, ty: cy - p.y * scale });
    });
  }
  $("graph-fit").addEventListener("click", () => graphFit());
  $("graph-full").addEventListener("click", toggleGraphFullscreen);
  document.addEventListener("fullscreenchange", onGraphFullscreenChange);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) graphPause();
    else requestGraphDraw();
  });
  graphMotion?.addEventListener("change", () => {
    if (!motionOk()) advanceGraphCamera(performance.now());
    requestGraphDraw();
  });
  $("graph-view").addEventListener("click", () => {
    const menu = $("graph-view-menu");
    menu.hidden = !menu.hidden;
    $("graph-view").setAttribute("aria-expanded", String(!menu.hidden));
  });
  document.addEventListener("click", (e) => {
    if ($("graph-view-menu").hidden) return;
    if (e.target.closest("#graph-view") || e.target.closest("#graph-view-menu")) return;
    closeGraphViewMenu();
  });
  $("graph-view-menu").querySelectorAll("input").forEach((box) => {
    box.checked = graph.layers[box.dataset.layer] !== false;
    box.addEventListener("change", () => {
      graph.layers[box.dataset.layer] = box.checked;
      savePref("graph-layers", graph.layers);
      requestGraphDraw();
    });
  });
  $("graph-card").addEventListener("click", onGraphCardClick);
  $("graph-sources").addEventListener("click", (e) => {
    const chip = e.target.closest(".chip");
    if (!chip) return;
    graph.source = graph.source === chip.dataset.source ? "" : chip.dataset.source;
    setPressed($("graph-sources"), (b) => b.dataset.source === graph.source);
    requestGraphDraw();
  });
  $("graph-period").addEventListener("click", (e) => {
    const seg = e.target.closest(".seg");
    if (!seg) return;
    graph.days = Number(seg.dataset.days) || 0;
    setPressed(seg.parentElement, (b) => b === seg);
    loadGraph(true);
  });
  $("graph-find").addEventListener("keydown", (e) => {
    if (e.key !== "Enter" || !graph.sim) return;
    const needle = $("graph-find").value.trim().toLowerCase();
    if (!needle) return;
    const group = graph.sim.groups.find((g) => g.project.toLowerCase().includes(needle));
    const hit = graph.sim.nodes.find((n) => n.title.toLowerCase().includes(needle));
    $("graph-find-note").textContent = "";
    if (hit) {
      graphSelect(hit.id);
    } else if (group) {
      const pad = 60;
      const scale = Math.min(GRAPH_FOCUS_MAX_SCALE, (Math.min(graph.size.w, graph.size.h) - pad * 2) / (group.r * 2));
      animateView({ scale, tx: graph.size.w / 2 - group.x * scale, ty: graph.size.h / 2 - group.y * scale });
    } else {
      $("graph-find-note").textContent = tr("Не нашлось: {q}", { q: $("graph-find").value.trim() });
    }
  });
  if (typeof ResizeObserver === "function") {
    new ResizeObserver(() => {
      if (state.page === "graph" && graph.sim) {
        const before = graph.size;
        sizeGraphCanvas();
        // Держим тот же мировой центр, пока event loop браузера меняет размер сцены.
        const dx = (graph.size.w - before.w) / 2;
        const dy = (graph.size.h - before.h) / 2;
        for (const view of [graph.view, graph.camera?.from, graph.camera?.target]) {
          if (view) { view.tx += dx; view.ty += dy; }
        }
        requestGraphDraw();
      }
    }).observe($("graph-stage"));
  }
}

// Сцена меняет размер, старый вид камеры теряет смысл: вписываем карту заново
// или окружение выбранной точки. Идёт в event loop браузера.
function onGraphFullscreenChange() {
  if (state.page !== "graph" || !graph.sim) return;
  sizeGraphCanvas();
  if (graph.selected != null) focusNode(graph.selected);
  else graphFit(false);
}

function toggleGraphFullscreen() {
  const stage = $("graph-stage");
  const request = document.fullscreenElement ? document.exitFullscreen() : stage.requestFullscreen();
  if (request && request.catch) request.catch(() => toast(tr("Полноэкранный режим недоступен")));
}

// -- ответы ------------------------------------------------------------------

const answerHistory = [];
const answerCache = new Map();
let answerContext = [];
let answerCards = [];
const answerTicket = latest();

function linkCitations(escaped, count) {
  return escaped.replace(/\[(\d+)\]/g, (match, n) => {
    const idx = Number(n);
    if (idx < 1 || idx > count) return match;
    return `<button class="cite" data-card="${idx - 1}" type="button" title="${tr("Источник {n}", { n: idx })}">${idx}</button>`;
  });
}

// Ветка читается сверху вниз: вопрос, вкладки, ответ с нумерованными
// цитатами, затем записи, на которых он стоит.
function threadHead(q, sources) {
  return `<h1 class="thread-q">${esc(q)}</h1>` +
    `<div class="thread-tabs"><button type="button" data-answer-tab="answer" aria-pressed="true"><svg class="icon"><use href="#i-spark"/></svg>${tr("Ответ")}</button>` +
    `<button type="button" data-answer-tab="sources" aria-pressed="false">${tr("Источники")} <b>${sources == null ? "…" : number(sources)}</b></button></div>`;
}

function renderAnswer(data, q) {
  $("answer-out").classList.remove("sources-only");
  const cards = data.cards || [];
  answerCards = cards;
  const parts = [threadHead(q, cards.length)];
  if (data.answer) parts.push(`<div class="answer-block rich">${linkCitations(richText(String(data.answer)), cards.length)}</div>`);
  if (cards.length) {
    const reasons = {
      not_configured: tr("Настройте модель в «Настройках», чтобы получить ответ."),
      local_only: tr("Эти записи доступны только локальной модели. Источники можно открыть здесь."),
      provider_error: tr("Модель временно недоступна. Найденные источники сохранены — попробуйте ещё раз."),
      empty_answer: tr("Модель вернула пустой ответ. Найденные источники доступны ниже."),
    };
    const head = data.used_llm
      ? `${tr("Источники")} · ${number(cards.length)}`
      : (reasons[data.reason] || tr("Модель не ответила или не настроена. Нашлось записей: {n}", { n: number(cards.length) }));
    parts.push(`<div class="answer-sources"><span class="micro">${esc(head)}</span>` + cards.map((c, i) =>
      `<button class="answer-source" data-id="${c.id}" type="button"><b>${i + 1}</b><strong>${esc(c.title)}</strong>` +
      `<small>${esc(sourceName(c.source))} · ${esc(shortDate(c.ts))}</small><span class="source-excerpt">${esc(c.snippet || "")}</span></button>`
    ).join("") + `</div>`);
  }
  if (!data.answer && !cards.length) parts.push(`<div class="state">${tr("По вопросу «{q}» ничего не нашлось.", { q: esc(q) })}</div>`);
  $("answer-out").innerHTML = parts.join("");
}

// История поиска хранится на сервере (search_history): переживает перезапуск,
// а каждый вопрос там же становится карточкой памяти «Поиск».
let searchHistory = [];
let historyCurrent = null;
const historyTicket = latest();
const HISTORY_COLLAPSED_KEY = "sb.sideHistoryCollapsed";

function renderSideHistory() {
  const short = (h) => esc(h.length > 60 ? h.slice(0, 59) + "…" : h);
  $("side-history").innerHTML = searchHistory.map((h) =>
    `<div class="side-item"><button class="side-open${h.q === historyCurrent ? " active" : ""}" data-history="${h.id}" type="button" title="${esc(h.q)}">${short(h.q)}</button>` +
    `<button class="side-forget" data-forget="${h.id}" type="button" aria-label="${tr("Убрать из истории")}" title="${tr("Убрать из истории")}"><svg class="icon"><use href="#i-win-close"/></svg></button></div>`
  ).join("");
  $("side-history-count").textContent = searchHistory.length ? `· ${number(searchHistory.length)}` : "";
  $("side-recent").hidden = !searchHistory.length;
}

function setHistoryCollapsed(collapsed) {
  $("side-history").hidden = collapsed;
  $("side-history-toggle").setAttribute("aria-expanded", String(!collapsed));
  savePref(HISTORY_COLLAPSED_KEY, collapsed);
}

async function loadSearchHistory() {
  const t = historyTicket();
  try {
    const data = await api("/api/history", { limit: 50 });
    if (!historyTicket.current(t)) return;
    searchHistory = data.items || [];
    renderSideHistory();
  } catch {
    // История — удобство: без неё вопросы по-прежнему задаются.
  }
}

// Прошлый ответ берём из сохранённого: повторный показ не платит за модель.
async function openHistoryItem(id) {
  const item = searchHistory.find((h) => h.id === id);
  if (!item) return;
  go("answers");
  if (answerCache.has(item.q)) {
    restoreAnswer(item.q);
    return;
  }
  const t = answerTicket();
  $("page-answers").classList.add("is-asked");
  $("answer-q").value = "";
  $("answer-q").placeholder = tr("Задайте уточняющий вопрос");
  $("answer-out").innerHTML = threadHead(item.q, null) + `<div class="state">${tr("Открываю…")}</div>`;
  try {
    const data = await api("/api/history/item", { id });
    if (!answerTicket.current(t)) return;
    renderAnswer(data, item.q);
    answerContext = [{ q: item.q, card_ids: answerCards.map((card) => card.id).slice(0, ANSWER_LIMIT) }];
    answerCache.set(item.q, { data, context: answerContext });
    renderAnswerHistory(item.q);
  } catch (e) {
    if (!answerTicket.current(t)) return;
    $("answer-out").innerHTML = threadHead(item.q, 0) + `<div class="state state-error">${tr("Не удалось открыть: {e}", { e: esc(e.message) })}</div>`;
  }
}

async function forgetHistoryItem(id) {
  try {
    await postJson("/api/history/delete", { id });
  } catch (e) {
    toast(tr("Не удалось убрать из истории: {e}", { e: e.message }));
    return;
  }
  searchHistory = searchHistory.filter((h) => h.id !== id);
  renderSideHistory();
}

function renderAnswerHistory(current) {
  const short = (h) => esc(h.length > 60 ? h.slice(0, 59) + "…" : h);
  $("answer-history").innerHTML = answerHistory
    .map((h, i) => (h === current ? "" : `<button class="chip" data-i="${i}" type="button">${short(h)}</button>`))
    .join("");
  historyCurrent = current;
  renderSideHistory();
}

// «Спросить память» на странице ask начинает заново, как новая ветка.
function resetAsk() {
  answerTicket();
  answerContext = [];
  answerCards = [];
  $("answer-out").innerHTML = "";
  $("answer-q").value = "";
  $("answer-q").placeholder = ASK_PLACEHOLDER;
  $("page-answers").classList.remove("is-asked");
  renderAnswerHistory(null);
  $("answer-q").focus();
}

async function askAnswer(q) {
  q = q.trim();
  if (!q) return;
  const t = answerTicket();
  const history = answerContext.slice(-4);
  const known = answerHistory.indexOf(q);
  if (known >= 0) answerHistory.splice(known, 1);
  answerHistory.unshift(q);
  answerHistory.length = Math.min(answerHistory.length, ANSWER_HISTORY_MAX);
  renderAnswerHistory(q);
  $("page-answers").classList.add("is-asked");
  $("answer-q").value = "";
  $("answer-q").placeholder = tr("Задайте уточняющий вопрос");
  $("answer-out").innerHTML = threadHead(q, null) + `<div class="state">${tr("Ищу и формулирую ответ…")}</div>`;
  try {
    const data = await postJson("/api/answer", { q, limit: ANSWER_LIMIT, history });
    if (!answerTicket.current(t)) return;
    renderAnswer(data || {}, q);
    answerContext = [...history, { q, card_ids: answerCards.map((card) => card.id).slice(0, ANSWER_LIMIT) }].slice(-4);
    answerCache.set(q, { data, context: answerContext });
    for (const question of answerCache.keys()) {
      if (!answerHistory.includes(question)) answerCache.delete(question);
    }
    loadSearchHistory();
  } catch (e) {
    if (!answerTicket.current(t)) return;
    $("answer-out").innerHTML = threadHead(q, 0) + `<div class="state state-error">${tr("Ответа нет: {e}", { e: esc(e.message) })}</div>`;
  }
}

// Событийный цикл браузера: повторное открытие результата не создаёт платный запрос.
function restoreAnswer(q) {
  const cached = answerCache.get(q);
  if (!cached) {
    answerContext = [];
    askAnswer(q);
    return;
  }
  answerTicket();
  answerContext = cached.context;
  $("page-answers").classList.add("is-asked");
  $("answer-q").value = "";
  $("answer-q").placeholder = tr("Задайте уточняющий вопрос");
  renderAnswer(cached.data || {}, q);
  renderAnswerHistory(q);
}

// -- передача в чат ----------------------------------------------------------
//
// Источник и цель выбираются чипами; список чатов перечитывается при смене
// источника или периода (tickets отбрасывают устаревшие ответы). Перенос идёт
// по одному чату за раз: выжимка — платный запрос к модели, параллель не нужна.
// Смена аккаунта — тот же харнесс в два шага: сервер хранит подготовленный
// перенос и снимок аккаунта, страница опрашивает его, пока владелец перелогинивается.

const HANDOFF_MODES = { full: tr("целиком"), brief: tr("выжимка") };
const handoff = {
  harnesses: [], from: loadPref("handoff.from", ""), to: loadPref("handoff.to", ""),
  hours: 72, mode: "auto", chats: [], picked: new Set(), busy: false,
  kind: loadPref("handoff.kind", "harness"), switch: null, poll: 0,
  groups: {}, refreshing: false,
};
const handoffTicket = latest();
const switchTicket = latest();
const SWITCH_POLL_MS = 4000;
const HANDOFF_GROUP_LIMIT = 20;
const HANDOFF_PICK_LIMIT = 40;
const HANDOFF_SOURCE_ORDER = ["claude", "codex", "kimi", "qoder", "opencode", "deepseek", "copilot", "antigravity"];

function handoffReadableSources() {
  return handoff.harnesses
    .filter((h) => h.can_source)
    .sort((a, b) => {
      const left = HANDOFF_SOURCE_ORDER.indexOf(a.id);
      const right = HANDOFF_SOURCE_ORDER.indexOf(b.id);
      return (left < 0 ? 99 : left) - (right < 0 ? 99 : right);
    });
}

async function loadHandoff() {
  try {
    handoff.harnesses = (await api("/api/handoff/harnesses")).harnesses || [];
  } catch (e) {
    $("handoff-chats").innerHTML = `<div class="state state-error">${tr("Харнессы недоступны: {e}", { e: esc(e.message) })}</div>`;
    return;
  }
  pickHandoffSource();
  pickHandoffTarget();
  renderHandoffRoute();
  if (handoff.kind === "account") loadHandoffChats();
  else loadAllHandoffChats();
  loadSwitch();
}

function handoffSources() {
  return handoff.harnesses.filter((h) => handoff.kind === "account" ? h.can_switch : h.can_source);
}

function pickHandoffSource() {
  const sources = handoffSources();
  if (!sources.some((h) => h.id === handoff.from)) {
    handoff.from = (sources.find((h) => h.running) || sources[0] || {}).id || "";
    savePref("handoff.from", handoff.from);
  }
}

function pickHandoffTarget() {
  const targets = handoff.harnesses.filter((h) => h.can_target);
  if (!targets.some((h) => h.id === handoff.to)) {
    // Перенос в тот же харнесс допустим (свежий контекст), но по умолчанию — в другой.
    const others = targets.filter((h) => h.id !== handoff.from);
    handoff.to = (others.find((h) => h.running) || others[0] || targets[0] || {}).id || "";
  }
}

function harnessChip(h, selected, disabled) {
  const dot = h.running === null ? "" : `<i class="harness-dot${h.running ? " on" : ""}" title="${h.running ? tr("Открыт") : tr("Не запущен")}"></i>`;
  const how = h.delivery === "app" ? tr("новая сессия в приложении")
    : h.delivery === "terminal" ? tr("новый чат в консоли") : tr("промпт в буфер обмена");
  return `<button class="chip" data-harness="${esc(h.id)}" type="button" aria-pressed="${selected}"` +
    `${disabled ? " disabled" : ""} title="${esc(how)}">${dot}${esc(h.name)}</button>`;
}

function renderHandoffRoute() {
  const account = handoff.kind === "account";
  setPressed($("handoff-kind"), (b) => b.dataset.kind === handoff.kind);
  $("handoff-from-side").hidden = !account;
  $("handoff-arrow").hidden = account;
  $("handoff-to-side").hidden = account;
  $("handoff-account").hidden = !account;
  $("handoff-from").innerHTML = handoffSources()
    .map((h) => harnessChip(h, h.id === handoff.from, false)).join("");
  $("handoff-to").innerHTML = handoff.harnesses.filter((h) => h.can_target)
    .map((h) => harnessChip(h, h.id === handoff.to, false)).join("") +
    `<button id="handoff-add" class="chip chip-add" type="button" title="${esc(tr("Добавить свой харнесс"))}">+ ${esc(tr("Свой"))}</button>`;
  updateHandoffActions();
}

function renderHarnessCustomList() {
  const custom = handoff.harnesses.filter((h) => h.custom);
  $("harness-custom-list").innerHTML = custom.length
    ? `<span class="micro">${esc(tr("Добавленные"))}</span>` + custom.map((h) =>
        `<div class="harness-custom-row"><span>${esc(h.name)}</span>` +
        `<code class="handoff-path">${esc(h.command || "")}</code>` +
        `<button class="sync-button" type="button" data-remove-harness="${esc(h.id)}">${esc(tr("Удалить"))}</button></div>`).join("")
    : "";
}

function openHarnessDialog() {
  $("harness-name").value = "";
  $("harness-command").value = "";
  $("harness-add-error").hidden = true;
  renderHarnessCustomList();
  $("harness-add").showModal();
  $("harness-name").focus();
}

async function saveHarness(event) {
  event.preventDefault();
  const name = $("harness-name").value.trim();
  const command = $("harness-command").value.trim();
  try {
    const data = await postJson("/api/handoff/harnesses", { name, command });
    handoff.harnesses = data.harnesses || handoff.harnesses;
    const added = handoff.harnesses.find((h) => h.custom && h.name === name);
    if (added) {
      handoff.to = added.id;
      savePref("handoff.to", handoff.to);
    }
    $("harness-add").close();
    renderHandoffRoute();
    toast(tr("Свой харнесс добавлен"));
  } catch (e) {
    const error = $("harness-add-error");
    error.textContent = e.message;
    error.hidden = false;
  }
}

async function deleteHarness(id) {
  try {
    await request(`/api/handoff/harnesses?id=${encodeURIComponent(id)}`, { method: "DELETE" });
    handoff.harnesses = (await api("/api/handoff/harnesses")).harnesses || [];
    if (handoff.to === id) pickHandoffTarget();
    renderHandoffRoute();
    renderHarnessCustomList();
  } catch (e) {
    toast(e.message);
  }
}

async function loadAllHandoffChats() {
  if (handoff.kind !== "harness") return;
  const sources = handoffReadableSources();
  const t = handoffTicket();
  handoff.refreshing = true;
  for (const harness of sources) {
    if (!handoff.groups[harness.id]) {
      handoff.groups[harness.id] = { chats: [], loading: false, error: "", stale: false };
    }
    handoff.groups[harness.id].loading = true;
  }
  renderHandoffChats();
  try {
    await Promise.allSettled(sources.map((harness) => loadHandoffGroup(harness, t)));
  } finally {
    if (handoffTicket.current(t)) handoff.refreshing = false;
  }
}

async function loadHandoffGroup(harness, ticket) {
  try {
    const data = await api("/api/handoff/chats", { harness: harness.id, hours: handoff.hours, limit: HANDOFF_GROUP_LIMIT });
    if (!handoffTicket.current(ticket)) return;
    const group = handoff.groups[harness.id] || (handoff.groups[harness.id] = { chats: [] });
    group.chats = data.chats || [];
    group.loading = false;
    group.error = "";
    group.stale = false;
    pruneHandoffPicked();
    renderHandoffChats();
  } catch (e) {
    if (!handoffTicket.current(ticket)) return;
    const group = handoff.groups[harness.id] || (handoff.groups[harness.id] = { chats: [] });
    group.loading = false;
    group.error = e.message;
    group.stale = group.chats.length > 0;
    renderHandoffChats();
  }
}

function pruneHandoffPicked() {
  const visible = new Set();
  for (const group of Object.values(handoff.groups)) {
    for (const chat of group.chats || []) visible.add(chat.id);
  }
  for (const id of [...handoff.picked]) {
    if (!visible.has(id)) handoff.picked.delete(id);
  }
}

function refreshHandoff() {
  if (state.page !== "handoff" || handoff.kind !== "harness" || handoff.busy || handoff.refreshing) return;
  if (document.visibilityState === "hidden") return;
  loadAllHandoffChats();
}

async function loadHandoffChats() {
  if (handoff.kind !== "account") return;
  const t = handoffTicket();
  handoff.chats = [];
  handoff.picked.clear();
  updateHandoffActions();
  $("handoff-chats").innerHTML = `<div class="state">${tr("Читаем сессии…")}</div>`;
  if (!handoff.from) {
    $("handoff-chats").innerHTML = `<div class="state">${tr("Нет харнессов, из которых можно читать чаты.")}</div>`;
    return;
  }
  try {
    const data = await api("/api/handoff/chats", { harness: handoff.from, hours: handoff.hours, limit: HANDOFF_PICK_LIMIT });
    if (!handoffTicket.current(t)) return;
    handoff.chats = data.chats || [];
  } catch (e) {
    if (!handoffTicket.current(t)) return;
    $("handoff-chats").innerHTML = `<div class="state state-error">${tr("Чаты недоступны: {e}", { e: esc(e.message) })}</div>`;
    return;
  }
  renderHandoffChats();
}

function handoffChatRow(chat) {
  const meta = [
    esc(leaf(String(chat.cwd).replaceAll("\\", "/")) || tr("папка неизвестна")),
    esc(ago(chat.mtime)),
  ];
  if (chat.turns != null) meta.push(`${number(chat.turns)} ${plural(chat.turns, "вопрос", "вопроса", "вопросов")}`);
  if (chat.chars != null) meta.push(tr("{n} симв.", { n: number(chat.chars) }));
  const badge = chat.fits_full == null ? "" :
    `<span class="handoff-badge">${chat.fits_full ? tr("целиком") : tr("выжимка")}</span>`;
  return `<label class="record-row handoff-chat"><input type="checkbox" data-chat="${esc(chat.id)}"${handoff.picked.has(chat.id) ? " checked" : ""}>` +
    `<span class="handoff-chat-main"><strong>${esc(chat.title)}</strong>` +
    `<small>${meta.join(" · ")}</small></span>${badge}</label>`;
}

function renderHandoffChats() {
  const list = $("handoff-chats");
  const scroll = list.scrollTop;
  if (handoff.kind === "account") renderAccountChats();
  else renderGroupedChats();
  list.scrollTop = scroll;
}

function renderGroupedChats() {
  const sources = handoffReadableSources();
  const total = sources.reduce((sum, h) => sum + ((handoff.groups[h.id] || {}).chats || []).length, 0);
  $("handoff-lede").textContent = sources.length
    ? `${total} ${plural(total, "чат", "чата", "чатов")} ${tr("в")} ${sources.length} ${plural(sources.length, "харнессе", "харнессах", "харнессах")} · ${tr("выбрано")} ${handoff.picked.size}`
    : tr("Нет харнессов, из которых можно читать чаты.");
  $("handoff-chats").innerHTML = sources.map((h) => {
    const group = handoff.groups[h.id] || { chats: [], loading: true, error: "", stale: false };
    const dot = h.running == null ? "" : `<i class="harness-dot${h.running ? " on" : ""}" title="${h.running ? tr("Открыт") : tr("Не запущен")}"></i>`;
    let status = "";
    if (group.loading) status = group.chats.length ? tr("Обновляем…") : tr("Читаем…");
    else if (group.error && !group.chats.length) status = tr("Недоступно");
    else if (!group.chats.length) status = tr("Нет чатов за период");
    else status = `${group.chats.length} ${plural(group.chats.length, "чат", "чата", "чатов")}`;
    const stale = group.stale ? `<span class="handoff-stale" title="${esc(group.error)}">${tr("не удалось обновить")}</span>` : "";
    const error = group.error && !group.chats.length
      ? `<div class="state state-error">${tr("Чаты недоступны: {e}", { e: esc(group.error) })}</div>` : "";
    return `<section class="handoff-group" data-harness="${esc(h.id)}">` +
      `<div class="handoff-group-head">${dot}<strong>${esc(h.name)}</strong><span class="subtle">${esc(status)}</span>${stale}</div>` +
      (group.chats.length ? `<div class="record-list handoff-group-list">${group.chats.map(handoffChatRow).join("")}</div>` : "") +
      error + `</section>`;
  }).join("");
  updateHandoffActions();
}

function renderAccountChats() {
  const name = (handoff.harnesses.find((h) => h.id === handoff.from) || {}).name || "";
  $("handoff-lede").textContent = handoff.chats.length
    ? `${name}: ${handoff.chats.length} ${plural(handoff.chats.length, "активный чат", "активных чата", "активных чатов")}`
    : `${name}: ${tr("активных чатов нет")}`;
  $("handoff-chats").innerHTML = handoff.chats.length ? handoff.chats.map(handoffChatRow).join("")
    : `<div class="state">${tr("За выбранный период в этом харнессе ничего не менялось.")}</div>`;
  updateHandoffActions();
}

function updateHandoffActions() {
  const n = handoff.picked.size;
  if (handoff.kind === "account") {
    const source = handoff.harnesses.find((h) => h.id === handoff.from);
    $("handoff-run").disabled = handoff.busy || !n;
    $("handoff-run").textContent = handoff.busy ? tr("Готовим…") : n > 1 ? `${tr("Подготовить смену аккаунта")} (${n})` : tr("Подготовить смену аккаунта");
    $("handoff-hint").textContent = source ? tr("Чаты сохранятся в файлы, затем войдите в {name} под другим аккаунтом", { name: source.name }) : "";
    return;
  }
  $("handoff-run").disabled = handoff.busy || !n || !handoff.to;
  $("handoff-run").textContent = handoff.busy ? tr("Переносим…") : n > 1 ? `${tr("Перенести")} (${n})` : tr("Перенести");
  const target = handoff.harnesses.find((h) => h.id === handoff.to);
  $("handoff-hint").textContent = !target ? "" : target.delivery === "app"
    ? tr("{name}: в приложении откроется новая сессия в папке чата с готовым промптом, отправьте его", { name: target.name })
    : target.delivery === "terminal"
      ? tr("{name} откроется в новом окне консоли в папке чата", { name: target.name })
      : tr("{name}: промпт скопируется, вставьте его в новый чат", { name: target.name });
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

function stopSwitchPoll() {
  clearTimeout(handoff.poll);
  handoff.poll = 0;
}

async function loadSwitch() {
  stopSwitchPoll();
  if (handoff.kind !== "account" || !handoff.from || state.page !== "handoff") return;
  const t = switchTicket();
  try {
    const status = await api("/api/handoff/switch", { harness: handoff.from });
    if (!switchTicket.current(t)) return;
    handoff.switch = status;
  } catch (e) {
    if (!switchTicket.current(t)) return;
    $("handoff-account").innerHTML = `<div class="state state-error">${tr("Состояние входа недоступно: {e}", { e: esc(e.message) })}</div>`;
    return;
  }
  renderSwitch();
  // Опрос нужен только пока владелец перелогинивается.
  if (handoff.switch.pending) handoff.poll = setTimeout(loadSwitch, SWITCH_POLL_MS);
}

function accountLabel(acc) {
  if (!acc || acc.id === null) return tr("вход не выполнен");
  return acc.label || (acc.exact ? tr("аккаунт без имени") : tr("аккаунт не показывается без чтения токенов"));
}

function renderSwitch() {
  const name = (handoff.harnesses.find((h) => h.id === handoff.from) || {}).name || "";
  const { pending, account, changed } = handoff.switch || {};
  if (!pending) {
    $("handoff-account").innerHTML =
      `<span class="micro">${tr("Смена аккаунта")} · ${esc(name)}</span>` +
      `<div>${tr("Сейчас:")} <strong>${esc(accountLabel(account))}</strong></div>` +
      `<ol><li class="now">${tr("Отметьте чаты и подготовьте перенос — они сохранятся в файлы целиком или выжимкой")}</li>` +
      `<li>${tr("Выйдите из {name} и войдите под другим аккаунтом — пароли и токены Brepsi Brain не трогает", { name: esc(name) })}</li>` +
      `<li>${tr("Вернитесь сюда и продолжите: откроется новый чат с файлом переноса")}</li></ol>`;
    return;
  }
  const ready = changed === true;
  const waiting = changed === null ? tr("Сейчас в {name} никто не вошёл — войдите под новым аккаунтом", { name: esc(name) })
    : ready ? (account.exact ? `${tr("Аккаунт сменился:")} <strong>${esc(accountLabel(account))}</strong>` : tr("Вход обновлён — можно продолжать"))
      : tr("Ждём смены аккаунта: выйдите из {name} и войдите под другим", { name: esc(name) });
  $("handoff-account").innerHTML =
    `<span class="micro">${tr("Смена аккаунта")} · ${esc(name)}</span>` +
    `<div>${tr("Подготовлено {n} {unit} {ago}.", { n: number(pending.items.length), unit: plural(pending.items.length, "чат", "чата", "чатов"), ago: esc(ago(pending.created)) })} ` +
    `${tr("Было:")} <strong>${esc(accountLabel(pending.account))}</strong></div>` +
    `<ol><li class="done">${tr("Чаты сохранены в файлы переноса")}</li><li class="${ready ? "done" : "now"}">${waiting}</li>` +
    `<li class="${ready ? "now" : ""}">${tr("Продолжить в новом аккаунте")}</li></ol>` +
    `<div class="handoff-account-actions"><button class="btn-cta" type="button" data-switch="resume">` +
    `${ready ? tr("Продолжить в новом аккаунте") : tr("Продолжить всё равно")}</button>` +
    `<button class="sync-button" type="button" data-switch="cancel">${tr("Отменить")}</button></div>`;
}

async function prepareSwitch() {
  handoff.busy = true;
  updateHandoffActions();
  const ids = handoff.chats.map((c) => c.id).filter((id) => handoff.picked.has(id));
  $("handoff-results").innerHTML = "";
  try {
    await postJson("/api/handoff/switch/prepare", { harness: handoff.from, chats: ids, mode: handoff.mode });
    toast(tr("Чаты сохранены. Смените аккаунт и вернитесь сюда"));
  } catch (e) {
    toast(tr("Не подготовлено: {e}", { e: e.message }));
  }
  handoff.busy = false;
  updateHandoffActions();
  loadSwitch();
}

async function resumeSwitch() {
  if (handoff.busy) return;
  handoff.busy = true;
  updateHandoffActions();
  stopSwitchPoll();
  const out = $("handoff-results");
  out.innerHTML = "";
  let lastPrompt = "";
  try {
    const r = await postJson("/api/handoff/switch/resume", { harness: handoff.from });
    for (const item of r.items) {
      lastPrompt = item.prompt;
      const where = item.delivered === "app" ? (r.prompt ? tr("в общей сессии приложения, промпт в поле ввода")
        : tr("новая сессия открыта в приложении"))
        : item.delivered === "terminal" ? tr("новый чат открыт в окне консоли") : tr("промпт готов к вставке в новый чат");
      const row = document.createElement("div");
      row.className = "surface handoff-result";
      row.innerHTML = `<strong>${esc(item.title)}</strong>` +
        `<span class="subtle">${esc(HANDOFF_MODES[item.mode] || item.mode)} · ${esc(where)}` +
        `${item.launch_error ? ` · ${tr("запуск не удался: {e}", { e: esc(item.launch_error) })}` : ""}</span>` +
        `<code class="handoff-path">${esc(item.path)}</code>` +
        `<button class="sync-button" type="button" data-prompt="${esc(item.prompt)}">${tr("Скопировать промпт")}</button>`;
      out.append(row);
    }
    if (r.prompt) lastPrompt = r.prompt;
  } catch (e) {
    toast(tr("Не продолжено: {e}", { e: e.message }));
  }
  if (lastPrompt && await copyText(lastPrompt)) toast(tr("Промпт переноса скопирован в буфер обмена"));
  handoff.busy = false;
  updateHandoffActions();
  loadSwitch();
}

async function runHandoff() {
  if (handoff.busy) return;
  if (handoff.kind === "account") return prepareSwitch();
  handoff.busy = true;
  updateHandoffActions();
  // Снимок на весь перенос: смена источника, периода, цели или режима посреди await
  // подменяла handoff.chats/to/mode — find давал undefined и кнопка навсегда «Переносим…»,
  // а запуск уходил не тому харнессу, для которого собраны файлы.
  const chats = [];
  for (const group of Object.values(handoff.groups)) {
    for (const chat of group.chats || []) {
      if (handoff.picked.has(chat.id)) chats.push(chat);
    }
  }
  const to = handoff.to;
  const mode = handoff.mode;
  try {
    const out = $("handoff-results");
    out.innerHTML = "";
    let lastPrompt = "";
    // Сначала все файлы, потом один запуск: несколько ссылок подряд в Claude теряются.
    const built = [];
    for (const chat of chats) {
      const id = chat.id;
      const row = document.createElement("div");
      row.className = "surface handoff-result";
      row.innerHTML = `<strong>${esc(chat.title)}</strong><span class="subtle">${tr("Готовим перенос…")}</span>`;
      out.append(row);
      try {
        const r = await postJson("/api/handoff", { chat: id, target: to, mode, launch: false });
        built.push({ chat, row, r });
      } catch (e) {
        row.innerHTML = `<strong>${esc(chat.title)}</strong><span class="state-error">${tr("Не перенесён: {e}", { e: esc(e.message) })}</span>`;
      }
    }
    let launched = { items: built.map(() => ({ delivered: "clipboard" })), prompt: null };
    if (built.length) {
      try {
        launched = await postJson("/api/handoff/launch", {
          target: to, items: built.map(({ r }) => ({ path: r.path, prompt: r.prompt, cwd: r.cwd || "" })),
        });
      } catch (e) {
        toast(tr("Запуск не удался: {e}", { e: e.message }));
      }
    }
    built.forEach(({ chat, row, r }, i) => {
      const d = launched.items[i];
      lastPrompt = r.prompt;
      const where = d.delivered === "app" ? (launched.prompt ? tr("в общей сессии приложения, промпт в поле ввода")
        : tr("открыт в приложении, промпт в поле ввода"))
        : d.delivered === "terminal" ? tr("запущен в новом окне консоли") : tr("промпт готов к вставке");
      row.innerHTML = `<strong>${esc(chat.title)}</strong>` +
        `<span class="subtle">${esc(HANDOFF_MODES[r.mode] || r.mode)} · ${esc(where)}${r.note ? ` · ${esc(r.note)}` : ""}` +
        `${d.launch_error ? ` · ${tr("запуск не удался: {e}", { e: esc(d.launch_error) })}` : ""}</span>` +
        `<code class="handoff-path">${esc(r.path)}</code>` +
        `<button class="sync-button" type="button" data-prompt="${esc(r.prompt)}">${tr("Скопировать промпт")}</button>`;
    });
    if (launched.prompt) lastPrompt = launched.prompt;
    // Последний промпт сразу в буфере: для Kimi Code и Qoder это и есть доставка.
    if (lastPrompt && await copyText(lastPrompt)) toast(tr("Промпт переноса скопирован в буфер обмена"));
  } finally {
    handoff.busy = false;
    updateHandoffActions();
  }
}

function initHandoff() {
  $("handoff-from").addEventListener("click", (e) => {
    if (handoff.kind !== "account") return;
    const chip = e.target.closest(".chip");
    if (!chip || chip.dataset.harness === handoff.from) return;
    handoff.from = chip.dataset.harness;
    savePref("handoff.from", handoff.from);
    pickHandoffTarget();
    renderHandoffRoute();
    loadHandoffChats();
    loadSwitch();
  });
  $("handoff-to").addEventListener("click", (e) => {
    if (e.target.closest("#handoff-add")) {
      openHarnessDialog();
      return;
    }
    const chip = e.target.closest(".chip");
    if (!chip || chip.disabled || !chip.dataset.harness) return;
    handoff.to = chip.dataset.harness;
    savePref("handoff.to", handoff.to);
    renderHandoffRoute();
  });
  $("harness-add-form").addEventListener("submit", saveHarness);
  $("harness-add-cancel").addEventListener("click", () => $("harness-add").close());
  $("harness-custom-list").addEventListener("click", (e) => {
    const button = e.target.closest("button[data-remove-harness]");
    if (button) deleteHarness(button.dataset.removeHarness);
  });
  $("handoff-hours").addEventListener("click", (e) => {
    const seg = e.target.closest(".seg");
    if (!seg) return;
    handoff.hours = Number(seg.dataset.hours);
    setPressed($("handoff-hours"), (b) => b === seg);
    if (handoff.kind === "account") loadHandoffChats();
    else loadAllHandoffChats();
  });
  $("handoff-mode").addEventListener("click", (e) => {
    const seg = e.target.closest(".seg");
    if (!seg) return;
    handoff.mode = seg.dataset.mode;
    setPressed($("handoff-mode"), (b) => b === seg);
  });
  $("handoff-chats").addEventListener("change", (e) => {
    const box = e.target.closest("input[data-chat]");
    if (!box) return;
    if (box.checked && handoff.picked.size >= HANDOFF_PICK_LIMIT) {
      box.checked = false;
      toast(tr("Можно выбрать не больше {n} чатов", { n: HANDOFF_PICK_LIMIT }));
      return;
    }
    if (box.checked) handoff.picked.add(box.dataset.chat);
    else handoff.picked.delete(box.dataset.chat);
    updateHandoffActions();
  });
  $("handoff-run").addEventListener("click", runHandoff);
  $("handoff-kind").addEventListener("click", (e) => {
    const seg = e.target.closest(".seg");
    if (!seg || seg.dataset.kind === handoff.kind) return;
    handoff.kind = seg.dataset.kind;
    savePref("handoff.kind", handoff.kind);
    handoff.picked.clear();
    handoff.chats = [];
    pickHandoffSource();
    renderHandoffRoute();
    if (handoff.kind === "account") loadHandoffChats();
    else loadAllHandoffChats();
    loadSwitch();
  });
  $("handoff-account").addEventListener("click", async (e) => {
    const button = e.target.closest("button[data-switch]");
    if (!button) return;
    if (button.dataset.switch === "resume") return resumeSwitch();
    stopSwitchPoll();
    await postJson("/api/handoff/switch/cancel", { harness: handoff.from }).catch(() => null);
    loadSwitch();
  });
  $("handoff-results").addEventListener("click", async (e) => {
    const button = e.target.closest("button[data-prompt]");
    if (button) toast(await copyText(button.dataset.prompt) ? tr("Промпт скопирован") : tr("Буфер обмена недоступен"));
  });
  document.addEventListener("visibilitychange", refreshHandoff);
  setInterval(refreshHandoff, 60000);
}

// -- модели и железо ---------------------------------------------------------
//
// Страница показывает снимок /api/local/status. Установка и замер идут фоновым
// заданием, поэтому пока job.active страница перечитывает статус. Слушатели
// висят на контейнерах: renderModels пересобирает их innerHTML целиком.

const MODELS_POLL_MS = 1200;
// Модели основного провайдера подгружаются по кнопке: их список — сетевой
// запрос к шлюзу, а снимок статуса обновляется каждые 1,2 с во время установки.
let modelsProviderModels = [];
const MODEL_TASKS = [
  ["distill", tr("Карточки")],
  ["answer", tr("Ответы")],
  ["handoff", tr("Перенос")],
  ["projects", tr("Проекты")],
];
const ROLE_NAMES = Object.fromEntries(MODEL_TASKS);
// Подсказка по фактическому требованию модели, а не по тиру: 27B и 32B
// не помещаются в 16 ГБ, хотя оба числятся в «высоком» тире.
function modelVramHint(model) {
  const mb = Number(model.min_vram_mb) || 0;
  return mb > 0
    ? tr("Нужно ~{n} ГБ видеопамяти", { n: Math.ceil(mb / 1024) })
    : tr("Поместится на процессор или в 2 ГБ видеопамяти");
}
const BACKEND_NAMES = { cuda: "CUDA", vulkan: "Vulkan", cpu: "CPU" };
const TIER_NAMES = {
  minimal: tr("Минимальный"), low: tr("Низкий"), mid: tr("Средний"), high: tr("Высокий"),
};
const MODEL_SOURCE_NAMES = { ollama: "Ollama", huggingface: "Hugging Face" };
const TRIGGER_NAMES = {
  manual: tr("вручную"), install: tr("установка"), remove: tr("удаление"), bench: tr("замер"),
  distill: tr("карточки"), answer: tr("ответы"), handoff: tr("перенос"), projects: tr("проекты"),
};
const PHASE_NAMES = {
  runtime: tr("Движок"), unpack: tr("Распаковка"), download: tr("Скачивание"), import: tr("Импорт"), verify: tr("Проверка"),
  bench: tr("Замер"), done: tr("Готово"), error: tr("Ошибка"),
};

let modelsStatus = null;
let modelsPoll = 0;
let modelsHadActiveJob = false;
let modelsInstallDismissed = false;
let modelsOfflineSaving = false;
const localStatusTicket = latest();
const wizardTicket = latest();
const wizard = { step: 1, id: "", root: "", busy: false, timer: 0, install: null };

function showModelsError(message) {
  const box = $("models-error");
  box.textContent = message;
  box.hidden = false;
  toast(message, 8000);
}

async function loadModels() {
  if (state.page !== "models") return;
  const t = localStatusTicket();
  clearTimeout(modelsPoll);
  try {
    const data = await api("/api/local/status");
    if (!localStatusTicket.current(t) || state.page !== "models") return;
    adoptModelsStatus(data);
  } catch (e) {
    if (!localStatusTicket.current(t)) return;
    showModelsError(tr("Модели не загрузились: {e}", { e: e.message }));
  }
}

// Снимок из ответа действия свежее любого опроса, ушедшего до него: старый
// `active:false` иначе гасил прогресс только что начатой установки.
function adoptActionStatus(data) {
  localStatusTicket();
  adoptModelsStatus(data);
}

// Ответ действия уже содержит новый статус: страница перерисовывается сразу,
// а опрос продолжается только пока фоновое задание действительно активно.
function adoptModelsStatus(data) {
  const wasActive = modelsHadActiveJob;
  const active = Boolean(data.install && data.install.active);
  modelsStatus = data;
  modelsHadActiveJob = active;
  $("models-error").hidden = true;
  // Пока идёт задание, перерисовывается только прогресс: опрос раз в 1,2 с иначе
  // сбрасывал бы несохранённый ввод в формах движка и моделей задач.
  if (wasActive && active) {
    renderModelsInstall(data.install);
    renderModelsOffline(data);
  } else {
    renderModels();
  }
  clearTimeout(modelsPoll);
  if (active) modelsPoll = setTimeout(loadModels, MODELS_POLL_MS);
  if (wasActive && !active && data.install) {
    const failed = data.install.phase === "error";
    const kind = data.install.kind;
    const finished = kind === "bench" ? tr("Замер завершён")
      : kind === "runtime" ? tr("Движок установлен") : tr("Установка завершена");
    toast(failed ? installFailure(data.install) : finished, failed ? 10000 : 5000);
  }
}

function renderModels() {
  const data = modelsStatus;
  if (!data) return;
  $("models-banner").hidden = (data.route || {}).kind !== "none";
  $("models-banner-text").textContent = (data.settings || {}).offline_only
    ? tr("Включены только локальные модели, но ни одна ещё не установлена. Пока её нет, карточки и ответы ждут.")
    : tr("Платная модель не настроена. Установите локальную — она работает без интернета");
  renderModelsHardware(data.hardware || {});
  renderModelsRecommended(data);
  renderModelsRuntime(data);
  renderModelsCatalog(data);
  renderModelsInstall(data.install);
  renderModelsTasks(data);
  renderModelsOffline(data);
}

function renderModelsHardware(hw) {
  const vram = Number(hw.vram_mb) || 0;
  const ram = Number(hw.ram_mb) || 0;
  const facts = [
    [hw.backend === "cpu" && !hw.name ? tr("Процессор") : tr("Видеокарта"), esc(hw.name || tr("Не определено"))],
    [tr("Видеопамять"), vram ? `${(vram / 1024).toLocaleString(LOCALE, { maximumFractionDigits: 1 })} ${tr("ГБ")}` : "—"],
    [tr("Ускорение"), esc(BACKEND_NAMES[hw.backend] || hw.backend || "—")],
    [tr("Класс"), esc(TIER_NAMES[hw.tier] || hw.tier || "—")],
    [tr("Оперативная память"), ram ? `${(ram / 1024).toLocaleString(LOCALE, { maximumFractionDigits: 1 })} ${tr("ГБ")}` : "—"],
  ];
  $("models-hardware").innerHTML = `<dl class="model-facts-list">` +
    facts.map(([label, value]) => `<dt>${label}</dt><dd>${value}</dd>`).join("") + `</dl>` +
    ((hw.notes || []).length ? `<ul class="model-notes">${hw.notes.map((note) => `<li>${esc(note)}</li>`).join("")}</ul>` : "");
}

// Рекомендация считается на сервере (/api/local/status): модель по VRAM и тиру
// плюс окно контекста. Кнопка применяет настройки движка и пересоздаёт модель
// с нужным num_ctx.
function renderModelsRecommended(data) {
  const rec = data.recommended || {};
  const model = (data.catalog || []).find((item) => item.id === rec.id);
  const note = $("models-recommended-note");
  const button = $("models-recommended-apply");
  if (!model) {
    note.textContent = tr("Рекомендация пока не рассчитана.");
    button.disabled = true;
    return;
  }
  note.textContent = tr(
    "Модель: {name} · {params} · {size}. Контекст: {context} токенов, режим по требованию.",
    { name: model.name, params: model.params, size: formatBytes(model.size_bytes), context: number(rec.context_tokens) },
  ) + " " + (model.installed
    ? tr("Уже установлена: кнопка применит настройки и пересоздаст её с рекомендуемым контекстом.")
    : tr("Модель будет скачана и подключена к задачам, которым нужен локальный маршрут."));
  button.disabled = Boolean(data.install && data.install.active);
}

function renderModelsRuntime(data) {
  const runtime = data.runtime || {};
  const settings = data.settings || {};
  const installed = Boolean(runtime.installed);
  const running = Boolean(runtime.running);
  const contexts = [8192, 16384, 32768];
  // Конфиг принимает любое значение кратное 1024; select не должен молча
  // подменять сохранённое число при следующем «Сохранить».
  const current = Number(settings.context_tokens) || 16384;
  if (!contexts.includes(current)) contexts.unshift(current);
  const modes = [["on_demand", tr("По требованию")], ["always", tr("Всегда запущен")]];
  $("models-runtime").innerHTML =
    `<dl class="model-facts-list">` +
    `<dt>${tr("Движок")}</dt><dd>${installed ? tr("Установлен") : tr("Не установлен")}</dd>` +
    `<dt>${tr("Версия")}</dt><dd>${esc(runtime.version || "—")}</dd>` +
    `<dt>${tr("Состояние")}</dt><dd>${running ? tr("Работает на порту {port}", { port: number(runtime.port) }) : tr("Остановлен")}</dd>` +
    `<dt>${tr("Папка моделей")}</dt><dd><code class="model-path">${esc(data.models_root || "—")}</code></dd>` +
    `<dt>${tr("Свободно")}</dt><dd>${data.free_bytes == null ? "—" : esc(formatBytes(data.free_bytes))}</dd>` +
    `</dl>` +
    `<div class="model-actions">` +
    (!installed ? `<button class="sync-button" data-models-runtime="install" type="button">${tr("Установить движок")}</button>` : "") +
    (installed && !running ? `<button class="sync-button" data-models-local="start" type="button">${tr("Запустить")}</button>` : "") +
    (running ? `<button class="sync-button" data-models-local="stop" type="button">${tr("Остановить")}</button>` : "") +
    `</div>` +
    `<form class="models-form">` +
    `<fieldset class="model-modes"><legend>${tr("Режим")}</legend>` +
    modes.map(([value, label]) => `<label class="check"><input type="radio" name="local-mode" value="${value}"${settings.mode === value ? " checked" : ""}> ${label}</label>`).join("") +
    `</fieldset>` +
    `<div class="model-form-row">` +
    `<label class="field"><span>${tr("Простой, секунд")}</span><input id="models-idle" class="input" type="number" min="10" max="3600" value="${Number(settings.idle_timeout_seconds) || 120}"></label>` +
    `<label class="field"><span>${tr("Контекст, токенов")}</span><select id="models-context" class="input">` +
    contexts.map((value) => `<option value="${value}"${value === current ? " selected" : ""}>${number(value)}</option>`).join("") +
    `</select></label>` +
    `</div>` +
    `<button class="btn-cta" type="submit">${tr("Сохранить")}</button>` +
    `</form>`;
}

function renderModelsCatalog(data) {
  const onlyFits = $("models-only-fits").checked;
  const onlyInstalled = $("models-only-installed").checked;
  const models = (data.catalog || []).filter((model) =>
    (!onlyFits || model.fits) && (!onlyInstalled || model.installed));
  $("models-catalog").innerHTML = models.length
    ? models.map(modelCard).join("")
    : `<div class="state">${tr("Под фильтры не подошла ни одна модель.")}</div>`;
}

function modelCard(model) {
  const roles = (model.roles || []).map((role) => ROLE_NAMES[role] || role).join(" · ");
  const languages = String(model.languages || "").split(",").filter(Boolean)
    .map((code) => (code === "ru" ? tr("Русский") : code === "en" ? tr("Английский") : code)).join(", ");
  const summary = LANG === "en" ? (model.summary_en || model.summary_ru) : model.summary_ru;
  const actions = model.installed
    ? `<button class="sync-button" data-model-action="remove" data-model-id="${esc(model.id)}" type="button">${tr("Удалить")}</button>` +
      `<button class="sync-button" data-model-action="bench" data-model-id="${esc(model.id)}" type="button">${tr("Замерить")}</button>`
    : `<button class="sync-button" data-model-action="install" data-model-id="${esc(model.id)}" type="button">${tr("Установить")}</button>`;
  return `<article class="model-card${model.installed ? " is-installed" : ""}">` +
    `<div class="model-card-top"><strong>${esc(model.name)}</strong>` +
    (model.recommended ? `<span class="model-badge">${tr("Рекомендуем")}</span>` : "") + `</div>` +
    `<p class="model-meta">${esc(model.params)} · ${esc(model.quant)} · ${esc(formatBytes(model.size_bytes))}</p>` +
    `<p class="model-summary">${esc(summary)}</p>` +
    (roles ? `<p class="model-roles">${tr("Роли:")} ${esc(roles)}</p>` : "") +
    `<p class="model-meta-line"><span>${tr("Языки:")} ${esc(languages || "—")}</span><span>${esc(MODEL_SOURCE_NAMES[model.source] || model.source)}</span></p>` +
    `<p class="subtle">${esc(modelVramHint(model))}</p>` +
    `<div class="model-actions">${actions}</div>` +
    `</article>`;
}

function formatEta(seconds) {
  const total = Math.max(0, Math.round(Number(seconds) || 0));
  if (total < 60) return tr("{n} с", { n: total });
  const minutes = Math.round(total / 60);
  return minutes >= 60
    ? tr("{h} ч {m} мин", { h: Math.floor(minutes / 60), m: minutes % 60 })
    : tr("{m} мин", { m: minutes });
}

// Сколько получено, сколько осталось, скорость и оставшееся время. Этапы без
// счётчика байт (проверка суммы, импорт) дают пустую строку.
function installFacts(progress) {
  const total = Number(progress.total) || 0;
  const done = Number(progress.done) || 0;
  if (!total && !done) return "";
  const amounts = { done: formatBytes(done), total: formatBytes(total) };
  const parts = [progress.phase === "unpack" ? tr("Распаковано {done} из {total}", amounts)
    : total ? tr("Скачано {done} из {total}", amounts) : tr("Скачано {done}", amounts)];
  if (total > done) parts.push(tr("осталось {left}", { left: formatBytes(total - done) }));
  if (Number(progress.speed_bps) > 0) parts.push(tr("{speed}/с", { speed: formatBytes(progress.speed_bps) }));
  if (Number(progress.eta_seconds) > 0) parts.push(tr("ещё примерно {eta}", { eta: formatEta(progress.eta_seconds) }));
  return parts.join(" · ");
}

// Без известного размера полоса пульсирует: пустая выглядела бы зависшей.
function installBar(progress) {
  const total = Number(progress.total) || 0;
  const pct = total ? Math.min(100, Math.round((Number(progress.done) || 0) / total * 100)) : 100;
  return `<div class="progress-track"><i class="is-grad${total ? "" : " is-wait"}" style="width:${pct}%"></i></div>`;
}

function installFailure(progress) {
  const reason = progress.error || progress.status || "";
  if (reason === "отменено") return progress.kind === "bench" ? tr("Замер отменён") : tr("Установка отменена");
  return progress.kind === "bench" ? tr("Замер не удался: {e}", { e: reason }) : tr("Установка не удалась: {e}", { e: reason });
}

function renderModelsInstall(progress) {
  const host = $("models-install");
  if (progress && progress.active) modelsInstallDismissed = false;
  const failed = Boolean(progress && !progress.active && progress.phase === "error" && !modelsInstallDismissed);
  if (failed) {
    host.hidden = false;
    host.innerHTML = `<p class="form-error">${esc(installFailure(progress))}</p>` +
      `<div class="model-install-row"><span></span><span class="model-install-buttons">` +
      (progress.kind === "bench" ? "" : `<button class="sync-button" data-models-retry type="button">${tr("Повторить")}</button>`) +
      `<button class="sync-button" data-models-dismiss type="button">${tr("Закрыть")}</button></span></div>`;
    return;
  }
  if (!progress || !progress.active) {
    host.hidden = true;
    host.innerHTML = "";
    return;
  }
  const title = progress.kind === "bench" ? tr("Идёт замер")
    : progress.kind === "runtime" ? tr("Установка движка") : tr("Идёт установка");
  host.hidden = false;
  host.innerHTML = `<div class="surface-head"><h2 class="section-title">${title}</h2>` +
    `<span class="subtle">${esc(PHASE_NAMES[progress.phase] || progress.phase || "")}</span></div>` +
    `<p class="subtle">${esc(progress.status || "")}</p>` +
    installBar(progress) +
    `<div class="model-install-row"><span class="subtle">${esc(installFacts(progress))}</span>` +
    `<button class="sync-button" data-models-cancel type="button">${tr("Отмена")}</button></div>`;
}

function retryModelsJob() {
  const job = (modelsStatus || {}).install;
  if (!job) return;
  if (job.kind === "runtime") return startModelsJob("/api/local/runtime", { action: "install" }, tr("Установка движка началась"));
  return startModelsJob("/api/local/install", { id: job.id }, tr("Установка началась"));
}

function renderModelsTasks(data) {
  const route = data.route || {};
  const current = data.tasks || {};
  const rows = [];
  const seen = new Set();
  const add = (value, label) => {
    if (seen.has(value)) return;
    seen.add(value);
    rows.push({ value, label });
  };
  add("", route.model ? tr("По умолчанию ({model})", { model: route.model }) : tr("По умолчанию"));
  for (const model of data.catalog || []) {
    if (model.installed) add(`brepsi/${model.id}`, `brepsi/${model.id} · ${model.name}`);
  }
  for (const id of data.installed || []) add(`brepsi/${id}`, `brepsi/${id}`);
  for (const name of modelsProviderModels) add(name, name);
  for (const [task] of MODEL_TASKS) {
    const value = current[task] || "";
    if (value) add(value, value);
  }
  const optionsFor = (selected) => rows.map(({ value, label }) =>
    `<option value="${esc(value)}"${value === selected ? " selected" : ""}>${esc(label)}</option>`).join("");
  $("models-tasks").innerHTML = `<table class="table model-task-table"><thead><tr>` +
    `<th>${tr("Задача")}</th><th>${tr("Модель")}</th></tr></thead><tbody>` +
    MODEL_TASKS.map(([task, label]) =>
      `<tr><td>${label}</td><td><select id="models-task-${task}" class="input" aria-label="${esc(label)}">` +
      optionsFor(current[task] || "") + `</select></td></tr>`).join("") +
    `</tbody></table>` +
    `<p class="model-meta-line"><span title="${tr("Модель маршрута — основная модель провайдера; пустое поле задачи наследует её.")}">${tr("Пусто — модель маршрута. Для удалённого провайдера подгрузите его список.")}</span>` +
    `<button id="models-provider-models" class="sync-button" type="button">${tr("Модели провайдера")}</button></p>`;
}

function renderModelsOffline(data) {
  const localModel = (data.route || {}).local_model || "";
  const offline = Boolean((data.settings || {}).offline_only);
  const box = $("models-offline");
  if (!modelsOfflineSaving) {
    box.checked = offline;
    box.disabled = false;
  }
  $("models-offline-note").textContent = offline
    ? (localModel
      ? tr("Запросы идут только в локальную модель {model}. Наружу ничего не уходит.", { model: localModel })
      : tr("Наружу ничего не уходит. Карточки и ответы заработают, как только установится локальная модель."))
    : (localModel
      ? tr("Локальный маршрут: {model}. Записи, доступные только локальной модели, пойдут ему.", { model: localModel })
      : tr("Можно включить заранее: платный провайдер перестанет использоваться ещё до установки модели."));
}

async function startModelsJob(path, body, message) {
  try {
    const data = await postJson(path, body);
    adoptActionStatus(data);
    toast(message);
  } catch (e) {
    showModelsError(e.status === 409 ? tr("Задача уже идёт.") : tr("Не удалось начать: {e}", { e: e.message }));
  }
}

async function onModelsCatalogClick(e) {
  const button = e.target.closest("[data-model-action]");
  if (!button) return;
  const id = button.dataset.modelId;
  const action = button.dataset.modelAction;
  if (action === "install") return startModelsJob("/api/local/install", { id }, tr("Установка началась"));
  if (action === "bench") return startModelsJob("/api/local/bench", { id }, tr("Замер начался"));
  if (action !== "remove") return;
  button.disabled = true;
  try {
    const data = await postJson("/api/local/models/remove", { id });
    adoptActionStatus(data);
    toast(tr("Модель удалена"));
  } catch (err) {
    button.disabled = false;
    showModelsError(tr("Модель не удалилась: {e}", { e: err.message }));
  }
}

async function onModelsRuntimeClick(e) {
  const runtime = e.target.closest("[data-models-runtime]");
  if (runtime) {
    return startModelsJob("/api/local/runtime", { action: runtime.dataset.modelsRuntime }, tr("Установка движка началась"));
  }
  const local = e.target.closest("[data-models-local]");
  if (!local) return;
  const action = local.dataset.modelsLocal;
  try {
    const data = await postJson(action === "start" ? "/api/local/start" : "/api/local/stop", {});
    adoptActionStatus(data);
    toast(action === "start" ? tr("Движок запущен") : tr("Движок остановлен"));
  } catch (err) {
    showModelsError(tr("Движок не переключился: {e}", { e: err.message }));
  }
}

async function applyModelsRecommended() {
  const button = $("models-recommended-apply");
  if (button) button.disabled = true;
  try {
    const data = await postJson("/api/local/recommended", {});
    adoptActionStatus(data);
    toast(tr("Применяю рекомендованные настройки и ставлю модель"));
  } catch (e) {
    if (button) button.disabled = false;
    showModelsError(e.status === 409 ? tr("Задача уже идёт.") : tr("Не удалось применить: {e}", { e: e.message }));
  }
}

async function saveModelsRuntime(e) {
  e.preventDefault();
  const checked = $("models-runtime").querySelector?.('input[name="local-mode"]:checked');
  const mode = checked ? checked.value : ((modelsStatus && modelsStatus.settings && modelsStatus.settings.mode) || "on_demand");
  const idle = Number($("models-idle").value);
  if (!Number.isInteger(idle) || idle < 10 || idle > 3600) {
    showModelsError(tr("Простой должен быть целым числом секунд от 10 до 3600."));
    return;
  }
  try {
    const data = await postJson("/api/local/settings", {
      mode, idle_timeout_seconds: idle, context_tokens: Number($("models-context").value),
    });
    adoptActionStatus(data);
    toast(tr("Сохранено"));
  } catch (err) {
    showModelsError(tr("Настройки движка не сохранились: {e}", { e: err.message }));
  }
}

async function saveModelsTasks() {
  const models = {};
  for (const [task] of MODEL_TASKS) {
    const select = $(`models-task-${task}`);
    // Пустая строка снимает модель, а таблица пуста до ответа /api/local/status:
    // сохранение в этот момент стёрло бы все четыре назначения.
    if (select) models[task] = select.value;
  }
  if (!Object.keys(models).length) {
    showModelsError(tr("Модели задач ещё не загрузились: сохранение отменено"));
    return;
  }
  try {
    const data = await postJson("/api/settings/tasks", { models });
    if (modelsStatus) {
      modelsStatus.tasks = (data && data.tasks) || models;
      renderModelsTasks(modelsStatus);
    }
    toast(tr("Модели задач сохранены"));
  } catch (e) {
    showModelsError(tr("Модели задач не сохранились: {e}", { e: e.message }));
  }
}

async function loadProviderModels() {
  const route = (modelsStatus && modelsStatus.route) || {};
  if (!route.base_url) {
    showModelsError(tr("Сначала настройте провайдера в «Настройках»."));
    return;
  }
  const button = $("models-provider-models");
  if (button) button.disabled = true;
  try {
    const data = await postJson("/api/models", { base_url: route.base_url });
    modelsProviderModels = data.models || [];
    if (modelsStatus) renderModelsTasks(modelsStatus);
    toast(tr("Моделей провайдера: {n}", { n: number(modelsProviderModels.length) }));
  } catch (e) {
    showModelsError(tr("Список моделей провайдера не получен: {e}", { e: e.message }));
  } finally {
    const again = $("models-provider-models");
    if (again) again.disabled = false;
  }
}

async function saveModelsOffline(enabled) {
  if (!modelsStatus) return;
  const box = $("models-offline");
  modelsOfflineSaving = true;
  box.disabled = true;
  try {
    const data = await postJson("/api/local/settings", { offline_only: enabled });
    modelsOfflineSaving = false;
    adoptActionStatus(data);
    toast(tr("Сохранено"));
  } catch (e) {
    modelsOfflineSaving = false;
    renderModelsOffline(modelsStatus);
    showModelsError(tr("Не сохранилось: {e}", { e: e.message }));
  }
  const inWizard = $("wizard-offline");
  if (inWizard) inWizard.checked = Boolean((modelsStatus.settings || {}).offline_only);
}

async function cancelModelsJob() {
  try {
    const data = await postJson("/api/local/cancel", {});
    if (!data.cancelled) {
      toast(tr("Отменять нечего."));
      return;
    }
    toast(tr("Отмена запрошена"));
    loadModels();
  } catch (e) {
    showModelsError(tr("Отмена не сработала: {e}", { e: e.message }));
  }
}

// -- мастер установки --------------------------------------------------------

function openModelsWizard() {
  if (!modelsStatus) return;
  const fits = (modelsStatus.catalog || []).filter((model) => model.fits);
  const best = fits.find((model) => model.recommended) || fits[0];
  Object.assign(wizard, {
    step: 1, id: best ? best.id : "", root: modelsStatus.models_root || "",
    busy: false, install: null,
  });
  clearTimeout(wizard.timer);
  $("wizard-error").hidden = true;
  $("models-wizard").showModal();
  renderWizard();
}

function renderWizard() {
  $("wizard-step-label").textContent = wizard.step === 1 ? tr("Шаг 1 из 3: модель")
    : wizard.step === 2 ? tr("Шаг 2 из 3: папка") : tr("Шаг 3 из 3: установка");
  $("wizard-back").hidden = wizard.step === 1 || (wizard.step === 3 && wizard.busy);
  $("wizard-next").hidden = wizard.step === 3;
  if (wizard.step === 1) renderWizardModel();
  if (wizard.step === 2) renderWizardFolder();
  if (wizard.step === 3) renderWizardInstall();
}

function renderWizardModel() {
  const fits = ((modelsStatus || {}).catalog || []).filter((model) => model.fits);
  if (!fits.length) {
    $("wizard-body").innerHTML = `<p class="state">${tr("Ни одна модель каталога не подходит к этому железу.")}</p>`;
    $("wizard-next").disabled = true;
    return;
  }
  if (!fits.some((model) => model.id === wizard.id)) wizard.id = fits[0].id;
  const chosen = fits.find((model) => model.id === wizard.id) || fits[0];
  $("wizard-next").disabled = false;
  $("wizard-body").innerHTML =
    `<p class="wizard-lede">${tr("Подберём модель под ваше железо. Позже её можно заменить.")}</p>` +
    `<label class="field"><span>${tr("Модель")}</span><select id="wizard-model" class="input">` +
    fits.map((model) => `<option value="${esc(model.id)}"${model.id === wizard.id ? " selected" : ""}>` +
      `${esc(model.name)} · ${esc(model.params)}${model.recommended ? ` · ${tr("Рекомендуем")}` : ""}</option>`).join("") +
    `</select></label>` +
    `<div id="wizard-model-note">${wizardModelNote(chosen)}</div>` +
    `<label class="check"><input id="wizard-offline" type="checkbox"${(modelsStatus.settings || {}).offline_only ? " checked" : ""}> ${tr("Использовать только локальные модели")}</label>`;
}

function wizardModelNote(model) {
  if (!model) return "";
  const summary = LANG === "en" ? (model.summary_en || model.summary_ru) : model.summary_ru;
  return `<p class="model-summary">${esc(summary)}</p>` +
    `<p class="subtle">${tr("Размер: {size}", { size: esc(formatBytes(model.size_bytes)) })} · ${esc(modelVramHint(model))}</p>` +
    `<p class="model-roles">${tr("Роли:")} ${esc((model.roles || []).map((role) => ROLE_NAMES[role] || role).join(" · "))}</p>`;
}

function renderWizardFolder() {
  const fits = ((modelsStatus || {}).catalog || []).filter((model) => model.fits);
  const model = fits.find((item) => item.id === wizard.id);
  $("wizard-next").disabled = false;
  $("wizard-body").innerHTML =
    `<p class="wizard-lede">${tr("Куда скачивать движок и модель?")}</p>` +
    `<div class="wizard-folder-row"><input id="wizard-folder" class="input" value="${esc(wizard.root)}" spellcheck="false" autocomplete="off">` +
    `<button id="wizard-pick" class="sync-button" type="button">${tr("Выбрать…")}</button></div>` +
    `<p id="wizard-folder-note" class="subtle">${tr("Понадобится около 4 ГБ на движок и {size} на модель. Диск должен пережить переустановку системы.", { size: esc(model ? formatBytes(model.size_bytes) : "") })}</p>`;
}

function renderWizardInstall() {
  const progress = wizard.install;
  const host = $("wizard-body");
  if (!progress) {
    host.innerHTML = `<p class="state">${tr("Запускаю установку…")}</p>`;
    return;
  }
  if (progress.phase === "done" && !progress.active) {
    host.innerHTML = `<p class="wizard-done">${tr("Готово. Модель подключена")}</p>` +
      `<p class="subtle">${tr("Проверьте её на первой синхронизации: карточки начнут писаться локально.")}</p>` +
      `<button class="btn-cta" data-wizard="check" type="button">${tr("Проверить на записи")}</button>`;
    return;
  }
  if (progress.phase === "error") {
    host.innerHTML = `<p class="form-error">${esc(installFailure(progress))}</p>` +
      `<button class="sync-button" data-wizard="retry" type="button">${tr("Повторить")}</button>`;
    return;
  }
  const facts = installFacts(progress);
  host.innerHTML = `<p class="wizard-lede">${esc(progress.status || tr("Установка идёт…"))}</p>` +
    installBar(progress) +
    `<p class="subtle">${esc(PHASE_NAMES[progress.phase] || progress.phase || "")}${facts ? " · " + esc(facts) : ""}</p>` +
    `<button class="sync-button" data-wizard="cancel" type="button">${tr("Отмена")}</button>`;
}

async function onWizardNext() {
  if (wizard.step === 1) {
    wizard.step = 2;
    renderWizard();
    return;
  }
  if (wizard.step === 2) await startWizardInstall();
}

async function startWizardInstall() {
  if (!wizard.id) return;
  const folderInput = $("wizard-folder");
  const folder = (folderInput ? folderInput.value : wizard.root).trim();
  wizard.root = folder;
  wizard.busy = true;
  wizard.install = null;
  $("wizard-error").hidden = true;
  wizard.step = 3;
  renderWizard();
  try {
    if (folder && folder !== ((modelsStatus || {}).models_root || "")) {
      modelsStatus = await postJson("/api/local/settings", { models_root: folder });
    }
    const data = await postJson("/api/local/install", { id: wizard.id });
    modelsStatus = data;
    wizard.install = data.install || null;
    wizard.busy = Boolean(data.install && data.install.active);
    renderWizard();
    pollWizard();
  } catch (e) {
    wizard.busy = false;
    $("wizard-error").textContent = tr("Установка не началась: {e}", { e: e.message });
    $("wizard-error").hidden = false;
    wizard.step = 2;
    renderWizard();
  }
}

function pollWizard() {
  clearTimeout(wizard.timer);
  if (wizard.step !== 3 || !$("models-wizard").open) return;
  wizard.timer = setTimeout(async () => {
    if (wizard.step !== 3 || !$("models-wizard").open) return;
    const t = wizardTicket();
    try {
      const data = await api("/api/local/status");
      if (!wizardTicket.current(t) || wizard.step !== 3) return;
      modelsStatus = data;
      wizard.install = data.install || null;
      wizard.busy = Boolean(data.install && data.install.active);
      renderWizard();
      if (wizard.busy) pollWizard();
      else if (state.page === "models") loadModels();
    } catch (e) {
      $("wizard-error").textContent = tr("Статус установки не получен: {e}", { e: e.message });
      $("wizard-error").hidden = false;
      // Один сбой опроса не должен замораживать прогресс идущей установки.
      if (wizardTicket.current(t)) pollWizard();
    }
  }, MODELS_POLL_MS);
}

function onWizardClick(e) {
  if (e.target.closest("#wizard-pick")) return pickWizardFolder();
  const action = e.target.closest("[data-wizard]");
  if (!action) return;
  const what = action.dataset.wizard;
  if (what === "cancel") {
    action.disabled = true;
    postJson("/api/local/cancel", {}).then((data) => {
      toast(data.cancelled ? tr("Отмена запрошена") : tr("Отменять нечего."));
    }).catch((err) => {
      action.disabled = false;
      $("wizard-error").textContent = tr("Отмена не сработала: {e}", { e: err.message });
      $("wizard-error").hidden = false;
    });
    return;
  }
  if (what === "retry") return startWizardInstall();
  if (what === "check") {
    closeModelsWizard();
    runSync();
  }
}

function onWizardChange(e) {
  if (e.target.id === "wizard-offline") return saveModelsOffline(e.target.checked);
  if (e.target.id !== "wizard-model") return;
  wizard.id = e.target.value;
  const model = ((modelsStatus || {}).catalog || []).find((item) => item.id === wizard.id);
  const note = $("wizard-model-note");
  if (note) note.innerHTML = wizardModelNote(model);
}

function onWizardInput(e) {
  if (e.target.id === "wizard-folder") wizard.root = e.target.value.trim();
}

async function pickWizardFolder() {
  const note = $("wizard-folder-note");
  const invoke = globalThis.__TAURI__?.core?.invoke;
  if (typeof invoke !== "function") {
    const input = $("wizard-folder");
    if (input) input.focus();
    if (note) note.textContent = tr("Выбор папки доступен в приложении. Введите путь вручную.");
    return;
  }
  try {
    const picked = await invoke("pick_folder");
    if (picked) {
      wizard.root = picked;
      $("wizard-folder").value = picked;
    }
  } catch (e) {
    if (note) note.textContent = tr("Папка не выбрана: {e}", { e: e.message });
  }
}

function closeModelsWizard() {
  if (wizard.busy) {
    const question = tr("Установка ещё идёт. Закрыть мастер, а модель будет ставиться в фоне?");
    if (typeof globalThis.confirm !== "function" || !globalThis.confirm(question)) return;
  }
  clearTimeout(wizard.timer);
  wizard.busy = false;
  $("models-wizard").close();
  if (state.page === "models") loadModels();
}

function wireModels() {
  $("models-banner-open").addEventListener("click", openModelsWizard);
  $("models-recommended-apply").addEventListener("click", applyModelsRecommended);
  $("models-only-fits").addEventListener("change", () => { if (modelsStatus) renderModelsCatalog(modelsStatus); });
  $("models-only-installed").addEventListener("change", () => { if (modelsStatus) renderModelsCatalog(modelsStatus); });
  $("models-catalog").addEventListener("click", onModelsCatalogClick);
  $("models-runtime").addEventListener("click", onModelsRuntimeClick);
  $("models-runtime").addEventListener("submit", saveModelsRuntime);
  $("models-tasks-save").addEventListener("click", saveModelsTasks);
  $("models-tasks").addEventListener("click", (e) => {
    if (e.target.closest("#models-provider-models")) loadProviderModels();
  });
  $("models-offline").addEventListener("change", (e) => saveModelsOffline(e.target.checked));
  $("models-install").addEventListener("click", (e) => {
    if (e.target.closest("[data-models-cancel]")) return cancelModelsJob();
    if (e.target.closest("[data-models-retry]")) return retryModelsJob();
    if (e.target.closest("[data-models-dismiss]")) {
      modelsInstallDismissed = true;
      renderModelsInstall((modelsStatus || {}).install);
    }
  });
  $("wizard-body").addEventListener("click", onWizardClick);
  $("wizard-body").addEventListener("change", onWizardChange);
  $("wizard-body").addEventListener("input", onWizardInput);
  $("wizard-close").addEventListener("click", closeModelsWizard);
  $("wizard-back").addEventListener("click", () => {
    if (wizard.step > 1) {
      wizard.step -= 1;
      renderWizard();
    }
  });
  $("wizard-next").addEventListener("click", onWizardNext);
  $("models-wizard").addEventListener("cancel", (e) => {
    e.preventDefault();
    closeModelsWizard();
  });
}

// -- обработка ---------------------------------------------------------------

const QUEUE_KINDS = [
  ["done", tr("Пересказано"), "var(--accent)"],
  ["pending", tr("В очереди"), "var(--warn)"],
  ["waiting", tr("Ждут тишины"), "var(--text-3)"],
  ["held_local", tr("Только локальная модель"), "#5c8fd6"],
  ["failed", tr("С ошибкой"), "var(--err)"],
];
const QUEUE_HINTS = {
  done: tr("Запись пересказана в карточку знаний."),
  pending: tr("Запись готова к пересказу и ждёт ближайшей синхронизации."),
  waiting: tr("Запись ждёт 30 минут тишины (день коммитов — конца суток), чтобы не пересказывать недописанное."),
  held_local: tr("Запись подходит под шаблоны приватности и уходит только локальной модели."),
  failed: tr("Пересказ не удался после нескольких попыток. Причина и повтор — ниже."),
};
const usageTicket = latest();

async function loadActivity() {
  await loadStats();
  const t = usageTicket();
  loadModelStats();
  loadQueueFailed();
  loadLog();
  scheduleLogAuto();
  $("usage-runs").innerHTML = `<div class="state">${tr("Загрузка…")}</div>`;
  try {
    const data = await api("/api/usage", { limit: 30 });
    if (!usageTicket.current(t)) return;
    const totals = data.totals || {};
    const runs = data.runs || [];
    $("usage-summary").innerHTML =
      `<div><span>${tr("Обращений")}</span><strong>${number(totals.runs)}</strong></div>` +
      `<div><span>${tr("Токенов на входе")}</span><strong>${number(totals.prompt_tokens)}</strong></div>` +
      `<div><span>${tr("Токенов на выходе")}</span><strong>${number(totals.completion_tokens)}</strong></div>` +
      `<div><span>${tr("Известная сумма")}</span><strong>${Number(totals.cost_rub) > 0 ? esc(Number(totals.cost_rub).toLocaleString(LOCALE, { maximumFractionDigits: 2 })) + " ₽" : tr("нет данных")}</strong></div>`;
    $("usage-runs").innerHTML = runs.length
      ? `<table class="table table-usage"><thead><tr><th>${tr("Когда")}</th><th>${tr("Модель")}</th><th>${tr("Карточек")}</th><th>${tr("Токенов")}</th><th>${tr("Сумма")}</th></tr></thead><tbody>` +
        runs.map((r) => `<tr><td>${esc(fullDate(r.ts))}</td><td>${esc(r.model)}<small>${esc(r.provider)}</small></td><td>${number(r.cards)}</td>` +
          `<td>${number(Number(r.prompt_tokens || 0) + Number(r.completion_tokens || 0))}</td>` +
          `<td>${Number(r.cost_rub) > 0 ? esc(Number(r.cost_rub).toLocaleString(LOCALE, { maximumFractionDigits: 2 })) + " ₽" : "—"}</td></tr>`).join("") +
        `</tbody></table>`
      : `<div class="state">${tr("Журнал пуст. Это не значит, что API не расходовался: прошлые обращения не восстановить.")}</div>`;
  } catch (e) {
    if (!usageTicket.current(t)) return;
    $("usage-runs").innerHTML = `<div class="state state-error">${tr("Журнал недоступен: {e}", { e: esc(e.message) })}</div>`;
  }
}

function renderQueue(stats) {
  const queue = stats.queue || {};
  const total = Math.max(1, Number(queue.records) || 0);
  const sync = stats.sync || {};
  const schedule = sync.mode === "manual"
    ? tr("Автосинк выключен: партию запускает кнопка «Синхронизировать».")
    : tr("Расписание запускает партию раз в {n} минут.", { n: number(sync.interval_minutes || 15) });
  $("activity-lede").textContent = queue.configured
    ? tr("{records} из {n} фрагментов. Новые записи берутся первыми. {schedule}", { records: records(queue.records), n: number(queue.fragments), schedule })
    : tr("Модель не настроена: записи индексируются, но карточки не пишутся. Откройте «Настройки» внизу боковой панели.");
  $("queue-bar").innerHTML = QUEUE_KINDS.filter(([key]) => queue[key]).map(([key, label, color]) =>
    `<i style="width:${(queue[key] / total * 100).toFixed(2)}%;background:${color}" title="${esc(label)}: ${number(queue[key])}"></i>`
  ).join("");
  $("queue-legend").innerHTML = QUEUE_KINDS.map(([key, label, color]) =>
    `<div><i style="background:${color}"></i><b>${number(queue[key])}</b><span title="${esc(QUEUE_HINTS[key] || "")}">${esc(label)}</span></div>`
  ).join("");
}

// -- failed-очередь и журналы --------------------------------------------------

const queueFailedTicket = latest();

async function loadQueueFailed() {
  const t = queueFailedTicket();
  try {
    const data = await api("/api/queue/failed");
    if (!queueFailedTicket.current(t)) return;
    renderQueueFailed(data.failed || []);
  } catch {
    if (queueFailedTicket.current(t)) $("queue-failed").innerHTML = "";
  }
}

function renderQueueFailed(failed) {
  const host = $("queue-failed");
  if (!failed.length) {
    host.innerHTML = "";
    return;
  }
  host.innerHTML = `<div class="surface-head"><h3 class="section-title">${tr("Не пересказано")}</h3>` +
    `<button class="text-button" type="button" data-retry-all>${tr("Повторить все")}</button></div>` +
    failed.map((f) => `<div class="failed-row"><span class="failed-title">${esc(f.title || f.unit)}</span>` +
      `<span class="subtle">${esc(sourceName(f.source))}${f.project ? " · " + esc(leaf(f.project)) : ""} · ${tr("{n} с ошибкой", { n: number(f.attempts) })}</span>` +
      `<span class="failed-error">${esc(f.last_error || "")}</span>` +
      `<button class="sync-button" type="button" data-retry="${esc(f.unit)}">${tr("Повторить")}</button></div>`).join("");
}

async function retryQueue(unit) {
  try {
    const data = await postJson("/api/queue/retry", unit ? { unit } : { all: true });
    toast(tr("Возвращено в очередь: {n}", { n: number(data.requeued) }));
    loadQueueFailed();
    loadStats();
  } catch (e) {
    toast(tr("Не возвращено: {e}", { e: e.message }));
  }
}

let logTimer = 0;

async function loadLog() {
  const name = $("log-name").value;
  try {
    const data = await api("/api/log", { name, tail_kb: 64 });
    $("log-text").textContent = data.text || "";
    $("log-note").textContent = data.truncated ? tr("Показан хвост файла (64 КБ).") : "";
  } catch (e) {
    $("log-text").textContent = "";
    $("log-note").textContent = e.status === 404 ? tr("Журнал ещё не создан.") : tr("Журнал недоступен: {e}", { e: e.message });
  }
}

function scheduleLogAuto() {
  clearTimeout(logTimer);
  if ($("log-auto").checked && state.page === "activity") {
    logTimer = setTimeout(() => {
      loadLog();
      scheduleLogAuto();
    }, 10_000);
  }
}

// -- состояние планировщика ----------------------------------------------------

async function loadScheduleState() {
  try {
    renderScheduleState(await api("/api/sync/schedule"));
  } catch {
    $("set-sync-state").textContent = "";
  }
}

function scheduleStateName(state) {
  return { ready: tr("готов"), disabled: tr("отключён"), running: tr("идёт"), unknown: tr("неизвестно") }[state] || state;
}

function renderScheduleState(data) {
  const el = $("set-sync-state");
  if (data.mode === "manual") {
    el.textContent = tr("Планировщик: только вручную.");
    return;
  }
  if (data.state === "absent") {
    el.textContent = tr("Задача планировщика не создана.");
    return;
  }
  const parts = [tr("Планировщик: {state}", { state: scheduleStateName(data.state) })];
  if (data.next_run_ts) parts.push(tr("следующий запуск {time}", { time: fullDate(data.next_run_ts) }));
  if (data.last_run_ts) parts.push(tr("последний {time}", { time: fullDate(data.last_run_ts) }));
  if (data.last_result != null) {
    parts.push(tr("результат {code}", { code: data.last_result === 0 ? tr("успех") : "0x" + Number(data.last_result).toString(16) }));
  }
  el.textContent = parts.join(" · ") + (data.error ? " · " + data.error : "");
}

// -- статистика моделей ------------------------------------------------------

const modelStatsTicket = latest();

function formatHoursSpan(hours) {
  const minutes = Math.max(0, Math.round((Number(hours) || 0) * 60));
  return minutes >= 60
    ? tr("{h} ч {m} мин", { h: Math.floor(minutes / 60), m: minutes % 60 })
    : tr("{m} мин", { m: minutes });
}

function formatSecondsSpan(seconds) {
  return formatHoursSpan((Number(seconds) || 0) / 3600);
}

function rubValue(value) {
  return Number(value) > 0
    ? `${Number(value).toLocaleString(LOCALE, { maximumFractionDigits: 2 })} ₽`
    : "—";
}

function triggerName(trigger) {
  return TRIGGER_NAMES[trigger] || trigger || "—";
}

function taskName(task) {
  return ROLE_NAMES[task] || task || "—";
}

async function loadModelStats() {
  const t = modelStatsTicket();
  try {
    const data = await api("/api/models/stats", { days: 30 });
    if (!modelStatsTicket.current(t)) return;
    renderModelStats(data || {});
  } catch (e) {
    if (!modelStatsTicket.current(t)) return;
    $("model-stats").innerHTML = `<div class="state state-error">${tr("Статистика моделей недоступна: {e}", { e: esc(e.message) })}</div>`;
  }
}

function renderModelStats(data) {
  const sessions = data.sessions || [];
  const tasks = data.tasks || [];
  const models = data.models || [];
  const bench = data.bench || [];
  const hours = data.hours || {};
  const totals = data.totals || {};
  const host = $("model-stats");
  const tiles = [
    [tr("Сегодня"), formatHoursSpan(hours.today)],
    [tr("7 дней"), formatHoursSpan(hours.week)],
    [tr("Всего"), formatHoursSpan(hours.total)],
  ].map(([label, value]) => `<div class="model-hours"><span>${label}</span><strong>${value}</strong></div>`).join("");
  const empty = !sessions.length && !tasks.length && !models.length && !bench.length;
  if (empty) {
    host.innerHTML = `<div class="model-hours-row">${tiles}</div>` +
      `<div class="state">${tr("Локальная модель ещё не запускалась")}</div>`;
    return;
  }
  const parts = [`<div class="model-hours-row">${tiles}</div>`];
  parts.push(`<h3 class="micro">${tr("Сессии движка")}</h3>`);
  if (sessions.length) {
    parts.push(`<div class="table-wrap"><table class="table"><thead><tr>` +
      `<th>${tr("Начало")}</th><th>${tr("Длительность")}</th><th>${tr("Модели")}</th><th title="${tr("Что запустило движок: синхронизация, кнопка или установка.")}">${tr("Триггер")}</th></tr></thead><tbody>` +
      sessions.map((session) => `<tr><td>${esc(fullDate(session.started_ts))}</td>` +
        `<td>${formatSecondsSpan(session.seconds)}</td><td>${esc(session.models || "—")}</td>` +
        `<td>${esc(triggerName(session.trigger))}</td></tr>`).join("") +
      `</tbody></table></div>`);
  } else {
    parts.push(`<p class="subtle">${tr("Сессий пока нет.")}</p>`);
  }
  if (tasks.length) {
    parts.push(`<h3 class="micro">${tr("По задачам")}</h3><div class="table-wrap"><table class="table"><thead><tr>` +
      `<th>${tr("Задача")}</th><th title="${tr("Сколько раз модель вызывалась для этой задачи.")}">${tr("Прогоны")}</th><th>${tr("Токены")}</th><th>${tr("Сумма")}</th></tr></thead><tbody>` +
      tasks.map((task) => `<tr><td>${esc(taskName(task.task))}</td><td>${number(task.runs)}</td>` +
        `<td>${number(Number(task.prompt_tokens || 0) + Number(task.completion_tokens || 0))}</td>` +
        `<td>${rubValue(task.cost_rub)}</td></tr>`).join("") +
      `</tbody></table></div>`);
  }
  if (models.length) {
    const tokens = (model) => Number(model.prompt_tokens || 0) + Number(model.completion_tokens || 0);
    const top = models.slice().sort((a, b) => (b.runs - a.runs) || (tokens(b) - tokens(a))).slice(0, 5);
    parts.push(`<h3 class="micro">${tr("Топ моделей")}</h3><div class="model-top">` +
      top.map((model) => `<div class="model-top-row"><span>${esc(model.model)}</span><b>${number(model.runs)}</b>` +
        `<small>${tr("{n} токенов", { n: number(tokens(model)) })}</small></div>`).join("") +
      `</div>`);
  }
  if (bench.length) {
    parts.push(`<h3 class="micro">${tr("Бенчмарк")}</h3><div class="bench-list">` +
      bench.map((row) => `<div class="bench-row"><span>${esc(row.model)}</span>` +
        `<small>${tr("≈ {tps} ток/с, загрузка {load} мс", { tps: Number(row.tps || 0).toFixed(1), load: number(row.load_ms) })}</small></div>`).join("") +
      `</div>`);
  }
  if (totals.runs) {
    parts.push(`<p class="subtle model-totals">${tr("Всего обращений: {n}. Токенов: {tokens}.", {
      n: number(totals.runs),
      tokens: number(Number(totals.prompt_tokens || 0) + Number(totals.completion_tokens || 0)),
    })}</p>`);
  }
  host.innerHTML = parts.join("");
}

// -- диалог настроек ---------------------------------------------------------

let settingsData = null;
let localIntervalShown = 15;
const modelsTicket = latest();

function renderSyncNext(mode, ts) {
  $("set-sync-next").textContent = mode === "manual" || !ts
    ? tr("Следующая: —")
    : tr("Следующая: {time}", { time: clock(ts) });
}

function renderLocalModel(route) {
  const current = route.model || "";
  const installed = route.installed || [];
  // Модель из config.toml могла быть удалена: показываем её, а не молча меняем выбор.
  const models = !current || installed.includes(current) ? installed : [current, ...installed];
  $("set-local-model").innerHTML = `<option value="">${tr("Не выбрана")}</option>` +
    models.map((model) => `<option value="${esc(model)}">${esc(model)}</option>`).join("");
  $("set-local-model").value = current;
  $("set-local-note").textContent = installed.length
    ? tr("Пишет приватные записи и обновляет названия проектов. Модели отдельных задач в «Модели и железо» важнее этого выбора.")
    : tr("Нет установленных моделей: поставьте их в «Модели и железо».");
}

function renderLocalInterval() {
  const manual = $("sync-mode-manual").checked;
  $("set-local-interval").disabled = manual;
  $("set-local-interval-note").textContent = manual
    ? tr("При синхронизации вручную сбор идёт только по кнопке.")
    : tr("Сбор данных: карточки и названия проектов. Не чаще синхронизации, кнопка «Синхронизировать» запускает его сразу.");
}

async function openSettings() {
  const dialog = $("settings");
  $("set-error").hidden = true;
  $("set-models-note").textContent = "";
  try {
    settingsData = await api("/api/settings");
  } catch (e) {
    toast(tr("Настройки недоступны: {e}", { e: e.message }), 8000);
    return;
  }
  const data = settingsData;
  $("set-provider").innerHTML = data.providers.map((p) =>
    `<option value="${esc(p.id)}">${esc(p.name)}</option>`).join("") + `<option value="custom">${tr("Другой адрес")}</option>`;
  $("settings-now").textContent = data.model
    ? `${tr("Сейчас:")} ${data.base_url} · ${data.model}`
    : tr("Модель не настроена: записи индексируются, карточки не пишутся.");
  $("set-url").value = data.base_url;
  $("set-model").value = data.model;
  $("set-fallback").value = data.fallback_model || "";
  $("set-key").value = "";
  $("set-json").checked = data.json_mode;
  syncProviderFromUrl();
  describeKey();
  const sync = data.sync || {};
  $("sync-mode-interval").checked = sync.mode !== "manual";
  $("sync-mode-manual").checked = sync.mode === "manual";
  $("set-sync-interval").value = String(Number(sync.interval_minutes) || 15);
  renderSyncNext(sync.mode, state.stats && state.stats.next_sync_ts);
  loadScheduleState();
  const localRoute = data.local_route || {};
  renderLocalModel(localRoute);
  localIntervalShown = Number(localRoute.collect_interval_minutes) || Number(sync.interval_minutes) || 15;
  $("set-local-interval").value = String(localIntervalShown);
  renderLocalInterval();
  if (!dialog.open) dialog.showModal();
}

function syncProviderFromUrl() {
  const url = $("set-url").value.trim().replace(/\/+$/, "");
  const match = settingsData.providers.find((p) => p.base_url === url);
  $("set-provider").value = match ? match.id : "custom";
}

function sameHost(a, b) {
  try {
    return new URL(a).hostname === new URL(b).hostname;
  } catch {
    return false;
  }
}

function describeKey() {
  const data = settingsData;
  const url = $("set-url").value.trim();
  const keyInput = $("set-key");
  let note;
  if (data.key_from_env) {
    note = tr("Ключ задан переменной BREPSI_BRAIN_API_KEY: она важнее того, что сохранено здесь.");
    keyInput.placeholder = "";
  } else if (data.has_key && sameHost(url, data.base_url)) {
    note = tr("Ключ сохранён для этого провайдера.");
    keyInput.placeholder = tr("Оставьте пустым, чтобы сохранить текущий ключ");
  } else if (data.has_key) {
    note = tr("Адрес изменился: прежний ключ этому провайдеру не передаётся и будет удалён при сохранении.");
    keyInput.placeholder = tr("Ключ нового провайдера");
  } else {
    note = tr("Ключ не сохранён. Для локальной модели он обычно не нужен.");
    keyInput.placeholder = "";
  }
  $("set-key-note").textContent = note;
}

async function refreshModels() {
  const t = modelsTicket();
  const button = $("set-refresh");
  button.disabled = true;
  $("set-models-note").textContent = tr("Запрашиваю список…");
  try {
    const data = await postJson("/api/models", {
      base_url: $("set-url").value.trim(),
      api_key: $("set-key").value.trim() || null,
    });
    if (!modelsTicket.current(t)) return;
    const models = data.models || [];
    $("set-models").innerHTML = models.map((m) => `<option value="${esc(m)}"></option>`).join("");
    $("set-models-note").textContent = tr("Доступно моделей: {n}. Можно выбрать из списка или ввести название.", { n: number(models.length) });
  } catch (e) {
    if (modelsTicket.current(t)) $("set-models-note").textContent = tr("Список не получен: {e}", { e: e.message });
  } finally {
    button.disabled = false;
  }
}

async function saveSettings(e) {
  e.preventDefault();
  $("set-error").hidden = true;
  const key = $("set-key").value.trim();
  const mode = $("sync-mode-manual").checked ? "manual" : "interval";
  const interval = Number($("set-sync-interval").value);
  if (!Number.isInteger(interval) || interval < 5 || interval > 1439) {
    $("set-error").textContent = tr("Интервал синхронизации: целое число минут от 5 до 1439.");
    $("set-error").hidden = false;
    return;
  }
  const loadedRoute = settingsData.local_route || {};
  const localInterval = Number($("set-local-interval").value);
  const storedInterval = loadedRoute.collect_interval_minutes;
  // Значение, подставленное из интервала синхронизации, не сохраняем: пока его
  // не меняли, сбор просто идёт с каждой синхронизацией.
  const sendLocalInterval = mode === "interval" && (storedInterval != null || localInterval !== localIntervalShown);
  if (sendLocalInterval && (!Number.isInteger(localInterval) || localInterval < interval || localInterval > 1440)) {
    $("set-error").textContent = tr("Интервал сбора данных: целое число минут от {min} до 1440.", { min: number(interval) });
    $("set-error").hidden = false;
    return;
  }
  const baseUrl = $("set-url").value.trim();
  const model = $("set-model").value.trim();
  try {
    // Только локальная модель: без провайдера сохранять нечего, сервер потребовал бы оба поля.
    if (baseUrl || model) {
      settingsData = await postJson("/api/settings", {
        base_url: baseUrl,
        model,
        fallback_model: $("set-fallback").value.trim(),
        api_key: key || null,
        json_mode: $("set-json").checked,
      });
    }
  } catch (err) {
    $("set-error").textContent = tr("Не сохранилось: {e}", { e: err.message });
    $("set-error").hidden = false;
    return;
  }
  const localModel = $("set-local-model").value;
  if (localModel !== (loadedRoute.model || "")) {
    try {
      settingsData = await postJson("/api/local/route", { model: localModel });
    } catch (err) {
      $("set-error").textContent = tr("Локальная модель не сохранилась: {e}", { e: err.message });
      $("set-error").hidden = false;
      return;
    }
  }
  let warning = "";
  try {
    const schedule = await postJson("/api/sync/schedule", {
      mode,
      interval_minutes: interval,
      ...(sendLocalInterval ? { local_interval_minutes: localInterval } : {}),
    });
    warning = schedule.warning || "";
    if (state.stats) {
      state.stats.next_sync_ts = schedule.next_sync_ts;
      state.stats.sync = { mode: schedule.mode, interval_minutes: schedule.interval_minutes };
    }
    renderSidebar();
  } catch (err) {
    warning = tr("Расписание не сохранено: {e}", { e: err.message });
  }
  $("settings").close();
  const saved = tr("Сохранено. Применится со следующего запуска.");
  toast(warning ? `${saved} ${warning}` : saved, warning ? 12000 : 4500);
  loadStats();
}

function wireSettings() {
  $("settings-open").addEventListener("click", openSettings);
  $("set-cancel").addEventListener("click", () => $("settings").close());
  $("settings-form").addEventListener("submit", saveSettings);
  $("set-refresh").addEventListener("click", refreshModels);
  $("set-provider").addEventListener("change", () => {
    const preset = settingsData.providers.find((p) => p.id === $("set-provider").value);
    if (preset) $("set-url").value = preset.base_url;
    $("set-models").innerHTML = "";
    $("set-models-note").textContent = "";
    describeKey();
  });
  $("set-url").addEventListener("input", () => {
    syncProviderFromUrl();
    describeKey();
  });
  $("sync-mode-interval").addEventListener("change", () => {
    renderSyncNext("interval", state.stats && state.stats.next_sync_ts);
    renderLocalInterval();
  });
  $("sync-mode-manual").addEventListener("change", () => {
    renderSyncNext("manual", null);
    renderLocalInterval();
  });
}

// -- палитра (Ctrl+K) --------------------------------------------------------

const PALETTE_ACTIONS = [
  { kind: tr("действие"), title: tr("Синхронизировать сейчас"), run: runSync },
  { kind: tr("действие"), title: tr("Настройки"), run: openSettings },
  { kind: tr("раздел"), title: tr("Обзор"), run: () => go("overview") },
  { kind: tr("раздел"), title: tr("Карта памяти"), run: () => go("graph") },
  { kind: tr("раздел"), title: tr("Проекты"), run: () => go("projects") },
  { kind: tr("раздел"), title: tr("Все записи"), run: () => go("records") },
  { kind: tr("раздел"), title: tr("Спросить память"), run: () => go("answers") },
  { kind: tr("раздел"), title: tr("Журнал обработки"), run: () => go("activity") },
  { kind: tr("раздел"), title: tr("Модели и железо"), run: () => go("models") },
  { kind: tr("раздел"), title: tr("Перенос чатов"), run: () => go("handoff") },
  { kind: tr("раздел"), title: tr("О программе"), run: () => go("about") },
];
let paletteItems = [];
let paletteIndex = 0;
let paletteTimer = 0;
const paletteTicket = latest();

function openPalette() {
  $("palette").hidden = false;
  $("palette-input").value = "";
  paletteItems = PALETTE_ACTIONS.slice();
  paletteIndex = 0;
  renderPalette();
  $("palette-input").focus();
}

function closePalette() {
  paletteTicket();
  $("palette").hidden = true;
}

function renderPalette() {
  $("palette-list").innerHTML = paletteItems.length ? paletteItems.map((item, i) =>
    `<div class="palette-item${i === paletteIndex ? " active" : ""}" data-i="${i}" role="option" aria-selected="${i === paletteIndex}">` +
    `<span class="pi-title">${esc(item.title)}</span><span class="pi-kind">${esc(item.kind)}</span></div>`
  ).join("") : `<div class="palette-empty">${tr("Ничего не нашлось.")}</div>`;
}

function runPaletteItem(i) {
  const item = paletteItems[i];
  if (!item) return;
  closePalette();
  if (item.run) item.run();
  else openRecord(item.id);
}

function onPaletteInput() {
  const q = $("palette-input").value.trim();
  clearTimeout(paletteTimer);
  if (!q) {
    paletteTicket();
    paletteItems = PALETTE_ACTIONS.slice();
    paletteIndex = 0;
    renderPalette();
    return;
  }
  const needle = q.toLowerCase();
  const actions = PALETTE_ACTIONS.filter((a) => a.title.toLowerCase().includes(needle));
  paletteTimer = setTimeout(async () => {
    const t = paletteTicket();
    try {
      const data = await api("/api/search", { q, limit: PALETTE_LIMIT });
      if (!paletteTicket.current(t)) return;
      paletteItems = actions.concat((data.cards || []).map((c) => ({
        id: c.id, title: c.title, kind: sourceName(c.source),
      })));
    } catch {
      if (!paletteTicket.current(t)) return;
      paletteItems = actions;
    }
    paletteIndex = 0;
    renderPalette();
  }, DEBOUNCE_MS);
}

function onPaletteKeydown(e) {
  if (e.key === "Escape") {
    // Иначе Esc всплывал к document и закрывал ещё и панель записи под палитрой.
    e.preventDefault();
    e.stopPropagation();
    closePalette();
  } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const delta = e.key === "ArrowDown" ? 1 : -1;
    paletteIndex = Math.max(0, Math.min(paletteItems.length - 1, paletteIndex + delta));
    renderPalette();
  } else if (e.key === "Enter") {
    e.preventDefault();
    e.stopPropagation();
    runPaletteItem(paletteIndex);
  }
}

// -- связывание --------------------------------------------------------------

function closeGraphViewMenu() {
  $("graph-view-menu").hidden = true;
  $("graph-view").setAttribute("aria-expanded", "false");
}

function onKeydown(e) {
  const key = e.key.toLowerCase();
  const editing = e.target && (e.target.isContentEditable || /INPUT|TEXTAREA|SELECT/.test(e.target.tagName || ""));
  if ((e.ctrlKey || e.metaKey) && key === "k") {
    e.preventDefault();
    if ($("palette").hidden) openPalette();
    else closePalette();
    return;
  }
  // Открытая палитра или диалог: переключение раздела закрыло бы их и утянуло
  // фоновые загрузки страниц, которых пользователь не выбирал.
  const blocked = !$("palette").hidden || $("settings").open || $("models-wizard").open || $("harness-add").open;
  if (e.ctrlKey && !e.shiftKey && !e.altKey && /^[1-9]$/.test(key)) {
    if (blocked) return;
    e.preventDefault();
    go(PAGES[Number(key) - 1]);
    return;
  }
  if (blocked) return;
  if (key === "escape") {
    if ($("sidebar").classList.contains("open")) setNav(false);
    else if (!$("graph-view-menu").hidden) closeGraphViewMenu();
    else if (!$("panel").hidden) closePanel();
    else if (state.page === "graph" && graph.selected != null) graphClearSelection();
    return;
  }
  if (editing || e.ctrlKey || e.metaKey || e.altKey) return;
  if (key === "/") {
    e.preventDefault();
    openPalette();
  } else if (key === "f" && state.page === "graph") {
    toggleGraphFullscreen();
  }
}

function setNav(open) {
  $("sidebar").classList.toggle("open", open);
  $("nav-scrim").hidden = !open;
  $("nav-toggle").setAttribute("aria-expanded", String(open));
}

function wire() {
  addEventListener("hashchange", applyRoute);
  document.addEventListener("keydown", onKeydown);
  // Подсказки хоткеев в сайдбаре видны только пока удержан Ctrl; blur снимает
  // класс, иначе отпущенная вне окна клавиша оставила бы цифры навсегда.
  const ctrlHeld = (on) => (document.body || document.documentElement)?.classList.toggle("ctrl-held", on);
  addEventListener("keydown", (e) => { if (e.key === "Control") ctrlHeld(true); });
  addEventListener("keyup", (e) => { if (e.key === "Control") ctrlHeld(false); });
  addEventListener("blur", () => ctrlHeld(false));
  // Узкое окно: меню выдвижное, закрывается scrim, Esc, переходом и возвратом к широкой раскладке.
  $("nav-toggle").addEventListener("click", () => setNav(!$("sidebar").classList.contains("open")));
  $("nav-scrim").addEventListener("click", () => setNav(false));
  $("sidebar").addEventListener("click", (e) => { if (e.target.closest(".nav-item")) setNav(false); });
  addEventListener("hashchange", () => setNav(false));
  matchMedia("(max-width: 700px)").addEventListener?.("change", (e) => { if (e && !e.matches) setNav(false); });
  document.addEventListener("click", (e) => {
    if (e.target.closest("[data-sync-now]")) { runSync(); return; }
    const open = e.target.closest(".preview-row[data-id], .record-row[data-id], .answer-source[data-id]");
    if (open) {
      openRecord(Number(open.dataset.id));
      return;
    }
    const project = e.target.closest(".source-line[data-project], .project-chip[data-project]");
    if (project) {
      openProject(project.dataset.project);
      return;
    }
    const source = e.target.closest(".source-line[data-source]");
    if (source) {
      Object.assign(state.records, { source: source.dataset.source, project: null, q: "" });
      $("records-q").value = "";
      if (state.page === "records") loadRecords();
      else go("records");
    }
  });
  $("sync-now").addEventListener("click", runSync);
  $("queue-failed").addEventListener("click", (e) => {
    const one = e.target.closest("[data-retry]");
    if (one) return retryQueue(one.dataset.retry);
    if (e.target.closest("[data-retry-all]")) retryQueue(null);
  });
  $("log-name").addEventListener("change", loadLog);
  $("log-refresh").addEventListener("click", loadLog);
  $("log-auto").addEventListener("change", scheduleLogAuto);
  $("scrim").addEventListener("click", closePanel);
  $("panel").addEventListener("click", onPanelClick);

  $("palette-open").addEventListener("click", openPalette);

  $("timeline-period").addEventListener("click", (e) => {
    const seg = e.target.closest(".seg");
    if (!seg) return;
    timelineWeeks = Number(seg.dataset.weeks) || 0;
    setPressed(seg.parentElement, (b) => b === seg);
    loadTimeline();
  });
  $("timeline").addEventListener("mousemove", onTimelineMove);
  $("timeline").addEventListener("mouseleave", () => {
    const tip = $("timeline-tip");
    if (tip) tip.hidden = true;
  });
  $("timeline-table-toggle").addEventListener("click", (e) => {
    const table = $("timeline-table");
    table.hidden = !table.hidden;
    e.target.setAttribute("aria-expanded", String(!table.hidden));
    e.target.textContent = table.hidden ? tr("Показать таблицей") : tr("Скрыть таблицу");
  });
  if (typeof ResizeObserver === "function") {
    new ResizeObserver(() => {
      if (state.page === "overview" && timeline) renderTimeline();
    }).observe($("timeline"));
  }

  $("projects-filter").addEventListener("input", renderProjects);
  $("projects-propose").addEventListener("click", proposeProjects);
  $("projects-proposals").addEventListener("click", onProjectsProposalClick);
  $("projects-sort").addEventListener("click", (e) => {
    const seg = e.target.closest(".seg");
    if (!seg) return;
    projectsSort = seg.dataset.sort;
    setPressed(seg.parentElement, (b) => b === seg);
    renderProjects();
  });

  let recordsTimer = 0;
  $("records-q").addEventListener("input", () => {
    clearTimeout(recordsTimer);
    recordsTimer = setTimeout(() => {
      state.records.q = $("records-q").value.trim();
      loadRecords();
    }, DEBOUNCE_MS);
  });
  $("records-sources").addEventListener("click", (e) => {
    const chip = e.target.closest(".chip");
    if (!chip) return;
    state.records.source = chip.dataset.source;
    loadRecords();
  });
  $("records-clear").addEventListener("click", () => {
    Object.assign(state.records, { q: "", source: "", project: null });
    $("records-q").value = "";
    loadRecords();
  });
  $("records-list").addEventListener("click", (e) => {
    if (e.target.closest("#records-more")) loadRecords(true);
  });

  $("answer-form").addEventListener("submit", (e) => {
    e.preventDefault();
    askAnswer($("answer-q").value);
  });
  $("answer-history").addEventListener("click", (e) => {
    const chip = e.target.closest(".chip");
    if (!chip) return;
    restoreAnswer(answerHistory[Number(chip.dataset.i)]);
  });
  $("side-history").addEventListener("click", (e) => {
    const forget = e.target.closest("button[data-forget]");
    if (forget) {
      forgetHistoryItem(Number(forget.dataset.forget));
      return;
    }
    const item = e.target.closest("button[data-history]");
    if (item) openHistoryItem(Number(item.dataset.history));
  });
  $("side-history-toggle").addEventListener("click", () => {
    setHistoryCollapsed(!$("side-history").hidden);
  });
  setHistoryCollapsed(loadPref(HISTORY_COLLAPSED_KEY, false) === true);
  loadSearchHistory();
  document.querySelector(".nav-new").addEventListener("click", () => {
    if (state.page === "answers") resetAsk();
  });
  $("answer-out").addEventListener("click", (e) => {
    const tab = e.target.closest("[data-answer-tab]");
    if (tab) {
      $("answer-out").classList.toggle("sources-only", tab.dataset.answerTab === "sources");
      $("answer-out").querySelectorAll("[data-answer-tab]").forEach((button) => {
        button.setAttribute("aria-pressed", String(button === tab));
      });
      return;
    }
    const cite = e.target.closest(".cite");
    if (cite && answerCards[Number(cite.dataset.card)]) openRecord(answerCards[Number(cite.dataset.card)].id);
  });
  $("usage-refresh").addEventListener("click", loadActivity);

  $("palette-input").addEventListener("input", onPaletteInput);
  $("palette-input").addEventListener("keydown", onPaletteKeydown);
  $("palette").addEventListener("mousedown", (e) => {
    if (e.target === $("palette")) closePalette();
  });
  $("palette-list").addEventListener("click", (e) => {
    const item = e.target.closest(".palette-item");
    if (item) runPaletteItem(Number(item.dataset.i));
  });

  wireGraph();
  wireSettings();
  wireModels();
  setInterval(() => { if (!syncRunning) loadStats(); }, STATS_REFRESH_MS);
}

// Кнопки окна есть только в оболочке Tauri (withGlobalTauri); в браузере блок остаётся скрытым.
function wireWindowControls() {
  const current = globalThis.__TAURI__?.window?.getCurrentWindow();
  if (!current) return;
  document.body.classList.add("has-window-controls");
  $("win-controls").hidden = false;
  $("win-controls").addEventListener("click", (e) => {
    const button = e.target.closest("[data-win]");
    if (button) current[button.dataset.win]().catch((err) => toast(tr("Окно: {e}", { e: err })));
  });
}

// -- о программе и язык ------------------------------------------------------

// Статическая разметка написана по-русски; в английском её тексты и подписи
// заменяются по тому же словарю. Только index.html: данные API сюда не попадают.
function translateStatic(root) {
  if (LANG !== "en") return;
  document.documentElement.lang = "en";
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    // Название языка остаётся на самом языке: «Русский» в переключателе — не
    // строка интерфейса, а ключ «Русский» нужен словарю для языков модели.
    if (node.parentElement && node.parentElement.hasAttribute("lang")) continue;
    const key = node.nodeValue.trim();
    if (key && EN[key] !== undefined) node.nodeValue = node.nodeValue.replace(key, EN[key]);
  }
  for (const attr of ["placeholder", "title", "aria-label"]) {
    root.querySelectorAll(`[${attr}]`).forEach((el) => {
      const key = el.getAttribute(attr);
      if (EN[key] !== undefined) el.setAttribute(attr, EN[key]);
    });
  }
}

function renderAbout() {
  const choice = document.documentElement?.dataset?.lang || "auto";
  $("about-lang").querySelectorAll("[data-lang]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.lang === choice)));
}

// -- обновление приложения --------------------------------------------------
//
// Команды есть только в окне Tauri; в обычном браузере раздел остаётся с подсказкой.

let updateInfo = null;
// Версия, которую уже пытались поставить сами: сбойная установка не должна
// повторяться после каждой синхронизации.
let autoUpdateTried = "";
let lastUpdateCheck = 0;
const AUTO_UPDATE_KEY = "brepsi.autoUpdate";
// Анонимный API GitHub даёт 60 запросов в час: синхронизация раз в 15 минут
// укладывается, но частые ручные — нет.
const UPDATE_CHECK_MIN_MS = 10 * 60 * 1000;

function autoUpdateEnabled() {
  try {
    return localStorage.getItem(AUTO_UPDATE_KEY) !== "off";
  } catch {
    return true;
  }
}

/** `auto` — при запуске и после синхронизации: новая версия ставится без кнопки. */
async function checkAppUpdate({ auto = false } = {}) {
  const invoke = globalThis.__TAURI__?.core?.invoke;
  if (typeof invoke !== "function") return;
  if (auto && Date.now() - lastUpdateCheck < UPDATE_CHECK_MIN_MS) return;
  lastUpdateCheck = Date.now();
  try {
    updateInfo = await invoke("check_update");
  } catch (e) {
    $("about-update-note").textContent = tr("Проверка обновлений не удалась: {e}", { e: String(e) });
    return;
  }
  const button = $("about-update");
  if (updateInfo.available && auto && autoUpdateEnabled() && autoUpdateTried !== updateInfo.latestVersion) {
    autoUpdateTried = updateInfo.latestVersion;
    toast(tr("Устанавливается версия {v}: приложение перезапустится.", { v: updateInfo.latestVersion }), 10000);
    installAppUpdate();
    return;
  }
  if (updateInfo.available) {
    $("about-update-note").textContent = tr("Установлена версия {current}, доступна {latest}.", { current: updateInfo.currentVersion, latest: updateInfo.latestVersion });
    button.textContent = tr("Обновить до {v}", { v: updateInfo.latestVersion });
    button.hidden = false;
    toast(tr("Доступна версия {v}: обновление — в разделе «О программе».", { v: updateInfo.latestVersion }), 8000);
  } else {
    $("about-update-note").textContent = tr("Установлена последняя версия {v}.", { v: updateInfo.currentVersion });
    button.hidden = true;
  }
}

async function installAppUpdate() {
  const tauri = globalThis.__TAURI__;
  const button = $("about-update");
  button.disabled = true;
  const note = $("about-update-note");
  note.textContent = tr("Загрузка обновления…");
  let unlisten = null;
  try {
    // Внутри try: отказ listen оставлял кнопку заблокированной навсегда.
    if (tauri?.event) {
      unlisten = await tauri.event.listen("update-progress", ({ payload }) => {
        if (payload.total > 0) note.textContent = tr("Загрузка обновления… {p}%", { p: Math.floor(payload.downloaded / payload.total * 100) });
      });
    }
    // При успехе приложение перезапускается новой версией и сюда не возвращается.
    await tauri.core.invoke("install_update");
  } catch (e) {
    note.textContent = tr("Обновление не установлено: {e}", { e: String(e) });
    button.disabled = false;
  } finally {
    if (unlisten) unlisten();
  }
}

function wireAbout() {
  $("about-update").addEventListener("click", installAppUpdate);
  $("about-auto-update").checked = autoUpdateEnabled();
  $("about-auto-update").addEventListener("change", (e) => {
    try {
      localStorage.setItem(AUTO_UPDATE_KEY, e.target.checked ? "on" : "off");
    } catch {
      toast(tr("Настройка не сохранилась: хранилище браузера недоступно."));
    }
  });
  $("about-lang").addEventListener("click", async (e) => {
    const button = e.target.closest("[data-lang]");
    if (!button) return;
    try {
      await postJson("/api/ui", { lang: button.dataset.lang });
      location.reload();
    } catch (err) {
      toast(tr("Язык не сохранён: {e}", { e: err.message }));
    }
  });
  $("page-about").addEventListener("click", async (e) => {
    const button = e.target.closest("[data-url]");
    if (!button) return;
    try {
      await postJson("/api/open-url", { url: button.dataset.url });
    } catch (err) {
      toast(tr("Ссылка не открылась: {e}", { e: err.message }));
    }
  });
}

function init() {
  translateStatic(document.body);
  wireWindowControls();
  wire();
  wireAbout();
  checkAppUpdate({ auto: true });
  initHandoff();
  loadStats().then(() => {
    if (state.page === "records") renderRecordFilters();
  });
  applyRoute();
}

init();
