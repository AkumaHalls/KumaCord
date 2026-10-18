//! A tiny HTML head scanner: Open Graph, Twitter-card and plain meta tags plus `<title>`.
//!
//! No DOM, no scripting, no allocation proportional to anything but the head. It only walks tags
//! (skipping comments, `<script>` and `<style>` bodies) and stops at `</head>` or `<body`.

/// What the page says about itself. All text is already cleaned (entities decoded, whitespace
/// collapsed, control and bidi characters removed, length capped).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PageMeta {
    pub title: String,
    pub description: String,
    pub site_name: String,
    /// The raw (possibly relative) image URL; the caller resolves it against the page URL.
    pub image: Option<String>,
}

pub const MAX_TITLE: usize = 200;
pub const MAX_DESCRIPTION: usize = 400;
pub const MAX_SITE_NAME: usize = 60;

#[derive(Default)]
struct Raw {
    og_title: Option<String>,
    tw_title: Option<String>,
    html_title: Option<String>,
    og_desc: Option<String>,
    tw_desc: Option<String>,
    meta_desc: Option<String>,
    og_site: Option<String>,
    og_image: Option<String>,
    tw_image: Option<String>,
}

fn set(slot: &mut Option<String>, v: &str) {
    if slot.is_none() && !v.trim().is_empty() {
        *slot = Some(v.to_string());
    }
}

pub fn parse(html: &str) -> PageMeta {
    let b = html.as_bytes();
    let mut raw = Raw::default();
    let mut i = 0;
    while let Some(off) = b[i..].iter().position(|&c| c == b'<') {
        i += off;
        let rest = &b[i..];
        if rest.starts_with(b"<!--") {
            i = find(b, b"-->", i + 4).map_or(b.len(), |e| e + 3);
            continue;
        }
        match rest.get(1) {
            Some(b'!') | Some(b'?') => {
                i = find(b, b">", i + 2).map_or(b.len(), |e| e + 1);
                continue;
            }
            Some(b'/') => {
                let (name, end) = tag_name(b, i + 2);
                if name == "head" {
                    break;
                }
                i = find(b, b">", end).map_or(b.len(), |e| e + 1);
                continue;
            }
            Some(c) if c.is_ascii_alphabetic() => {}
            _ => {
                i += 1;
                continue;
            }
        }
        let (name, name_end) = tag_name(b, i + 1);
        let (attrs, after) = attributes(b, name_end);
        i = after;
        match name.as_str() {
            "body" => break,
            "meta" => meta(&attrs, &mut raw),
            "title" => {
                let close = find_ci(b, b"</title", i).unwrap_or(b.len());
                set(&mut raw.html_title, &String::from_utf8_lossy(&b[i..close]));
                i = close;
            }
            "script" | "style" | "noscript" | "template" => {
                let close_tag = format!("</{name}");
                i = find_ci(b, close_tag.as_bytes(), i).unwrap_or(b.len());
            }
            _ => {}
        }
    }
    finish(raw)
}

fn meta(attrs: &[(String, String)], raw: &mut Raw) {
    let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let (Some(content), Some(key)) = (get("content"), get("property").or_else(|| get("name")).or_else(|| get("itemprop")))
    else {
        return;
    };
    match key.trim().to_ascii_lowercase().as_str() {
        "og:title" => set(&mut raw.og_title, content),
        "twitter:title" => set(&mut raw.tw_title, content),
        "og:description" => set(&mut raw.og_desc, content),
        "twitter:description" => set(&mut raw.tw_desc, content),
        "description" => set(&mut raw.meta_desc, content),
        "og:site_name" => set(&mut raw.og_site, content),
        "og:image" | "og:image:url" | "og:image:secure_url" => set(&mut raw.og_image, content),
        "twitter:image" | "twitter:image:src" => set(&mut raw.tw_image, content),
        _ => {}
    }
}

fn finish(raw: Raw) -> PageMeta {
    let first = |opts: [Option<String>; 3], max: usize| {
        opts.into_iter().flatten().map(|s| clean_text(&s, max)).find(|s| !s.is_empty()).unwrap_or_default()
    };
    PageMeta {
        title: first([raw.og_title, raw.tw_title, raw.html_title], MAX_TITLE),
        description: first([raw.og_desc, raw.tw_desc, raw.meta_desc], MAX_DESCRIPTION),
        site_name: raw.og_site.map(|s| clean_text(&s, MAX_SITE_NAME)).unwrap_or_default(),
        image: raw
            .og_image
            .or(raw.tw_image)
            .map(|s| decode_entities(s.trim()))
            .filter(|s| !s.is_empty() && s.len() <= 2048 && !s.chars().any(char::is_control)),
    }
}

