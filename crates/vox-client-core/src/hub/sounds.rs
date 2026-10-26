//! Soundboard calls of [`HubClient`] (the contract and limits are in `vox_proto::sound`).

use std::time::Duration;

use anyhow::{Context, Result};
use vox_proto::{OkResponse, Sound, SoundId, SoundRenameRequest};

use super::*;

/// A stored clip is a canonical WAV of at most 1 MiB; a bigger answer is not from a hub that
/// follows the contract.
pub const MAX_SOUND_DOWNLOAD_BYTES: usize = vox_proto::SOUND_MAX_UPLOAD_BYTES;

/// How the hub answered `GET /api/sounds`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundsReply {
    Ready(Vec<Sound>),
    /// `404`: a hub older than the soundboard has no such route. The app hides the feature.
    Unsupported,
    Failed(String),
}

/// A failed soundboard call, with the HTTP status so the app can say what went wrong
/// (`403` not yours, `409` full, `413` too big, `429` slow down...). Find it with
/// `error.downcast_ref::<HttpStatusError>()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpStatusError {
    pub status: u16,
    /// The hub's own (English) message.
    pub message: String,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.message)
    }
}

impl std::error::Error for HttpStatusError {}

async fn checked(what: &str, resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ApiError>(&body).map(|e| e.error).unwrap_or(body);
    Err(anyhow::Error::new(HttpStatusError { status: status.as_u16(), message }).context(format!("{what} failed")))
}

