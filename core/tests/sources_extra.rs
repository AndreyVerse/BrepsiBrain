use memory_core::config::{ChatGptExportSettings, Config};
use memory_core::db::Database;
use memory_core::sources_extra::ingest_extra;

#[test]
fn chatgpt_export_is_imported_and_watermarked() {
    let dir = tempfile::tempdir().unwrap();
    let export = dir.path().join("conversations.json");
    std::fs::write(
        &export,
        r#"[
          {"id":"c1","title":"Архитектура","create_time":1700000000,"update_time":1700000100,
           "current_node":"n2",
           "mapping":{
             "n0":{"parent":null,"message":null},
             "n1":{"parent":"n0","message":{"author":{"role":"user"},"create_time":1700000001,
                   "content":{"parts":["Спроектируй память"]}}},
             "n2":{"parent":"n1","message":{"author":{"role":"assistant"},"create_time":1700000002,
                   "content":{"parts":["Сделаем SQLite FTS5"]}}}
           }}
        ]"#,
    )
    .unwrap();
    let db = Database::open(dir.path().join("memory.db")).unwrap();
    let mut config = Config::default();
    config.extra_sources.chatgpt_export = ChatGptExportSettings { path: Some(export.clone()) };
    let counts = ingest_extra(&db, &config);
    assert_eq!(counts["chatgpt"], 1);
    let card = db.get_card_by_ref("chatgpt:c1").unwrap().unwrap();
    assert_eq!(card.title, "Архитектура");
    assert!(card.body.contains("SQLite FTS5"));
    let watermark = db
        .get_state(&format!("chatgpt:watermark:{}", export.canonicalize().unwrap().display()))
        .unwrap()
        .unwrap();
    assert_eq!(watermark, "1700000100");
    assert_eq!(ingest_extra(&db, &config)["chatgpt"], 0);
}
