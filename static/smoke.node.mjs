/* Smoke-тест app.js на заглушке DOM: идёт в обычном Node, без браузера.
 *
 * Заглушек ровно столько, чтобы зафиксировать контракт клиента: DOM (id, стоки
 * innerHTML, слушатели) и fetch (заготовленный JSON API). Проверяются рендер с
 * экранированием, маршруты, фильтры и постраничность записей, камера графа при
 * выборе, порядок запросов и то, что диалог провайдера не возвращает ключ.
 *
 * Запуск: node smoke.node.mjs   (exit 0 = все проверки прошли)
 */
"use strict";

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

// -- заглушка DOM --------------------------------------------------------------

const els = new Map();

function makeCtx2d() {
  const ctx = new Proxy({}, {
    get(t, p) { return p in t ? t[p] : () => ctx; },
    set(t, p, v) { t[p] = v; return true; },
  });
  ctx.dots = [];
  ctx.lines = [];
  ctx.clearRect = () => { ctx.dots.length = 0; ctx.lines.length = 0; };
  ctx.moveTo = (x, y) => { ctx.from = { x, y }; };
  ctx.lineTo = (x, y) => { ctx.to = { x, y }; };
  ctx.stroke = () => { if (ctx.from && ctx.to) ctx.lines.push({ a: ctx.from, b: ctx.to, alpha: ctx.globalAlpha ?? 1, color: ctx.strokeStyle }); ctx.from = ctx.to = null; };
  ctx.arc = (x, y) => { ctx.dot = { x, y }; };
  ctx.fill = () => ctx.dots.push({ ...ctx.dot, alpha: ctx.globalAlpha ?? 1 });
  return ctx;
}

function makeEl(id) {
  const listeners = {};
  const classes = new Set();
  const attrs = {};
  return {
    id,
    innerHTML: "",
    textContent: "",
    value: "",
    hidden: false,
    checked: false,
    open: false,
    dataset: {},
    clientWidth: 800,
    clientHeight: 560,
    offsetWidth: 240,
    offsetHeight: 40,
    width: 0,
    height: 0,
    style: {},
    className: "",
    scrollTop: 0,
    isConnected: true,
    classList: {
      toggle(name, on) { if (on ?? !classes.has(name)) classes.add(name); else classes.delete(name); },
      add(name) { classes.add(name); },
      remove(name) { classes.delete(name); },
      contains: (name) => classes.has(name),
    },
    attrs,
    setAttribute(name, value) { attrs[name] = String(value); },
    getAttribute: (name) => attrs[name] ?? null,
    removeAttribute(name) { delete attrs[name]; },
    append(child) { this.innerHTML += child.innerHTML || ""; },
    focus() {},
    select() {},
    showModal() { this.open = true; },
    close() { this.open = false; },
    addEventListener(type, fn) { (listeners[type] ??= []).push(fn); },
    listeners,
    context: makeCtx2d(),
    getContext() { return this.context; },
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 800, height: 560 }),
    appendChild() {},
    querySelectorAll: () => [],
    querySelector: () => null,
    closest: () => null,
    parentElement: null,
  };
}

function el(id) {
  if (!els.has(id)) els.set(id, makeEl(id));
  return els.get(id);
}

const docListeners = {};
const period = new Map();
globalThis.document = {
  getElementById: el,
  createElement: (tag) => makeEl(String(tag)),
  addEventListener(type, fn) { (docListeners[type] ??= []).push(fn); },
  querySelectorAll: () => [],
  querySelector(selector) {
    if (!period.has(selector)) period.set(selector, makeEl(selector));
    return period.get(selector);
  },
};
const winListeners = {};
globalThis.addEventListener = (type, fn) => { (winListeners[type] ??= []).push(fn); };
globalThis.scrollTo = () => {};
globalThis.setInterval = () => 0;
globalThis.location = { hash: "" };
const prefStore = new Map([["projects.proposals", JSON.stringify([
  { key: "kimi", name: "kimi", path: "", evidence: "" },
  { key: "saved/project", name: "Сохранённое имя", path: "", evidence: "" },
])]]);
globalThis.localStorage = {
  getItem: (key) => prefStore.get(key) ?? null,
  setItem: (key, value) => prefStore.set(key, value),
};
globalThis.history = { replaceState(_s, _t, url) { location.hash = url; } };
const clipboard = [];
Object.defineProperty(globalThis, "navigator", {
  value: { clipboard: { writeText: async (text) => clipboard.push(text) } },
  configurable: true,
});

function goto(hash) {
  location.hash = hash;
  for (const fn of winListeners.hashchange || []) fn();
}

// -- заглушка fetch ------------------------------------------------------------

const NOW = Math.floor(Date.now() / 1000);
const settingsFixture = {
  providers: [{ id: "ollama", name: "Ollama", base_url: "http://127.0.0.1:11434/v1" }],
  base_url: "https://one.example/v1",
  model: "m1",
  fallback_model: "fb1",
  has_key: true,
  key_from_env: false,
  json_mode: true,
  tasks: { distill: "", answer: "", handoff: "", projects: "" },
  local_route: { configured: true, model: "brepsi/a", installed: ["brepsi/a", "brepsi/b"], collect_interval_minutes: null },
  sync: { mode: "interval", interval_minutes: 15 },
};
const localStatusFixture = {
  hardware: {
    vendor: "nvidia",
    name: "NVIDIA GeForce RTX 3060",
    vram_mb: 12288,
    ram_mb: 32768,
    backend: "cuda",
    tier: "mid",
    notes: ["12 ГБ видеопамяти: модели до 14B"],
  },
  runtime: {
    installed: true,
    version: "0.40.3",
    running: false,
    port: 11434,
    path: "D:/Models/BrepsiBrain/runtime/ollama",
    log: "D:/BrepsiBrain/data/ollama-serve.log",
  },
  models_root: "D:/Models/BrepsiBrain",
  free_bytes: 68719476736,
  installed: ["qwen25-7b"],
  catalog: [
    {
      id: "gemma3-1b", name: "Gemma 3 1B", params: "1B", quant: "Q4_K_M", size_bytes: 806058400,
      min_vram_mb: 0,
      min_tier: "minimal", fits: true, roles: ["distill", "answer", "handoff", "projects"],
      languages: "ru,en", summary_ru: "Самая лёгкая: считается на процессоре.",
      summary_en: "The lightest model: runs on a CPU.", source: "ollama", installed: false, recommended: false,
    },
    {
      id: "qwen25-7b", name: "Qwen2.5 7B Instruct", params: "7B", quant: "Q4_K_M", size_bytes: 4683073952,
      min_vram_mb: 7000,
      min_tier: "low", fits: true, roles: ["distill", "answer", "handoff", "projects"],
      languages: "ru,en", summary_ru: "Лучший баланс для 8 ГБ: аккуратные карточки.",
      summary_en: "The best balance for 8 GB: careful cards.", source: "ollama", installed: true, recommended: true,
    },
    {
      id: "gemma3-27b", name: "Gemma 3 27B", params: "27B", quant: "Q4_K_M", size_bytes: 17413400640,
      min_vram_mb: 20000,
      min_tier: "high", fits: false, roles: ["distill", "answer"], languages: "ru,en",
      summary_ru: "Крупная модель для 15 ГБ и больше.", summary_en: "A large model for 15 GB and more.",
      source: "huggingface", installed: false, recommended: false,
    },
  ],
  settings: { mode: "on_demand", idle_timeout_seconds: 120, context_tokens: 16384, offline_only: false },
  tasks: { distill: "", answer: "", handoff: "", projects: "" },
  route: { kind: "none", base_url: "", model: "", local_model: "" },
  install: null,
};
const installStatusFixture = {
  ...localStatusFixture,
  install: {
    kind: "install", id: "qwen25-7b", phase: "download", status: "скачивание…",
    done: 120 * 1024 * 1024, total: 480 * 1024 * 1024, speed_bps: 12 * 1024 * 1024, eta_seconds: 30,
    active: true, error: null,
  },
};
const handoffHarnesses = [
  { id: "claude", name: "Claude Code", running: true, can_source: true, can_switch: true, can_target: true, custom: false, delivery: "app" },
  { id: "codex", name: "Codex", running: true, can_source: true, can_switch: true, can_target: true, custom: false, delivery: "terminal" },
  { id: "kimi", name: "Kimi Code", running: false, can_source: true, can_switch: true, can_target: true, custom: false, delivery: "clipboard" },
  { id: "qoder", name: "Qoder", running: false, can_source: true, can_switch: true, can_target: true, custom: false, delivery: "clipboard" },
  { id: "opencode", name: "OpenCode", running: false, can_source: true, can_switch: false, can_target: true, custom: false, delivery: "clipboard" },
  { id: "deepseek", name: "DeepSeek Harness", running: false, can_source: true, can_switch: false, can_target: true, custom: false, delivery: "clipboard" },
  { id: "copilot", name: "GitHub Copilot", running: false, can_source: true, can_switch: false, can_target: true, custom: false, delivery: "clipboard" },
  { id: "antigravity", name: "Antigravity", running: false, can_source: true, can_switch: false, can_target: true, custom: false, delivery: "clipboard" },
  { id: "chatgpt", name: "ChatGPT", running: false, can_source: false, can_switch: false, can_target: true, custom: false, delivery: "clipboard" },
  { id: "gigacode", name: "GigaCode", running: false, can_source: false, can_switch: false, can_target: true, custom: false, delivery: "clipboard" },
];
const handoffChats = {
  claude: [{ id: "claude:a", harness: "claude", title: "Claude чат", cwd: "D:/work", mtime: NOW - 60, turns: 2, chars: 100, fits_full: true }],
  codex: [{ id: "codex:b", harness: "codex", title: "Codex чат", cwd: "D:/work", mtime: NOW - 120, turns: 1, chars: 80, fits_full: true }],
  kimi: [],
  qoder: [],
  opencode: [{ id: "opencode:c", harness: "opencode", title: "OpenCode чат", cwd: "D:/work", mtime: NOW - 180, turns: null, chars: null, fits_full: null }],
  deepseek: [{ id: "deepseek:d", harness: "deepseek", title: "DSH чат", cwd: "D:/work", mtime: NOW - 240, turns: null, chars: null, fits_full: null }],
  copilot: [],
  antigravity: [{ id: "antigravity:e", harness: "antigravity", title: "Antigravity чат", cwd: "d:/work", mtime: NOW - 300, turns: 1, chars: 50, fits_full: true }],
};