impl HubClient {
    /// `GET /api/sounds`
    pub async fn sounds(&self) -> SoundsReply {
        let req = match self.url("api/sounds").and_then(|u| self.auth(self.http.get(u))) {
            Ok(r) => r,
            Err(e) => return SoundsReply::Failed(format!("{e:#}")),
        };
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return SoundsReply::Failed(format!("sounds request: {e}")),
        };
        match resp.status().as_u16() {
            404 | 405 => SoundsReply::Unsupported,
            s if (200..300).contains(&s) => match resp.json::<Vec<Sound>>().await {
                Ok(list) => SoundsReply::Ready(list),
                Err(e) => SoundsReply::Failed(format!("sounds: invalid response body: {e}")),
            },
            s => SoundsReply::Failed(format!("sounds: HTTP {s}")),
        }
    }

    /// `GET /api/sounds/{id}/audio?v=<hash>`: the stored WAV.
    pub async fn sound_audio(&self, id: SoundId, hash: &str) -> Result<Vec<u8>> {
        let mut url = self.url(&format!("api/sounds/{id}/audio"))?;
        url.query_pairs_mut().append_pair("v", hash);
        let resp = self.auth(self.http.get(url))?.timeout(Duration::from_secs(30)).send().await.context("sound download request")?;
        let resp = checked("sound download", resp).await?;
        if resp.content_length().is_some_and(|n| n > MAX_SOUND_DOWNLOAD_BYTES as u64) {
            anyhow::bail!("sound download: the sound is larger than any sound can be");
        }
        let bytes = resp.bytes().await.context("sound download body")?;
        anyhow::ensure!(bytes.len() <= MAX_SOUND_DOWNLOAD_BYTES, "sound download: the sound is larger than any sound can be");
        Ok(bytes.to_vec())
    }

    /// `POST /api/sounds?name=&emoji=`: `wav` is the output of `soundboard::prepare`.
    pub async fn upload_sound(&self, name: &str, emoji: &str, wav: Vec<u8>) -> Result<Sound> {
        let mut url = self.url("api/sounds")?;
        url.query_pairs_mut().append_pair("name", name).append_pair("emoji", emoji);
        let req = self.auth(self.http.post(url))?.timeout(Duration::from_secs(60)).header("content-type", "audio/wav").body(wav);
        let resp = req.send().await.context("sound upload request")?;
        let resp = checked("sound upload", resp).await?;
        resp.json::<Sound>().await.context("sound upload: invalid response body")
    }

    /// `POST /api/sounds/{id}/rename`; `emoji: None` keeps the emoji.
    pub async fn rename_sound(&self, id: SoundId, name: &str, emoji: Option<&str>) -> Result<Sound> {
        let body = SoundRenameRequest { name: name.to_string(), emoji: emoji.map(str::to_string) };
        let resp = self.auth(self.http.post(self.url(&format!("api/sounds/{id}/rename"))?))?.json(&body).send().await.context("sound rename request")?;
        checked("sound rename", resp).await?.json::<Sound>().await.context("sound rename: invalid response body")
    }

    /// `POST /api/sounds/{id}/delete`
    pub async fn delete_sound(&self, id: SoundId) -> Result<()> {
        let resp = self.auth(self.http.post(self.url(&format!("api/sounds/{id}/delete"))?))?.json(&serde_json::json!({})).send().await.context("sound delete request")?;
        checked("sound delete", resp).await?.json::<OkResponse>().await.map(|_| ()).context("sound delete: invalid response body")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One answer per connection, whatever the request.
    async fn serve(raw: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else { return };
                let raw = raw.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(&raw).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn http(status: &str, ctype: &str, body: &[u8]) -> Vec<u8> {
        let mut v = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        v.extend(body);
        v
    }

    async fn client(raw: Vec<u8>) -> HubClient {
        let c = HubClient::new(&serve(raw).await).unwrap();
        c.set_token(Some("t".into()));
        c
    }

    #[tokio::test]
    async fn an_old_hub_means_no_soundboard_and_errors_keep_their_status() {
        // 404: the feature is hidden, quietly.
        assert_eq!(client(http("404 Not Found", "text/plain", b"")).await.sounds().await, SoundsReply::Unsupported);
        assert!(matches!(client(http("500 Internal Server Error", "text/plain", b"x")).await.sounds().await, SoundsReply::Failed(_)));
        assert!(matches!(client(http("200 OK", "application/json", b"nope")).await.sounds().await, SoundsReply::Failed(_)));
        assert_eq!(client(http("200 OK", "application/json", b"[]")).await.sounds().await, SoundsReply::Ready(vec![]));
        // Not logged in: a failure, nothing sent.
        assert!(matches!(HubClient::new("http://127.0.0.1:1").unwrap().sounds().await, SoundsReply::Failed(_)));

        // Refusals carry the hub's status and message for the app to translate.
        let full = client(http("409 Conflict", "application/json", br#"{"error":"the soundboard is full"}"#)).await;
        let err = full.upload_sound("a", "", vec![1]).await.unwrap_err();
        let st = err.downcast_ref::<HttpStatusError>().expect("typed error");
        assert_eq!((st.status, st.message.as_str()), (409, "the soundboard is full"));
        let err = client(http("403 Forbidden", "application/json", br#"{"error":"nope"}"#)).await.delete_sound(3).await.unwrap_err();
        assert_eq!(err.downcast_ref::<HttpStatusError>().unwrap().status, 403);
        let ok = client(http("200 OK", "application/json", br#"{"ok":true}"#)).await;
        ok.delete_sound(3).await.unwrap();
    }

    #[tokio::test]
    async fn downloads_are_size_capped() {
        let small = client(http("200 OK", "audio/wav", &[1, 2, 3])).await;
        assert_eq!(small.sound_audio(1, "h").await.unwrap(), vec![1, 2, 3]);
        let huge = client(http("200 OK", "audio/wav", &vec![0u8; MAX_SOUND_DOWNLOAD_BYTES + 1])).await;
        assert!(huge.sound_audio(1, "h").await.is_err());
        let missing = client(http("404 Not Found", "application/json", br#"{"error":"sound not found"}"#)).await;
        assert_eq!(missing.sound_audio(1, "h").await.unwrap_err().downcast_ref::<HttpStatusError>().unwrap().status, 404);
    }
}
