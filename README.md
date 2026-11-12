<div align="center">

<img src="static/brand.svg" width="120" height="120" alt="Brepsi Brain" />

# Brepsi Brain

**One local memory for all your AI coding harnesses: search, knowledge cards, an MCP server, and live chat handoff between harnesses and accounts.**

[![Download Brepsi Brain](https://img.shields.io/badge/Download-BrepsiBrain.exe-2ea44f?style=for-the-badge&logo=windows&logoColor=white)](https://github.com/AndreyVerse/BrepsiBrain/releases/latest/download/BrepsiBrain.exe)

[![Version](https://img.shields.io/badge/version-1.3.1-4f86ff)](https://github.com/AndreyVerse/BrepsiBrain/releases)
[![Tauri](https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white)](https://tauri.app/)
[![Rust](https://img.shields.io/badge/Rust-core%20%2B%20MCP-B7410E?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![SQLite](https://img.shields.io/badge/SQLite-FTS5-003B57?logo=sqlite&logoColor=white)](https://www.sqlite.org/fts5.html)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-4f86ff.svg)](LICENSE)

**Portable · no installer · no account · your data stays on your disk**

English · [Русский](README.ru.md)

</div>

---

## ⭐ Why Brepsi Brain

Your agents already write everything they do to disk: Claude Code, Codex and Kimi Code session files, plus the git history of your repositories. Brepsi Brain turns all of it into a single local knowledge base you can ask in plain language, and moves a live chat to another harness when the work isn't finished.

Qoder, OpenCode, DeepSeek Harness, Copilot and Antigravity sessions are read for chat handoff only; they are not indexed into memory search.

- **Agents remember each other.** No plugins, hooks or proxies inside the harness, nothing to install per project. A decision made in Codex on Monday is found by Claude Code on Friday.
- **Hand a live chat over to another harness or another account.** The handoff tab lists recent chats of every readable harness — Claude Code, Codex, Kimi Code, Qoder, OpenCode, DeepSeek Harness, Copilot and Antigravity — in separate, self-refreshing lists. Pick chats, and Brepsi Brain writes a handoff file (the full chat, or a brief with the goal, what was done and what is left) and opens a new chat with it. Targets: Claude Code, Codex, Kimi Code, Qoder, Antigravity, OpenCode, DeepSeek Harness, ChatGPT, GigaCode and GitHub Copilot, **plus any harness you add in one click.** Several chats sent to the Claude desktop app open as one session that spreads them into sessions of their own. The **account switch** flow saves the chat, waits for you to sign in with another account, notices the change and continues there. It never reads passwords or tokens.
- **An MCP server in the same exe, answering in 9 ms.** `BrepsiBrain.exe mcp` serves `search_memory`, `recent` and `get_card` over stdio to any MCP client.
- **Search understands Russian and English word forms.** BM25 over title, keywords, summary, questions and atoms, weighted by freshness, one result per conversation.
- **Your data stays with you.** SQLite in `data/`, secrets are masked before they are stored. Summaries come from any OpenAI-compatible provider you choose, and records matching your privacy patterns go to a local model only.
- **No paid API? Install a local model.** The built-in installer downloads a portable engine and a model sized for your GPU (8 GB of VRAM and up, lighter models for weaker machines) and wires everything up. Models are assigned per kind of data, and the engine stops when idle instead of holding VRAM all day.

## 🚀 Getting started

**[Download BrepsiBrain.exe](https://github.com/AndreyVerse/BrepsiBrain/releases/latest/download/BrepsiBrain.exe)** — save it and run · [smaller .zip archive](https://github.com/AndreyVerse/BrepsiBrain/releases/latest/download/BrepsiBrain.zip) (the one the in-app updater uses) · [all releases](https://github.com/AndreyVerse/BrepsiBrain/releases)

Brepsi Brain is a portable Windows exe: **no installer, no runtime, no Python**.

1. Save `BrepsiBrain.exe` into a folder of your choice, for example `brepsi-brain\` on any drive — or take the `.zip` (about 2.2 times smaller) and unpack it there.
2. Run `BrepsiBrain.exe`. A window opens, a tray icon appears, and `data\memory.db` is created next to the exe.

The SHA-256 of `BrepsiBrain.exe` is listed in the release notes.

> Starting with 1.2.0 the app updates itself from GitHub releases: on start and after each sync it checks for a new version, verifies the archive's signature, installs it and restarts. The checkbox under **About** turns this off. Version 1.1.0 has to be replaced by hand once.

```bash
BrepsiBrain.exe          # window and tray
BrepsiBrain.exe mcp      # MCP server over stdio for your harnesses
```

Want the data somewhere else (a synced drive, a shared folder)? Point the root explicitly and leave the exe where it is:

```powershell
setx BREPSI_BRAIN_ROOT "D:\Knowledge\BrepsiBrain"
BrepsiBrain.exe
```

The root holds `config.toml`, `data\` and the handoff files. A working directory that contains `data\memory.db` or `config.toml` takes precedence over the exe folder, so a shortcut can keep the data in one place and the program in another.

Registering the MCP server in a harness (`.mcp.json`, `~/.kimi-code/mcp.json` and the like):

```json
"memory-base": { "command": "D:/Apps/brepsi-brain/BrepsiBrain.exe", "args": ["mcp"] }
```

## 🤝 Your own harnesses

The handoff list shows only the harnesses that are actually on this computer: their data folder, an installed program, a program on `PATH` or a running process gives them away. Everything else stays out of the way. Any other one is added in one click: **Chat handoff → To → "+ Custom"**, give it a name and, optionally, a program to launch. The target is stored in `data/harnesses.json` next to the database, shows up in the list right away and is removed the same way. The handoff prompt always lands in the clipboard, so any chat window can accept it.

## 🧠 Local models without a paid API

Open **Models and hardware** and click **Install a local model**. The wizard detects your GPU (NVIDIA via `nvidia-smi`, AMD and Intel via DXGI), recommends the largest model that fits, asks where to keep it, and downloads:

1. the portable Ollama engine (`v0.40.3`, SHA-256 verified) into `<models>/runtime/ollama`;
2. one model into `<models>/ollama` — from the Ollama registry or as a GGUF file from Hugging Face, verified either way.

The page shows what has been downloaded, what is left, the speed and the time remaining. Interrupted downloads resume, failing sources are retried with a pause, the engine has a second source, and an error stays on the page with its cause and a retry button.

AMD Polaris and Vega cards (Radeon RX 400/500, Vega) run on the Vulkan backend. Every install creates a derived model `brepsi/<id>` with a fixed context window. The engine starts on demand and by default stops after 120 seconds of idle time; `[local_ai] mode = "always"` keeps it running.

`[llm.tasks]` assigns models to four kinds of data: memory cards, answers, handoff briefs and project names. **Local model only** sends everything to the local model and nothing leaves the computer; it can be turned on even before a model is installed. The processing log shows how long the model ran in each session, which models were used and how many tokens each task took.

| VRAM | Models |
|---|---|
| CPU or under 7 GB | Gemma 3 1B, Qwen2.5 1.5B/3B, Gemma 3 4B, Phi-4 Mini |
| 7–10.9 GB (RX 580 8 GB) | Qwen2.5 7B (recommended), Llama 3.1 8B, Hugging Face variants |
| 11–14.9 GB | Mistral Nemo 12B, Gemma 3 12B, Qwen2.5 14B |
| 15 GB and up | Gemma 3 27B, Qwen2.5 32B |

## ✨ Features

- **Ask your memory.** Answers with numbered `[n]` links to your records; follow-up questions continue the thread.
- **All records.** Full-text search with source and project filters, one card per conversation: summary, atoms (decisions, errors, commands, facts), keywords and the original text.
- **Memory map.** A graph of records grouped by project and linked by shared topics.
- **Projects and timeline.** Where the work happened and how memory grows week by week.
- **Processing log.** The summary queue and every model call with tokens and cost.
- **Model statistics.** Local model hours today, this week and in total, sessions, models used and a tokens-per-second benchmark.
- **Sync schedule.** Every N minutes via Windows Task Scheduler, or manual only. Indexing runs on every sync; the local model has its own, longer collection interval.
- **Up-to-date project names.** Collection renames projects with the local model (never with a paid provider, and never over a name you set or reset), and a button asks the model for suggestions you apply in one click.
- **Mail and notes.** Optional Gmail / Yandex Mail (IMAP, read-only, messages stay unread), GitHub commits, Notion pages, ChatGPT exports and Obsidian vaults.
- **Tray.** Sync status, fresh cards, sources, language, autostart; double-click opens the window.
- **The window** remembers its size, position and maximized state, closes to the tray, and runs as a single instance per user.
- **Russian and English:** the language follows Windows and can be switched in the tray or under "About".

## 🛠 Building from source

```bash
cargo test --manifest-path core/Cargo.toml          # Rust core: storage, search, MCP, handoff
node static/smoke.node.mjs                          # dashboard smoke test
cd app && pnpm install && pnpm tauri build          # release: app/src-tauri/target/release/BrepsiBrain.exe
```

The release profile uses LTO, `opt-level = "s"`, `strip`, a single codegen unit and `panic = "abort"`.

## 📄 License and authorship

Apache License 2.0, see [LICENSE](LICENSE). Copyright 2026 Andrey Shkenev ([AndreyVerse](https://github.com/AndreyVerse)).

Under §4(d) of the license, any redistribution of this work or a derivative of it **must keep the [NOTICE](NOTICE) file** with the author's name and links to the author's profile and repository.
