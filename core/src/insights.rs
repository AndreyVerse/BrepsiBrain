//! Надстройки над памятью: закреплённые записи, дайджест за период, похожие
//! ошибки, экспорт карточки в Obsidian и смысловой поиск по эмбеддингам.
//!
//! Все функции синхронные и исполняются в потоке HTTP-запроса панели или в
//! процессе синхронизации; общего состояния, кроме базы, у модуля нет.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::config::Config;
use crate::db::{now_ts, Card, Database, Filters, UnitInfo, CARD_KIND_DISTILLED};
use crate::text::split_card_sections;

/// Папка экспорта внутри хранилища. Коннектор Obsidian её не читает: иначе
/// выгруженная карточка вернулась бы в память дублем.
pub const EXPORT_DIR: &str = "Brepsi Brain";

const DIGEST_ATOMS_PER_KIND: usize = 3;
const SIMILAR_ERRORS_MAX: usize = 5;
/// Процент общих основ слов от более короткой ошибки, начиная с которого это «та же ошибка».
const SIMILAR_ERROR_SHARE: usize = 40;
const EMBED_BATCH: usize = 16;
const EMBED_TEXT_CHARS: usize = 2000;
/// Ниже этого косинуса совпадение по смыслу — шум: bge-m3 даёт ~0.3 и несвязанным текстам.
const SEMANTIC_MIN_SIMILARITY: f32 = 0.45;
const RRF_K: f64 = 60.0;

// -- атомы карточки ------------------------------------------------------------------------

/// Атомы карточки знаний как пары (тип, текст); у сырых фрагментов атомов нет.
pub fn atoms(card: &Card) -> Vec<(String, String)> {
    if card.kind != CARD_KIND_DISTILLED {
        return Vec::new();
    }
    split_card_sections(&card.body)
        .atoms
        .lines()
        .map(|line| line.trim().trim_start_matches(['-', '*', '•']).trim())
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once(':') {
            Some((head, rest)) if matches!(head.trim(), "факт" | "решение" | "ошибка" | "команда") && !rest.trim().is_empty() => {
                (head.trim().to_owned(), rest.trim().to_owned())
            }
            _ => ("факт".to_owned(), line.to_owned()),
        })
        .collect()
}

fn of_kind(atoms: &[(String, String)], kind: &str) -> Vec<String> {
    atoms.iter().filter(|(k, _)| k == kind).map(|(_, text)| text.clone()).collect()
}

// -- закрепы -------------------------------------------------------------------------------

pub fn set_pin(db: &Database, unit: &str, pinned: bool) -> Result<(), String> {
    let result = if pinned {
        db.connection().execute("INSERT INTO pins(unit, ts) VALUES(?1, ?2) ON CONFLICT(unit) DO NOTHING", params![unit, now_ts()])
    } else {
        db.connection().execute("DELETE FROM pins WHERE unit=?1", [unit])
    };
    result.map(|_| ()).map_err(|error| error.to_string())
}

/// Закреплённые записи, последние закрепления первыми.
pub fn pinned_units(db: &Database) -> Vec<String> {
    let Ok(mut stmt) = db.connection().prepare("SELECT unit FROM pins ORDER BY ts DESC, unit") else {
        return Vec::new();
    };
    stmt.query_map([], |row| row.get::<_, String>(0)).map(|rows| rows.flatten().collect()).unwrap_or_default()
}

pub fn is_pinned(db: &Database, unit: &str) -> bool {
    !unit.is_empty()
        && db.connection().query_row("SELECT 1 FROM pins WHERE unit=?1", [unit], |_| Ok(())).optional().ok().flatten().is_some()
}

/// Карточки закреплённых записей впереди, остальные в прежнем порядке (MCP `recent`).
pub fn pinned_first(db: &Database, cards: Vec<Card>) -> Vec<Card> {
    let pins: HashSet<String> = pinned_units(db).into_iter().collect();
    if pins.is_empty() {
        return cards;
    }
    let (mut first, rest): (Vec<Card>, Vec<Card>) = cards.into_iter().partition(|card| pins.contains(&card.unit));
    first.extend(rest);
    first
}

