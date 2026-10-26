//! Invite calls of [`HubClient`] (v0.8; the contract and the `vox1-` code are in
//! `vox_proto::invite`). Joining with an invite is a login: [`LoginRequest::invite`].

use anyhow::{Context, Result};
use vox_proto::{Invite, InviteCreateRequest, InviteCreated, InviteId, InviteRedeemRequest, InviteRedeemResponse, OkResponse};

use super::*;

impl HubClient {
    /// `POST /api/invites` (Admin+).
    pub async fn create_invite(&self, req: &InviteCreateRequest) -> Result<InviteCreated> {
        self.post("create invite", "api/invites", req).await
    }

    /// `GET /api/invites` (Admin+), newest first.
    pub async fn invites(&self) -> Result<Vec<Invite>> {
        self.get("invites", self.url("api/invites")?).await
    }

    /// `POST /api/invites/{id}/revoke` (Admin+).
    pub async fn revoke_invite(&self, id: InviteId) -> Result<()> {
        self.post::<_, OkResponse>("revoke invite", &format!("api/invites/{id}/revoke"), &serde_json::json!({})).await.map(|_| ())
    }

    /// `POST /api/invites/redeem`: does this token work here? (No login needed, no use taken.)
    pub async fn redeem_invite(&self, token: &str) -> Result<InviteRedeemResponse> {
        let resp = self
            .http
            .post(self.url("api/invites/redeem")?)
            .json(&InviteRedeemRequest { token: token.to_string() })
            .send()
            .await
            .context("redeem invite request")?;
        Self::parse("redeem invite", resp).await
    }
}
