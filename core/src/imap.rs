//! Gmail и Яндекс.Почта по IMAP over TLS.
//!
//! Только чтение: вход по паролю приложения, EXAMINE (ящик без записи),
//! UID SEARCH после прошлого UID, UID FETCH BODY.PEEK[] (без флага \Seen). MIME разбирается минимально (text/plain, text/html без
//! тегов, base64 и quoted-printable); вложения игнорируются.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use native_tls::TlsConnector;
use crate::config::MailSettings;
use crate::db::{now_ts, CardDraft, Database};
use crate::ingest::{mask_secrets, truncate};

const BODY_MAX_CHARS: usize = 4_000;
const TITLE_MAX_CHARS: usize = 80;
const MAX_MESSAGES: usize = 100;

struct Session {
    reader: BufReader<native_tls::TlsStream<TcpStream>>,
    tag: usize,
}

impl Session {
    fn connect(host: &str) -> Result<Self, String> {
        let stream = TcpStream::connect((host, 993)).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let tls = TlsConnector::new().map_err(|error| error.to_string())?;
        let stream = tls.connect(host, stream).map_err(|error| error.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut greeting = String::new();
        reader.read_line(&mut greeting).map_err(|error| error.to_string())?;
        Ok(Session { reader, tag: 0 })
    }

    fn next_tag(&mut self) -> String {
        self.tag += 1;
        format!("A{}", self.tag)
    }

    fn send(&mut self, tag: &str, command: &str) -> Result<(), String> {
        let stream = self.reader.get_mut();
        stream.write_all(format!("{tag} {command}\r\n").as_bytes()).map_err(|error| error.to_string())?;
        stream.flush().map_err(|error| error.to_string())
    }

    fn read_line(&mut self) -> Result<Vec<u8>, String> {
        let mut line = Vec::new();
        self.reader.read_until(b'\n', &mut line).map_err(|error| error.to_string())?;
        if line.is_empty() {
            return Err("IMAP: соединение закрыто".to_owned());
        }
        Ok(line)
    }

    /// Ответ до тегированной строки. `NO`/`BAD` — ошибка: иначе неудачный LOGIN
    /// или SELECT выглядели успехом, и пустой UIDVALIDITY сбрасывал прогресс.
    fn command(&mut self, command: &str) -> Result<String, String> {
        let tag = self.next_tag();
        self.send(&tag, command)?;
        let mut response = String::new();
        loop {
            let line = String::from_utf8_lossy(&self.read_line()?).into_owned();
            response.push_str(&line);
            if let Some(status) = line.strip_prefix(&format!("{tag} ")) {
                return if status.starts_with("OK") { Ok(response) } else { Err(format!("IMAP: {}", status.trim())) };
            }
        }
    }

    /// Литерал `{n}` читается ровно по длине: искать конец ответа по тегу в
    /// буфере нельзя — строка тела «A4 …» обрезала бы письмо.
    fn fetch(&mut self, uid: &str) -> Result<Vec<u8>, String> {
        let tag = self.next_tag();
        self.send(&tag, &format!("UID FETCH {uid} (BODY.PEEK[])"))?;
        let mut body = None;
        loop {
            let line = self.read_line()?;
            if line.starts_with(format!("{tag} ").as_bytes()) {
                let status = String::from_utf8_lossy(&line[tag.len() + 1..]).trim().to_owned();
                if !status.starts_with("OK") {
                    return Err(format!("IMAP: {status}"));
                }
                return body.ok_or_else(|| "IMAP: в ответе нет тела письма".to_owned());
            }
            let trimmed = line.strip_suffix(b"\r\n").or_else(|| line.strip_suffix(b"\n")).unwrap_or(&line);
            if !trimmed.ends_with(b"}") {
                continue;
            }
            let Some(open) = trimmed.iter().rposition(|byte| *byte == b'{') else { continue };
            let size = std::str::from_utf8(&trimmed[open + 1..trimmed.len() - 1])
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .ok_or_else(|| "IMAP: неверная длина литерала".to_owned())?;
            let mut literal = vec![0u8; size];
            self.reader.read_exact(&mut literal).map_err(|error| error.to_string())?;
            if body.is_none() {
                body = Some(literal);
            }
        }
    }
}

/// Строки-продолжения (начинаются с пробела или таба) склеиваются с предыдущей:
/// иначе `boundary` на второй строке Content-Type терялся вместе со всем письмом.
fn unfold_headers(headers: &str) -> String {
    headers.replace("\r\n ", " ").replace("\r\n\t", " ").replace("\n ", " ").replace("\n\t", " ")
}

/// Письмо читается как Latin-1 — байт в символ без потерь: заголовки и границы
/// ASCII, а тело 8bit в windows-1251/koi8-r после UTF-8-lossy уже не восстановить.
fn latin1(raw: &[u8]) -> String {
    raw.iter().map(|&byte| char::from(byte)).collect()
}

/// Обратно к байтам. Символы вне Latin-1 приходят только из уже декодированного
/// текста и кодируются в UTF-8.
fn raw_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for ch in text.chars() {
        match u8::try_from(u32::from(ch)) {
            Ok(byte) => out.push(byte),
            Err(_) => out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out
}

/// Неизвестная или не указанная кодировка читается как UTF-8.
fn decode_charset(bytes: &[u8], charset: Option<&str>) -> String {
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.trim().as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(bytes).0.into_owned()
}

fn decode_base64(text: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut values = Vec::new();
    for byte in text.bytes().filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=') {
        if let Some(index) = TABLE.iter().position(|candidate| *candidate == byte) {
            values.push(index as u32);
        }
    }
    let mut out = Vec::new();
    for chunk in values.chunks(4) {
        let mut number = 0u32;
        for (index, value) in chunk.iter().enumerate() {
            number |= value << (18 - index * 6);
        }
        // Неполная четвёрка без '=' несёт len-1 байт, иначе хвост получает NUL.
        for shift in [16, 8, 0].into_iter().take(chunk.len().saturating_sub(1)) {
            out.push(((number >> shift) & 0xff) as u8);
        }
    }
    out
}

fn decode_quoted_printable(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'=' {
            // Мягкий перенос строки в QP: письма ходят с CRLF, а не только с LF,
            // иначе на каждой перенесённой строке в текст попадает лишний '='.
            let rest = &bytes[index + 1..];
            if rest.starts_with(b"\r\n") {
                index += 3;
                continue;
            }
            if rest.starts_with(b"\n") {
                index += 2;
                continue;
            }
            if index + 2 < bytes.len() {
                if let Ok(value) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[index + 1..index + 3]), 16) {
                    out.push(value);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn decode_rfc2047(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    let mut after_word = false;
    while let Some(start) = rest.find("=?") {
        let Some(len) = encoded_word_len(&rest[start..]) else {
            out.push_str(&rest[..start + 2]);
            rest = &rest[start + 2..];
            after_word = false;
            continue;
        };
        let gap = &rest[..start];
        // RFC 2047 §6.2: пробелы между соседними encoded-words не значимы, а
        // длинная тема почти всегда разбита на несколько слов.
        if !(after_word && gap.chars().all(char::is_whitespace)) {
            out.push_str(gap);
        }
        let mut parts = rest[start + 2..start + len - 2].splitn(3, '?');
        // RFC 2231: `charset*lang` — язык после звёздочки кодировку не меняет.
        let charset = parts.next().map(|charset| charset.split('*').next().unwrap_or(charset));
        let encoding = parts.next().unwrap_or("");
        let payload = parts.next().unwrap_or("");
        out.push_str(&match encoding.to_ascii_uppercase().as_str() {
            "B" => decode_charset(&decode_base64(payload), charset),
            "Q" => decode_charset(&decode_quoted_printable(&payload.replace('_', " ")), charset),
            _ => payload.to_owned(),
        });
        rest = &rest[start + len..];
        after_word = true;
    }
    out.push_str(rest);
    out
}

/// Длина `=?charset?enc?payload?=`. Закрывающий `?=` ищется только после третьего
/// `?`: в Q-кодировке полезная нагрузка сама начинается с `=`.
fn encoded_word_len(word: &str) -> Option<usize> {
    let body = word.strip_prefix("=?")?;
    let charset_end = body.find('?')?;
    let encoding_end = charset_end + 1 + body[charset_end + 1..].find('?')?;
    let payload_end = encoding_end + 1 + body[encoding_end + 1..].find("?=")?;
    Some(2 + payload_end + 2)
}

fn strip_html(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn decode_body(body: &str, encoding: &str, charset: Option<&str>) -> String {
    let bytes = match encoding.to_ascii_lowercase().as_str() {
        "base64" => decode_base64(body),
        "quoted-printable" => decode_quoted_printable(body),
        _ => raw_bytes(body),
    };
    decode_charset(&bytes, charset)
}

/// Параметр заголовка MIME: имя сравнивается без учёта регистра, значение
/// возвращается как есть и только до следующей `;`.
fn mime_param(header: &str, name: &str) -> Option<String> {
    header.split(';').skip(1).find_map(|part| {
        let (key, value) = part.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

fn part_text(content_type: &str, encoding: &str, body: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    if lower.starts_with("multipart/") {
        // Граница раздела регистрозависима, поэтому берём её из исходного
        // заголовка, а не из lowercased-копии.
        let boundary = mime_param(content_type, "boundary")?;
        let marker = format!("--{boundary}");
        let mut texts = Vec::new();
        for part in body.split(&marker).skip(1) {
            if part.starts_with("--") {
                break;
            }
            if let Some((headers, content)) = part.split_once("\r\n\r\n").or_else(|| part.split_once("\n\n")) {
                let mut part_type = "text/plain".to_owned();
                let mut part_encoding = "".to_owned();
                for line in unfold_headers(headers).lines() {
                    if let Some((name, value)) = line.split_once(':') {
                        match name.trim().to_ascii_lowercase().as_str() {
                            "content-type" => part_type = value.trim().to_owned(),
                            "content-transfer-encoding" => part_encoding = value.trim().to_owned(),
                            _ => {}
                        }
                    }
                }
                if let Some(text) = part_text(&part_type, &part_encoding, content) {
                    texts.push(text);
                }
            }
        }
        return Some(texts.join("\n"));
    }
    let charset = mime_param(content_type, "charset");
    if lower.starts_with("text/plain") {
        return Some(decode_body(body, encoding, charset.as_deref()));
    }
    if lower.starts_with("text/html") {
        return Some(strip_html(&decode_body(body, encoding, charset.as_deref())));
    }
    None
}

fn parse_message(raw: &[u8]) -> Option<(String, String, i64, String)> {
    let text = latin1(raw);
    let (headers, body) = text.split_once("\r\n\r\n").or_else(|| text.split_once("\n\n"))?;
    let mut subject = String::new();
    let mut content_type = "text/plain".to_owned();
    let mut encoding = String::new();
    let mut message_id = String::new();
    for line in unfold_headers(headers).lines() {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                // Сырые 8-битные заголовки по RFC 6532 — UTF-8.
                "subject" => subject = decode_rfc2047(&String::from_utf8_lossy(&raw_bytes(value.trim()))),
                "content-type" => content_type = value.trim().to_owned(),
                "content-transfer-encoding" => encoding = value.trim().to_owned(),
                "message-id" => message_id = value.trim().trim_matches(['<', '>']).to_owned(),
                _ => {}
            }
        }
    }
    let text = part_text(&content_type, &encoding, body)?.trim().to_owned();
    let title = if subject.trim().is_empty() { text.lines().next().unwrap_or("(no subject)").to_owned() } else { subject };
    Some((message_id, text, now_ts(), title))
}

/// Quoted string IMAP: экранируются `\` и `"`.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Ключ состояния с последней ошибкой источника: её читают `sync.log` и панель,
/// иначе отказ входа выглядел как «0 новых писем».
pub fn error_key(source: &str) -> String {
    format!("source_error:{source}")
}

pub fn ingest_mail(db: &Database, source: &str, host: &str, settings: &MailSettings) -> usize {
    if !settings.enabled {
        let _ = db.set_state(&error_key(source), "");
        return 0;
    }
    // Сбой посреди пачки не отменяет уже записанные письма, но и не должен выглядеть
    // успешным прогоном: UID на нём остановился.
    let (inserted, error) = match mail(db, source, host, settings) {
        Ok((inserted, stopped)) => (inserted, stopped.unwrap_or_default()),
        Err(error) => (0, error),
    };
    let _ = db.set_state(&error_key(source), &error);
    inserted
}

fn mail(db: &Database, source: &str, host: &str, settings: &MailSettings) -> Result<(usize, Option<String>), String> {
    let (Some(email), Some(password)) = (settings.email.as_deref(), settings.app_password.as_deref()) else {
        return Err("не указаны адрес или пароль приложения".to_owned());
    };
    let mut session = Session::connect(host).map_err(|e| format!("нет соединения с {host}: {e}"))?;
    session
        .command(&format!("LOGIN {} {}", quote(email), quote(password)))
        .map_err(|e| format!("вход отклонён, проверьте адрес и пароль приложения ({e})"))?;
    let select = session.command("EXAMINE INBOX").map_err(|e| format!("не открылась папка «Входящие»: {e}"))?;
    let uidvalidity = select.lines().find_map(|line| line.strip_prefix("* OK [UIDVALIDITY ")).and_then(|rest| rest.split(']').next()).map(str::to_owned).unwrap_or_default();
    // Без UIDVALIDITY нельзя понять, действителен ли сохранённый UID: сравнение с
    // пустой строкой сбросило бы прогресс и перекачало весь ящик.
    if uidvalidity.is_empty() {
        return Err("сервер не сообщил UIDVALIDITY".to_owned());
    }
    let validity_key = format!("{source}:uidvalidity");
    let previous_validity = db.get_state(&validity_key).ok().flatten().unwrap_or_default();
    let uid_key = format!("{source}:last_uid");
    let mut last_uid = if previous_validity == uidvalidity {
        db.get_state(&uid_key).ok().flatten().and_then(|value| value.parse::<u64>().ok()).unwrap_or(0)
    } else {
        0
    };
    let search = session.command(&format!("UID SEARCH UID {}:*", last_uid + 1)).map_err(|e| format!("поиск писем не удался: {e}"))?;
    let mut uids: Vec<u64> = search.lines().filter_map(|line| line.strip_prefix("* SEARCH ")).flat_map(|rest| rest.split_whitespace()).filter_map(|uid| uid.parse::<u64>().ok()).collect();
    uids.sort_unstable();
    // Первый прогон по ящику на десятки тысяч писем начинал бы с писем
    // десятилетней давности и шёл до свежих днями: берём последние.
    if last_uid == 0 && uids.len() > MAX_MESSAGES {
        uids.drain(..uids.len() - MAX_MESSAGES);
    }
    uids.truncate(MAX_MESSAGES);
    let mut inserted = 0;
    let mut stopped = None;
    for uid in uids {
        // Сбой загрузки останавливает продвижение UID: иначе следующий UID
        // перешагнул бы через письмо, и оно потерялось бы навсегда.
        let raw = match session.fetch(&uid.to_string()) {
            Ok(raw) => raw,
            Err(error) => {
                stopped = Some(format!("загрузка письма UID {uid} прервалась: {error}"));
                break;
            }
        };
        // Письмо без текстовой части (только вложение) сохранять нечего, но UID
        // сдвигаем: иначе оно перекачивалось бы целиком каждую синхронизацию.
        let Some((message_id, text, _ts, title)) = parse_message(&raw) else {
            last_uid = last_uid.max(uid);
            continue;
        };
        let reference = if message_id.is_empty() { format!("{source}:uid:{uid}") } else { format!("{source}:{message_id}") };
        let draft = CardDraft::new(
            source,
            &reference,
            _ts,
            &truncate(&mask_secrets(title.trim()), TITLE_MAX_CHARS),
            &truncate(&mask_secrets(text.trim()), BODY_MAX_CHARS),
        );
        // Ошибка записи (занятая база) тоже держит UID, как и сбой загрузки.
        match db.insert_card(&draft) {
            Ok(true) => inserted += 1,
            Ok(false) => {}
            Err(error) => {
                stopped = Some(format!("письмо UID {uid} не записалось: {error}"));
                break;
            }
        }
        last_uid = last_uid.max(uid);
    }
    let _ = db.set_state(&validity_key, &uidvalidity);
    let _ = db.set_state(&uid_key, &last_uid.to_string());
    Ok((inserted, stopped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_and_quoted_printable() {
        assert_eq!(decode_base64("0J/RgNC40LLQtdGC"), "Привет".as_bytes());
        assert_eq!(decode_quoted_printable("Hello=20world=21"), b"Hello world!");
    }

    #[test]
    fn quoted_printable_drops_soft_line_breaks() {
        // Письма ходят с CRLF: без этого на каждой перенесённой строке остаётся '='.
        assert_eq!(decode_quoted_printable("Hello=\r\nworld"), b"Helloworld");
        assert_eq!(decode_quoted_printable("Hello=\nworld"), b"Helloworld");
        assert_eq!(decode_quoted_printable("Hello=20world"), b"Hello world");
    }

    #[test]
    fn multipart_boundary_keeps_case_and_stops_at_next_parameter() {
        let body = "--AbC\r\nContent-Type: text/plain\r\n\r\nтекст\r\n--AbC--\r\n";
        let text = part_text(r#"multipart/alternative; boundary="AbC"; charset=utf-8"#, "", body);
        assert_eq!(text.as_deref(), Some("текст\r\n"));
    }

    #[test]
    fn rfc2047_handles_q_payload_split_words_and_garbage() {
        assert_eq!(decode_rfc2047("=?UTF-8?Q?=D0=9F=D1=80?="), "Пр");
        assert_eq!(decode_rfc2047("=?UTF-8?B?0J/RgNC4?= =?UTF-8?B?0LLQtdGC?="), "Привет");
        assert_eq!(decode_rfc2047("a=?=b"), "a=?=b");
        assert_eq!(decode_rfc2047("x =?UTF-8?Q?a?= y"), "x a y");
    }

    #[test]
    fn folded_headers_keep_boundary_and_whole_subject() {
        let raw = "Subject: =?UTF-8?B?0J/RgNC4?=\r\n =?UTF-8?B?0LLQtdGC?=\r\nContent-Type: multipart/alternative;\r\n\tboundary=\"xyz\"\r\n\r\n--xyz\r\nContent-Type: text/plain\r\n\r\nтело\r\n--xyz--\r\n";
        let (_, text, _, title) = parse_message(raw.as_bytes()).expect("письмо разобрано");
        assert_eq!(text, "тело");
        assert_eq!(title, "Привет");
    }

    #[test]
    fn legacy_cyrillic_charsets_are_decoded() {
        // «Привет» в windows-1251 и koi8-r: тема B и Q, тело 8bit.
        assert_eq!(decode_rfc2047("=?windows-1251?B?z/Do4uXy?="), "Привет");
        let mut raw = b"Subject: =?koi8-r?Q?=F0=D2=C9=D7=C5=D4?=\r\nContent-Type: text/plain; charset=windows-1251\r\n\r\n".to_vec();
        raw.extend_from_slice(&[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
        let (_, text, _, title) = parse_message(&raw).expect("письмо разобрано");
        assert_eq!((text.as_str(), title.as_str()), ("Привет", "Привет"));
        let (_, text, _, title) = parse_message("Subject: тема\r\n\r\nтело".as_bytes()).expect("письмо разобрано");
        assert_eq!((text.as_str(), title.as_str()), ("тело", "тема"));
    }

    #[test]
    fn mail_failure_is_recorded_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("m.db")).unwrap();
        let broken = MailSettings { enabled: true, email: Some("a@b.c".into()), app_password: None };
        assert_eq!(ingest_mail(&db, "gmail", "imap.invalid", &broken), 0);
        let error = db.get_state(&error_key("gmail")).unwrap().unwrap();
        assert!(error.contains("пароль"), "{error}");
        assert_eq!(crate::sources_extra::source_errors(&db)["gmail"], error.as_str());
        ingest_mail(&db, "gmail", "imap.invalid", &MailSettings::default());
        assert!(crate::sources_extra::source_errors(&db).is_empty());
    }

    #[test]
    fn imap_quote_escapes_backslash_and_quote() {
        assert_eq!(quote(r#"a\b"c"#), r#""a\\b\"c""#);
    }

    #[test]
    fn parses_plain_and_html_parts() {
        assert_eq!(part_text("text/plain", "", "hello").as_deref(), Some("hello"));
        assert_eq!(part_text("text/html", "", "<b>hello</b>").as_deref(), Some("hello"));
    }
}
