//! Текстовые правила поиска и карточек — порт функций `db.py` без базы.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::pystr;

pub const FRESHNESS_HALF_LIFE_DAYS: f64 = 120.0;
pub const SEARCH_QUERY_WORDS: usize = 64;
pub const CHAT_SOURCES: [&str; 3] = ["kimi", "claude", "codex"];
pub const COMMIT_SOURCES: [&str; 2] = ["git", "github"];
pub const DISTILLED_PREFIX: &str = "llm:";

/// Стоп-слова `similarity.STOPWORDS`: общие для поиска и графа близости.
pub const STOPWORDS: &[&str] = &[
    "a", "all", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "for", "from", "had",
    "has", "have", "if", "in", "is", "it", "its", "me", "my", "not", "of", "on", "or", "our", "so",
    "that", "the", "their", "them", "then", "they", "this", "to", "was", "we", "were", "what", "when",
    "who", "will", "with", "you", "your", "и", "в", "во", "на", "с", "со", "по", "за", "из", "от",
    "не", "но", "а", "же", "ли", "бы", "что", "это", "как", "так", "все", "всё", "его", "её", "их",
    "мой", "моя", "моё", "мои", "твой", "наш", "ваш", "он", "она", "оно", "они", "мы", "вы", "я",
    "ещё", "уже", "только", "где", "когда", "если", "чтобы", "потому", "поэтому", "который",
    "которая", "которые", "через", "здесь", "сам",
];

fn stopwords() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| STOPWORDS.iter().copied().collect())
}

fn search_words() -> &'static Regex {
    // Python `[^\W_]+(?:_[^\W_]+)*`: буквы и цифры Unicode, подчёркивание только внутри слова.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\p{L}\p{N}]+(?:_[\p{L}\p{N}]+)*").expect("static regex"))
}

/// Слова запроса без пунктуации и служебных слов, не больше 64, порядок первого вхождения.
pub fn search_terms(query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    let mut seen = HashSet::new();
    let mut words = Vec::new();
    for m in search_words().find_iter(&lower) {
        let word = m.as_str();
        if stopwords().contains(word) || !seen.insert(word.to_string()) {
            continue;
        }
        words.push(word.to_string());
        if words.len() == SEARCH_QUERY_WORDS {
            break;
        }
    }
    words
}

/// Основа слова как в референсе: −2 буквы у слов от 6, −1 у слов из 4–5.
pub fn query_stem(word: &str) -> Option<String> {
    if !pystr::is_alpha(word) {
        return None;
    }
    let lower = word.to_lowercase();
    let len = pystr::char_len(&lower);
    let cut = match len {
        n if n >= 6 => 2,
        n if n >= 4 => 1,
        _ => return None,
    };
    Some(pystr::take_chars(&lower, len - cut).to_string())
}

fn phrase(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// FTS5-выражение: каждое слово, его основа как префикс и вся фраза через OR.
/// Каждая часть в кавычках, поэтому произвольный ввод не даёт синтаксической ошибки.
pub fn build_fts_query(query: &str) -> Option<String> {
    let words = search_terms(query);
    if words.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let push = |part: String, parts: &mut Vec<String>| {
        if !parts.contains(&part) {
            parts.push(part);
        }
    };
    for word in &words {
        push(phrase(word), &mut parts);
        if let Some(stem) = query_stem(word) {
            push(format!("{}*", phrase(&stem)), &mut parts);
        }
    }
    if words.len() > 1 {
        push(phrase(&words.join(" ")), &mut parts);
    }
    Some(parts.join(" OR "))
}

/// 0.5 ** (age_days / 120); будущее время даёт 1.0.
pub fn freshness_factor(ts: i64, now: i64) -> f64 {
    let age_days = ((now - ts) as f64 / 86400.0).max(0.0);
    0.5f64.powf(age_days / FRESHNESS_HALF_LIFE_DAYS)
}

#[derive(Debug, Default, PartialEq)]
pub struct CardSections {
    pub summary: String,
    pub questions: String,
    pub atoms: String,
    pub rest: String,
}

/// Разделы карточки знаний: Кратко, Вопросы, Атомы; всё прочее — в rest.
pub fn split_card_sections(body: &str) -> CardSections {
    let mut parts: [Vec<&str>; 4] = Default::default(); // summary, questions, atoms, rest
    let mut current = 3;
    for line in pystr::splitlines(body) {
        let stripped = pystr::strip(line);
        if stripped.starts_with('#') {
            let heading = pystr::strip(stripped.trim_start_matches('#')).to_lowercase();
            let first = pystr::split_whitespace(&heading).next().unwrap_or("").trim_end_matches(':');
            current = match first {
                "кратко" | "summary" => 0,
                "вопросы" | "questions" => 1,
                "атомы" | "atoms" => 2,
                _ => 3,
            };
            if current != 3 {
                continue;
            }
        }
        parts[current].push(line);
    }
    let join = |lines: &Vec<&str>| pystr::strip(&lines.join("\n")).to_string();
    CardSections {
        summary: join(&parts[0]),
        questions: join(&parts[1]),
        atoms: join(&parts[2]),
        rest: join(&parts[3]),
    }
}

/// `YYYY-MM-DD` по UTC.
pub fn format_date(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Дни от 1970-01-01 → (год, месяц, день), алгоритм Хиннанта: без зависимостей от chrono.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Запись, к которой относится сырой фрагмент: разговор, день коммитов, иначе сам фрагмент.
pub fn unit_of(source: &str, source_ref: &str, ts: i64) -> String {
    if CHAT_SOURCES.contains(&source) {
        if let Some((head, tail)) = source_ref.rsplit_once(':') {
            let digits = !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit());
            if digits && head.starts_with(&format!("{source}:")) {
                return head.to_string();
            }
        }
        return source_ref.to_string();
    }
    if COMMIT_SOURCES.contains(&source) {
        let parts: Vec<&str> = source_ref.split(':').collect();
        if parts.len() >= 3 {
            return format!("{source}:{}:{}", parts[1..parts.len() - 1].join(":"), format_date(ts));
        }
    }
    source_ref.to_string()
}

pub fn unit_for(source: &str, source_ref: &str, ts: i64, kind: &str) -> String {
    if kind == crate::db::CARD_KIND_DISTILLED {
        return unit_of(
            source.strip_prefix(DISTILLED_PREFIX).unwrap_or(source),
            source_ref.strip_prefix(DISTILLED_PREFIX).unwrap_or(source_ref),
            ts,
        );
    }
    unit_of(source, source_ref, ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1_793_917_000), "2026-11-05");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_date(-1), "1969-12-31");
    }

    #[test]
    fn units() {
        assert_eq!(unit_of("claude", "claude:abc:12", 0), "claude:abc");
        assert_eq!(unit_of("kimi", "kimi:s:a:3", 0), "kimi:s:a");
        assert_eq!(unit_of("claude", "claude:abc:x", 0), "claude:abc:x");
        assert_eq!(unit_of("git", "git:Programs/GitPro:abcdef", 86_400), "git:Programs/GitPro:1970-01-02");
        assert_eq!(unit_of("obsidian", "obsidian:v:a.md", 0), "obsidian:v:a.md");
    }
}