/// Lower-cased tag name starting at `from`, and the index just after it.
fn tag_name(b: &[u8], from: usize) -> (String, usize) {
    let mut e = from;
    while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'-' || b[e] == b':') {
        e += 1;
    }
    (String::from_utf8_lossy(&b[from..e]).to_ascii_lowercase(), e)
}

/// Attributes (lower-cased names, raw values) up to the closing `>`; returns the index after it.
fn attributes(b: &[u8], mut i: usize) -> (Vec<(String, String)>, usize) {
    let mut out = Vec::new();
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            return (out, b.len());
        }
        if b[i] == b'>' {
            return (out, i + 1);
        }
        let s = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let name = String::from_utf8_lossy(&b[s..i]).to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vs = i + 1;
                let ve = b[vs..].iter().position(|&c| c == q).map_or(b.len(), |p| vs + p);
                value = String::from_utf8_lossy(&b[vs..ve]).into_owned();
                i = (ve + 1).min(b.len());
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                value = String::from_utf8_lossy(&b[vs..i]).into_owned();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| from + p)
}

fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle)).map(|p| from + p)
}

/// Entities decoded, whitespace collapsed, control / bidi / zero-width characters dropped, at
/// most `max` characters (with an ellipsis when cut).
pub fn clean_text(s: &str, max: usize) -> String {
    let decoded = decode_entities(s);
    let mut out = String::with_capacity(decoded.len().min(max * 4));
    let mut pending_space = false;
    let mut count = 0;
    for c in decoded.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() || is_invisible(c) {
            continue;
        }
        if pending_space {
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count >= max {
            out.pop();
            let trimmed = out.trim_end().len();
            out.truncate(trimmed);
            out.push('…');
            return out;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// Bidi overrides and zero-width characters could be used to dress a title up as something else.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{00AD}')
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        // An entity is at most ~10 characters long; look for ';' nearby.
        let semi = rest.char_indices().take(12).find(|(_, c)| *c == ';').map(|(i, _)| i);
        let decoded = semi.and_then(|e| entity(&rest[1..e]).map(|c| (c, e)));
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code).filter(|c| *c != '\0');
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "euro" => '€',
        "aacute" => 'á',
        "agrave" => 'à',
        "acirc" => 'â',
        "atilde" => 'ã',
        "ccedil" => 'ç',
        "eacute" => 'é',
        "ecirc" => 'ê',
        "iacute" => 'í',
        "oacute" => 'ó',
        "ocirc" => 'ô',
        "otilde" => 'õ',
        "uacute" => 'ú',
        "Aacute" => 'Á',
        "Eacute" => 'É',
        "Ccedil" => 'Ç',
        _ => return None,
    })
}

/// Decode a page body to text using the charset in the `Content-Type` header, else a
/// `<meta charset>` in the first bytes. UTF-8 (the default) and the Latin-1 family are handled;
/// anything else is read as lossy UTF-8, which only costs some garbled accents.
pub fn decode_body(body: &[u8], content_type: &str) -> String {
    let charset = charset_of(content_type).or_else(|| {
        let head = &body[..body.len().min(2048)];
        let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
        let p = lower.find("charset")?;
        charset_of(&lower[p..])
    });
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252" | "us-ascii" | "iso-8859-15") => {
            body.iter().map(|&c| cp1252(c)).collect()
        }
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

fn charset_of(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let p = lower.find("charset")?;
    let v = lower[p + 7..].trim_start().strip_prefix('=')?.trim_start();
    let v = v.trim_start_matches(['"', '\'']);
    let end = v.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')).unwrap_or(v.len());
    (end > 0).then(|| v[..end].to_string())
}

