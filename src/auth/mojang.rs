use std::time::Duration;

use md5::{Digest, Md5};
use serde::Serialize;
use url::Url;
use uuid::Uuid;

use crate::auth::azauth::is_loopback_host;
use crate::auth::{parse_auth_json_or_http_error, AccountMeta, AuthAccount};
use crate::error::{CoreError, CoreResult};

/// Default Mojang Yggdrasil authentication server URL.
pub const DEFAULT_MOJANG_AUTH_URL: &str = "https://authserver.mojang.com";

/// Computes Minecraft's exact deterministic offline UUIDv3 (`OfflinePlayer:<username>`),
/// matching `java.util.UUID.nameUUIDFromBytes(("OfflinePlayer:" + username).getBytes(UTF_8))`.
pub fn offline_uuid(username: &str) -> Uuid {
    let mut hasher = Md5::new();
    hasher.update(b"OfflinePlayer:");
    hasher.update(username.as_bytes());
    let digest: [u8; 16] = hasher.finalize().into();
    uuid::Builder::from_md5_bytes(digest).into_uuid()
}

/// Creates a deterministic offline [`AuthAccount`] using UUIDv3 (`OfflinePlayer:<username>`).
pub fn offline_login(username: &str) -> AuthAccount {
    let clean_name = username.trim();
    let uuid_hex = offline_uuid(clean_name).simple().to_string();

    AuthAccount {
        access_token: uuid_hex.clone(),
        client_token: uuid_hex.clone(),
        refresh_token: None,
        expires_at: None,
        uuid: uuid_hex,
        name: clean_name.to_string(),
        user_properties: "{}".to_string(),
        meta: AccountMeta {
            online: false,
            account_type: "Mojang".to_string(),
            offline: Some(true),
            access_token_expires_in: None,
            demo: Some(false),
        },
        user_info: None,
        profile: None,
        xbox: None,
    }
}

/// Client for Yggdrasil-compatible authentication servers (legacy Mojang or custom Yggdrasil).
#[derive(Debug, Clone)]
pub struct YggdrasilClient {
    base_url: String,
    http: reqwest::Client,
}

impl Default for YggdrasilClient {
    fn default() -> Self {
        Self::new(None)
    }
}

impl YggdrasilClient {
    /// Creates a new [`YggdrasilClient`] (defaults to `https://authserver.mojang.com`), reusing
    /// [`crate::shared_http_client()`] (with `.no_proxy()` bypass for local test servers).
    pub fn new(base_url: Option<&str>) -> Self {
        let resolved = resolve_yggdrasil_base(base_url);
        let is_local = Url::parse(&resolved)
            .ok()
            .map(|u| is_loopback_host(u.host_str()))
            .unwrap_or(false);

        let http = if is_local {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(15))
                .tcp_nodelay(true)
                .user_agent(concat!("minecraft-rust-core/", env!("CARGO_PKG_VERSION")))
                .no_proxy()
                .build()
                .unwrap_or_else(|_| crate::shared_http_client())
        } else {
            crate::shared_http_client()
        };

