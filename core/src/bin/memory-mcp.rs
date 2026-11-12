//! Отдельный MCP-сервер для сверки с Python до встраивания в BrepsiBrain.exe.
//! База — BREPSI_BRAIN_DB или data/memory.db; BREPSI_BRAIN_NOW (unix-время) закрепляет
//! «сейчас» для побайтной сверки рангов; ошибки пишутся в stderr.

use std::io::{stdin, stdout, BufReader};

fn main() {
    let path = memory_core::db::default_db_path();
    let db = match memory_core::db::Database::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("memory-base: cannot open {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let now = std::env::var("BREPSI_BRAIN_NOW").ok().and_then(|v| v.parse().ok());
    if let Err(e) = memory_core::mcp::serve(&db, now, BufReader::new(stdin().lock()), stdout().lock()) {
        eprintln!("memory-base: stdio: {e}");
        std::process::exit(1);
    }
}
