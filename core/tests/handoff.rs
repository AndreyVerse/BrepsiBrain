use std::fs;

use memory_core::handoff;

#[test]
fn builtin_targets_cover_owner_harnesses_without_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let ids: Vec<String> = handoff::all(dir.path()).into_iter().map(|harness| harness.id).collect();
    for expected in ["claude", "codex", "kimi", "qoder", "antigravity", "opencode", "deepseek", "chatgpt", "gigacode", "copilot"] {
        assert!(ids.iter().any(|id| id == expected), "нет встроенной цели {expected}");
    }
    assert!(!ids.iter().any(|id| id == "cursor"), "Cursor не должен быть целью переноса");
}

#[test]
fn custom_harness_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("data")).unwrap();
    let harness = handoff::add_custom(dir.path(), "Windsurf", None).expect("добавление");
    assert_eq!(harness.id, "windsurf");
    assert!(harness.custom);
    assert!(handoff::find(dir.path(), "windsurf").is_some());

    let json = handoff::harnesses_json(dir.path(), None);
    let listed = json["harnesses"].as_array().unwrap();
    assert!(listed.iter().any(|item| item["id"] == "windsurf" && item["can_target"] == true && item["can_source"] == false));

    assert!(handoff::remove_custom(dir.path(), "windsurf").unwrap());
    assert!(handoff::find(dir.path(), "windsurf").is_none());
    assert!(!handoff::remove_custom(dir.path(), "windsurf").unwrap());
}

#[test]
fn only_harnesses_present_on_the_machine_are_listed() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("data")).unwrap();
    handoff::add_custom(root.path(), "Windsurf", None).unwrap();
    let home = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".codex")).unwrap();
    fs::create_dir_all(local.path().join("Programs").join("Kimi Code")).unwrap();
    fs::create_dir_all(local.path().join("Packages").join("OpenAI.ChatGPT-Desktop_abc")).unwrap();
    let running = Some(std::collections::HashSet::from(["qoder.exe".to_owned()]));
    let json = handoff::harnesses_json_in(root.path(), home.path(), local.path(), running);
    let ids: Vec<&str> = json["harnesses"].as_array().unwrap().iter().filter_map(|item| item["id"].as_str()).collect();
    // Папка данных, папка программы, пакет Store, запущенный процесс, своя цель.
    for id in ["codex", "kimi", "chatgpt", "qoder", "windsurf"] {
        assert!(ids.contains(&id), "{id} должен быть в списке: {ids:?}");
    }
    // Следов нет ни в профиле, ни в программах, ни среди процессов.
    for id in ["copilot", "deepseek", "antigravity"] {
        assert!(!ids.contains(&id), "{id} не должен быть в списке: {ids:?}");
    }
}

#[test]
fn custom_harness_rejects_missing_program() {
    let dir = tempfile::tempdir().unwrap();
    let error = handoff::add_custom(dir.path(), "Ghost", Some("C:/nowhere/ghost.exe")).unwrap_err();
    assert!(error.contains("не найден"), "неожиданная ошибка: {error}");
}

#[test]
fn broken_custom_file_does_not_break_list() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("data")).unwrap();
    fs::write(dir.path().join("data").join("harnesses.json"), b"{not json").unwrap();
    assert!(handoff::find(dir.path(), "claude").is_some());
    assert_eq!(handoff::all(dir.path()).len(), 10);
}

#[test]
fn account_switch_is_only_for_old_sources() {
    let dir = tempfile::tempdir().unwrap();
    let switches: Vec<String> = handoff::all(dir.path())
        .into_iter()
        .filter(|harness| harness.can_switch)
        .map(|harness| harness.id)
        .collect();
    assert_eq!(switches, ["claude", "codex", "kimi", "qoder"]);
}

fn opencode_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(".local").join("share").join("opencode").join("opencode.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session_v2 (
            id TEXT PRIMARY KEY, parent_id TEXT, time_archived INTEGER,
            title TEXT, directory TEXT, time_updated INTEGER NOT NULL
        );
        CREATE TABLE session_message (
            session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL,
            time_created INTEGER NOT NULL, data TEXT NOT NULL
        );",
    )
    .unwrap();
    let now = handoff::now_seconds();
    conn.execute(
        "INSERT INTO session_v2 (id, parent_id, time_archived, title, directory, time_updated)
         VALUES (?1, NULL, NULL, ?2, ?3, ?4)",
        rusqlite::params!["ses_top", "OpenCode чат", "D:/work", (now - 60) * 1000],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, parent_id, time_archived, title, directory, time_updated)
         VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
        rusqlite::params!["ses_child", "ses_top", "Дочерняя", "D:/work", (now - 50) * 1000],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, parent_id, time_archived, title, directory, time_updated)
         VALUES (?1, NULL, 1, ?2, ?3, ?4)",
        rusqlite::params!["ses_arch", "Архивная", "D:/work", (now - 40) * 1000],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session_v2 (id, parent_id, time_archived, title, directory, time_updated)
         VALUES ('ses_bad', NULL, NULL, 'Битая', 'D:/work', 'bad')",
        [],
    )
    .unwrap();
    let user = format!(r#"{{"text":"Привет","time":{{"created":{}}}}}"#, (now - 60) * 1000);
    let assistant = format!(
        r#"{{"content":[{{"type":"reasoning","text":"думаю"}},{{"type":"text","text":"Ответ"}},{{"type":"tool","name":"read"}}],"time":{{"created":{}}}}}"#,
        (now - 59) * 1000
    );
    for (session, kind, seq, data) in [
        ("ses_top", "user", 1, user.as_str()),
        ("ses_top", "assistant", 2, assistant.as_str()),
        ("ses_child", "user", 1, user.as_str()),
        ("ses_arch", "user", 1, user.as_str()),
        ("ses_bad", "user", 1, user.as_str()),
    ] {
        conn.execute(
            "INSERT INTO session_message (session_id, type, seq, time_created, data)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![session, kind, seq, (now - 60) * 1000, data],
        )
        .unwrap();
    }
    (dir, db)
}