const routes = {
  "/api/stats": {
    total: 42,
    by_source: { kimi: 30, git: 12 },
    records_total: 10,
    records_7d: 4,
    projects_total: 3,
    projects_active_30d: 2,
    knowledge: { "решение": 3, "ошибка": 1, "команда": 0, "факт": 5, total: 9 },
    queue: { configured: true, records: 10, cards: 7, done: 6, pending: 2, waiting: 1, held_local: 1, failed: 0, fragments: 42 },
    sources: [{ source: "kimi", records: 7, cards: 5 }, { source: "git", records: 3, cards: 2 }],
    last_sync_ts: NOW - 180,
    next_sync_ts: NOW + 720,
    sync: { mode: "interval", interval_minutes: 15 },
    server_ts: NOW,
  },
  "/api/timeline": {
    total: 3,
    sources: ["git", "kimi"],
    by_source: { git: 1, kimi: 2 },
    weeks: [
      { key: "2026-10-19", label: "19 окт", end_label: "25 окт", total: 0, by_source: {} },
      { key: "2026-10-26", label: "26 окт", end_label: "1 ноя", total: 3, by_source: { git: 1, kimi: 2 } },
    ],
  },
  "/api/projects": {
    projects: [
      { project: "Programs/memory-base", key: "Programs/memory-base", overridden: false, name: "memory-base", path: "D:/projects/memory-base", source: "git", sources: { git: 2, kimi: 1 }, count: 20, records: 3, last_ts: NOW - 100, spark: Array(12).fill(0).map((_, i) => (i === 11 ? 3 : 0)), keywords: ["sqlite", "<b>fts</b>"] },
      { project: "kimi", key: "kimi", overridden: false, name: "kimi", path: "kimi", source: "kimi", sources: { kimi: 2 }, count: 30, records: 2, last_ts: NOW - 900000, spark: Array(12).fill(0), keywords: [] },
      { project: "Brepsi Brain", key: "projects/brepsi-brain", overridden: true, name: "Brepsi Brain", path: "D:/Projects/brepsi-brain", source: "git", sources: { git: 4 }, count: 30, records: 5, last_ts: NOW - 300, spark: Array(12).fill(0), keywords: [] },
    ],
  },
  "/api/records": {
    total: 3,
    offset: 0,
    limit: 50,
    records: [
      { id: 1, source: "kimi", ts: NOW - 100, title: "<script>alert(1)</script> врезка", snippet: "сниппет <b>жирный</b>", rank: null, project: "Programs/memory-base", fragments: 4, has_card: true },
      { id: 2, source: "git", ts: NOW - 200, title: "коммит", snippet: "сниппет два", rank: null, project: "repo", fragments: 1, has_card: false },
    ],
  },
  "/api/search": {
    cards: [{ id: 3, source: "git", ts: NOW - 300, title: "hit <title> one", snippet: "found it", rank: 0.98, project: "repo" }],
  },
  "/api/card": {
    id: 3,
    source: "llm:git",
    ts: NOW - 300,
    title: "Card <em>три</em>",
    kind: "distilled",
    body: "тело карточки",
    summary: "Итог <img src=x onerror=alert(1)> и **важно**",
    questions: ["Как <это> искать?"],
    atoms: [{ type: "решение", text: "взяли <FTS5>" }, { type: "команда", text: "uv run" }],
    keywords: ["fts", "<k>"],
    similar: [{ id: 4, title: "похожая <запись>", source: "kimi", project: "a/b", weight: 0.4 }],
    fragments: [{ id: 11, title: "первый ход", date: "2026-11-01" }],
    transcript: "расшифровка <pre>",
    transcript_chars: 200000,
    record: { started_ts: NOW - 900, last_ts: NOW - 300, fragments: 2, project: "repo", project_name: "repo", project_path: "D:/projects/repo", file: "C:/s/one.jsonl", node_id: 3, has_card: true },
  },
  "/api/landscape": {
    nodes: [
      { id: 1, source: "git", title: "git <one>", ts: NOW, project: "a/alpha", has_card: true },
      { id: 2, source: "git", title: "git два", ts: NOW, project: "a/alpha", has_card: false },
      { id: 3, source: "kimi", title: "kimi три", ts: NOW, project: "a/alpha", has_card: true },
      { id: 4, source: "codex", title: "codex четыре", ts: NOW, project: "beta", has_card: true },
      { id: 5, source: "obsidian", title: "obsidian пять", ts: NOW, project: "gamma", has_card: true },
    ],
    edges: [[1, 2, 0.912], [2, 3, 0.45], [4, 5, 0.12]],
    projects: [
      { project: "a/alpha", name: "alpha", records: 3 },
      { project: "beta", name: "beta", records: 1 },
      { project: "gamma", name: "gamma", records: 1 },
    ],
    generated_ms: 12,
  },
  "/api/answer": {
    answer: "Ответ <b>жирный</b> со ссылкой [1] и ложной [9].",
    used_llm: true,
    cards: [{ id: 1, source: "kimi", ts: NOW, title: "источник один", snippet: "", rank: 0.9 }],
  },
  "/api/history": { items: [
    { id: 7, q: "вопрос", ts: NOW, date: "2026-11-05", answered: true, cards: 1 },
    { id: 8, q: "старый <вопрос>", ts: NOW - 100, date: "2026-11-05", answered: false, cards: 2 },
  ] },
  "/api/history/item": {
    id: 8, q: "старый <вопрос>", ts: NOW - 100, date: "2026-11-05", answer: null, used_llm: false,
    memory_ref: "llm:search:abc", cards: [{ id: 2, source: "git", ts: NOW, title: "сохранённый источник", snippet: "", rank: null }],
  },
  "/api/history/delete": { removed: true },
  "/api/usage": { totals: { runs: 0, prompt_tokens: 0, completion_tokens: 0, cost_rub: 0 }, runs: [] },
  "/api/settings": settingsFixture,
  "/api/settings/tasks": settingsFixture,
  "/api/models": { models: ["a", "b<c"] },
  "/api/sync": { new_cards: 2, llm: { configured: true, written: 1, candidates: 1 } },
  "/api/local/status": localStatusFixture,
  "/api/local/install": installStatusFixture,
  "/api/local/recommended": installStatusFixture,
  "/api/local/settings": localStatusFixture,
  "/api/local/route": settingsFixture,
  "/api/local/cancel": { cancelled: true },
  "/api/local/runtime": installStatusFixture,
  "/api/local/start": { ...localStatusFixture, runtime: { ...localStatusFixture.runtime, running: true } },
  "/api/local/stop": localStatusFixture,
  "/api/local/models/remove": localStatusFixture,
  "/api/local/bench": installStatusFixture,
  "/api/models/stats": {
    days: 30,
    hours: { today: 3.2, week: 9.75, total: 12.5 },
    sessions: [
      { started_ts: NOW - 7200, ended_ts: NOW - 3600, seconds: 3600, models: "brepsi/qwen25-7b", trigger: "distill" },
      { started_ts: NOW - 90000, ended_ts: NOW - 86400, seconds: 3600, models: "brepsi/qwen25-7b, brepsi/gemma3-1b", trigger: "manual" },
    ],
    tasks: [{ task: "distill", runs: 4, prompt_tokens: 1000, completion_tokens: 500, cost_rub: 0 }],
    models: [{ model: "brepsi/qwen25-7b", runs: 4, prompt_tokens: 1000, completion_tokens: 500, cost_rub: 0 }],
    bench: [{ model: "brepsi/qwen25-7b", ts: NOW - 1000, tokens: 64, load_ms: 320, seconds: 2.5, tps: 24.5 }],
    totals: { runs: 4, prompt_tokens: 1000, completion_tokens: 500, cost_rub: 0 },
  },
  "/api/sync/schedule": {
    mode: "interval", interval_minutes: 5, task: true, state: "ready",
    next_run_ts: NOW + 300, last_run_ts: NOW - 100, last_result: 0, next_sync_ts: NOW + 300,
    warning: "Задача планировщика создана",
  },
  "/api/queue/failed": { failed: [
    { unit: "claude:x", title: "упавшая <запись>", source: "claude", project: "a/b", last_ts: NOW - 50, attempts: 3, last_error: "timeout <err>" },
  ] },
  "/api/queue/retry": { requeued: 1 },
  "/api/log": { name: "sync.log", text: "строка журнала <b>\nlast", bytes: 30, truncated: false },
  "/api/projects/propose": {
    proposals: [{
      key: "projects/brepsi-brain", name: "Brepsi Brain", path: "D:/Projects/brepsi-brain",
      evidence: "remote origin и свежие записи",
    }],
  },
  "/api/projects/apply": { ok: true },
  "/api/projects/reset": { removed: true },
  "/api/handoff/harnesses": { harnesses: handoffHarnesses },
  "/api/handoff/switch": { pending: null, account: { id: null, exact: false, label: null }, changed: null },
  "/api/handoff": {
    chat: "claude:a", title: "Claude чат", cwd: "D:/work", mode: "full", requested: "auto",
    note: null, path: "D:/work/brepsi-brain/data/handoffs/x.md", prompt: "prompt", chars: 100,
  },
  "/api/handoff/launch": { items: [{ delivered: "clipboard" }, { delivered: "clipboard" }], prompt: "prompt" },
};

const fetchCalls = [];
const failPaths = new Set();
const defaultFetch = async (url, init) => {
  const u = String(url);
  fetchCalls.push({ url: u, init });
  const [path, query = ""] = u.split("?");
  if (failPaths.has(path)) throw new Error("сервер недоступен");
  if (path === "/api/handoff/chats") {
    const harness = new URLSearchParams(query).get("harness");
    return { ok: true, status: 200, json: async () => ({ chats: handoffChats[harness] || [] }) };
  }
  return { ok: true, status: 200, json: async () => routes[path] };
};
globalThis.fetch = defaultFetch;

let queuedFrames = false;
let frameId = 0;
const frames = new Map();
globalThis.requestAnimationFrame = (fn) => {
  const id = ++frameId;
  if (queuedFrames) frames.set(id, fn);
  else fn(performance.now() + 1000); // за пределами любого перехода: немедленный кадр его завершает
  return id;
};
globalThis.cancelAnimationFrame = (id) => frames.delete(id);
let reducedMotion = false;
const motionListeners = [];
const graphResizes = [];
globalThis.ResizeObserver = class {
  constructor(callback) { graphResizes.push(callback); }
  observe() {}
};
globalThis.matchMedia = () => ({
  get matches() { return reducedMotion; },
  addEventListener(_type, fn) { motionListeners.push(fn); },
});

