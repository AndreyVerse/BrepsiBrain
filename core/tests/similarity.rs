use std::collections::{HashMap, HashSet};

use memory_core::similarity;

fn set(tokens: &[&str]) -> HashSet<String> {
    tokens.iter().map(|token| (*token).to_owned()).collect()
}

fn vector(entries: &[(&str, f64)]) -> HashMap<String, f64> {
    entries
        .iter()
        .map(|(token, weight)| ((*token).to_owned(), *weight))
        .collect()
}

fn close(left: f64, right: f64) {
    assert!((left - right).abs() < 1e-12, "{left} != {right}");
}

#[test]
fn tokenize_lowercases_latin() {
    assert_eq!(similarity::tokenize("SQLite FTS5 Search"), ["sqlite", "fts5", "search"]);
}

#[test]
fn tokenize_handles_cyrillic() {
    let tokens = similarity::tokenize("Поиск по Памяти через SQLite");
    for expected in ["поиск", "памяти", "sqlite"] {
        assert!(tokens.iter().any(|token| token == expected), "{expected} missing in {tokens:?}");
    }
}

#[test]
fn tokenize_drops_short_tokens() {
    assert!(similarity::tokenize("a я it xy").is_empty());
}

#[test]
fn tokenize_drops_stopwords() {
    let tokens = similarity::tokenize("это какой-то тест для the and проверки");
    for missing in ["это", "the"] {
        assert!(!tokens.iter().any(|token| token == missing));
    }
    for expected in ["какой", "тест", "проверки"] {
        assert!(tokens.iter().any(|token| token == expected));
    }
}

#[test]
fn tokenize_mixed_text() {
    let tokens: HashSet<String> = similarity::tokenize("BM25 ranking и freshness-полураспад").into_iter().collect();
    for expected in ["bm25", "ranking", "freshness", "полураспад"] {
        assert!(tokens.contains(expected));
    }
}

#[test]
fn token_set_deduplicates() {
    assert_eq!(similarity::token_set("sqlite sqlite sqlite fts5"), set(&["sqlite", "fts5"]));
}

#[test]
fn idf_rarer_tokens_weigh_more() {
    let sets = [set(&["alpha", "beta"]), set(&["alpha", "gamma"]), set(&["alpha", "delta"])];
    let weights = similarity::idf_weights(&sets);
    close(weights["alpha"], (1.0 + 3.0 / 3.0f64).ln());
    close(weights["beta"], (1.0 + 3.0f64).ln());
    assert!(weights["beta"] > weights["alpha"]);
}

#[test]
fn cosine_identical_is_one() {
    let vec = vector(&[("sqlite", 0.7), ("fts5", 1.1)]);
    close(similarity::cosine(&vec, &vec), 1.0);
}

#[test]
fn cosine_disjoint_is_zero() {
    close(similarity::cosine(&vector(&[("alpha", 1.0)]), &vector(&[("beta", 1.0)])), 0.0);
}

#[test]
fn cosine_empty_is_zero() {
    assert_eq!(similarity::cosine(&HashMap::new(), &vector(&[("beta", 1.0)])), 0.0);
    assert_eq!(similarity::cosine(&HashMap::new(), &HashMap::new()), 0.0);
}

#[test]
fn cosine_known_value() {
    let a = vector(&[("x", 1.0)]);
    let b = vector(&[("x", 1.0), ("y", 1.0)]);
    close(similarity::cosine(&a, &b), 1.0 / 2.0f64.sqrt());
    close(similarity::cosine(&b, &a), similarity::cosine(&a, &b));
}

#[test]
fn cosine_zero_norm_vector_is_zero() {
    assert_eq!(similarity::cosine(&vector(&[("x", 0.0)]), &vector(&[("x", 0.0)])), 0.0);
}

#[test]
fn vectorize_drops_unknown_tokens() {
    let vec = similarity::vectorize(&set(&["sqlite", "mystery"]), &vector(&[("sqlite", 1.0)]));
    assert_eq!(vec, vector(&[("sqlite", 1.0)]));
}

fn vectors() -> Vec<HashMap<String, f64>> {
    vec![
        vector(&[("alpha", 1.0), ("beta", 1.0)]),
        vector(&[("alpha", 1.0), ("beta", 0.9)]),
        vector(&[("gamma", 1.0)]),
        vector(&[("beta", 1.0), ("gamma", 0.1)]),
    ]
}

#[test]
fn nearest_orders_by_similarity() {
    let nearest = similarity::nearest(&vectors(), 0, 3, 0.0);
    assert_eq!(nearest.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [1, 3]);
    assert!(nearest[0].1 > nearest[1].1 && nearest[1].1 > 0.0);
}

#[test]
fn nearest_respects_k() {
    let nearest = similarity::nearest(&vectors(), 0, 1, 0.0);
    assert_eq!(nearest.len(), 1);
    assert_eq!(nearest[0].0, 1);
}

#[test]
fn nearest_threshold_drops_weak_matches() {
    let nearest = similarity::nearest(&vectors(), 0, 3, 0.9);
    assert_eq!(nearest.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [1]);
}

#[test]
fn nearest_is_deterministic_on_ties() {
    let vectors = vec![
        vector(&[("x", 1.0), ("y", 1.0)]),
        vector(&[("x", 1.0)]),
        vector(&[("y", 1.0)]),
        vector(&[("z", 1.0)]),
    ];
    let first = similarity::nearest(&vectors, 0, 2, 0.0);
    let second = similarity::nearest(&vectors, 0, 2, 0.0);
    assert_eq!(first, second);
    assert_eq!(first.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [1, 2]);
}