        Self {
            base_url: resolved,
            http,
        }
    }

    /// Creates a new [`YggdrasilClient`] with a caller-provided [`reqwest::Client`].
    pub fn with_client(base_url: Option<&str>, http: reqwest::Client) -> Self {
        Self {
            base_url: resolve_yggdrasil_base(base_url),
            http,
        }
    }

    /// Authenticates via Yggdrasil `POST /authenticate`, or returns [`offline_login`] if `password` is `None`/empty.
    pub async fn login(&self, username: &str, password: Option<&str>) -> CoreResult<AuthAccount> {
        let Some(pw) = password.filter(|p| !p.is_empty()) else {
            return Ok(offline_login(username));
        };

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct AuthenticateReq<'a> {
            agent: AgentInfo,
            username: &'a str,
            password: &'a str,
            client_token: String,
            request_user: bool,
        }

        #[derive(Serialize)]
        struct AgentInfo {
            name: &'static str,
            version: u8,
        }

        let client_token = Uuid::new_v4().simple().to_string();
        let resp = self
            .http
            .post(format!("{}/authenticate", self.base_url))
            .json(&AuthenticateReq {
                agent: AgentInfo {
                    name: "Minecraft",
                    version: 1,
                },
                username: username.trim(),
                password: pw,
                client_token,
                request_user: true,
            })
            .send()
            .await?;

        let status = resp.status();
        let body_text = resp.text().await.map_err(CoreError::Http)?;
        let raw = parse_auth_json_or_http_error("Yggdrasil authentication", status, &body_text)?;

        if !status.is_success() || raw.get("error").is_some() {
            let reason = raw
                .get("error")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("http_{}", status.as_u16()));
            let message = raw
                .get("errorMessage")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "Yggdrasil authentication failed (HTTP {})",
                        status.as_u16()
                    )
                });
            return Err(CoreError::Auth { reason, message });
        }

        parse_yggdrasil_account(&raw)
    }

    /// Refreshes an existing Yggdrasil session via `POST /refresh`.
    pub async fn refresh(&self, account: &AuthAccount) -> CoreResult<AuthAccount> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct RefreshReq<'a> {
            access_token: &'a str,
            client_token: &'a str,
            request_user: bool,
        }

        let resp = self
            .http
            .post(format!("{}/refresh", self.base_url))
            .json(&RefreshReq {
                access_token: &account.access_token,
                client_token: &account.client_token,
                request_user: true,
            })
            .send()
            .await?;

        let status = resp.status();
        let body_text = resp.text().await.map_err(CoreError::Http)?;
        let raw = parse_auth_json_or_http_error("Yggdrasil refresh", status, &body_text)?;

        if !status.is_success() || raw.get("error").is_some() {
            let reason = raw
                .get("error")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("http_{}", status.as_u16()));
            let message = raw
                .get("errorMessage")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!("Yggdrasil token refresh failed (HTTP {})", status.as_u16())
                });
            return Err(CoreError::Auth { reason, message });
        }

        parse_yggdrasil_account(&raw)
    }

    /// Validates a Yggdrasil token via `POST /validate` (expects HTTP `204 No Content`).
    pub async fn validate(&self, account: &AuthAccount) -> CoreResult<bool> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct ValidateReq<'a> {
            access_token: &'a str,
            client_token: &'a str,
        }

        let resp = self
            .http
            .post(format!("{}/validate", self.base_url))
            .json(&ValidateReq {
                access_token: &account.access_token,
                client_token: &account.client_token,
            })
            .send()
            .await?;

        Ok(resp.status().as_u16() == 204 || resp.status().is_success())
    }

    /// Invalidates a Yggdrasil token via `POST /invalidate`.
    pub async fn signout(&self, account: &AuthAccount) -> CoreResult<bool> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct InvalidateReq<'a> {
            access_token: &'a str,
            client_token: &'a str,
        }

        let resp = self
            .http
            .post(format!("{}/invalidate", self.base_url))
            .json(&InvalidateReq {
                access_token: &account.access_token,
                client_token: &account.client_token,
            })
            .send()
            .await?;

        if resp.status().as_u16() == 204 {
            return Ok(true);
        }
        let body = resp.text().await.unwrap_or_default();
        Ok(body.trim().is_empty())
    }
}

fn resolve_yggdrasil_base(base_url: Option<&str>) -> String {
    base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_MOJANG_AUTH_URL)
        .trim_end_matches('/')
        .to_string()
}

fn parse_yggdrasil_account(raw: &serde_json::Value) -> CoreResult<AuthAccount> {
    let access_token = raw
        .get("accessToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Auth {
            reason: "missing_access_token".to_string(),
            message: "Yggdrasil response missing accessToken".to_string(),
        })?
        .to_string();

    let client_token = raw
        .get("clientToken")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let selected = raw.get("selectedProfile").ok_or_else(|| CoreError::Auth {
        reason: "no_profile".to_string(),
        message: "Yggdrasil account has no Minecraft profile".to_string(),
    })?;

    let uuid = selected
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let name = selected
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    Ok(AuthAccount {
        access_token,
        client_token,
        refresh_token: None,
        expires_at: None,
        uuid,
        name,
        user_properties: "{}".to_string(),
        meta: AccountMeta {
            online: true,
            account_type: "Mojang".to_string(),
            offline: Some(false),
            access_token_expires_in: None,
            demo: Some(false),
        },
        user_info: None,
        profile: None,
        xbox: None,
    })
}