function advanceFrame(now) {
  const callbacks = [...frames.values()];
  frames.clear();
  callbacks.forEach((fn) => fn(now));
}

const tick = () => new Promise((r) => setTimeout(r, 0));
const settle = async () => { for (let i = 0; i < 4; i++) await tick(); };
const fire = (target, type, event = {}) => {
  for (const fn of target.listeners[type] || []) fn(event);
};
const key = (name, extra = {}) => {
  for (const fn of docListeners.keydown || []) fn({ key: name, preventDefault() {}, target: { tagName: "BODY" }, ...extra });
};
const fakeTarget = (map) => ({ closest: (sel) => map[sel] || null });
const lastCall = (path) => [...fetchCalls].reverse().find((c) => c.url.startsWith(path));

let passed = 0;
function check(name, fn) {
  return Promise.resolve()
    .then(fn)
    .then(() => { passed += 1; console.log(`ok ${passed} — ${name}`); })
    .catch((e) => { console.error(`FAIL — ${name}:`, e.message); process.exitCode = 1; });
}

// -- запуск --------------------------------------------------------------------

el("palette").hidden = true; // скрыта в index.html
el("models-only-fits").checked = true; // отмечена в index.html; заглушка DOM разметку не разбирает
await import(pathToFileURL(join(here, "i18n.js")).href);
await import(pathToFileURL(join(here, "app.js")).href);
await settle();

