//! Profile calls of [`HubClient`] (the contract, limits and validation are in `vox_proto::profile`).

use anyhow::{Context, Result};
use vox_proto::{CustomStatus, ModClearProfileRequest, OkResponse, Presence, PresenceResponse, PresenceUpdate, ProfileUpdate, StatusResponse, StatusUpdate, UserId, UserProfile};

use super::*;

/// How the hub answered `GET /api/users/{id}/profile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileReply {
    Ready(Box<UserProfile>),
    /// `404`: a hub older than profiles has no such route (or the person does not exist). The app
    /// hides the profile features and falls back to what it already knows.
    Unsupported,
    Failed(String),
}

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
    /// `GET /api/users/{id}/profile`
    pub async fn profile(&self, user: UserId) -> ProfileReply {
        let req = match self.url(&format!("api/users/{user}/profile")).and_then(|u| self.auth(self.http.get(u))) {
            Ok(r) => r,
            Err(e) => return ProfileReply::Failed(format!("{e:#}")),
        };
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return ProfileReply::Failed(format!("profile request: {e}")),
        };
        match resp.status().as_u16() {
            404 | 405 => ProfileReply::Unsupported,
            s if (200..300).contains(&s) => match resp.json::<UserProfile>().await {
                Ok(p) => ProfileReply::Ready(Box::new(p)),
                Err(e) => ProfileReply::Failed(format!("profile: invalid response body: {e}")),
            },
            s => ProfileReply::Failed(format!("profile: HTTP {s}")),
        }
    }

    async fn put_json<B: Serialize, T: DeserializeOwned>(&self, what: &str, path: &str, body: &B) -> Result<T> {
        let resp = self.auth(self.http.put(self.url(path)?))?.json(body).send().await.with_context(|| format!("{what} request"))?;
        let resp = checked(what, resp).await?;
        resp.json::<T>().await.with_context(|| format!("{what}: invalid response body"))
    }

    /// `PUT /api/me/profile`: my display name, bio and colour (`None` leaves a field, `Some("")` clears it).
    pub async fn update_profile(&self, update: &ProfileUpdate) -> Result<UserProfile> {
        self.put_json("update profile", "api/me/profile", update).await
    }

    /// `PUT /api/me/presence`: what I am afterwards (an automatic change may have been ignored).
    pub async fn set_presence(&self, presence: Presence, auto: bool) -> Result<Presence> {
        let r: PresenceResponse = self.put_json("set presence", "api/me/presence", &PresenceUpdate { presence, auto }).await?;
        Ok(r.presence)
    }

    /// `PUT /api/me/status`: an empty emoji and text clears it.
    pub async fn set_status(&self, update: &StatusUpdate) -> Result<Option<CustomStatus>> {
        let r: StatusResponse = self.put_json("set status", "api/me/status", update).await?;
        Ok(r.status)
    }

    /// `POST /api/mod/users/{id}/clear-profile`
    pub async fn mod_clear_profile(&self, user: UserId, what: ModClearProfileRequest) -> Result<()> {
        self.post::<_, OkResponse>("clear profile", &format!("api/mod/users/{user}/clear-profile"), &what).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-connection-per-request HTTP server that answers every request with `raw`.
    async fn client(status: &str, body: &str) -> HubClient {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let raw = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes();
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
        let c = HubClient::new(&format!("http://{addr}")).unwrap();
        c.set_token(Some("t".into()));
        c
    }

    #[tokio::test]
    async fn an_older_hub_means_unsupported_not_an_error() {
        let old = client("404 Not Found", "").await;
        assert_eq!(old.profile(3).await, ProfileReply::Unsupported);
        let broken = client("500 Internal Server Error", "x").await;
        assert!(matches!(broken.profile(3).await, ProfileReply::Failed(_)));
        let junk = client("200 OK", "not json").await;
        assert!(matches!(junk.profile(3).await, ProfileReply::Failed(_)));
        let anon = HubClient::new("http://127.0.0.1:1").unwrap();
        assert!(matches!(anon.profile(3).await, ProfileReply::Failed(_)));
    }

    #[tokio::test]
    async fn a_profile_and_the_edit_calls_round_trip() {
        let body = r##"{"user":{"id":3,"username":"ana","display_name":"Ana"},"bio":"oi","accent":"#112233","presence":"dnd","online":true,"member_since_ms":5}"##;
        let hub = client("200 OK", body).await;
        let ProfileReply::Ready(p) = hub.profile(3).await else { panic!() };
        assert_eq!((p.user.shown_name(), p.bio.as_str(), p.accent.as_deref(), p.presence, p.online), ("Ana", "oi", Some("#112233"), Presence::Dnd, true));
        let edited = hub.update_profile(&ProfileUpdate { bio: Some("novo".into()), ..Default::default() }).await.unwrap();
        assert_eq!(edited.user.id, 3);
        let presence = client("200 OK", r#"{"presence":"away"}"#).await;
        assert_eq!(presence.set_presence(Presence::Away, true).await.unwrap(), Presence::Away);
        let status = client("200 OK", r#"{"status":{"text":"x"}}"#).await;
        assert_eq!(status.set_status(&StatusUpdate { text: "x".into(), ..Default::default() }).await.unwrap().map(|s| s.text), Some("x".into()));
    }

    #[tokio::test]
    async fn refusals_carry_the_http_status() {
        let hub = client("400 Bad Request", r#"{"error":"display name must be 1-32 characters"}"#).await;
        let err = hub.update_profile(&ProfileUpdate::default()).await.unwrap_err();
        let st = err.downcast_ref::<HttpStatusError>().expect("a status error");
        assert_eq!((st.status, st.message.as_str()), (400, "display name must be 1-32 characters"));
        let limited = client("429 Too Many Requests", r#"{"error":"too many profile changes, wait 20 s"}"#).await;
        let err = limited.set_status(&StatusUpdate::default()).await.unwrap_err();
        assert_eq!(err.downcast_ref::<HttpStatusError>().map(|s| s.status), Some(429));
    }
}