// -- дайджест ------------------------------------------------------------------------------

/// Сводка за `days` дней по готовым карточкам: решения, ошибки и команды по
/// проектам. Модель не вызывается — всё уже пересказано при синхронизации.
/// Записи проекта за период и его атомы: решения, ошибки, команды.
type DigestRow = (i64, Vec<Value>, Vec<Value>, Vec<Value>);

pub fn digest(db: &Database, index: &HashMap<String, UnitInfo>, days: i64, now: i64) -> Value {
    let cutoff = now - days * 86_400;
    let mut projects: HashMap<String, DigestRow> = HashMap::new();
    let mut units: Vec<&UnitInfo> = index.values().filter(|info| info.last_ts >= cutoff).collect();
    units.sort_by_key(|info| std::cmp::Reverse(info.last_ts));
    let total = units.len();
    for info in units {
        let entry = projects.entry(info.project.clone()).or_default();
        entry.0 += 1;
        if info.card_id == 0 {
            continue;
        }
        let Some(card) = db.get_card(info.card_id).ok().flatten() else { continue };
        let card_atoms = atoms(&card);
        for (kind, list) in [("решение", &mut entry.1), ("ошибка", &mut entry.2), ("команда", &mut entry.3)] {
            for text in of_kind(&card_atoms, kind) {
                if list.len() < DIGEST_ATOMS_PER_KIND {
                    list.push(json!({"text": text, "card_id": card.id}));
                }
            }
        }
    }
    let mut rows: Vec<Value> = projects
        .into_iter()
        .map(|(project, (records, decisions, errors, commands))| {
            json!({
                "project": project,
                "project_name": project.rsplit('/').next().unwrap_or(""),
                "records": records,
                "decisions": decisions,
                "errors": errors,
                "commands": commands,
            })
        })
        .collect();
    rows.sort_by(|a, b| b["records"].as_i64().cmp(&a["records"].as_i64()).then_with(|| a["project"].as_str().cmp(&b["project"].as_str())));
    json!({"days": days, "records": total, "projects": rows})
}

// -- похожие ошибки --------------------------------------------------------------------------

fn words(text: &str) -> HashSet<String> {
    crate::text::search_terms(text).into_iter().collect()
}

/// Прошлые записи с той же ошибкой и их решения: как это чинили раньше.
pub fn similar_errors(db: &Database, card: &Card, index: &HashMap<String, UnitInfo>) -> Vec<Value> {
    let own = of_kind(&atoms(card), "ошибка");
    let mut seen: HashSet<String> = HashSet::from([card.unit.clone()]);
    let mut out = Vec::new();
    for error in own.iter().take(3) {
        let wanted = words(error);
        let Ok(hits) = db.search(error, 30, None, &Filters::default()) else { continue };
        for hit in hits {
            if out.len() >= SIMILAR_ERRORS_MAX {
                return out;
            }
            if hit.kind != CARD_KIND_DISTILLED || seen.contains(&hit.unit) {
                continue;
            }
            let hit_atoms = atoms(&hit);
            // Совпадение должно быть именно с ошибкой той записи, а не с любым текстом.
            let best = of_kind(&hit_atoms, "ошибка")
                .into_iter()
                .map(|text| {
                    let theirs = words(&text);
                    let overlap = wanted.intersection(&theirs).count();
                    // Доля от короткой из двух ошибок: пара общих слов в длинных фразах
                    // («потеря», «исправили») связывала ошибки из разных проектов.
                    let shorter = wanted.len().min(theirs.len()).max(1);
                    (overlap, overlap * 100 / shorter, text)
                })
                .filter(|(overlap, share, _)| *overlap >= 2 && *share >= SIMILAR_ERROR_SHARE)
                .map(|(overlap, _, text)| (overlap, text))
                .max_by_key(|(overlap, _)| *overlap);
            let Some((_, matched)) = best else { continue };
            seen.insert(hit.unit.clone());
            out.push(json!({
                "id": hit.id,
                "title": hit.title,
                "ts": hit.ts,
                "project": index.get(&hit.unit).map(|info| info.project.clone()),
                "error": matched,
                "fixes": of_kind(&hit_atoms, "решение").into_iter().take(3).collect::<Vec<_>>(),
            }));
        }
    }
    out
}

