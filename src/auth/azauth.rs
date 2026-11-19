use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::auth::{
    parse_auth_json_or_http_error, summarize_non_json_body, AccountMeta, AuthAccount,
    MinecraftProfile, RoleInfo, SkinInfo, UserInfo,
};
use crate::error::{CoreError, CoreResult};

/// Outcome of [`AzAuthClient::login`], cleanly modeling Azuriom 2FA challenges.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AzAuthLoginResult {
    Success { account: Box<AuthAccount> },
    Requires2fa,
}

/// Async client for Azuriom's AzAuth & Skin-API endpoints.
///
/// Preserves installation subpaths (e.g. `https://example.com/azuriom`) when joining
/// `api/auth/*` and `api/skin-api/skins/*`.
#[derive(Debug, Clone)]
pub struct AzAuthClient {
    http: reqwest::Client,
    auth_base_url: String,
    skin_base_url: String,
}

impl AzAuthClient {
    /// Creates a new [`AzAuthClient`] from an Azuriom base URL, reusing [`crate::shared_http_client()`]
    /// (with automatic `.no_proxy()` bypass for `localhost` / `127.0.0.1` test endpoints).
    pub fn new(base_url: &str) -> CoreResult<Self> {
        let parsed = parse_and_validate_azauth_base(base_url)?;
        let http = if is_loopback_host(parsed.host_str()) {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(15))
                .tcp_nodelay(true)
                .user_agent(concat!("minecraft-rust-core/", env!("CARGO_PKG_VERSION")))
                .no_proxy()
                .build()?
        } else {
            crate::shared_http_client()
        };

