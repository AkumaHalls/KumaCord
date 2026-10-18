//! Local LiveKit token minting for "direct" mode (testing without the hub).
//!
//! In production the hub mints tokens (`POST /api/voice/{id}/join`). Direct mode exists so the
//! client can be exercised against a bare `livekit-server --dev` while the hub is not ready.

use std::time::Duration;

use anyhow::{Context, Result};
use livekit_api::access_token::{AccessToken, VideoGrants};

/// Mint a room-join token signed with `api_secret`.
pub fn mint_token(
    api_key: &str,
    api_secret: &str,
    room: &str,
    identity: &str,
    name: Option<&str>,
    ttl: Duration,
) -> Result<String> {
    let grants = VideoGrants {
        room_join: true,
        room: room.to_string(),
        can_publish: Some(true),
        can_subscribe: Some(true),
        can_publish_data: Some(true),
        ..Default::default()
    };
    let mut token = AccessToken::with_api_key(api_key, api_secret)
        .with_identity(identity)
        .with_grants(grants)
        .with_ttl(ttl);
    if let Some(name) = name {
        token = token.with_name(name);
    }
    token.to_jwt().context("failed to sign LiveKit token")
}

#[cfg(test)]
mod tests {
    use super::*;
    use livekit_api::access_token::TokenVerifier;

    #[test]
    fn minted_token_verifies_and_carries_grants() {
        let jwt = mint_token("devkey", "secret", "test", "alice", Some("Alice"), Duration::from_secs(600)).unwrap();
        let claims = TokenVerifier::with_api_key("devkey", "secret").verify(&jwt).unwrap();
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.name, "Alice");
        assert_eq!(claims.iss, "devkey");
        assert!(claims.video.room_join);
        assert_eq!(claims.video.room, "test");
        assert_eq!(claims.video.can_publish, Some(true));
        assert_eq!(claims.video.can_subscribe, Some(true));
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let jwt = mint_token("devkey", "secret", "test", "bob", None, Duration::from_secs(600)).unwrap();
        assert!(TokenVerifier::with_api_key("devkey", "other").verify(&jwt).is_err());
    }
}
