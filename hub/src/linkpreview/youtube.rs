//! YouTube special case: the oEmbed endpoint gives a dependable title, channel and thumbnail.

use reqwest::Url;

/// The video id of a youtube.com / youtu.be video link (watch, shorts, live, embed, youtu.be).
pub fn video_id(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let mut segs = url.path_segments()?.filter(|s| !s.is_empty());
    let id = match host {
        "youtu.be" => segs.next()?.to_string(),
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com" => match segs.next()? {
            "watch" => url.query_pairs().find(|(k, _)| k == "v")?.1.into_owned(),
            "shorts" | "live" | "embed" | "v" => segs.next()?.to_string(),
            _ => return None,
        },
        _ => return None,
    };
    let ok = (6..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(id)
}

/// `<endpoint>?url=<canonical watch url>&format=json`
pub fn oembed_url(endpoint: &str, id: &str) -> Option<Url> {
    Url::parse_with_params(endpoint, [("url", format!("https://www.youtube.com/watch?v={id}")), ("format", "json".to_string())])
        .ok()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Oembed {
    pub title: String,
    pub author: String,
    pub thumbnail_url: Option<String>,
}

pub fn parse_oembed(body: &[u8]) -> Option<Oembed> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let text = |k: &str, max: usize| v.get(k).and_then(|x| x.as_str()).map(|s| super::html::clean_text(s, max)).unwrap_or_default();
    let title = text("title", super::html::MAX_TITLE);
    if title.is_empty() {
        return None;
    }
    Some(Oembed {
        title,
        author: text("author_name", super::html::MAX_SITE_NAME),
        thumbnail_url: v.get("thumbnail_url").and_then(|x| x.as_str()).map(str::to_string),
    })
}

/// Thumbnails to try, best first: the clean `maxresdefault`, then what the oEmbed itself named.
pub fn thumbnail_candidates(named: &Url) -> Vec<Url> {
    let mut out = Vec::new();
    if let Some((dir, file)) = named.as_str().rsplit_once('/') {
        if file.split('?').next() != Some("maxresdefault.jpg") {
            if let Ok(u) = Url::parse(&format!("{dir}/maxresdefault.jpg")) {
                out.push(u);
            }
        }
    }
    out.push(named.clone());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> Option<String> {
        video_id(&Url::parse(s).unwrap())
    }

    #[test]
    fn finds_video_ids() {
        assert_eq!(id("https://www.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(id("https://www.youtube.com/watch?list=PL1&v=dQw4w9WgXcQ&t=43s").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(id("https://youtu.be/dQw4w9WgXcQ?si=abc").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(id("https://m.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(id("https://www.youtube.com/shorts/abcDEF12345").as_deref(), Some("abcDEF12345"));
        assert_eq!(id("https://www.youtube.com/live/abcDEF12345?feature=share").as_deref(), Some("abcDEF12345"));
        assert_eq!(id("https://music.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
    }

    #[test]
    fn other_links_are_not_videos() {
        assert_eq!(id("https://www.youtube.com/@canal"), None);
        assert_eq!(id("https://www.youtube.com/playlist?list=PL123456"), None);
        assert_eq!(id("https://www.youtube.com/watch"), None);
        assert_eq!(id("https://www.youtube.com/watch?v=x"), None);
        assert_eq!(id("https://evil.example/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(id("https://notyoutube.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(id("https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(id("https://www.youtube.com/watch?v=../../etc/passwd"), None);
    }

    #[test]
    fn oembed_request_and_answer() {
        let u = oembed_url("https://www.youtube.com/oembed", "dQw4w9WgXcQ").unwrap();
        assert_eq!(u.host_str(), Some("www.youtube.com"));
        assert_eq!(u.path(), "/oembed");
        let q: Vec<_> = u.query_pairs().into_owned().collect();
        assert!(q.contains(&("url".into(), "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into())));
        assert!(q.contains(&("format".into(), "json".into())));

        let body = br#"{"title":"Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)","author_name":"Rick Astley","author_url":"https://www.youtube.com/@RickAstleyYT","type":"video","height":113,"width":200,"version":"1.0","provider_name":"YouTube","provider_url":"https://www.youtube.com/","thumbnail_height":360,"thumbnail_width":480,"thumbnail_url":"https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg","html":"<iframe></iframe>"}"#;
        let o = parse_oembed(body).unwrap();
        assert_eq!(o.title, "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)");
        assert_eq!(o.author, "Rick Astley");
        assert_eq!(o.thumbnail_url.as_deref(), Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"));
        assert_eq!(parse_oembed(b"Unauthorized"), None);
        assert_eq!(parse_oembed(br#"{"title":"  "}"#), None);
    }

    #[test]
    fn thumbnail_candidates_prefer_maxres() {
        let named = Url::parse("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg").unwrap();
        let c = thumbnail_candidates(&named);
        assert_eq!(c[0].as_str(), "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg");
        assert_eq!(c[1], named);
        let max = Url::parse("https://i.ytimg.com/vi/x/maxresdefault.jpg").unwrap();
        assert_eq!(thumbnail_candidates(&max), vec![max]);
    }
}