// -- экспорт в Obsidian ----------------------------------------------------------------------

/// Первое существующее из настроенных хранилищ, иначе встроенное, если оно есть.
pub fn export_vault(config: &Config) -> Option<PathBuf> {
    config.obsidian.vaults.iter().find(|vault| vault.is_dir()).cloned().or_else(|| {
        let vault = crate::obsidian::default_vault();
        vault.is_dir().then_some(vault)
    })
}

fn file_stem(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| if c.is_control() || r#"\/:*?"<>|#^[]"#.contains(c) { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned = cleaned.trim_matches('.').trim();
    let stem: String = cleaned.chars().take(80).collect();
    if stem.is_empty() { "Без названия".to_owned() } else { stem }
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Markdown-заметка из ответа `/api/card` (тот же JSON, что видит панель).
pub fn markdown(card: &Value) -> String {
    let text = |key: &str| card.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let record = card.get("record").cloned().unwrap_or(Value::Null);
    let project = record.get("project").and_then(Value::as_str).unwrap_or("");
    let mut out = format!(
        "---\nbrepsi_card: {}\nsource: {}\ndate: {}\nproject: {}\n---\n\n# {}\n",
        card.get("id").and_then(Value::as_i64).unwrap_or(0),
        yaml_string(&text("source")),
        yaml_string(&text("date")),
        yaml_string(project),
        text("title"),
    );
    let summary = text("summary");
    let lists = |key: &str| -> Vec<Value> { card.get(key).and_then(Value::as_array).cloned().unwrap_or_default() };
    if summary.trim().is_empty() {
        out.push_str(&format!("\n{}\n", text("body").trim()));
    } else {
        out.push_str(&format!("\n## Кратко\n\n{}\n", summary.trim()));
    }
    let atoms = lists("atoms");
    if !atoms.is_empty() {
        out.push_str("\n## Знания\n\n");
        for atom in atoms {
            let kind = atom.get("type").and_then(Value::as_str).unwrap_or("факт");
            let body = atom.get("text").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("- **{kind}**: {body}\n"));
        }
    }
    let questions = lists("questions");
    if !questions.is_empty() {
        out.push_str("\n## Вопросы\n\n");
        for question in questions.iter().filter_map(Value::as_str) {
            out.push_str(&format!("- {question}\n"));
        }
    }
    let keywords: Vec<String> = lists("keywords").iter().filter_map(Value::as_str).map(|k| format!("#{}", k.replace(' ', "_"))).collect();
    if !keywords.is_empty() {
        out.push_str(&format!("\n{}\n", keywords.join(" ")));
    }
    out
}

/// Записать заметку в `<хранилище>/Brepsi Brain/<название>.md`; повторный экспорт
/// той же карточки перезаписывает файл, а не плодит копии.
pub fn export_card(vault: &Path, card: &Value) -> Result<PathBuf, String> {
    let dir = vault.join(EXPORT_DIR);
    std::fs::create_dir_all(&dir).map_err(|error| format!("не создать папку экспорта: {error}"))?;
    let stem = file_stem(card.get("title").and_then(Value::as_str).unwrap_or(""));
    let id = card.get("id").and_then(Value::as_i64).unwrap_or(0);
    let mut path = dir.join(format!("{stem}.md"));
    // Заметка с тем же названием от другой карточки не затирается.
    if path.is_file() && !std::fs::read_to_string(&path).is_ok_and(|text| text.contains(&format!("brepsi_card: {id}\n"))) {
        path = dir.join(format!("{stem} ({id}).md"));
    }
    let tmp = path.with_extension("md.tmp");
    let written = std::fs::write(&tmp, markdown(card)).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("не записать заметку: {error}"));
    }
    Ok(path)
}

