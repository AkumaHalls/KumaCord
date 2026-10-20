//! Chat text with clickable links. Only `http://`, `https://` and `www.` links are recognised,
//! and only http(s) URLs are ever handed to the OS, so a message can't launch local files. The one
//! exception is the signature's fixed `mailto:` link (`brand::OWNER_MAILTO`), allowed by exact match
//! only: a `mailto:` in a message is still plain text and never opened.

use eframe::egui;

#[derive(Debug, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    /// `shown` is the text as typed; `url` is what gets opened (`www.` gains `https://`).
    Link { shown: &'a str, url: String },
}

/// Split one line into plain text and links.
pub fn segments(line: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = find_link_start(rest) {
        let tail = &rest[start..];
        let end = tail.find(|c: char| c.is_whitespace() || c.is_control()).unwrap_or(tail.len());
        let shown = trim_trailing_punct(&tail[..end]);
        let url = if shown.starts_with("www.") { format!("https://{shown}") } else { shown.to_string() };
        if !is_openable(&url) {
            // A bare scheme like "https://" with nothing after it: keep it as text.
            out.push(Segment::Text(&rest[..start + shown.len().max(1)]));
            rest = &rest[start + shown.len().max(1)..];
            continue;
        }
        if start > 0 {
            out.push(Segment::Text(&rest[..start]));
        }
        out.push(Segment::Link { shown, url });
        rest = &rest[start + shown.len()..];
    }
    if !rest.is_empty() {
        out.push(Segment::Text(rest));
    }
    out
}

fn find_link_start(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    ["https://", "http://", "www."]
        .iter()
        .filter_map(|p| {
            // `www.` only counts at a word start, so "awww.ok" is not a link.
            lower.match_indices(p).map(|(i, _)| i).find(|&i| {
                *p != "www." || i == 0 || !lower.as_bytes()[i - 1].is_ascii_alphanumeric()
            })
        })
        .min()
}

/// Sentence punctuation right after a link isn't part of it: "veja https://x.com." -> "https://x.com".
/// A closing bracket is kept when the link itself opened one (Wikipedia-style URLs).
fn trim_trailing_punct(s: &str) -> &str {
    let mut s = s;
    loop {
        let Some(last) = s.chars().last() else { return s };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' => true,
            ')' => s.matches('(').count() < s.matches(')').count(),
            ']' => s.matches('[').count() < s.matches(']').count(),
            _ => false,
        };
        if !strip {
            return s;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

fn is_openable(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let host_part = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"));
    matches!(host_part, Some(h) if !h.is_empty() && !h.starts_with('/'))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"')
}

/// Render a chat message: plain text and clickable links, wrapping like a normal label.
pub fn message(ui: &mut egui::Ui, content: &str) {
    if find_link_start(content).is_none() {
        ui.label(content);
        return;
    }
    for line in content.lines() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for seg in segments(line) {
                match seg {
                    Segment::Text(t) => {
                        ui.label(t);
                    }
                    Segment::Link { shown, url } => {
                        let resp = ui.link(shown).on_hover_text(&url);
                        if resp.clicked() {
                            open_url(&url);
                        }
                    }
                }
            }
        });
    }
}

/// The distinct links of a message, in order, at most `max` (the preview cards under it).
pub fn urls(content: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in content.lines() {
        for seg in segments(line) {
            if let Segment::Link { url, .. } = seg {
                if !out.contains(&url) {
                    out.push(url);
                    if out.len() == max {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// What [`open_url`] hands to the OS: an http(s) URL, or exactly the signature's mailto: link.
fn may_open(url: &str) -> bool {
    is_openable(url) || url == crate::brand::OWNER_MAILTO
}

/// Open an http(s) URL in the default browser (or the signature's address in the mail app).
pub fn open_url(url: &str) {
    if !may_open(url) {
        tracing::warn!("refusing to open non-http link");
        return;
    }
    if vox_client_core::testmode::background() {
        // Autotests never open a browser (it would take the user's foreground).
        tracing::info!(url, "background test mode: link not opened");
        return;
    }
    #[cfg(target_os = "macos")]
    crate::platform::open(std::ffi::OsStr::new(url));
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (op, target) = (wide("open"), wide(url));
        // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call.
        let r = unsafe {
            ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
        };
        // ShellExecuteW returns a value > 32 on success.
        if r as isize <= 32 {
            tracing::warn!(code = r as isize, "could not open link in browser");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(s: &str) -> Vec<String> {
        segments(s)
            .into_iter()
            .filter_map(|s| match s {
                Segment::Link { url, .. } => Some(url),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn finds_links_in_text() {
        assert_eq!(
            segments("olha https://www.youtube.com/watch?v=daMlcCXzDGI&t=2259s legal"),
            vec![
                Segment::Text("olha "),
                Segment::Link {
                    shown: "https://www.youtube.com/watch?v=daMlcCXzDGI&t=2259s",
                    url: "https://www.youtube.com/watch?v=daMlcCXzDGI&t=2259s".into()
                },
                Segment::Text(" legal"),
            ]
        );
        assert_eq!(links("a http://a.com e https://b.com"), ["http://a.com", "https://b.com"]);
    }

    #[test]
    fn www_gets_https_and_needs_word_start() {
        assert_eq!(links("vai em www.google.com"), ["https://www.google.com"]);
        assert!(links("awww.que fofo").is_empty());
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_link() {
        assert_eq!(links("veja https://x.com."), ["https://x.com"]);
        assert_eq!(links("(https://x.com/a)"), ["https://x.com/a"]);
        assert_eq!(links("https://en.wikipedia.org/wiki/Rust_(language)"), ["https://en.wikipedia.org/wiki/Rust_(language)"]);
        assert_eq!(links("https://x.com/?q=1!"), ["https://x.com/?q=1"]);
    }

    #[test]
    fn only_http_urls_are_openable() {
        assert!(links("https://").is_empty());
        assert!(links("file:///C:/Windows/System32/calc.exe").is_empty());
        assert!(links("javascript:alert(1)").is_empty());
        assert!(!is_openable("https:///etc"));
        assert!(!is_openable("C:\\Windows\\notepad.exe"));
        assert!(is_openable("https://www.instagram.com/reel/DeRslJageXF/?dlrf=ZmlubXlpYjdzYmxm"));
    }

    #[test]
    fn only_the_signature_mailto_may_be_opened() {
        assert!(may_open("mailto:cezar.callado@gmail.com"));
        assert!(may_open("https://example.com/a"));
        for other in [
            "mailto:someone@example.com",
            "mailto:cezar.callado@gmail.com?subject=x",
            "MAILTO:cezar.callado@gmail.com",
            " mailto:cezar.callado@gmail.com",
            "mailto:cezar.callado@gmail.com\"",
            "file:///C:/Windows/notepad.exe",
        ] {
            assert!(!may_open(other), "{other}");
        }
        // In a message, a mailto: is never a link.
        assert!(links("escreve pra mailto:cezar.callado@gmail.com").is_empty());
    }

    #[test]
    fn urls_are_distinct_ordered_and_capped() {
        assert_eq!(urls("a https://a.com b www.b.com c https://a.com", 5), ["https://a.com", "https://www.b.com"]);
        assert_eq!(urls("https://a.com
https://b.com
https://c.com", 2), ["https://a.com", "https://b.com"]);
        assert!(urls("sem link, só file:///C:/x.exe", 2).is_empty());
        assert!(urls("", 2).is_empty());
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(segments("alooooo"), vec![Segment::Text("alooooo")]);
        assert_eq!(segments(""), vec![]);
    }
}
