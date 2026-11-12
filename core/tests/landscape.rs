use std::collections::{HashMap, HashSet};

use memory_core::db::{now_ts, CardDraft, Database};
use memory_core::landscape::{build_landscape, nearest_pairs};
use memory_core::similarity;

fn seed(db: &Database, specs: &[(&str, &str, i64, &str, &str)]) {
    for (source, reference, ts, title, body) in specs {
        assert!(db.insert_card(&CardDraft::new(source, reference, *ts, title, body)).unwrap());
    }
}

fn open_temp() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("memory.db")).unwrap();
    (dir, db)
}

fn node_keys(value: &serde_json::Value) -> HashSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

#[test]
fn shape_is_one_node_per_record() {
    let (_dir, db) = open_temp();
    let now = now_ts();
    seed(
        &db,
        &[
            ("git", "git:repo:a1", now - 300, "коммит 1", "фиксация архитектуры памяти"),
            ("git", "git:repo:a2", now - 200, "коммит 2", "фиксация архитектуры памяти"),
            ("git", "git:repo:a3", now - 100, "коммит 3", "фиксация архитектуры памяти"),
            ("kimi", "kimi:s1:a:0", now - 50, "сессия 1", "разговор про архитектуру памяти"),
            ("claude", "claude:s2:0", now - 40, "сессия 2", "банан гитара океан редкие слова"),
            ("obsidian", "obsidian:v:Dir/one.md", now - 30, "заметка 1", "заметка про океан и гитару"),
            ("obsidian", "obsidian:v:Dir/two.md", now - 20, "заметка 2", "заметка про банан"),
        ],
    );
    let payload = build_landscape(&db, None, 0, Some(now)).unwrap();
    let nodes = payload["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 5);
    let expected: HashSet<String> =
        ["id", "title", "source", "ts", "project", "has_card"].iter().map(|key| (*key).to_owned()).collect();
    assert!(nodes.iter().all(|node| node_keys(node) == expected));
    let git = nodes.iter().find(|node| node["source"] == "git").unwrap();
    assert_eq!(git["project"], "repo");
    assert_eq!(git["has_card"], false);
    assert_eq!(git["title"], "коммит 3");
}

#[test]
fn projects_are_counted_and_named() {
    let (_dir, db) = open_temp();
    let now = now_ts();
    seed(
        &db,
        &[
            ("git", "git:repo:a1", now - 20, "коммит", "архитектура памяти"),
            ("kimi", "kimi:s1:a:0", now - 10, "сессия", "разговор"),
            ("obsidian", "obsidian:v:Dir/one.md", now, "заметка 1", "океан"),
            ("obsidian", "obsidian:v:Dir/two.md", now, "заметка 2", "банан"),
        ],
    );
    let payload = build_landscape(&db, None, 0, Some(now)).unwrap();
    let projects = payload["projects"].as_array().unwrap();
    assert_eq!(projects[0]["project"], "Dir");
    assert_eq!(projects[0]["name"], "Dir");
    assert_eq!(projects[0]["records"], 2);
}

#[test]
fn knowledge_card_stands_for_its_record() {
    let (_dir, db) = open_temp();
    let now = now_ts();
    seed(&db, &[("kimi", "kimi:s1:a:0", now, "сессия 1", "разговор про архитектуру")]);
    let mut draft = CardDraft::new(
        "llm:kimi",
        "llm:kimi:s1:a",
        now,
        "Карточка разговора",
        "### Кратко\nпро архитектуру",
    )
    .distilled();
    draft.keywords = "архитектура".into();
    assert!(db.write_unit_card("kimi:s1:a", &draft, 10_i64.pow(9)).unwrap());
    let payload = build_landscape(&db, None, 0, Some(now)).unwrap();
    let node = payload["nodes"].as_array().unwrap().iter().find(|node| node["source"] == "kimi").unwrap();
    assert_eq!(node["title"], "Карточка разговора");
    assert_eq!(node["has_card"], true);
    assert_eq!(node["id"], db.get_card_by_ref("llm:kimi:s1:a").unwrap().unwrap().id);
}

#[test]
fn edges_are_thresholded_rounded_and_deduplicated() {
    let (_dir, db) = open_temp();
    let now = now_ts();
    let shared = "альфа бета гамма дельта эпсилон дзета";
    seed(
        &db,
        &[
            ("kimi", "k:1", now, "тема а", &format!("{shared} один уникальный хвост")),
            ("kimi", "k:2", now - 10, "тема б", &format!("{shared} другой уникальный хвост")),
            ("git", "g:1", now - 20, "тема в", &format!("{shared} третий уникальный хвост")),
            ("git", "g:2", now - 30, "посторонняя", "банан гитара океан совершенно иные слова"),
            ("git", "g:3", now - 40, "ещё одна", "лодка берег маяк другие отдельные слова"),
            ("git", "g:4", now - 50, "и ещё", "поезд станция рельсы особые свои термины"),
        ],
    );
    let payload = build_landscape(&db, None, 0, Some(now)).unwrap();
    let edges = payload["edges"].as_array().unwrap();
    let orphan = payload["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["title"] == "посторонняя")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    assert!(!edges.iter().any(|edge| edge[0] == orphan || edge[1] == orphan));
    let mut pairs = HashSet::new();
    for edge in edges {
        let left = edge[0].as_i64().unwrap();
        let right = edge[1].as_i64().unwrap();
        let weight = edge[2].as_f64().unwrap();
        assert_ne!(left, right);
        assert!(weight >= 0.08);
        assert_eq!(weight, (weight * 1000.0).round() / 1000.0);
        assert!(pairs.insert((left.min(right), left.max(right))));
    }
    assert!(!edges.is_empty());
}

#[test]
fn nearest_pairs_matches_brute_force() {
    let source: Vec<Vec<(&str, f64)>> = vec![
        vec![("a", 1.0), ("b", 2.0)],
        vec![("a", 1.0), ("c", 3.0)],
        vec![("b", 2.0), ("c", 1.0)],
        vec![("d", 5.0)],
    ];
    let vectors: Vec<HashMap<String, f64>> = source
        .iter()
        .map(|entries| entries.iter().map(|(token, weight)| ((*token).to_owned(), *weight)).collect())
        .collect();
    let pairs = nearest_pairs(&vectors, 3, 0.01);
    let expected: HashSet<(usize, usize)> = [(0, 1), (0, 2), (1, 2)].into_iter().collect();
    assert_eq!(pairs.iter().map(|(i, j, _)| (*i, *j)).collect::<HashSet<_>>(), expected);
    for (left, right, weight) in pairs {
        assert!((weight - similarity::cosine(&vectors[left], &vectors[right])).abs() < 1e-12);
    }
}

#[test]
fn limit_keeps_newest_records() {
    let (_dir, db) = open_temp();
    let now = now_ts();
    seed(
        &db,
        &[
            ("git", "git:repo:a1", now - 20, "старый", "архитектура"),
            ("git", "git:repo:a2", now - 10, "новый", "банан"),
        ],
    );
    let payload = build_landscape(&db, Some(1), 0, Some(now)).unwrap();
    assert_eq!(payload["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(payload["nodes"][0]["title"], "новый");
}