// -- смысловой поиск -------------------------------------------------------------------------

fn embed_text(card: &Card) -> String {
    let body: String = card.body.chars().take(EMBED_TEXT_CHARS).collect();
    format!("{}\n{}", card.title, body)
}

/// Отпечаток содержимого: переписанная карточка получает новый вектор.
pub(crate) fn signature(card: &Card) -> String {
    format!("{}:{}:{}", card.ts, card.title.len(), card.body.len())
}

fn normalized(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|x| *x /= norm);
    }
    vector
}

fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(blob: &[u8]) -> Vec<f32> {
    let (chunks, _) = blob.as_chunks::<4>();
    chunks.iter().map(|bytes| f32::from_le_bytes(*bytes)).collect()
}

/// Сколько карточек знаний и сколько из них уже с вектором текущей модели.
pub fn embedding_coverage(db: &Database, model: Option<&str>) -> (i64, i64) {
    let con = db.connection();
    let total = con.query_row("SELECT COUNT(*) FROM cards WHERE kind=?1", [CARD_KIND_DISTILLED], |row| row.get(0)).unwrap_or(0);
    let done = model.map_or(0, |model| {
        con.query_row(
            "SELECT COUNT(*) FROM embeddings e JOIN cards c ON c.id=e.card_id WHERE e.model=?1",
            [model],
            |row| row.get(0),
        )
        .unwrap_or(0)
    });
    (total, done)
}

/// Довекторизовать до `budget` карточек знаний. Исполняется в синхронизации.
pub fn index_embeddings(db: &Database, config: &Config, budget: usize) -> Result<usize, String> {
    let Some(model) = config.search.embed_model.as_deref() else { return Ok(0) };
    let con = db.connection();
    con.execute("DELETE FROM embeddings WHERE model<>?1 OR card_id NOT IN (SELECT id FROM cards)", [model]).map_err(|e| e.to_string())?;
    let stored: HashMap<i64, String> = {
        let mut stmt = con.prepare("SELECT card_id, sig FROM embeddings").map_err(|e| e.to_string())?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).map_err(|e| e.to_string())?;
        rows.flatten().collect()
    };
    let pending: Vec<Card> = {
        let mut stmt = con.prepare("SELECT id FROM cards WHERE kind=?1 ORDER BY ts DESC").map_err(|e| e.to_string())?;
        let ids: Vec<i64> = stmt.query_map([CARD_KIND_DISTILLED], |row| row.get(0)).map_err(|e| e.to_string())?.flatten().collect();
        ids.into_iter()
            .filter_map(|id| db.get_card(id).ok().flatten())
            .filter(|card| stored.get(&card.id) != Some(&signature(card)))
            .take(budget)
            .collect()
    };
    let mut written = 0;
    for batch in pending.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(embed_text).collect();
        let vectors = crate::local::embed(config, model, &texts)?;
        for (card, vector) in batch.iter().zip(vectors) {
            con.execute(
                "INSERT INTO embeddings(card_id, model, sig, vec) VALUES(?1, ?2, ?3, ?4) \
                 ON CONFLICT(card_id) DO UPDATE SET model=excluded.model, sig=excluded.sig, vec=excluded.vec",
                params![card.id, model, signature(card), to_blob(&normalized(vector))],
            )
            .map_err(|e| e.to_string())?;
            written += 1;
        }
    }
    Ok(written)
}

/// Не плодить потоки прогрева, пока первый ещё поднимает движок.
static WARMING: AtomicBool = AtomicBool::new(false);

