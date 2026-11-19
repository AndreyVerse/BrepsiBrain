//! Сходство множеств токенов с IDF-весами.
//!
//! Порт `memory_base/similarity.py`: панель ранжирует соседей карточки и строит
//! граф сходства. Результат детерминирован — одинаковый вход даёт одинаковые float.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;

const MIN_TOKEN_LEN: usize = 3;

/// Английские и русские служебные слова — тот же встроенный список, что в Python.
const STOPWORDS: &[&str] = &[
    // en
    "a", "all", "an", "and", "are", "as", "at", "be", "but", "by", "can",
    "do", "for", "from", "had", "has", "have", "if", "in", "is", "it",
    "its", "me", "my", "not", "of", "on", "or", "our", "so", "that",
    "the", "their", "them", "then", "they", "this", "to", "was", "we",
    "were", "what", "when", "who", "will", "with", "you", "your",
    // ru
    "и", "в", "во", "на", "с", "со", "по", "за", "из", "от", "не", "но",
    "а", "же", "ли", "бы", "что", "это", "как", "так", "все", "всё",
    "его", "её", "их", "мой", "моя", "моё", "мои", "твой", "наш",
    "ваш", "он", "она", "оно", "они", "мы", "вы", "я", "ещё", "уже",
    "только", "где", "когда", "если", "чтобы", "потому", "поэтому",
    "который", "которая", "которые", "через", "здесь", "сам",
];

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)[a-zа-яё0-9]+").expect("static token regex"))
}

fn stopwords() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| STOPWORDS.iter().copied().collect())
}

/// Токены в нижнем регистре (латиница и кириллица) без стоп-слов.
pub fn tokenize(text: &str) -> Vec<String> {
    let stop = stopwords();
    let lower = text.to_lowercase();
    token_re()
        .find_iter(&lower)
        .map(|m| m.as_str())
        .filter(|raw| raw.chars().count() >= MIN_TOKEN_LEN && !stop.contains(*raw))
        .map(str::to_owned)
        .collect()
}

/// [`tokenize`] без повторов — мешок слов карточки.
pub fn token_set(text: &str) -> HashSet<String> {
    tokenize(text).into_iter().collect()
}

/// `idf = ln(1 + N/df)` для каждого токена корпуса.
pub fn idf_weights(token_sets: &[HashSet<String>]) -> HashMap<String, f64> {
    let mut df: HashMap<&str, usize> = HashMap::new();
    for tokens in token_sets {
        for token in tokens {
            *df.entry(token.as_str()).or_default() += 1;
        }
    }
    let n = token_sets.len() as f64;
    df.into_iter()
        .map(|(token, count)| (token.to_owned(), (1.0 + n / count as f64).ln()))
        .collect()
}

/// Множество токенов -> `{token: idf-вес}`; токены без веса отбрасываются.
pub fn vectorize(tokens: &HashSet<String>, weights: &HashMap<String, f64>) -> HashMap<String, f64> {
    tokens
        .iter()
        .filter_map(|token| weights.get(token).map(|weight| (token.clone(), *weight)))
        .collect()
}

/// Косинусное сходство двух разреженных векторов с IDF-весами; 0.0, если пуст любой.
pub fn cosine(a: &HashMap<String, f64>, b: &HashMap<String, f64>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let norm_a = a.values().map(|w| w * w).sum::<f64>().sqrt();
    let norm_b = b.values().map(|w| w * w).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    let (small, large) = if a.len() > b.len() { (b, a) } else { (a, b) };
    let dot: f64 = small
        .iter()
        .map(|(token, w)| w * large.get(token).copied().unwrap_or(0.0))
        .sum();
    // Веса неотрицательны: точный косинус лежит в [0, 1], срезаем выброс float.
    (dot / (norm_a * norm_b)).clamp(0.0, 1.0)
}

/// Top-k самых похожих векторов на `vectors[index]` по убыванию косинуса.
///
/// Исключает сам индекс, отбрасывает оценки `<= threshold`, при равенстве — по индексу.
pub fn nearest(vectors: &[HashMap<String, f64>], index: usize, k: usize, threshold: f64) -> Vec<(usize, f64)> {
    let mut scored: Vec<(usize, f64)> = vectors
        .iter()
        .enumerate()
        .filter(|(j, _)| *j != index)
        .map(|(j, vector)| (j, cosine(&vectors[index], vector)))
        .filter(|(_, score)| *score > threshold)
        .collect();
    scored.sort_by(|(left_index, left_score), (right_index, right_score)| {
        right_score
            .partial_cmp(left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left_index.cmp(right_index))
    });
    scored.truncate(k);
    scored
}
