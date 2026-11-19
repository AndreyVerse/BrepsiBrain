//! Граф записей для вида «Карта памяти».
//!
//! Порт `memory_base/landscape.py`: узлы — записи, рёбра — близость по темам
//! (косинус IDF-взвешенных множеств токенов карточки знаний или сырого текста).
//! Кандидаты берутся из обратного индекса, а не перебором всех пар.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use serde_json::{json, Value};

use crate::db::{now_ts, Database, UnitInfo};
use crate::similarity;
use crate::text::split_card_sections;

pub const MIN_EDGE_WEIGHT: f64 = 0.08;
pub const K_NEIGHBOURS: usize = 4;
pub const MAX_TOKEN_SHARE: f64 = 0.25;
pub const MIN_TOKEN_CAP: usize = 10;
pub const RAW_TEXT_CHARS: usize = 3000;

fn record_texts(db: &Database, units: &[UnitInfo]) -> Result<Vec<String>, String> {
    let mut cards: HashMap<String, String> = HashMap::new();
    let mut stmt = db
        .connection()
        .prepare("SELECT unit, body FROM cards WHERE kind='distilled' AND source_ref='llm:'||unit")
        .map_err(|e| e.to_string())?;
    for row in stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?
    {
        let (unit, body) = row.map_err(|e| e.to_string())?;
        cards.insert(unit, body);
    }

    let wanted: HashSet<&str> = units
        .iter()
        .filter(|info| info.card_id == 0)
        .map(|info| info.unit.as_str())
        .collect();
    let mut raw: HashMap<String, Vec<String>> = HashMap::new();
    let mut raw_stmt = db
        .connection()
        .prepare("SELECT unit, body FROM cards WHERE kind='raw' ORDER BY id")
        .map_err(|e| e.to_string())?;
    for row in raw_stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?
    {
        let (unit, body) = row.map_err(|e| e.to_string())?;
        if !wanted.contains(unit.as_str()) {
            continue;
        }
        let parts = raw.entry(unit).or_default();
        if parts.iter().map(String::len).sum::<usize>() < RAW_TEXT_CHARS {
            parts.push(body);
        }
    }

    let mut texts = Vec::with_capacity(units.len());
    for info in units {
        if info.card_id != 0 {
            let body = cards.get(&info.unit).map(String::as_str).unwrap_or("");
            let summary = split_card_sections(body).summary;
            texts.push(format!("{}\n{}\n{}", info.title, info.keywords, summary));
        } else {
            let body = raw.get(&info.unit).cloned().unwrap_or_default().join("\n");
            texts.push(format!("{}\n{}", info.title, body.chars().take(RAW_TEXT_CHARS).collect::<String>()));
        }
    }
    Ok(texts)
}

/// Top-k соседей через обратный индекс; каждая пара один раз как `(i, j, cosine)`, i < j.
pub fn nearest_pairs(vectors: &[HashMap<String, f64>], k: usize, min_weight: f64) -> Vec<(usize, usize, f64)> {
    let mut postings: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, vector) in vectors.iter().enumerate() {
        for token in vector.keys() {
            postings.entry(token.as_str()).or_default().push(index);
        }
    }
    let cap = MIN_TOKEN_CAP.max(((vectors.len() as f64) * MAX_TOKEN_SHARE) as usize);
    let norms: Vec<f64> = vectors
        .iter()
        .map(|vector| vector.values().map(|weight| weight * weight).sum::<f64>().sqrt())
        .collect();
    let mut pairs: BTreeMap<(usize, usize), f64> = BTreeMap::new();

    for (index, vector) in vectors.iter().enumerate() {
        let mut scores: HashMap<usize, f64> = HashMap::new();
        for (token, weight) in vector {
            let docs = match postings.get(token.as_str()) {
                Some(docs) if docs.len() <= cap => docs,
                _ => continue,
            };
            for &other in docs {
                if other == index {
                    continue;
                }
                let other_weight = vectors[other].get(token).copied().unwrap_or(0.0);
                *scores.entry(other).or_default() += weight * other_weight;
            }
        }
        let mut ranked: Vec<(usize, f64)> = scores
            .into_iter()
            .filter_map(|(other, dot)| {
                if norms[index] == 0.0 || norms[other] == 0.0 {
                    return None;
                }
                let score = (dot / (norms[index] * norms[other])).min(1.0);
                (score >= min_weight).then_some((other, score))
            })
            .collect();
        ranked.sort_by(|(left_index, left_score), (right_index, right_score)| {
            right_score
                .partial_cmp(left_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left_index.cmp(right_index))
        });
        for (other, score) in ranked.into_iter().take(k) {
            pairs.insert((index.min(other), index.max(other)), score);
        }
    }

    pairs.into_iter().map(|((left, right), weight)| (left, right, weight)).collect()
}

/// `{nodes, edges, projects, generated_ms}` для новейших записей.
pub fn build_landscape(db: &Database, limit: Option<usize>, days: i64, now: Option<i64>) -> Result<Value, String> {
    let started = Instant::now();
    let mut units: Vec<UnitInfo> = db.unit_index().map_err(|e| e.to_string())?.into_values().collect();
    units.sort_by(|left, right| right.last_ts.cmp(&left.last_ts).then(right.max_id.cmp(&left.max_id)));
    if days > 0 {
        let cutoff = now.unwrap_or_else(now_ts) - days * 86_400;
        units.retain(|info| info.last_ts >= cutoff);
    }
    if let Some(limit) = limit {
        units.truncate(limit);
    }

    let texts = record_texts(db, &units)?;
    let token_sets: Vec<HashSet<String>> = texts.iter().map(|text| similarity::token_set(text)).collect();
    let weights = similarity::idf_weights(&token_sets);
    let vectors: Vec<HashMap<String, f64>> =
        token_sets.iter().map(|tokens| similarity::vectorize(tokens, &weights)).collect();
    let edges: Vec<Value> = nearest_pairs(&vectors, K_NEIGHBOURS, MIN_EDGE_WEIGHT)
        .into_iter()
        .map(|(left, right, weight)| {
            json!([
                units[left].node_id(),
                units[right].node_id(),
                (weight * 1000.0).round() / 1000.0
            ])
        })
        .collect();

    let mut projects: HashMap<String, (String, usize)> = HashMap::new();
    for info in &units {
        let name = info.project.rsplit('/').next().unwrap_or(&info.project).to_owned();
        let entry = projects.entry(info.project.clone()).or_insert((name, 0));
        entry.1 += 1;
    }
    let mut projects: Vec<Value> = projects
        .into_iter()
        .map(|(project, (name, records))| json!({"project": project, "name": name, "records": records}))
        .collect();
    projects.sort_by(|left, right| {
        let left_records = left["records"].as_u64().unwrap_or(0);
        let right_records = right["records"].as_u64().unwrap_or(0);
        right_records
            .cmp(&left_records)
            .then_with(|| left["project"].as_str().unwrap_or("").cmp(right["project"].as_str().unwrap_or("")))
    });

    let nodes: Vec<Value> = units
        .iter()
        .map(|info| {
            json!({
                "id": info.node_id(),
                "title": info.title,
                "source": info.source,
                "ts": info.last_ts,
                "project": info.project,
                "has_card": info.card_id != 0,
            })
        })
        .collect();
    let generated_ms = started.elapsed().as_millis() as u64;
    Ok(json!({
        "nodes": nodes,
        "edges": edges,
        "projects": projects,
        "generated_ms": generated_ms,
    }))
}
