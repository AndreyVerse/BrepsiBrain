//! Статика панели, встроенная в exe. В dev-режиме сервер сначала читает
//! `static/` с диска (удобно править без пересборки), а при отсутствии файла
//! отдаёт встроенную копию — портативной сборке папка `static` не нужна.

pub fn get(path: &str) -> Option<&'static [u8]> {
    Some(match path {
        "index.html" => include_bytes!("../../static/index.html"),
        "app.js" => include_bytes!("../../static/app.js"),
        "i18n.js" => include_bytes!("../../static/i18n.js"),
        "style.css" => include_bytes!("../../static/style.css"),
        "brand.svg" => include_bytes!("../../static/brand.svg"),
        "brand-small.svg" => include_bytes!("../../static/brand-small.svg"),
        "fonts/manrope-cyrillic-400-normal.woff2" => include_bytes!("../../static/fonts/manrope-cyrillic-400-normal.woff2"),
        "fonts/manrope-cyrillic-500-normal.woff2" => include_bytes!("../../static/fonts/manrope-cyrillic-500-normal.woff2"),
        "fonts/manrope-cyrillic-600-normal.woff2" => include_bytes!("../../static/fonts/manrope-cyrillic-600-normal.woff2"),
        "fonts/manrope-cyrillic-700-normal.woff2" => include_bytes!("../../static/fonts/manrope-cyrillic-700-normal.woff2"),
        "fonts/manrope-latin-400-normal.woff2" => include_bytes!("../../static/fonts/manrope-latin-400-normal.woff2"),
        "fonts/manrope-latin-500-normal.woff2" => include_bytes!("../../static/fonts/manrope-latin-500-normal.woff2"),
        "fonts/manrope-latin-600-normal.woff2" => include_bytes!("../../static/fonts/manrope-latin-600-normal.woff2"),
        "fonts/manrope-latin-700-normal.woff2" => include_bytes!("../../static/fonts/manrope-latin-700-normal.woff2"),
        "fonts/OFL-Manrope.txt" => include_bytes!("../../static/fonts/OFL-Manrope.txt"),
        _ => return None,
    })
}