fn cp1252(c: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’',
        '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match c {
        0x80..=0x9f => HIGH[(c - 0x80) as usize],
        _ => c as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEWS: &str = r#"<!DOCTYPE html>
<html lang="pt-BR" prefix="og: https://ogp.me/ns#"><head>
<meta charset="utf-8">
<title>Título da página &#8211; Portal de Notícias</title>
<!-- <meta property="og:title" content="commented out"> -->
<meta name="description" content="Descrição simples da página.">
<meta property="og:site_name" content="Portal de Notícias">
<meta property="og:type" content="article" />
<meta property="og:title" content="Governo anuncia &quot;novo plano&quot; para 2027">
<meta property="og:description" content="Medida deve atingir  milhões
  de brasileiros &amp; empresas.">
<meta property="og:image" content="https://cdn.portal.example/img/capa.jpg?w=1200&amp;h=630">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="Outro título (twitter)">
<script type="application/ld+json">{"headline": "<meta property='og:title' content='from script'>"}</script>
<style>body > meta { display:none }</style>
</head><body><h1>Oi</h1><meta property="og:title" content="from body"></body></html>"#;

    #[test]
    fn parses_a_news_page() {
        let m = parse(NEWS);
        assert_eq!(m.title, "Governo anuncia \"novo plano\" para 2027");
        assert_eq!(m.description, "Medida deve atingir milhões de brasileiros & empresas.");
        assert_eq!(m.site_name, "Portal de Notícias");
        assert_eq!(m.image.as_deref(), Some("https://cdn.portal.example/img/capa.jpg?w=1200&h=630"));
    }

    #[test]
    fn falls_back_to_twitter_then_title_tag() {
        let m = parse(r#"<head><title>Só o título</title><meta name="twitter:description" content="tw desc"><meta name="twitter:image" content="/a.png"></head>"#);
        assert_eq!(m.title, "Só o título");
        assert_eq!(m.description, "tw desc");
        assert_eq!(m.image.as_deref(), Some("/a.png"));
        let m = parse(r#"<meta name="twitter:title" content="tw"><title>t</title>"#);
        assert_eq!(m.title, "tw");
    }

    #[test]
    fn handles_odd_markup() {
        // Single quotes, unquoted values, reversed attribute order, upper case, no closing tags.
        let m = parse("<HTML><HEAD><META CONTENT='Meu Site' PROPERTY=og:title><meta content=desc name=description><TITLE>Ignored\n title");
        assert_eq!(m.title, "Meu Site");
        assert_eq!(m.description, "desc");
        // Garbage and truncated input never panic.
        for s in ["", "<", "<meta", "<meta property=\"og:title\" content=\"abc", "<!--", "<title>", "<<<>>>&&&;", "<meta content=>"] {
            let _ = parse(s);
        }
    }

    #[test]
    fn youtube_like_page() {
        let html = r#"<html><head><title>Rick Astley - Never Gonna Give You Up (Official Music Video) - YouTube</title>
<meta name="title" content="Rick Astley - Never Gonna Give You Up (Official Music Video)">
<meta property="og:site_name" content="YouTube">
<meta property="og:url" content="https://www.youtube.com/watch?v=dQw4w9WgXcQ">
<meta property="og:title" content="Rick Astley - Never Gonna Give You Up (Official Music Video)">
<meta property="og:image" content="https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg">
<meta property="og:description" content="The official video for “Never Gonna Give You Up” by Rick Astley">
</head>"#;
        let m = parse(html);
        assert_eq!(m.site_name, "YouTube");
        assert!(m.title.starts_with("Rick Astley"));
        assert!(m.description.contains("Never Gonna Give You Up"));
        assert_eq!(m.image.as_deref(), Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg"));
    }

    #[test]
    fn text_is_cleaned_and_capped() {
        assert_eq!(clean_text("  a \t b\n\nc  ", 50), "a b c");
        assert_eq!(clean_text("evil\u{202E}txt\u{200B}\u{0007}!", 50), "eviltxt!");
        let long = "palavra ".repeat(100);
        let c = clean_text(&long, 40);
        assert!(c.chars().count() <= 41 && c.ends_with('…'), "{c:?}");
        assert_eq!(clean_text("&lt;b&gt; &amp;amp; &#x41;&#66; &unknown; &", 50), "<b> &amp; AB &unknown; &");
    }

    #[test]
    fn bodies_are_decoded_by_charset() {
        let latin1 = b"<meta charset=\"iso-8859-1\"><title>Informa\xe7\xe3o</title>";
        assert!(decode_body(latin1, "text/html").contains("Informação"));
        assert!(decode_body(b"<title>Informa\xe7\xe3o</title>", "text/html; charset=ISO-8859-1").contains("Informação"));
        assert!(decode_body("<title>Informação</title>".as_bytes(), "text/html; charset=utf-8").contains("Informação"));
        assert!(decode_body("<title>Informação</title>".as_bytes(), "text/html").contains("Informação"));
    }

    #[test]
    fn relative_image_is_kept_raw() {
        assert_eq!(parse(r#"<meta property="og:image" content=" //cdn.example/x.png ">"#).image.as_deref(), Some("//cdn.example/x.png"));
        assert_eq!(parse(r#"<meta property="og:image" content="">"#).image, None);
    }
}