/// Карточки, близкие к запросу по смыслу: `(card_id, косинус)`. Пока движок не
/// запущен, поиск не ждёт его подъёма (до 90 с): прогрев уходит в фон, а этот
/// запрос обходится словами.
pub fn semantic_hits(db: &Database, config: &Config, query: &str, top: usize) -> Vec<(i64, f32)> {
    let Some(model) = config.search.embed_model.clone() else { return Vec::new() };
    if embedding_coverage(db, Some(&model)).1 == 0 {
        return Vec::new();
    }
    if !crate::local::status_running() {
        if !WARMING.swap(true, Ordering::AcqRel) {
            let config = config.clone();
            std::thread::spawn(move || {
                let _ = crate::local::embed(&config, &model, &["прогрев".to_owned()]);
                WARMING.store(false, Ordering::Release);
            });
        }
        return Vec::new();
    }
    let Ok(mut vectors) = crate::local::embed(config, &model, &[query.to_owned()]) else { return Vec::new() };
    let query_vector = normalized(vectors.pop().unwrap_or_default());
    let Ok(mut stmt) = db.connection().prepare("SELECT card_id, vec FROM embeddings WHERE model=?1") else { return Vec::new() };
    let Ok(rows) = stmt.query_map([&model], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))) else { return Vec::new() };
    let mut hits: Vec<(i64, f32)> = rows
        .flatten()
        .filter_map(|(id, blob)| {
            let vector = from_blob(&blob);
            (vector.len() == query_vector.len()).then(|| (id, vector.iter().zip(&query_vector).map(|(a, b)| a * b).sum::<f32>()))
        })
        .filter(|(_, similarity)| *similarity >= SEMANTIC_MIN_SIMILARITY)
        .collect();
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(top);
    hits
}