        Self::with_client_parsed(parsed, http)
    }

    /// Creates a new [`AzAuthClient`] from an Azuriom base URL and a caller-supplied [`reqwest::Client`].
    pub fn with_client(base_url: &str, http: reqwest::Client) -> CoreResult<Self> {
        let parsed = parse_and_validate_azauth_base(base_url)?;
        Self::with_client_parsed(parsed, http)
    }

    fn with_client_parsed(parsed: Url, http: reqwest::Client) -> CoreResult<Self> {
        let clean_base = parsed.as_str().trim_end_matches('/');
        let auth_base_url = format!("{clean_base}/api/auth");
        let skin_base_url = format!("{clean_base}/api/skin-api/skins");

        Ok(Self {
            http,
            auth_base_url,
            skin_base_url,
        })
    }

    /// Returns the resolved `/api/auth` base endpoint (preserving any subfolder prefix).
    pub fn auth_base_url(&self) -> &str {
        &self.auth_base_url
    }

    /// Returns the resolved `/api/skin-api/skins` base endpoint (preserving any subfolder prefix).
    pub fn skin_base_url(&self) -> &str {
        &self.skin_base_url
    }

    /// Authenticates a player via `POST {base}/api/auth/authenticate`.
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        two_factor_code: Option<&str>,
    ) -> CoreResult<AzAuthLoginResult> {
        #[derive(Serialize)]
        struct AuthPayload<'a> {
            email: &'a str,
            password: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            code: Option<&'a str>,
        }

        let code = two_factor_code
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let resp = self
            .http
            .post(format!("{}/authenticate", self.auth_base_url))
            .json(&AuthPayload {
                email: username.trim(),
                password,
                code,
            })
            .send()
            .await?;

        let http_status = resp.status();
        let body_text = resp.text().await.map_err(CoreError::Http)?;
        let raw =
            parse_auth_json_or_http_error("Azuriom authentication", http_status, &body_text)?;

        let status = raw
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let reason = raw
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if status == "pending" && reason == "2fa" {
            return Ok(AzAuthLoginResult::Requires2fa);
        }

        if !http_status.is_success() || status == "error" || raw.get("access_token").is_none() {
            let err_reason = if !reason.is_empty() {
                reason.to_string()
            } else if !http_status.is_success() {
                format!("http_{}", http_status.as_u16())
            } else {
                "auth_failed".to_string()
            };
            let err_msg = raw
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "Azuriom authentication failed (HTTP {})",
                        http_status.as_u16()
                    )
                });
            return Err(CoreError::Auth {
                reason: err_reason,
                message: err_msg,
            });
        }

        let account = self.parse_account_payload(&raw).await?;
        Ok(AzAuthLoginResult::Success {
            account: Box::new(account),
        })
    }

    /// Verifies and refreshes an existing Azuriom session via `POST {base}/api/auth/verify`.
    pub async fn verify(&self, access_token: &str) -> CoreResult<AuthAccount> {
        #[derive(Serialize)]
        struct VerifyPayload<'a> {
            access_token: &'a str,
        }

        let resp = self
            .http
            .post(format!("{}/verify", self.auth_base_url))
            .json(&VerifyPayload {
                access_token: access_token.trim(),
            })
            .send()
            .await?;

        let http_status = resp.status();
        let body_text = resp.text().await.map_err(CoreError::Http)?;
        let raw = parse_auth_json_or_http_error("Azuriom verify", http_status, &body_text)?;

        let status = raw
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if !http_status.is_success() || status == "error" || raw.get("access_token").is_none() {
            let reason = raw
                .get("reason")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if !http_status.is_success() {
                        format!("http_{}", http_status.as_u16())
                    } else {
                        "invalid_credentials".to_string()
                    }
                });
            let message = raw
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "Azuriom session verification failed (HTTP {})",
                        http_status.as_u16()
                    )
                });
            return Err(CoreError::Auth { reason, message });
        }

        self.parse_account_payload(&raw).await
    }

    /// Invalidates an Azuriom session token via `POST {base}/api/auth/logout`.
    pub async fn logout(&self, access_token: &str) -> CoreResult<bool> {
        #[derive(Serialize)]
        struct LogoutPayload<'a> {
            access_token: &'a str,
        }

        let resp = self
            .http
            .post(format!("{}/logout", self.auth_base_url))
            .json(&LogoutPayload {
                access_token: access_token.trim(),
            })
            .send()
            .await?;

        if !resp.status().is_success() {
            return Ok(false);
        }

        if let Ok(raw) = resp.json::<serde_json::Value>().await {
            if raw.get("status").and_then(|v| v.as_str()) == Some("error")
                || raw.get("error").and_then(|v| v.as_bool()) == Some(true)
            {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Uploads a new PNG player skin via `multipart/form-data` to
    /// `POST {base}/api/skin-api/skins/update` (defaulting to classic arm model when unspecified).
    pub async fn upload_skin(&self, access_token: &str, png_bytes: &[u8]) -> CoreResult<()> {
        self.upload_skin_inner(access_token, png_bytes, None).await
    }

    /// Uploads a new PNG player skin along with arm model `variant` (`"slim"` when `slim == true`,
    /// `"classic"` when `slim == false`) to `POST {base}/api/skin-api/skins/update`.
    pub async fn upload_skin_with_variant(
        &self,
        access_token: &str,
        skin_png_bytes: &[u8],
        slim: bool,
    ) -> CoreResult<()> {
        let variant = if slim { "slim" } else { "classic" };
        self.upload_skin_inner(access_token, skin_png_bytes, Some(variant))
            .await
    }

    async fn upload_skin_inner(
        &self,
        access_token: &str,
        png_bytes: &[u8],
        variant: Option<&str>,
    ) -> CoreResult<()> {
        if png_bytes.is_empty() {
            return Err(CoreError::InvalidConfig(
                "Skin PNG payload cannot be empty".to_string(),
            ));
        }

        let skin_part = Part::bytes(png_bytes.to_vec())
            .file_name("skin.png")
            .mime_str("image/png")
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;

        let mut form = Form::new().text("access_token", access_token.trim().to_string());
        if let Some(v) = variant {
            form = form.text("variant", v.to_string());
        }
        form = form.part("skin", skin_part);

        let resp = self
            .http
            .post(format!("{}/update", self.skin_base_url))
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            let parsed_msg = serde_json::from_str::<serde_json::Value>(&body_text)
                .ok()
                .and_then(|v| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| {
                    let summary = summarize_non_json_body(&body_text);
                    format!("HTTP {}: {summary}", status.as_u16())
                });

            return Err(CoreError::Auth {
                reason: "skin_upload_failed".to_string(),
                message: parsed_msg,
            });
        }

        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body_text) {
            if json.get("status").and_then(|v| v.as_str()) == Some("error") {
                let message = json
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Skin upload rejected by Azuriom")
                    .to_string();
                return Err(CoreError::Auth {
                    reason: "skin_upload_failed".to_string(),
                    message,
                });
            }
        }

        Ok(())
    }

    /// Fetches a player's skin from `GET {base}/api/skin-api/skins/{identifier}`.
    pub async fn fetch_skin(&self, identifier: &str) -> CoreResult<SkinInfo> {
        let skin_url = format!("{}/{}", self.skin_base_url, identifier.trim());
        let resp = match self.http.get(&skin_url).send().await {
            Ok(r) => r,
            Err(_) => {
                return Ok(SkinInfo {
                    id: None,
                    state: Some("ACTIVE".to_string()),
                    url: skin_url,
                    texture_key: None,
                    variant: None,
                    alias: None,
                    base64: None,
                });
            }
        };

        if !resp.status().is_success() {
            return Ok(SkinInfo {
                id: None,
                state: Some("ACTIVE".to_string()),
                url: skin_url,
                texture_key: None,
                variant: None,
                alias: None,
                base64: None,
            });
        }

        let bytes = resp.bytes().await?;
        let base64 = if bytes.is_empty() {
            None
        } else {
            Some(format!(
                "data:image/png;base64,{}",
                BASE64_STANDARD.encode(&bytes)
            ))
        };

        Ok(SkinInfo {
            id: None,
            state: Some("ACTIVE".to_string()),
            url: skin_url,
            texture_key: None,
            variant: None,
            alias: None,
            base64,
        })
    }

    async fn parse_account_payload(&self, data: &serde_json::Value) -> CoreResult<AuthAccount> {
        let access_token = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Auth {
                reason: "missing_token".to_string(),
                message: "AzAuth response missing 'access_token'".to_string(),
            })?
            .to_string();

        let uuid = data
            .get("uuid")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let name = data
            .get("username")
            .or_else(|| data.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let id_str = match data.get("id") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };

        let banned = data
            .get("banned")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let money = data
            .get("money")
            .or_else(|| data.get("monnaie"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let verified = data
            .get("email_verified")
            .or_else(|| data.get("verified"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let maintenance_bypass = data
            .get("maintenance_bypass")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let role: Option<RoleInfo> = match data.get("role") {
            Some(serde_json::Value::String(role_name)) if !role_name.is_empty() => Some(RoleInfo {
                id: None,
                name: role_name.clone(),
                color: None,
                background: None,
            }),
            Some(v @ serde_json::Value::Object(_)) => serde_json::from_value(v.clone()).ok(),
            _ => None,
        };

        let skin_lookup_id = id_str
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&uuid);
        let skin = self.fetch_skin(skin_lookup_id).await?;

        Ok(AuthAccount {
            access_token,
            client_token: uuid.clone(),
            refresh_token: None,
            expires_at: None,
            uuid: uuid.clone(),
            name: name.clone(),
            user_properties: "{}".to_string(),
            meta: AccountMeta {
                online: false,
                account_type: "AZauth".to_string(),
                offline: Some(false),
                access_token_expires_in: None,
                demo: Some(false),
            },
            user_info: Some(UserInfo {
                id: id_str,
                banned,
                money,
                role,
                verified,
                maintenance_bypass,
                ban_reason: None,
            }),
            profile: Some(MinecraftProfile {
                id: uuid,
                name,
                skins: vec![skin],
                capes: Vec::new(),
            }),
            xbox: None,
        })
    }
}

fn parse_and_validate_azauth_base(base_url: &str) -> CoreResult<Url> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return Err(CoreError::InvalidConfig(
            "AzAuth base URL cannot be empty".to_string(),
        ));
    }

    let mut parsed = Url::parse(trimmed)?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(CoreError::InvalidConfig(format!(
            "Unsupported URL scheme '{}' for AzAuth",
            parsed.scheme()
        )));
    }
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed)
}

pub(crate) fn is_loopback_host(host: Option<&str>) -> bool {
    matches!(host, Some("127.0.0.1") | Some("localhost") | Some("[::1]"))
}