await check("every Russian UI string has an English translation", () => {
  const app = readFileSync(join(here, "app.js"), "utf8");
  const keys = new Set([...app.matchAll(/tr\("((?:[^"\\]|\\.)*)"/g)].map((m) => m[1]));
  for (const m of app.matchAll(/plural\([^,]+,\s*"([^"]+)",\s*"[^"]+",\s*"([^"]+)"\)/g)) keys.add(m[1]).add(m[2]);
  for (const kind of ["решение", "ошибка", "команда", "факт", "решений", "ошибок", "команд", "фактов"]) keys.add(kind);
  // Текст и подписи index.html, кроме названия языка на самом языке.
  const html = readFileSync(join(here, "index.html"), "utf8").replace(/<(script|style)[\s\S]*?<\/\1>/g, "");
  for (const m of html.matchAll(/(?:placeholder|title|aria-label)="([^"]*[А-Яа-яЁё][^"]*)"|>([^<]*[А-Яа-яЁё][^<]*)</g)) {
    const key = (m[1] ?? m[2]).trim();
    if (key !== "Русский") keys.add(key);
  }
  const missing = [...keys].filter((k) => globalThis.SB_EN[k] === undefined);
  assert.deepEqual(missing, []);
  const placeholders = (s) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join();
  const broken = [...keys].filter((k) => placeholders(k) !== placeholders(globalThis.SB_EN[k]));
  assert.deepEqual(broken, []);
});

await check("start opens the overview", () => {
  assert.equal(el("page-overview").hidden, false);
  assert.equal(el("page-graph").hidden, true);
  assert.ok(lastCall("/api/stats"));
  assert.ok(lastCall("/api/timeline").url.includes("weeks=0"));
  assert.equal(el("breadcrumb-current").textContent, "Обзор");
});

await check("typing in the project filter keeps a load error visible", async () => {
  failPaths.add("/api/projects");
  goto("#projects");
  await settle();
  assert.ok(el("projects-grid").innerHTML.includes("Проекты не загрузились"));
  el("projects-filter").value = "kimi";
  fire(el("projects-filter"), "input");
  assert.ok(el("projects-grid").innerHTML.includes("Проекты не загрузились"), el("projects-grid").innerHTML);
  failPaths.delete("/api/projects");
  el("projects-filter").value = "";
  goto("#overview");
  await settle();
});

await check("sidebar says when the last sync ran and the next one runs", () => {
  assert.ok(el("sync-status").textContent.includes("3 мин назад"), el("sync-status").textContent);
  assert.ok(el("sync-built").textContent.startsWith("следующая в"));
  assert.equal(el("nav-graph").textContent, "10");
  assert.ok(el("stats").innerHTML.includes("<b>10</b> записей"));
});

await check("tiles separate records, projects, knowledge and coverage", () => {
  const tiles = el("tiles").innerHTML;
  assert.ok(tiles.includes("4 активных за 7 дней"));
  assert.ok(tiles.includes("2 живых за 30 дней"));
  assert.ok(tiles.includes("3 решения"));
  assert.ok(tiles.includes("66%"), "6 done of 9 eligible: held records do not drag it");
  assert.ok(tiles.includes("3 в очереди"));
  assert.ok(el("source-breakdown").innerHTML.includes("Kimi Code"));
  assert.ok(tiles.includes("только для локальной модели"));
});

await check("timeline draws stacks, a labelled peak and keeps empty weeks", () => {
  const svg = el("timeline").innerHTML;
  assert.ok(svg.includes('class="tl-peak"'));
  assert.equal((svg.match(/class="tl-col"/g) || []).length, 2);
  assert.ok(el("timeline-legend").innerHTML.includes("Kimi Code"));
  assert.ok(el("timeline-table").innerHTML.includes("19 окт") === false, "empty week not in the table");
  assert.ok(el("timeline-table").innerHTML.includes("26 окт"));
});

await check("overview lists top projects and fresh records, escaped", () => {
  assert.ok(el("top-projects").innerHTML.includes("memory-base"));
  const fresh = el("fresh").innerHTML;
  assert.ok(fresh.includes("&lt;script&gt;alert(1)&lt;/script&gt;"));
  assert.ok(!fresh.includes("<script>"));
});

await check("records page lists rows by day with state and escaping", async () => {
  goto("#records");
  await settle();
  assert.equal(el("page-records").hidden, false);
  const html = el("records-list").innerHTML;
  assert.ok(html.includes("сниппет &lt;b&gt;жирный&lt;/b&gt;"));
  assert.ok(html.includes("пересказа пока нет"));
  assert.ok(html.includes("4 фрагмента"));
});

await check("more button loads the next page with an offset", async () => {
  assert.ok(el("records-list").innerHTML.includes("осталось 1"));
  fire(el("records-list"), "click", { target: fakeTarget({ "#records-more": {} }) });
  await settle();
  assert.ok(lastCall("/api/records").url.includes("offset=2"));
});

await check("source chip filters and stays pressed", async () => {
  fire(el("records-sources"), "click", { target: fakeTarget({ ".chip": { dataset: { source: "git" } } }) });
  await settle();
  assert.ok(lastCall("/api/records").url.includes("source=git"));
  assert.ok(el("records-sources").innerHTML.includes('aria-pressed="true"'));
  assert.equal(el("records-clear").hidden, false);
  fire(el("records-clear"), "click");
  await settle();
  assert.ok(!lastCall("/api/records").url.includes("source="));
  assert.equal(el("records-clear").hidden, true);
});

await check("a project card opens its records", async () => {
  goto("#projects");
  await settle();
  assert.ok(el("projects-grid").innerHTML.includes("D:/projects/memory-base"));
  assert.ok(el("projects-grid").innerHTML.includes("&lt;b&gt;fts&lt;/b&gt;"));
  const card = { dataset: { project: "Programs/memory-base" } };
  const target = fakeTarget({ ".source-line[data-project], .project-chip[data-project]": card });
  for (const fn of docListeners.click || []) fn({ target });
  await settle();
  goto("#records");
  await settle();
  assert.ok(lastCall("/api/records").url.includes("project=Programs%2Fmemory-base"));
  fire(el("records-clear"), "click");
  await settle();
});

await check("projects filter narrows the grid locally", async () => {
  goto("#projects");
  await settle();
  el("projects-filter").value = "kimi";
  fire(el("projects-filter"), "input");
  assert.ok(!el("projects-grid").innerHTML.includes("memory-base"));
  el("projects-filter").value = "";
  fire(el("projects-filter"), "input");
});

await check("a source line opens its records", async () => {
  goto("#overview");
  await settle();
  const target = fakeTarget({ ".source-line[data-source]": { dataset: { source: "git" } } });
  for (const fn of docListeners.click || []) fn({ target });
  await settle();
  goto("#records");
  await settle();
  assert.ok(lastCall("/api/records").url.includes("source=git"));
  fire(el("records-clear"), "click");
  await settle();
});

await check("record panel shows facts, sections and escapes everything", async () => {
  fire(el("records-list"), "click", { target: fakeTarget({}) });
  for (const fn of docListeners.click || []) {
    fn({ target: fakeTarget({ ".preview-row[data-id], .record-row[data-id], .answer-source[data-id]": { dataset: { id: "3" } } }) });
  }
  await settle();
  const html = el("panel").innerHTML;
  assert.equal(el("panel").hidden, false);
  assert.equal(el("scrim").hidden, false);
  for (const expected of ["Последнее сообщение", "C:/s/one.jsonl", "Похожие записи", "Атомы", "Как это будет искать", "Ключевые слова", "Расшифровка", "#graph/3"]) {
    assert.ok(html.includes(expected), expected);
  }
  assert.ok(html.includes("Card &lt;em&gt;три&lt;/em&gt;"));
  assert.ok(html.includes("<strong>важно</strong>"), "light markdown is rendered");
  assert.ok(!html.includes("<img"));
  assert.ok(html.includes("is-command"));
  assert.ok(html.includes("Показано"), "a cut transcript says so");
});

await check("panel copies the card and closes on Escape", async () => {
  await el("panel").listeners.click[0]({ target: fakeTarget({ "[data-action]": { dataset: { action: "copy" } } }) });
  const [title, origin, file, blank, ...body] = clipboard.at(-1).split("\n");
  assert.equal(title, "# Card <em>три</em>");
  assert.match(origin, /^Git · .+ · repo$/);
  assert.equal(file, "C:/s/one.jsonl");
  assert.equal(blank, "");
  assert.equal(body.join("\n"), "тело карточки");
  key("Escape");
  assert.equal(el("panel").hidden, true);
  assert.equal(el("scrim").hidden, true);
});

await check("slow card response cannot reopen a closed panel", async () => {
  let finish;
  globalThis.fetch = () => new Promise((resolve) => { finish = resolve; });
  try {
    goto("#record/3");
    await tick();
    key("Escape");
    finish({ ok: true, status: 200, json: async () => routes["/api/card"] });
    await settle();
    assert.equal(el("panel").hidden, true);
    assert.ok(!el("panel").innerHTML.includes("Расшифровка"));
  } finally { globalThis.fetch = defaultFetch; }
});

await check("tray deep link opens a record and keeps the page", async () => {
  goto("#record/3");
  await settle();
  assert.equal(el("panel").hidden, false);
  assert.equal(location.hash, "#records");
  key("Escape");
});

await check("slow older search cannot replace newer results", async () => {
  const pending = new Map();
  globalThis.fetch = (url) => {
    const q = new URL(String(url), "http://localhost").searchParams.get("q");
    return new Promise((resolve) => pending.set(q, (title) => resolve({
      ok: true, status: 200, json: async () => ({ cards: [{ id: 9, source: "git", ts: NOW, title, snippet: "" }] }),
    })));
  };
  try {
    el("records-q").value = "older";
    fire(el("records-q"), "input");
    await new Promise((r) => setTimeout(r, 200));
    el("records-q").value = "newer";
    fire(el("records-q"), "input");
    await new Promise((r) => setTimeout(r, 200));
    pending.get("newer")("newer result");
    await settle();
    pending.get("older")("older result");
    await settle();
    assert.ok(el("records-list").innerHTML.includes("newer result"));
    assert.ok(!el("records-list").innerHTML.includes("older result"));
  } finally {
    globalThis.fetch = defaultFetch;
    el("records-q").value = "";
    fire(el("records-clear"), "click");
    await settle();
  }
});

await check("records error state renders", async () => {
  failPaths.add("/api/records");
  goto("#overview");
  await settle();
  goto("#records");
  await settle();
  failPaths.delete("/api/records");
  assert.ok(el("records-list").innerHTML.includes("Ошибка загрузки"));
});

await check("graph page loads all records and describes them", async () => {
  goto("#graph");
  await settle();
  assert.equal(el("page-graph").hidden, false);
  assert.ok(lastCall("/api/landscape"));
  assert.ok(!lastCall("/api/landscape").url.includes("limit"), "every record, no cap");
  const lede = el("graph-lede").textContent;
  assert.ok(lede.includes("5 записей") && lede.includes("3 проекта") && lede.includes("3 связи"), lede);
  assert.equal(el("breadcrumb-current").textContent, "Карта памяти");
  assert.ok(el("graph-sources").innerHTML.includes("Obsidian"));
  assert.ok(el("graph-canvas").__graph);
});

await check("records gather around their project's hub", () => {
  const probe = el("graph-canvas").__graph;
  const groups = probe.groups();
  const alpha = groups.find((g) => g.project === "a/alpha");
  assert.equal(alpha.size, 3);
  for (const id of [1, 2, 3]) {
    const n = probe.nodeXY(id);
    const own = Math.hypot(n.x - alpha.x, n.y - alpha.y);
    assert.ok(own <= alpha.r);
    for (const other of groups) {
      if (other !== alpha) assert.ok(own < Math.hypot(n.x - other.x, n.y - other.y), `${id} nearer ${other.project}`);
    }
  }
});

await check("project hubs keep apart", () => {
  const groups = el("graph-canvas").__graph.groups();
  assert.equal(groups.length, 3);
  for (let i = 0; i < groups.length; i++) {
    for (let j = i + 1; j < groups.length; j++) {
      const d = Math.hypot(groups[i].x - groups[j].x, groups[i].y - groups[j].y);
      assert.ok(d > 20, `${groups[i].project} / ${groups[j].project}: ${d}`);
    }
  }
});

await check("a click frames the point's tree; a list selection centres the point", async () => {
  const probe = el("graph-canvas").__graph;
  const scaleNow = () => probe.view().scale;
  // Сцена 800x560, карточка min(340, 44%) + 12 по ширине: свободно 448 px.
  const centred = (id) => {
    const n = probe.nodeXY(id);
    const q = probe.toScreen(n.x, n.y);
    return Math.abs(q.x - 224) < 1e-6 && Math.abs(q.y - 280) < 1e-6;
  };
  const focus = async (id) => {
    fire(el("graph-card"), "click", { target: fakeTarget({ "[data-focus]": { dataset: { focus: String(id) } } }) });
    await settle();
  };
  const click = async (id) => {
    const n = probe.nodeXY(id);
    const q = probe.toScreen(n.x, n.y);
    fire(el("graph-canvas"), "click", { offsetX: q.x, offsetY: q.y });
    await settle();
  };
  fire(el("graph-canvas"), "dblclick");
  await settle();
  const overview = probe.view();
  await click(2);
  assert.equal(probe.selected(), 2);
  assert.ok(scaleNow() > overview.scale, "the camera moves in on the tree");
  const alpha = probe.groups().find((g) => g.project === "a/alpha");
  for (const p of [probe.nodeXY(1), probe.nodeXY(2), probe.nodeXY(3), alpha]) {
    const q = probe.toScreen(p.x, p.y);
    assert.ok(q.x > 0 && q.x < 448 && q.y > 0 && q.y < 560, "linked records and the project stay left of the card");
  }
  key("Escape");
  await settle();
  assert.deepEqual(probe.view(), overview, "clearing zooms back out");
  await focus(4);
  assert.equal(probe.selected(), 4);
  assert.ok(centred(4), "the point sits in the middle of the free area");
  assert.ok(Math.abs(scaleNow() / overview.scale - 1.4) < 1e-9, "moves in a little from the overview");
  for (let i = 0; i < 8; i++) fire(el("graph-zoom-in"), "click");
  const close = scaleNow();
  await focus(5);
  assert.equal(probe.selected(), 5);
  assert.ok(centred(5));
  assert.equal(scaleNow(), close, "a closer camera keeps its scale");
});

await check("selection shows the small card and updates the address", async () => {
  assert.equal(el("graph-card").hidden, false);
  const html = el("graph-card").innerHTML;
  assert.ok(html.includes("Открыть карточку") && html.includes("Только связанные"));
  assert.ok(html.includes("похожая &lt;запись&gt;"));
  assert.equal(location.hash, "#graph/5");
});

await check("Escape and empty space clear the selection", async () => {
  const probe = el("graph-canvas").__graph;
  key("Escape");
  assert.equal(probe.selected(), null);
  assert.equal(el("graph-card").hidden, true);
  assert.equal(location.hash, "#graph");
  const n = probe.nodeXY(2);
  const q = probe.toScreen(n.x, n.y);
  fire(el("graph-canvas"), "click", { offsetX: q.x, offsetY: q.y });
  await settle();
  assert.equal(probe.selected(), 2);
  fire(el("graph-canvas"), "click", { offsetX: -5000, offsetY: -5000 });
  assert.equal(probe.selected(), null);
});

await check("a second click opens what the first one selected", async () => {
  const probe = el("graph-canvas").__graph;
  const clickAt = async (p) => {
    const q = probe.toScreen(p.x, p.y);
    fire(el("graph-canvas"), "click", { offsetX: q.x, offsetY: q.y });
    await settle();
  };
  await clickAt(probe.nodeXY(4));
  assert.equal(el("panel").hidden, true);
  await clickAt(probe.nodeXY(4));
  assert.equal(el("panel").hidden, false, "record panel opened");
  key("Escape");
  key("Escape");
  const beta = probe.groups().find((g) => g.project === "beta");
  await clickAt(beta);
  assert.equal(probe.selected(), "p:beta");
  assert.ok(el("graph-card").innerHTML.includes("Открыть записи"));
  assert.ok(el("graph-card").innerHTML.includes("codex четыре"));
  key("Escape");
});

await check("a project zooms to its cloud and the camera returns on clearing", async () => {
  const probe = el("graph-canvas").__graph;
  fire(el("graph-canvas"), "dblclick");
  await settle();
  const before = probe.view();
  const alpha = probe.groups().find((g) => g.project === "a/alpha");
  const q = probe.toScreen(alpha.x, alpha.y);
  fire(el("graph-canvas"), "click", { offsetX: q.x, offsetY: q.y });
  await settle();
  assert.equal(probe.selected(), "p:a/alpha");
  assert.ok(probe.view().scale > before.scale, "the camera moves in");
  const free = 800 - (Math.min(340, 800 * 0.44) + 12);
  for (const id of [1, 2, 3]) {
    const n = probe.nodeXY(id);
    const s = probe.toScreen(n.x, n.y);
    assert.ok(s.x > 0 && s.x < free && s.y > 0 && s.y < 560, `record ${id} framed left of the card`);
  }
  fire(el("graph-canvas"), "click", { offsetX: -5000, offsetY: -5000 });
  await settle();
  assert.equal(probe.selected(), null);
  assert.deepEqual(probe.view(), before, "clearing zooms back out");
  fire(el("graph-canvas"), "click", { offsetX: q.x, offsetY: q.y });
  await settle();
  fire(el("graph-zoom-in"), "click");
  await settle();
  const mine = probe.view();
  key("Escape");
  await settle();
  assert.deepEqual(probe.view(), mine, "a zoom by hand keeps the reader's view");
  fire(el("graph-canvas"), "dblclick");
  await settle();
});

await check("full screen frames the whole map again", () => {
  const probe = el("graph-canvas").__graph;
  for (let i = 0; i < 6; i++) fire(el("graph-zoom-in"), "click");
  const zoomed = probe.view().scale;
  assert.ok(zoomed > probe.overviewScale());
  for (const fn of docListeners.fullscreenchange || []) fn({});
  assert.equal(probe.view().scale, probe.overviewScale());
});

await check("hover shows an escaped tooltip", () => {
  const probe = el("graph-canvas").__graph;
  const n = probe.nodeXY(1);
  const q = probe.toScreen(n.x, n.y);
  fire(el("graph-canvas"), "mousemove", { offsetX: q.x, offsetY: q.y });
  assert.equal(el("graph-tip").hidden, false);
  assert.ok(el("graph-tip").innerHTML.includes("git &lt;one&gt;"));
  assert.ok(el("graph-tip").innerHTML.includes(", alpha, "), "source, project, date");
  const alpha = probe.groups().find((g) => g.project === "a/alpha");
  const h = probe.toScreen(alpha.x, alpha.y);
  fire(el("graph-canvas"), "mousemove", { offsetX: h.x, offsetY: h.y });
  assert.ok(el("graph-tip").innerHTML.includes("проект, 3 записи"));
  fire(el("graph-canvas"), "mouseleave");
  assert.equal(el("graph-tip").hidden, true);
});

await check("a pan on empty space never counts as a click", async () => {
  const probe = el("graph-canvas").__graph;
  const canvas = el("graph-canvas");
  const before = probe.view();
  fire(canvas, "pointerdown", { button: 0, offsetX: 2, offsetY: 2, pointerId: 1 });
  fire(canvas, "pointermove", { offsetX: 32, offsetY: 2 });
  fire(canvas, "pointerup", {});
  fire(canvas, "click", { offsetX: 32, offsetY: 2 });
  assert.equal(probe.view().tx, before.tx + 30);
  assert.equal(probe.selected(), null);
});

await check("dragging a point pulls its links along, then the map cools and stops", async () => {
  const probe = el("graph-canvas").__graph;
  const canvas = el("graph-canvas");
  const n = probe.nodeXY(2);
  const q = probe.toScreen(n.x, n.y);
  const linked = probe.nodeXY(1);
  const hub = probe.groups().find((g) => g.project === "a/alpha");
  const view = probe.view();
  queuedFrames = true;
  try {
    const start = performance.now();
    fire(canvas, "pointerdown", { button: 0, offsetX: q.x, offsetY: q.y, pointerId: 1 });
    fire(canvas, "pointermove", { offsetX: q.x + 60, offsetY: q.y + 20 });
    for (let i = 1; i <= 40; i++) advanceFrame(start + 16 * i);
    const held = probe.toScreen(probe.nodeXY(2).x, probe.nodeXY(2).y);
    assert.ok(Math.hypot(held.x - q.x - 60, held.y - q.y - 20) < 1e-6, "the point stays under the pointer");
    const moved = probe.nodeXY(1);
    assert.ok(Math.hypot(moved.x - linked.x, moved.y - linked.y) > 1, "a linked record follows");
    const hubNow = probe.groups().find((g) => g.project === "a/alpha");
    assert.ok(Math.hypot(hubNow.x - hub.x, hubNow.y - hub.y) > 1, "the project hub follows");
    assert.deepEqual(probe.view(), view, "the camera does not pan");
    assert.equal(frames.size, 1, "frames run while the point is held");
    fire(canvas, "pointerup", {});
    fire(canvas, "click", { offsetX: q.x + 60, offsetY: q.y + 20 });
    assert.equal(probe.selected(), null, "a drag is not a click");
    for (let i = 41; i < 400 && frames.size; i++) advanceFrame(start + 16 * i);
    assert.equal(frames.size, 0, "the simulation cools and stops");
  } finally { queuedFrames = false; frames.clear(); }
});

await check("a selection glides the point on a straight line with an ease-out", async () => {
  goto("#graph");
  await settle();
  key("Escape");
  fire(el("graph-canvas"), "dblclick");
  const probe = el("graph-canvas").__graph;
  const screen = () => {
    const n = probe.nodeXY(4);
    return probe.toScreen(n.x, n.y);
  };
  const from = screen();
  queuedFrames = true;
  try {
    const start = performance.now();
    fire(el("graph-card"), "click", { target: fakeTarget({ "[data-focus]": { dataset: { focus: "4" } } }) });
    assert.deepEqual(screen(), from, "the camera must not jump on input");
    advanceFrame(start + 168);
    const mid = screen();
    advanceFrame(start + 600);
    const end = screen();
    const run = Math.hypot(end.x - from.x, end.y - from.y);
    const done = Math.hypot(mid.x - from.x, mid.y - from.y) / run;
    assert.ok(done > 0.55 && done < 0.8, `ease-out: ${done} of the way at 30% of the time`);
    const cross = (mid.x - from.x) * (end.y - from.y) - (mid.y - from.y) * (end.x - from.x);
    assert.ok(Math.abs(cross) / run < 0.5, "straight screen path");
    assert.equal(frames.size, 0, "no frames after the move");
  } finally { queuedFrames = false; frames.clear(); }
  key("Escape");
});

await check("graph deep link focuses the record after loading", async () => {
  goto("#overview");
  await settle();
  goto("#graph/4");
  await settle();
  assert.equal(el("graph-canvas").__graph.selected(), 4);
  key("Escape");
});

await check("graph error state renders", async () => {
  failPaths.add("/api/landscape");
  goto("#overview");
  await settle();
  goto("#graph");
  await settle();
  failPaths.delete("/api/landscape");
  assert.ok(el("graph-state").textContent.includes("Граф не построился"));
});

await check("answer renders escaped text with only real citations clickable", async () => {
  goto("#answers");
  await settle();
  el("answer-q").value = "вопрос";
  fire(el("answer-form"), "submit", { preventDefault() {} });
  await settle();
  const html = el("answer-out").innerHTML;
  assert.ok(html.includes("Ответ &lt;b&gt;жирный&lt;/b&gt;"));
  assert.ok(html.includes('class="cite" data-card="0"'));
  assert.ok(html.includes("[9]"), "a citation past the list stays text");
  assert.ok(html.includes('<h1 class="thread-q">вопрос</h1>'), "the question heads the thread");
  assert.ok(el("side-history").innerHTML.includes("вопрос"), "the sidebar lists it");
  assert.equal(el("side-recent").hidden, false);
  assert.equal(el("answer-q").value, "", "the composer is free for a follow-up");
  const request = fetchCalls.filter((call) => call.url.startsWith("/api/answer")).at(-1);
  assert.equal(request.init.method, "POST");
  assert.deepEqual(JSON.parse(request.init.body).history, []);
});

await check("answer without a model still lists what was found", async () => {
  const saved = routes["/api/answer"];
  routes["/api/answer"] = { answer: null, used_llm: false, cards: saved.cards };
  el("answer-q").value = "второй вопрос";
  fire(el("answer-form"), "submit", { preventDefault() {} });
  await settle();
  routes["/api/answer"] = saved;
  assert.ok(el("answer-out").innerHTML.includes("Модель не ответила"));
  assert.ok(el("answer-out").innerHTML.includes("источник один"));
  assert.ok(el("answer-history").innerHTML.includes(">вопрос<"), "the earlier question is offered below");
  assert.ok(!el("answer-history").innerHTML.includes("второй вопрос"), "the current one is not");
  const request = fetchCalls.filter((call) => call.url.startsWith("/api/answer")).at(-1);
  assert.deepEqual(JSON.parse(request.init.body).history, [{ q: "вопрос", card_ids: [1] }]);
});

await check("opening a recent answer uses its cached sources without another model call", async () => {
  const before = fetchCalls.length;
  const pick = (attr, id) => ({ target: { closest: (selector) => selector === `button[${attr}]` ? { dataset: { [attr === "data-history" ? "history" : "forget"]: id } } : null } });
  fire(el("side-history"), "click", pick("data-history", "7"));
  await settle();
  assert.equal(fetchCalls.length, before);
  assert.ok(el("answer-out").innerHTML.includes('<h1 class="thread-q">вопрос</h1>'));
});

await check("search history survives restarts: an old question opens from storage, not the model", async () => {
  assert.ok(el("side-history").innerHTML.includes("старый &lt;вопрос&gt;"), "server history is listed and escaped");
  assert.ok(el("side-history-count").textContent.includes("2"));
  const pick = { target: { closest: (selector) => selector === "button[data-history]" ? { dataset: { history: "8" } } : null } };
  fire(el("side-history"), "click", pick);
  await settle();
  assert.ok(lastCall("/api/history/item").url.includes("id=8"));
  assert.ok(el("answer-out").innerHTML.includes("сохранённый источник"));
  const asked = fetchCalls.filter((call) => call.url.startsWith("/api/answer")).length;
  assert.equal(asked, 2, "no new paid answer");
});

await check("search history collapses, remembers it, and forgets an entry", async () => {
  fire(el("side-history-toggle"), "click");
  assert.equal(el("side-history").hidden, true);
  assert.equal(el("side-history-toggle").getAttribute("aria-expanded"), "false");
  fire(el("side-history-toggle"), "click");
  assert.equal(el("side-history").hidden, false);
  const pick = { target: { closest: (selector) => selector === "button[data-forget]" ? { dataset: { forget: "8" } } : null } };
  fire(el("side-history"), "click", pick);
  await settle();
  const call = lastCall("/api/history/delete");
  assert.deepEqual(JSON.parse(call.init.body), { id: 8 });
  assert.ok(!el("side-history").innerHTML.includes("старый"));
});

await check("sources and answer buttons switch the visible content", async () => {
  fire(el("answer-out"), "click", { target: { closest: (selector) => selector === "[data-answer-tab]" ? { dataset: { answerTab: "sources" } } : null } });
  assert.ok(el("answer-out").classList.contains("sources-only"));
  fire(el("answer-out"), "click", { target: { closest: (selector) => selector === "[data-answer-tab]" ? { dataset: { answerTab: "answer" } } : null } });
  assert.equal(el("answer-out").classList.contains("sources-only"), false);
});

await check("processing page shows the queue and an honest empty log", async () => {
  goto("#activity");
  await settle();
  assert.ok(el("queue-legend").innerHTML.includes("Только локальная модель"));
  assert.ok(el("usage-runs").innerHTML.includes("не значит, что API не расходовался"));
  assert.ok(el("usage-summary").innerHTML.includes("нет данных"));
});

await check("sync posts, reports and refreshes", async () => {
  fire(el("sync-now"), "click");
  await settle();
  const call = lastCall("/api/sync");
  assert.equal(call.init.method, "POST");
  assert.ok(el("toast").textContent.includes("новых фрагментов: 2"));
  assert.equal(el("sync-now").disabled, false);
});

await check("settings dialog opens by hash, never shows a key, warns on a new host", async () => {
  goto("#settings");
  await settle();
  assert.equal(el("settings").open, true);
  assert.equal(el("set-key").value, "");
  assert.equal(el("set-provider").value, "custom");
  assert.ok(el("set-key-note").textContent.includes("сохранён для этого провайдера"));
  el("set-url").value = "https://two.example/v1";
  fire(el("set-url"), "input");
  assert.ok(el("set-key-note").textContent.includes("будет удалён"));
});

await check("model list asks the edited address", async () => {
  fire(el("set-refresh"), "click");
  await settle();
  const body = JSON.parse(lastCall("/api/models").init.body);
  assert.equal(body.base_url, "https://two.example/v1");
  assert.equal(body.api_key, null);
  assert.ok(el("set-models-note").textContent.includes("Доступно моделей: 2"));
  assert.ok(el("set-models").innerHTML.includes("b&lt;c"));
});

await check("saving posts JSON and closes; failure stays open with the reason", async () => {
  el("set-model").value = "m2";
  const saved = globalThis.fetch;
  globalThis.fetch = async () => ({ ok: false, status: 400, json: async () => ({ error: "model must be" }) });
  fire(el("settings-form"), "submit", { preventDefault() {} });
  await settle();
  assert.equal(el("settings").open, true);
  assert.ok(el("set-error").textContent.includes("model must be"));
  globalThis.fetch = saved;
  fire(el("settings-form"), "submit", { preventDefault() {} });
  await settle();
  const call = lastCall("/api/settings");
  assert.equal(call.init.method, "POST");
  assert.equal(call.init.headers["Content-Type"], "application/json");
  assert.equal(JSON.parse(call.init.body).model, "m2");
  assert.equal(el("settings").open, false);
});

await check("palette opens on Ctrl+K, finds records and runs actions", async () => {
  key("k", { ctrlKey: true });
  assert.equal(el("palette").hidden, false);
  assert.ok(el("palette-list").innerHTML.includes("Синхронизировать сейчас"));
  el("palette-input").value = "hit";
  fire(el("palette-input"), "input");
  await new Promise((r) => setTimeout(r, 200));
  await settle();
  assert.ok(el("palette-list").innerHTML.includes("hit &lt;title&gt; one"));
  // Esc не должен всплыть к document и закрыть заодно панель под палитрой.
  const escape = { key: "Escape", preventDefault() {}, stopped: false, stopPropagation() { this.stopped = true; } };
  fire(el("palette-input"), "keydown", escape);
  assert.equal(el("palette").hidden, true);
  assert.equal(escape.stopped, true);
});

await check("Ctrl+number switches pages, '/' opens the palette", async () => {
  key("3", { ctrlKey: true });
  assert.equal(location.hash, "#records");
  key("4", { ctrlKey: true });
  assert.equal(location.hash, "#graph");
  goto("#graph");
  await settle();
  key("/");
  assert.equal(el("palette").hidden, false);
  fire(el("palette-input"), "keydown", { key: "Escape", preventDefault() {}, stopPropagation() {} });
  assert.equal(el("palette").hidden, true);
});

await check("refresh updates changed records even when graph counts stay the same", async () => {
  const node = routes["/api/landscape"].nodes.find((n) => n.id === 1);
  const title = node.title;
  node.title = "updated record title";
  goto("#overview");
  goto("#graph");
  await settle();
  const probe = el("graph-canvas").__graph;
  const n = probe.nodeXY(1);
  const q = probe.toScreen(n.x, n.y);
  fire(el("graph-canvas"), "mousemove", { offsetX: q.x, offsetY: q.y });
  assert.ok(el("graph-tip").innerHTML.includes(node.title));
  fire(el("graph-canvas"), "mouseleave");
  node.title = title;
  goto("#overview");
  goto("#graph");
  await settle();
  assert.deepEqual(el("graph-canvas").__graph.nodeXY(1), n, "a fresh layout is deterministic");
});

await check("an empty graph clears the previous record's small card", async () => {
  goto("#graph/4");
  await settle();
  const data = routes["/api/landscape"];
  try {
    routes["/api/landscape"] = { nodes: [], edges: [], projects: [] };
    goto("#overview");
    goto("#graph");
    await settle();
    assert.equal(el("graph-card").hidden, true);
    assert.equal(location.hash, "#graph");
    assert.ok(el("graph-state").textContent.includes("Записей пока нет"));
  } finally {
    routes["/api/landscape"] = data;
    goto("#overview");
    goto("#graph");
    await settle();
  }
});

await check("with a selection, a hovered point shows its lines dimmed like the surroundings", async () => {
  goto("#graph");
  await settle();
  key("Escape");
  const canvas = el("graph-canvas");
  const probe = canvas.__graph;
  const at = (id) => { const n = probe.nodeXY(id); return probe.toScreen(n.x, n.y); };
  const line = (i, j) => {
    const a = probe.nodeXY(i), b = probe.nodeXY(j);
    return canvas.context.lines.find((l) => (l.a.x === a.x && l.b.x === b.x) || (l.a.x === b.x && l.b.x === a.x));
  };
  fire(canvas, "click", { offsetX: at(4).x, offsetY: at(4).y });
  await settle();
  assert.equal(probe.selected(), 4);
  assert.equal(line(1, 2), undefined, "unrelated lines are hidden");
  fire(canvas, "mousemove", { offsetX: at(2).x, offsetY: at(2).y });
  await settle();
  const own = line(4, 5);
  const glance = line(1, 2);
  assert.ok(own && own.alpha > 0.8 && own.color === "#cfd2d7", "the selection keeps its bright lines");
  assert.ok(glance && glance.alpha <= 0.2 && glance.alpha > 0.1, `hovered lines are dimmed: ${glance?.alpha}`);
  assert.equal(glance.color, "#9a9da2");
  assert.ok(line(2, 3), "every line of the hovered point shows");
  fire(canvas, "mouseleave");
  await settle();
  assert.equal(line(1, 2), undefined, "they go once the pointer leaves");
  key("Escape");
  await settle();
});

await check("hover fades unrelated dots gradually and returns to an idle graph", async () => {
  goto("#graph");
  await settle();
  key("Escape");
  const canvas = el("graph-canvas");
  const probe = canvas.__graph;
  const dotAlpha = (id) => {
    const n = probe.nodeXY(id);
    return canvas.context.dots.find((d) => d.x === n.x && d.y === n.y)?.alpha;
  };
  const n = probe.nodeXY(1);
  const q = probe.toScreen(n.x, n.y);
  queuedFrames = true;
  try {
    const start = performance.now();
    const before = dotAlpha(5);
    fire(canvas, "mousemove", { offsetX: q.x, offsetY: q.y });
    advanceFrame(start + 100);
    const during = dotAlpha(5);
    assert.ok(during > 0.1 && during < before, `intermediate opacity: ${during}`);
    assert.ok(dotAlpha(1) > during, "hovered record stays bright");
    advanceFrame(start + 400);
    assert.ok(dotAlpha(5) <= 0.2, "unrelated background is subdued");
    assert.equal(frames.size, 0, "no perpetual rendering after the transition");
    fire(canvas, "mouseleave");
    advanceFrame(performance.now() + 400);
    assert.equal(dotAlpha(5), before);
    assert.equal(frames.size, 0);
  } finally { queuedFrames = false; frames.clear(); }
});

await check("wheel zoom eases around the pointer and coalesces rapid events", () => {
  const canvas = el("graph-canvas");
  const probe = canvas.__graph;
  const before = probe.view();
  const pointer = { x: 137, y: 219 };
  const world = { x: (pointer.x - before.tx) / before.scale, y: (pointer.y - before.ty) / before.scale };
  queuedFrames = true;
  try {
    const start = performance.now();
    const wheel = () => fire(canvas, "wheel", {
      deltaY: -100, offsetX: pointer.x, offsetY: pointer.y, preventDefault() {},
    });
    wheel();
    wheel();
    assert.deepEqual(probe.view(), before, "camera must not jump on input");
    assert.equal(frames.size, 1, "one owner of the animation frame");
    const target = before.scale * 1.0025 ** 200;
    advanceFrame(start + 108);
    const done = Math.log(probe.view().scale / before.scale) / Math.log(target / before.scale);
    assert.ok(done > 0.55 && done < 0.8, `ease-out: ${done} of the zoom at 30% of the time`);
    const at = probe.toScreen(world.x, world.y);
    assert.ok(Math.hypot(at.x - pointer.x, at.y - pointer.y) < 1e-6, "pointer anchor does not drift");
    advanceFrame(start + 400);
    assert.ok(Math.abs(probe.view().scale - target) < 1e-9);
    assert.equal(frames.size, 0);
  } finally { queuedFrames = false; frames.clear(); }
});

await check("leaving the graph and hiding the document cancel pending frames", async () => {
  queuedFrames = true;
  try {
    fire(el("graph-zoom-in"), "click");
    assert.equal(frames.size, 1);
    goto("#overview");
    assert.equal(frames.size, 0, "navigation cancels animation ownership");
    goto("#graph");
    await settle();
    advanceFrame(performance.now() + 400);
    fire(el("graph-zoom-in"), "click");
    document.hidden = true;
    for (const fn of docListeners.visibilitychange || []) fn();
    assert.equal(frames.size, 0, "hidden windows do not render");
    document.hidden = false;
    for (const fn of docListeners.visibilitychange || []) fn();
    advanceFrame(performance.now() + 400);
    assert.equal(frames.size, 0);
  } finally { document.hidden = false; queuedFrames = false; frames.clear(); }
});

await check("reduced motion settles an in-flight camera without another tween", () => {
  queuedFrames = true;
  try {
    fire(el("graph-zoom-in"), "click");
    assert.equal(frames.size, 1);
    reducedMotion = true;
    motionListeners.forEach((fn) => fn());
    const settled = el("graph-canvas").__graph.view();
    advanceFrame(performance.now() + 16);
    assert.deepEqual(el("graph-canvas").__graph.view(), settled);
    assert.equal(frames.size, 0);
  } finally { reducedMotion = false; queuedFrames = false; frames.clear(); }
});

await check("resizing the canvas preserves the world point at its centre", () => {
  const canvas = el("graph-canvas");
  const stage = el("graph-stage");
  const probe = canvas.__graph;
  const n = probe.nodeXY(1);
  const before = probe.toScreen(n.x, n.y);
  try {
    stage.clientWidth = 1200;
    stage.clientHeight = 760;
    graphResizes.forEach((fn) => fn());
    const after = probe.toScreen(n.x, n.y);
    assert.ok(Math.abs(after.x - before.x - 200) < 1e-6);
    assert.ok(Math.abs(after.y - before.y - 100) < 1e-6);
  } finally {
    stage.clientWidth = 800;
    stage.clientHeight = 560;
    graphResizes.forEach((fn) => fn());
  }
});

await check("models page shows the banner, hardware, runtime and a fitting catalog", async () => {
  goto("#models");
  await settle();
  assert.equal(el("page-models").hidden, false);
  assert.equal(el("breadcrumb-current").textContent, "Модели и железо");
  assert.equal(el("models-banner").hidden, false);
  assert.ok(el("models-hardware").innerHTML.includes("RTX 3060"));
  assert.ok(el("models-hardware").innerHTML.includes("12"));
  assert.ok(el("models-runtime").innerHTML.includes("Установлен"));
  assert.equal(el("models-offline").disabled, false, "the switch works before any model is installed");
  assert.ok(el("models-offline-note").textContent.includes("Можно включить заранее"));
  const html = el("models-catalog").innerHTML;
  assert.ok(html.includes("Qwen2.5 7B Instruct"));
  assert.ok(html.includes("Рекомендуем"), "recommended badge");
  assert.ok(html.includes("Роли:"));
  assert.ok(html.includes("Ollama"));
  assert.ok(!html.includes("Gemma 3 27B"), "a model that does not fit is filtered out by default");
  el("models-only-fits").checked = false;
  fire(el("models-only-fits"), "change");
  assert.ok(el("models-catalog").innerHTML.includes("Gemma 3 27B"));
  el("models-only-fits").checked = true;
  fire(el("models-only-fits"), "change");
});

await check("local-only is saved before any model is installed and names the empty state", async () => {
  const saved = routes["/api/local/settings"];
  routes["/api/local/settings"] = { ...localStatusFixture, settings: { ...localStatusFixture.settings, offline_only: true } };
  try {
    fire(el("models-offline"), "change", { target: { checked: true } });
    await settle();
    assert.deepEqual(JSON.parse(lastCall("/api/local/settings").init.body), { offline_only: true });
    assert.equal(el("models-offline").checked, true);
    assert.ok(el("models-offline-note").textContent.includes("Наружу ничего не уходит"));
    assert.ok(el("models-banner-text").textContent.includes("ни одна ещё не установлена"));
  } finally {
    routes["/api/local/settings"] = saved;
    goto("#models");
    await settle();
  }
});

await check("a failed install stays on the page with the reason and a retry", async () => {
  const saved = routes["/api/local/status"];
  routes["/api/local/status"] = {
    ...localStatusFixture,
    install: {
      kind: "runtime", id: "", phase: "error", status: "x", done: 0, total: 0, speed_bps: 0, eta_seconds: null,
      active: false, error: "загрузка движка: источники недоступны (github.com: HTTP 504)",
    },
  };
  try {
    goto("#activity");
    await settle();
    goto("#models");
    await settle();
    assert.equal(el("models-install").hidden, false);
    assert.ok(el("models-install").innerHTML.includes("HTTP 504"));
    assert.ok(el("models-install").innerHTML.includes("Повторить"));
    fire(el("models-install"), "click", { target: fakeTarget({ "[data-models-retry]": {} }) });
    await settle();
    assert.deepEqual(JSON.parse(lastCall("/api/local/runtime").init.body), { action: "install" });
  } finally {
    routes["/api/local/status"] = saved;
    goto("#activity");
    await settle();
    goto("#models");
    await settle();
  }
});

await check("the progress panel shows downloaded, left, speed and time", async () => {
  fire(el("models-recommended-apply"), "click");
  await settle();
  const html = el("models-install").innerHTML;
  assert.equal(el("models-install").hidden, false);
  assert.ok(html.includes("Скачано 120 МБ из 480 МБ"), html);
  assert.ok(html.includes("осталось 360 МБ"));
  assert.ok(html.includes("12 МБ/с"));
  assert.ok(html.includes("ещё примерно 30 с"));
  fire(el("models-install"), "click", { target: fakeTarget({ "[data-models-cancel]": {} }) });
  await settle();
  goto("#activity");
  await settle();
  goto("#models");
  await settle();
});

await check("task model selects save all four tasks in one request", async () => {
  el("models-task-distill").value = "brepsi/qwen25-7b";
  el("models-task-handoff").value = "qwen25-7b";
  fire(el("models-tasks-save"), "click");
  await settle();
  const call = lastCall("/api/settings/tasks");
  assert.equal(call.init.method, "POST");
  assert.deepEqual(JSON.parse(call.init.body), {
    models: { distill: "brepsi/qwen25-7b", answer: "", handoff: "qwen25-7b", projects: "" },
  });
  assert.ok(el("toast").textContent.includes("Модели задач сохранены"));
});

await check("the sync schedule is saved after the provider", async () => {
  goto("#settings");
  await settle();
  assert.equal(el("settings").open, true);
  assert.equal(el("set-sync-interval").value, "15");
  assert.ok(el("set-sync-next").textContent.startsWith("Следующая:"));
  el("sync-mode-interval").checked = true;
  el("sync-mode-manual").checked = false;
  el("set-sync-interval").value = "5";
  fire(el("settings-form"), "submit", { preventDefault() {} });
  await settle();
  const schedule = lastCall("/api/sync/schedule");
  assert.equal(schedule.init.method, "POST");
  assert.deepEqual(JSON.parse(schedule.init.body), { mode: "interval", interval_minutes: 5 });
  assert.ok(el("settings").open === false);
  assert.ok(el("toast").textContent.includes("Задача планировщика создана"), "warning goes to the toast");
});

await check("settings choose the local model and its collection interval", async () => {
  goto("#settings");
  await settle();
  assert.ok(el("set-local-model").innerHTML.includes("brepsi/b"));
  assert.equal(el("set-local-model").value, "brepsi/a");
  assert.equal(el("set-local-interval").value, "15", "defaults to the sync interval");

  el("sync-mode-manual").checked = true;
  el("sync-mode-interval").checked = false;
  fire(el("sync-mode-manual"), "change");
  assert.equal(el("set-local-interval").disabled, true);
  el("sync-mode-manual").checked = false;
  el("sync-mode-interval").checked = true;
  fire(el("sync-mode-interval"), "change");
  assert.equal(el("set-local-interval").disabled, false);

  el("set-sync-interval").value = "30";
  el("set-local-interval").value = "20";
  fire(el("settings-form"), "submit", { preventDefault() {} });
  await settle();
  assert.equal(el("settings").open, true, "collection cannot run more often than the scheduler");
  assert.ok(el("set-error").textContent.includes("от 30 до 1440"));

  el("set-local-interval").value = "60";
  el("set-local-model").value = "brepsi/b";
  fire(el("settings-form"), "submit", { preventDefault() {} });
  await settle();
  assert.deepEqual(JSON.parse(lastCall("/api/local/route").init.body), { model: "brepsi/b" });
  assert.deepEqual(JSON.parse(lastCall("/api/sync/schedule").init.body), {
    mode: "interval", interval_minutes: 30, local_interval_minutes: 60,
  });
  assert.equal(el("settings").open, false);
});

await check("a local-only setup saves without a provider", async () => {
  goto("#settings");
  await settle();
  el("set-url").value = "";
  el("set-model").value = "";
  const saves = () => fetchCalls.filter((call) => call.url === "/api/settings" && call.init && call.init.method === "POST").length;
  const before = saves();
  fire(el("settings-form"), "submit", { preventDefault() {} });
  await settle();
  assert.equal(saves(), before);
  assert.equal(el("settings").open, false);
});

await check("processing log shows model hours, sessions, tasks and benchmarks", async () => {
  goto("#activity");
  await settle();
  const html = el("model-stats").innerHTML;
  assert.ok(html.includes("3 ч 12 мин"), "today: 3.2 h");
  assert.ok(html.includes("9 ч 45 мин"), "week: 9.75 h");
  assert.ok(html.includes("Сессии движка"));
  assert.ok(html.includes("brepsi/qwen25-7b"));
  assert.ok(html.includes("карточки"), "the trigger is named in Russian");
  assert.ok(html.includes("≈ 24.5 ток/с, загрузка 320 мс"));
  assert.ok(html.includes("Топ моделей"));
});

await check("saved project proposals survive a restart, settled ones are dropped", async () => {
  goto("#projects");
  await settle();
  const html = el("projects-proposals").innerHTML;
  assert.ok(html.includes("Сохранённое имя"), "the saved proposal is shown without asking the model");
  assert.ok(!html.includes("стало: kimi"), "a name that is already set is not offered again");
  assert.deepEqual(JSON.parse(prefStore.get("projects.proposals")).map((item) => item.key), ["saved/project"]);
});

await check("project proposals render, apply by key and reset a renamed project", async () => {
  goto("#projects");
  await settle();
  assert.ok(el("projects-proposals").innerHTML.includes("Переименованные"));
  fire(el("projects-propose"), "click");
  await settle();
  const html = el("projects-proposals").innerHTML;
  assert.ok(html.includes("Предложения модели"));
  assert.ok(html.includes("projects/brepsi-brain"));
  assert.ok(html.includes("remote origin и свежие записи"));
  fire(el("projects-proposals"), "click", {
    target: fakeTarget({ "[data-proposal-apply]": { dataset: { proposalApply: "0" } } }),
  });
  await settle();
  const apply = lastCall("/api/projects/apply");
  assert.equal(apply.init.method, "POST");
  assert.deepEqual(JSON.parse(apply.init.body), {
    key: "projects/brepsi-brain", name: "Brepsi Brain", path: "D:/Projects/brepsi-brain",
  });
  assert.ok(lastCall("/api/projects"), "the list is reloaded after applying");
  fire(el("projects-proposals"), "click", {
    target: fakeTarget({ "[data-project-reset]": { dataset: { projectReset: "Brepsi Brain" } } }),
  });
  await settle();
  const reset = lastCall("/api/projects/reset");
  assert.equal(reset.init.method, "POST");
  assert.deepEqual(JSON.parse(reset.init.body), { project: "Brepsi Brain" });
});

await check("the install wizard asks for a model and a folder, then starts the install", async () => {
  goto("#models");
  await settle();
  fire(el("models-banner-open"), "click");
  assert.equal(el("models-wizard").open, true);
  assert.ok(el("wizard-body").innerHTML.includes("Qwen2.5 7B Instruct"));
  assert.ok(el("wizard-body").innerHTML.includes("wizard-offline"), "the local-only choice is in the first step");
  fire(el("wizard-next"), "click");
  assert.ok(el("wizard-body").innerHTML.includes("Понадобится около 4 ГБ"));
  fire(el("wizard-next"), "click");
  await settle();
  const call = lastCall("/api/local/install");
  assert.equal(call.init.method, "POST");
  assert.deepEqual(JSON.parse(call.init.body), { id: "qwen25-7b" });
  assert.ok(el("wizard-body").innerHTML.includes("progress-track"));
  assert.ok(el("wizard-body").innerHTML.includes("Отмена"));
  assert.ok(el("wizard-body").innerHTML.includes("осталось 360 МБ"));
  assert.ok(el("wizard-body").innerHTML.includes("12 МБ/с"));
  assert.equal(typeof globalThis.confirm, "undefined");
  fire(el("wizard-close"), "click");
  assert.equal(el("models-wizard").open, true, "an active install needs confirmation to close");
  globalThis.confirm = () => true;
  try {
    fire(el("wizard-close"), "click");
  } finally {
    delete globalThis.confirm;
  }
  assert.equal(el("models-wizard").open, false);
  assert.ok(lastCall("/api/local/status"), "the page reloads the status after closing");
});

await check("handoff shows every readable harness as its own list", async () => {
  goto("#handoff");
  await settle();
  const html = el("handoff-chats").innerHTML;
  for (const name of ["Claude Code", "Codex", "Kimi Code", "Qoder", "OpenCode", "DeepSeek Harness", "GitHub Copilot", "Antigravity"]) {
    assert.ok(html.includes(name), name);
  }
  assert.ok(!html.includes("ChatGPT"), "ChatGPT is not a source");
  assert.equal(el("handoff-from-side").hidden, true);
  assert.ok(html.includes("Claude чат"));
  assert.ok(html.includes("OpenCode чат"));
});

await check("handoff keeps picked chats across a silent refresh", async () => {
  fire(el("handoff-chats"), "change", {
    target: { closest: (sel) => sel === "input[data-chat]" ? { dataset: { chat: "claude:a" }, checked: true } : null },
  });
  assert.equal(el("handoff-run").disabled, false);
  el("handoff-chats").scrollTop = 120;
  fire(el("handoff-hours"), "click", { target: fakeTarget({ ".seg": { dataset: { hours: "24" } } }) });
  await settle();
  const html = el("handoff-chats").innerHTML;
  assert.ok(html.includes('data-chat="claude:a"'));
  assert.ok(html.includes('data-chat="claude:a" checked'));
  assert.equal(el("handoff-chats").scrollTop, 120);
});

await check("handoff keeps other lists when one source fails", async () => {
  const saved = globalThis.fetch;
  globalThis.fetch = async (url, init) => {
    if (String(url).includes("harness=opencode")) throw new Error("база занята");
    return saved(url, init);
  };
  try {
    goto("#handoff");
    await settle();
    const html = el("handoff-chats").innerHTML;
    assert.ok(html.includes("Claude чат"));
    assert.ok(html.includes("сервер недоступен"));
    assert.ok(html.includes("OpenCode"));
  } finally {
    globalThis.fetch = saved;
  }
});

await check("handoff transfers chats picked in different lists", async () => {
  goto("#handoff");
  await settle();
  for (const id of ["claude:a", "opencode:c"]) {
    fire(el("handoff-chats"), "change", {
      target: { closest: (sel) => sel === "input[data-chat]" ? { dataset: { chat: id }, checked: true } : null },
    });
  }
  const before = fetchCalls.length;
  fire(el("handoff-run"), "click");
  await settle();
  const posts = fetchCalls.slice(before).filter((call) => call.url === "/api/handoff" && call.init?.method === "POST");
  assert.equal(posts.length, 2);
  const ids = posts.map((call) => JSON.parse(call.init.body).chat).sort();
  assert.deepEqual(ids, ["claude:a", "opencode:c"]);
  assert.ok(fetchCalls.slice(before).some((call) => call.url === "/api/handoff/launch"));
});

await check("account switch lists only sources with account detection", async () => {
  fire(el("handoff-kind"), "click", { target: fakeTarget({ ".seg": { dataset: { kind: "account" } } }) });
  await settle();
  assert.equal(el("handoff-from-side").hidden, false);
  const html = el("handoff-from").innerHTML;
  assert.ok(html.includes("Claude Code"));
  assert.ok(!html.includes("OpenCode"));
  assert.ok(!html.includes("DeepSeek"));
});

await check("saving task models before the table renders does not clear them", async () => {
  // Заглушка создаёт элемент по любому обращению, поэтому «селектора ещё нет»
  // изображается подменой getElementById — как подменяют fetch в других проверках.
  const realGet = globalThis.document.getElementById;
  globalThis.document.getElementById = (id) => (id.startsWith("models-task-") ? undefined : realGet(id));
  const before = fetchCalls.length;
  try {
    fire(el("models-tasks-save"), "click");
    await settle();
  } finally {
    globalThis.document.getElementById = realGet;
  }
  const saves = fetchCalls.slice(before).filter((call) => call.url === "/api/settings/tasks");
  assert.equal(saves.length, 0, "пустая таблица не должна уходить на сервер");
  assert.ok(el("models-error").textContent.includes("ещё не загрузились"));
  assert.ok(!el("toast").textContent.includes("Модели задач сохранены"));
});

await check("empty base shows a first-step call to action", async () => {
  const stats = routes["/api/stats"];
  const saved = stats.queue.records;
  stats.queue.records = 0;
  goto("#overview");
  await settle();
  const tiles = el("tiles").innerHTML;
  assert.ok(tiles.includes("Память пока пуста"), tiles);
  assert.ok(tiles.includes("data-sync-now"));
  stats.queue.records = saved;
  goto("#overview");
  await settle();
});

await check("sync toast names the model error and points to the log", async () => {
  const saved = routes["/api/sync"];
  routes["/api/sync"] = { new_cards: 0, llm: { configured: true, written: 0, candidates: 1, error: "провайдер <err>", failed: 2 } };
  fire(el("sync-now"), "click");
  await settle();
  const text = el("toast").textContent;
  assert.ok(text.includes("Готово с ошибками модели"), text);
  assert.ok(text.includes("модель: провайдер <err>"), text);
  assert.ok(text.includes("Журнал обработки"), text);
  routes["/api/sync"] = saved;
});

await check("failed queue lists the reason and retries one record", async () => {
  goto("#activity");
  await settle();
  const html = el("queue-failed").innerHTML;
  assert.ok(html.includes("упавшая &lt;запись&gt;"), html);
  assert.ok(html.includes("timeout &lt;err&gt;"), html);
  fire(el("queue-failed"), "click", { target: fakeTarget({ "[data-retry]": { dataset: { retry: "claude:x" } } }) });
  await settle();
  const call = lastCall("/api/queue/retry");
  assert.equal(call.init.method, "POST");
  assert.deepEqual(JSON.parse(call.init.body), { unit: "claude:x" });
});

await check("log viewer reads the selected log as text", async () => {
  goto("#activity");
  await settle();
  assert.ok(el("log-text").textContent.includes("строка журнала <b>"));
  el("log-name").value = "ollama-serve.log";
  fire(el("log-name"), "change");
  await settle();
  assert.ok(lastCall("/api/log").url.includes("name=ollama-serve.log"));
  el("log-name").value = "sync.log";
});

await check("settings expose the fallback model and the real scheduler state", async () => {
  goto("#settings");
  await settle();
  assert.equal(el("set-fallback").value, "fb1");
  assert.ok(el("set-sync-state").textContent.includes("Планировщик: готов"), el("set-sync-state").textContent);
  el("set-fallback").value = "fb2";
  el("set-model").value = "m1";
  fire(el("settings-form"), "submit", { preventDefault() {} });
  await settle();
  assert.equal(JSON.parse(lastCall("/api/settings").init.body).fallback_model, "fb2");
});

await check("an orphaned record row is visible but not clickable", async () => {
  const rec = routes["/api/records"].records[0];
  const savedId = rec.id;
  rec.id = null;
  goto("#overview");
  await settle();
  goto("#records");
  await settle();
  const html = el("records-list").innerHTML;
  assert.ok(html.includes("карточка недоступна"), html);
  assert.ok(html.includes("is-orphan"), html);
  rec.id = savedId;
  goto("#overview");
  await settle();
  goto("#records");
  await settle();
});

await check("the graph View menu closes on Escape and outside click", async () => {
  goto("#graph");
  await settle();
  fire(el("graph-view"), "click");
  assert.equal(el("graph-view-menu").hidden, false);
  key("Escape");
  assert.equal(el("graph-view-menu").hidden, true);
  fire(el("graph-view"), "click");
  assert.equal(el("graph-view-menu").hidden, false);
  for (const fn of docListeners.click || []) fn({ target: fakeTarget({}) });
  assert.equal(el("graph-view-menu").hidden, true);
  key("Escape");
});

await check("double resume click posts once (busy guard)", async () => {
  const savedSwitch = routes["/api/handoff/switch"];
  routes["/api/handoff/switch"] = {
    pending: { items: [{ title: "чат", prompt: "p", path: "d/x.md", mode: "full", delivered: "clipboard" }], created: NOW, account: { id: "a", exact: true, label: "old" } },
    account: { id: "b", exact: true, label: "new" }, changed: true,
  };
  goto("#handoff");
  await settle();
  fire(el("handoff-kind"), "click", { target: fakeTarget({ ".seg": { dataset: { kind: "account" } } }) });
  await settle();
  const saved = globalThis.fetch;
  let resumePosts = 0;
  let finish;
  globalThis.fetch = (url, init) => {
    if (String(url).includes("/api/handoff/switch/resume")) {
      resumePosts += 1;
      return new Promise((res) => { finish = () => res({ ok: true, status: 200, json: async () => ({ items: [{ title: "чат", prompt: "p", path: "d/x.md", mode: "full", delivered: "clipboard" }], prompt: "" }) }); });
    }
    return saved(url, init);
  };
  const clickResume = () => fire(el("handoff-account"), "click", { target: fakeTarget({ "button[data-switch]": { dataset: { switch: "resume" } } }) });
  clickResume();
  clickResume();
  await tick();
  finish();
  await settle();
  globalThis.fetch = saved;
  routes["/api/handoff/switch"] = savedSwitch;
  assert.equal(resumePosts, 1, "busy guard blocks the second resume");
});

console.log(`\n${passed} checks passed${process.exitCode ? " (with failures)" : ""}`);