#[test]
fn opencode_lists_top_level_and_opens_text() {
    let (dir, _db) = opencode_fixture();
    let chats = handoff::active_chats_in(dir.path(), dir.path(), "opencode", 72, 20).unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["title"], "OpenCode чат");
    assert_eq!(chats[0]["turns"], 1);
    assert!(chats[0]["chars"].as_u64().unwrap() > 0);
    assert_eq!(chats[0]["fits_full"], true);

    let chat = handoff::open_chat_in(dir.path(), dir.path(), "opencode:ses_top").unwrap();
    assert_eq!(chat.harness, "opencode");
    assert_eq!(chat.locator, "ses_top");
    assert_eq!(chat.events.len(), 2);
    assert_eq!(chat.events[0].0, "user");
    assert_eq!(chat.events[1].1, "Ответ");
    assert!(handoff::open_chat_in(dir.path(), dir.path(), "opencode:ses_child").is_ok());
    assert!(handoff::open_chat_in(dir.path(), dir.path(), "opencode:nope").is_err());
}

fn write_dsh_session(root: &std::path::Path, name: &str, lines: &[String]) {
    let dir = root.join("--Z-Git--").join(name);
    fs::create_dir_all(&dir).unwrap();
    let text = lines.join("\n") + "\n";
    let packed = zstd::stream::encode_all(std::io::Cursor::new(text.as_bytes()), 3).unwrap();
    fs::write(dir.join("session.v4.jsonl.zstd"), packed).unwrap();
}

fn dsh_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".dsh").join("sessions");
    let now = handoff::now_seconds();
    let header = r#"{"type":"session","version":4,"id":"session-1","cwd":"D:\\work","origin":"standard"}"#.to_owned();
    let user = format!(
        r#"{{"type":"user/message","seq":1,"time":{},"data":{{"source":{{"kind":"user"}},"content":[{{"type":"text","text":"Сделай X"}}]}}}}"#,
        (now - 30) * 1000
    );
    let titled = format!(
        r#"{{"type":"session/title","seq":2,"time":{},"data":{{"title":"Сгенерированный заголовок"}}}}"#,
        (now - 29) * 1000
    );
    let service = format!(
        r#"{{"type":"user/message","seq":3,"time":{},"data":{{"source":{{"kind":"runtime-context"}},"content":[{{"type":"text","text":"служебное"}}]}}}}"#,
        (now - 29) * 1000
    );
    let assistant = format!(
        r#"{{"type":"assistant/message","seq":4,"time":{},"data":{{"message":{{"content":[{{"type":"reasoning","text":"думаю"}},{{"type":"text","text":"Готово"}},{{"type":"tool-call","name":"read"}}]}}}}}}"#,
        (now - 28) * 1000
    );
    write_dsh_session(&root, "session-1", &[header, user, titled, service, assistant]);

    let subagent = r#"{"type":"session","version":4,"id":"session-2","cwd":"D:\\work","origin":"subagent"}"#.to_owned();
    write_dsh_session(&root, "session-2", &[subagent]);
    (dir, root)
}

#[test]
fn deepseek_lists_real_sessions_and_opens_text() {
    let (dir, _root) = dsh_fixture();
    let chats = handoff::active_chats_in(dir.path(), dir.path(), "deepseek", 72, 20).unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["title"], "Сгенерированный заголовок");
    assert_eq!(chats[0]["turns"], serde_json::Value::Null);
    assert_eq!(chats[0]["chars"], serde_json::Value::Null);
    assert_eq!(chats[0]["fits_full"], serde_json::Value::Null);

    let id = chats[0]["id"].as_str().unwrap().to_owned();
    let chat = handoff::open_chat_in(dir.path(), dir.path(), &id).unwrap();
    assert_eq!(chat.cwd, "D:\\work");
    assert_eq!(chat.events.len(), 2);
    assert_eq!(chat.events[0].1, "Сделай X");
    assert_eq!(chat.events[1].1, "Готово");
}

