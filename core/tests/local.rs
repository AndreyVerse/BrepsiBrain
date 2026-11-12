//! Локальные сессии, бенчи и переопределения проектов в базе.

use memory_core::db::{now_ts, BenchResult, CardDraft, Database, UsageRun};

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("memory.db")).unwrap();
    (dir, db)
}

#[test]
fn session_lifecycle_counts_hours_and_models() {
    let (_dir, db) = db();
    let now = now_ts();
    let id = db.start_local_session("boot-1", "brepsi/qwen25-7b", "distill", 4242, now - 3600).unwrap();
    db.touch_local_session(id, "brepsi/qwen25-14b", now - 1800).unwrap();
    db.touch_local_session(id, "brepsi/qwen25-14b", now - 1700).unwrap();
    db.end_local_session(id, now).unwrap();

    let stats = db.model_stats(0).unwrap();
    assert_eq!(stats.sessions.len(), 1);
    assert_eq!(stats.sessions[0].seconds(), 3600);
    // Модели без дублей.
    assert_eq!(stats.sessions[0].models, "brepsi/qwen25-7b,brepsi/qwen25-14b");
    assert_eq!(stats.seconds_total, 3600);
    assert!(stats.seconds_week >= 3600);
}

#[test]
fn stale_sessions_close_on_next_boot() {
    let (_dir, db) = db();
    let old = db.start_local_session("boot-old", "m", "sync", 1, 1_000).unwrap();
    db.touch_local_session(old, "m", 2_000).unwrap();
    let fresh = db.start_local_session("boot-new", "m", "sync", 2, 3_000).unwrap();
    assert_eq!(db.sweep_local_sessions("boot-new").unwrap(), 1);
    let stats = db.model_stats(0).unwrap();
    let old_row = stats.sessions.iter().find(|session| session.id == old).unwrap();
    assert_eq!(old_row.ended_ts, Some(2_000));
    let fresh_row = stats.sessions.iter().find(|session| session.id == fresh).unwrap();
    assert_eq!(fresh_row.ended_ts, None);
}

#[test]
fn bench_upserts_one_row_per_model() {
    let (_dir, db) = db();
    for tps in [10.0, 12.5] {
        db.record_bench(&BenchResult {
            model: "brepsi/qwen25-7b".into(),
            ts: 100,
            tokens: 64,
            load_ms: 1200,
            seconds: 5.0,
            tps,
        })
        .unwrap();
    }
    let stats = db.model_stats(0).unwrap();
    assert_eq!(stats.bench.len(), 1);
    assert_eq!(stats.bench[0].tps, 12.5);
}

#[test]
fn usage_tasks_are_grouped() {
    let (_dir, db) = db();
    for task in ["distill", "distill", "answer"] {
        db.record_usage(&UsageRun {
            ts: 10,
            provider: "127.0.0.1".into(),
            model: "brepsi/qwen25-7b".into(),
            cards: 1,
            prompt_tokens: 10,
            completion_tokens: 5,
            cost_rub: 0.0,
            task: task.into(),
        })
        .unwrap();
    }
    let stats = db.model_stats(0).unwrap();
    let distill = stats.tasks.iter().find(|row| row.task == "distill").unwrap();
    assert_eq!((distill.runs, distill.prompt_tokens), (2, 20));
    assert_eq!(stats.runs, 3);
}

#[test]
fn project_override_changes_display_name_and_path() {
    let (_dir, db) = db();
    db.insert_card(&CardDraft::new("git", "git:memory-base:abc123", now_ts(), "коммит", "тело")).unwrap();
    let index = db.unit_index().unwrap();
    assert!(index.values().any(|info| info.project == "memory-base"), "{index:?}");
    db.set_project_meta("memory-base", "Brepsi Brain", "D:/Projects/brepsi-brain").unwrap();
    let index = db.unit_index().unwrap();
    let info = index.values().next().unwrap();
    assert_eq!(info.project, "Brepsi Brain");
    assert_eq!(info.project_path, "D:/Projects/brepsi-brain");
    assert!(db.delete_project_meta("Brepsi Brain").unwrap());
    let index = db.unit_index().unwrap();
    assert_eq!(index.values().next().unwrap().project, "memory-base");
}
