//! Индексация в указанную базу из указанных источников — для сверки с Python
//! (tests/test_rust_ingest_parity.py и прогон на живых источниках).
//!
//!     ingest_cli DB [--kimi P] [--claude P] [--codex P] [--git P] [--idle SEC] [--vault P]... [--exclude DIR]...
//!     ingest_cli DB --defaults        источники и хранилища как у `memory_base ingest`

use std::path::PathBuf;

use memory_core::config::{load_config_or_default, DEFAULT_OBSIDIAN_EXCLUDE_DIRS};
use memory_core::db::Database;
use memory_core::ingest::{ingest_all, ingest_claude, ingest_codex, ingest_git, ingest_kimi, sources_for};
use memory_core::obsidian::ingest_obsidian;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let db = Database::open(&args[0]).expect("open database");
    if args.get(1).map(String::as_str) == Some("--defaults") {
        let config = load_config_or_default();
        let counts = ingest_all(&db, &sources_for(&config), &config).expect("ingest");
        println!("{counts:?}");
        return;
    }
    let mut idle = 0.0;
    let mut vaults: Vec<PathBuf> = Vec::new();
    let mut exclude: Vec<String> = Vec::new();
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    let mut it = args[1..].iter();
    while let Some(flag) = it.next() {
        let value = it.next().expect("flag value");
        match flag.as_str() {
            "--idle" => idle = value.parse().expect("seconds"),
            "--vault" => vaults.push(PathBuf::from(value)),
            "--exclude" => exclude.push(value.clone()),
            other => roots.push((other.trim_start_matches("--").to_string(), PathBuf::from(value))),
        }
    }
    if exclude.is_empty() {
        exclude = DEFAULT_OBSIDIAN_EXCLUDE_DIRS.map(String::from).to_vec();
    }
    for (kind, root) in &roots {
        let n = match kind.as_str() {
            "kimi" => ingest_kimi(&db, root, idle),
            "claude" => ingest_claude(&db, root, idle),
            "codex" => ingest_codex(&db, root, idle),
            "git" => ingest_git(&db, root, None),
            other => panic!("unknown source {other}"),
        }
        .expect("ingest");
        println!("{kind}: {n}");
    }
    for vault in &vaults {
        println!("obsidian: {}", ingest_obsidian(&db, vault, &exclude).expect("ingest"));
    }
}