/// Слияние выдачи по словам и по смыслу (reciprocal rank fusion): одна запись — один результат.
pub fn fuse(db: &Database, lexical: Vec<Card>, semantic: &[(i64, f32)]) -> Vec<Card> {
    if semantic.is_empty() {
        return lexical;
    }
    let mut scores: HashMap<i64, f64> = HashMap::new();
    let mut cards: HashMap<i64, Card> = HashMap::new();
    for (rank, card) in lexical.into_iter().enumerate() {
        *scores.entry(card.id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        cards.insert(card.id, card);
    }
    for (rank, (id, _)) in semantic.iter().enumerate() {
        if !cards.contains_key(id) {
            let Some(card) = db.get_card(*id).ok().flatten() else { continue };
            cards.insert(*id, card);
        }
        *scores.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    let mut ordered: Vec<(f64, Card)> = cards.into_values().map(|card| (scores[&card.id], card)).collect();
    ordered.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(b.1.ts.cmp(&a.1.ts)));
    let mut seen = HashSet::new();
    ordered
        .into_iter()
        .filter(|(_, card)| seen.insert(card.unit.clone()))
        .map(|(score, mut card)| {
            card.score = Some(score);
            card
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CardDraft;

    fn db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("m.db")).unwrap();
        (dir, db)
    }

    fn distilled(db: &Database, unit: &str, title: &str, atoms: &[&str]) -> Card {
        let body = crate::llm::render_card_body("кратко", &[], &atoms.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let mut raw = CardDraft::new("claude", &format!("{unit}:1"), now_ts(), title, "фрагмент");
        raw.unit = unit.to_owned();
        db.insert_card(&raw).unwrap();
        let mut draft = CardDraft::new("claude", &format!("llm:{unit}"), now_ts(), title, &body).distilled();
        draft.unit = unit.to_owned();
        db.insert_card(&draft).unwrap();
        db.get_card_by_ref(&format!("llm:{unit}")).unwrap().unwrap()
    }

    #[test]
    fn pins_toggle_and_lead_recent() {
        let (_dir, db) = db();
        let a = distilled(&db, "u1", "первая", &[]);
        let b = distilled(&db, "u2", "вторая", &[]);
        set_pin(&db, "u2", true).unwrap();
        set_pin(&db, "u2", true).unwrap();
        assert!(is_pinned(&db, "u2") && !is_pinned(&db, "u1"));
        let order: Vec<i64> = pinned_first(&db, vec![a.clone(), b.clone()]).iter().map(|c| c.id).collect();
        assert_eq!(order, vec![b.id, a.id]);
        set_pin(&db, "u2", false).unwrap();
        assert!(pinned_units(&db).is_empty());
    }

    #[test]
    fn similar_errors_find_past_fix_of_the_same_error() {
        let (_dir, db) = db();
        let old = distilled(&db, "old", "старая", &["ошибка: cargo build падает с os error 5 на build-script", "решение: собирать в штатный target"]);
        distilled(&db, "other", "другая", &["ошибка: сеть недоступна"]);
        // Две общие основы в длинной чужой фразе — не та же ошибка.
        distilled(&db, "far", "далёкая", &["ошибка: враг зависал после error в контроллере, потому что загрузка мира теряла цель и build падал"]);
        let new = distilled(&db, "new", "новая", &["ошибка: os error 5 при запуске build-script"]);
        let hits = similar_errors(&db, &new, &HashMap::new());
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0]["id"], old.id);
        assert_eq!(hits[0]["fixes"][0], "собирать в штатный target");
    }

    #[test]
    fn digest_groups_atoms_by_kind() {
        let (_dir, db) = db();
        distilled(&db, "u1", "запись", &["решение: взять WAL", "ошибка: SQLITE_BUSY", "команда: cargo test"]);
        let index = db.unit_index().unwrap();
        let value = digest(&db, &index, 7, now_ts());
        assert_eq!(value["records"], 1);
        let project = &value["projects"][0];
        assert_eq!(project["decisions"][0]["text"], "взять WAL");
        assert_eq!(project["errors"][0]["text"], "SQLITE_BUSY");
        assert_eq!(project["commands"][0]["text"], "cargo test");
    }

    #[test]
    fn export_writes_markdown_and_keeps_foreign_notes() {
        let dir = tempfile::tempdir().unwrap();
        let card = json!({"id": 7, "title": "Фикс: WAL/блокировки?", "source": "claude", "date": "01.11.2026",
            "summary": "суть", "atoms": [{"type": "решение", "text": "взять WAL"}], "questions": [], "keywords": ["sqlite"]});
        let path = export_card(dir.path(), &card).unwrap();
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "Фикс WAL блокировки.md");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("brepsi_card: 7\n") && text.contains("- **решение**: взять WAL") && text.contains("#sqlite"));
        assert_eq!(export_card(dir.path(), &card).unwrap(), path);
        let other = json!({"id": 8, "title": "Фикс: WAL/блокировки?"});
        assert!(export_card(dir.path(), &other).unwrap().ends_with("Фикс WAL блокировки (8).md"));
    }

    #[test]
    fn fusion_merges_lexical_and_semantic_ranks() {
        let (_dir, db) = db();
        let a = distilled(&db, "a", "a", &[]);
        let b = distilled(&db, "b", "b", &[]);
        let c = distilled(&db, "c", "c", &[]);
        let fused = fuse(&db, vec![a.clone(), b.clone()], &[(c.id, 0.9), (b.id, 0.8)]);
        assert_eq!(fused[0].id, b.id, "в обоих списках — выше");
        assert_eq!(fused.len(), 3);
        assert_eq!(fuse(&db, vec![a.clone()], &[]).len(), 1);
    }

    #[test]
    fn blob_roundtrip_and_normalization() {
        let v = normalized(vec![3.0, 4.0]);
        assert_eq!(from_blob(&to_blob(&v)), v);
        assert!((v[0] - 0.6).abs() < 1e-6);
    }
}