fn copilot_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(".copilot").join("session-store.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT, branch TEXT,
            summary TEXT, created_at TEXT, updated_at TEXT
        );
        CREATE TABLE turns (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
            turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT,
            timestamp TEXT
        );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, cwd, summary, updated_at)
         VALUES ('s1', 'D:/work', 'Copilot чат', datetime('now', '-1 hour'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (id, cwd, summary, updated_at)
         VALUES ('s2', 'D:/work', 'Без ходов', datetime('now', '-2 hour'))",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO turns (session_id, turn_index, user_message, assistant_response, timestamp)
         VALUES ('s1', 1, 'Сделай Y', 'Сделано', datetime('now', '-59 minutes'))",
        [],
    )
    .unwrap();
    (dir, db)
}

#[test]
fn copilot_lists_sessions_with_turns_and_opens_text() {
    let (dir, _db) = copilot_fixture();
    let chats = handoff::active_chats_in(dir.path(), dir.path(), "copilot", 72, 20).unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["title"], "Copilot чат");
    assert_eq!(chats[0]["turns"], 1);

    let chat = handoff::open_chat_in(dir.path(), dir.path(), "copilot:s1").unwrap();
    assert_eq!(chat.harness, "copilot");
    assert_eq!(chat.events.len(), 2);
    assert_eq!(chat.events[0].1, "Сделай Y");
    assert_eq!(chat.events[1].1, "Сделано");
    assert!(handoff::open_chat_in(dir.path(), dir.path(), "copilot:s2").is_err());
}

fn antigravity_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".gemini").join("antigravity");
    fs::create_dir_all(&root).unwrap();
    let conn = rusqlite::Connection::open(root.join("conversation_summaries.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE conversation_summaries (
            conversation_id TEXT PRIMARY KEY, title TEXT, last_modified_time TEXT,
            workspace_uris TEXT, parent_conversation_id TEXT, nesting_depth INTEGER
        );",
    )
    .unwrap();
    for (id, title, parent, depth) in [
        ("conv-1", "Antigravity чат", "", 0),
        ("conv-child", "Вложенная", "conv-1", 1),
    ] {
        conn.execute(
            "INSERT INTO conversation_summaries
             (conversation_id, title, last_modified_time, workspace_uris, parent_conversation_id, nesting_depth)
             VALUES (?1, ?2, datetime('now', '-1 hour'), ?3, ?4, ?5)",
            rusqlite::params![id, title, r#"["file:///d%3A/work"]"#, parent, depth],
        )
        .unwrap();
        let logs = root.join("brain").join(id).join(".system_generated").join("logs");
        fs::create_dir_all(&logs).unwrap();
        let lines = [
            r#"{"type":"USER_INPUT","content":"<USER_REQUEST>\nСделай Z\n</USER_REQUEST>\n<ADDITIONAL_METADATA>время</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>модель</USER_SETTINGS_CHANGE>"}"#,
            r#"{"type":"PLANNER_RESPONSE","content":"Готово","thinking":"думаю"}"#,
            r#"{"type":"GENERIC","content":"вывод инструмента"}"#,
        ];
        fs::write(logs.join("transcript.jsonl"), lines.join("\n") + "\n").unwrap();
    }
    (dir, root)
}

#[test]
fn antigravity_strips_wrappers_and_skips_nested() {
    let (dir, _root) = antigravity_fixture();
    let chats = handoff::active_chats_in(dir.path(), dir.path(), "antigravity", 72, 20).unwrap();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["title"], "Antigravity чат");
    assert_eq!(chats[0]["cwd"], "d:/work");

    let chat = handoff::open_chat_in(dir.path(), dir.path(), "antigravity:conv-1").unwrap();
    assert_eq!(chat.events.len(), 2);
    assert_eq!(chat.events[0].1, "Сделай Z");
    assert_eq!(chat.events[1].1, "Готово");
    assert!(handoff::open_chat_in(dir.path(), dir.path(), "antigravity:conv-child").is_ok());
    assert!(handoff::open_chat_in(dir.path(), dir.path(), "antigravity:../conv-1").is_err());
    assert!(handoff::open_chat_in(dir.path(), dir.path(), "antigravity:C:..").is_err());
    assert!(handoff::open_chat_in(dir.path(), dir.path(), "antigravity:C:").is_err());
}

#[test]
fn brief_marks_unknown_counts_as_null() {
    let chat = handoff::Chat {
        id: "x:1".to_owned(),
        harness: "x".to_owned(),
        harness_name: "X".to_owned(),
        locator: "1".to_owned(),
        title: "Заголовок\nвторая строка".to_owned(),
        cwd: "D:/work".to_owned(),
        mtime: 1,
        events: vec![],
    };
    let brief = chat.brief_with(None, None);
    assert_eq!(brief["turns"], serde_json::Value::Null);
    assert_eq!(brief["chars"], serde_json::Value::Null);
    assert_eq!(brief["fits_full"], serde_json::Value::Null);
    assert_eq!(brief["title"], "Заголовок");
}
