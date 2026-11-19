//! Строковые операции с семантикой Python 3: ответы MCP и FTS-запросы должны
//! совпадать с прежней реализацией символ в символ, а у Rust свои правила
//! пробелов, разбиения строк и экранирования.

/// `str.isspace()`: Unicode White_Space плюс разделители \x1c–\x1f, которых у Rust нет.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// `str.split()` без аргументов.
pub fn split_whitespace(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|part| !part.is_empty())
}

fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// `str.splitlines()`: все разделители строк Python, `\r\n` — один разрыв, без пустого хвоста.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !is_line_break(c) {
            continue;
        }
        lines.push(&s[start..i]);
        let mut end = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(j, '\n')) = chars.peek() {
                chars.next();
                end = j + 1;
            }
        }
        start = end;
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// `str.isalpha()`: непустая строка из одних букв.
pub fn is_alpha(s: &str) -> bool {
    !s.is_empty() && s.chars().all(char::is_alphabetic)
}

/// Первые `n` символов (кодовых точек), как срез `s[:n]`.
pub fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Непечатаемые для `str.isprintable()` символы, которые реально встречаются
/// в запросах: управляющие, пробелы кроме обычного, форматирующие, суррогаты нет.
fn is_unprintable(c: char) -> bool {
    let u = c as u32;
    u < 0x20
        || (0x7f..=0xa0).contains(&u)
        || u == 0xad
        || u == 0x1680
        || (0x2000..=0x200f).contains(&u)
        || (0x2028..=0x202f).contains(&u)
        || (0x205f..=0x2064).contains(&u)
        || u == 0x3000
        || u == 0xfeff
        || (0xe000..=0xf8ff).contains(&u)
        || (0xfff9..=0xfffb).contains(&u)
}

/// `repr(str)`: одинарные кавычки, если внутри нет `'` без `"`.
pub fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_unprintable(c) => {
                let u = c as u32;
                if u <= 0xff {
                    out.push_str(&format!("\\x{u:02x}"));
                } else {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(splitlines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
        assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(splitlines("a\u{2028}b"), vec!["a", "b"]);
        assert!(splitlines("").is_empty());
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(repr("   "), "'   '");
        assert_eq!(repr("it's"), "\"it's\"");
        assert_eq!(repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(repr("x\ny\\"), "'x\\ny\\\\'");
        assert_eq!(repr("мозг\u{a0}"), "'мозг\\xa0'");
    }

    #[test]
    fn strip_uses_python_whitespace() {
        assert_eq!(strip("\u{1c} x \u{3000}"), "x");
        assert_eq!(split_whitespace(" a\t b ").collect::<Vec<_>>(), vec!["a", "b"]);
    }
}
