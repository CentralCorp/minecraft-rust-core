use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::auth::{AccountMeta, AuthAccount, MinecraftProfile, SkinVariant, XboxInfo};
use crate::error::{CoreError, CoreResult};

/// Default official Microsoft Live client ID for Minecraft launchers.
pub const DEFAULT_MS_CLIENT_ID: &str = "00000000402b5328";

/// Standard desktop OAuth2 redirect URI intercepted by the launcher WebviewWindow.
pub const MS_REDIRECT_URI: &str = "https://login.live.com/oauth20_desktop.srf";

const MS_COBRAND_ID: &str = "8058f65d-ce06-4c30-9559-473c9275a65d";

/// Customizable endpoint URLs for [`MicrosoftClient`] (used for testing against mock servers).
#[derive(Debug, Clone)]
pub struct MicrosoftEndpoints {
    pub oauth_authorize: String,
    pub oauth_token: String,
    pub xbl_authenticate: String,
    pub xsts_authorize: String,
    pub mc_services_base: String,
}

impl Default for MicrosoftEndpoints {
    fn default() -> Self {
        Self {
            oauth_authorize: "https://login.live.com/oauth20_authorize.srf".to_string(),
            oauth_token: "https://login.live.com/oauth20_token.srf".to_string(),
            xbl_authenticate: "https://user.auth.xboxlive.com/user/authenticate".to_string(),
            xsts_authorize: "https://xsts.auth.xboxlive.com/xsts/authorize".to_string(),
            mc_services_base: "https://api.minecraftservices.com".to_string(),
        }
    }
}

/// Async client implementing the complete Microsoft Live OAuth2 -> Xbox Live -> XSTS ->
/// Minecraft Services authentication, token refresh, and Skin/Cape management pipeline.
#[derive(Debug, Clone)]
pub struct MicrosoftClient {
    client_id: String,
    http: reqwest::Client,
    endpoints: MicrosoftEndpoints,
}

impl MicrosoftClient {
    /// Creates a [`MicrosoftClient`], falling back to [`DEFAULT_MS_CLIENT_ID`] when `client_id`
    /// is `None` or blank.
    pub fn new(client_id: Option<&str>) -> Self {
        let resolved_id = client_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_MS_CLIENT_ID)
            .to_string();

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("minecraft-rust-core/5.0")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            client_id: resolved_id,
            http,
            endpoints: MicrosoftEndpoints::default(),
        }
    }

    /// Creates a [`MicrosoftClient`] overriding endpoint URLs (automatically bypassing proxies for `127.0.0.1` / `localhost`).
    pub fn with_endpoints(client_id: Option<&str>, endpoints: MicrosoftEndpoints) -> Self {
        let resolved_id = client_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_MS_CLIENT_ID)
            .to_string();

        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("minecraft-rust-core/5.0");
        if endpoints.oauth_token.contains("127.0.0.1")
            || endpoints.oauth_token.contains("localhost")
        {
            builder = builder.no_proxy();
        }
        let http = builder.build().unwrap_or_else(|_| reqwest::Client::new());

        Self {
            client_id: resolved_id,
            http,
            endpoints,
        }
    }

    /// Returns the configured OAuth2 `client_id`.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Builds the Microsoft Live OAuth2 authorization URL to open in a browser or WebviewWindow.
    pub fn authorization_url(&self) -> String {
        let encoded_client_id = urlencoding::encode(&self.client_id);
        let encoded_redirect = urlencoding::encode(MS_REDIRECT_URI);
        format!(
            "{}?client_id={}&response_type=code&redirect_uri={}&scope=XboxLive.signin%20offline_access&cobrandid={}&prompt=select_account",
            self.endpoints.oauth_authorize,
            encoded_client_id,
            encoded_redirect,
            MS_COBRAND_ID
        )
    }

    /// Inspects a navigated WebView URL during OAuth2 sign-in:
    /// - Returns `None` for intermediate login pages (`login.live.com/oauth20_authorize.srf`, etc.).
    /// - Returns `Some(Ok(code))` when redirected to `oauth20_desktop.srf?code=...`.
    /// - Returns `Some(Err(description))` when redirected to `oauth20_desktop.srf?error=...`.
    pub fn extract_code_from_redirect_url(url: &str) -> Option<Result<String, String>> {
        let parsed = Url::parse(url.trim()).ok()?;
        let is_redirect = (parsed
            .host_str()
            .map(|h| h.eq_ignore_ascii_case("login.live.com"))
            .unwrap_or(false)
            && parsed.path().eq_ignore_ascii_case("/oauth20_desktop.srf"))
            || url
                .trim()
                .to_ascii_lowercase()
                .starts_with(&MS_REDIRECT_URI.to_ascii_lowercase());

        if !is_redirect {
            return None;
        }

        let mut code: Option<String> = None;
        let mut err: Option<String> = None;
        let mut err_desc: Option<String> = None;

        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" if !v.trim().is_empty() => code = Some(v.into_owned()),
                "error" if !v.trim().is_empty() => err = Some(v.into_owned()),
                "error_description" if !v.trim().is_empty() => err_desc = Some(v.into_owned()),
                _ => {}
            }
        }

        // Fallback to URL fragment if OAuth parameters were placed after '#'
        if code.is_none() && err.is_none() {
            if let Some(fragment) = parsed.fragment() {
                for (k, v) in url::form_urlencoded::parse(fragment.as_bytes()) {
                    match k.as_ref() {
                        "code" if !v.trim().is_empty() => code = Some(v.into_owned()),
                        "error" if !v.trim().is_empty() => err = Some(v.into_owned()),
                        "error_description" if !v.trim().is_empty() => {
                            err_desc = Some(v.into_owned())
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some(c) = code {
            Some(Ok(c))
        } else if let Some(desc) = err_desc {
            Some(Err(desc))
        } else if let Some(e) = err {
            Some(Err(e))
        } else {
            Some(Err(
                "Microsoft OAuth2 redirect did not include an authorization code".to_string(),
            ))
        }
    }

    /// Executes the full OAuth2 authorization code -> Xbox Live -> XSTS -> Minecraft Services
    /// authentication chain.
    pub async fn authenticate_with_code(&self, code: &str) -> CoreResult<AuthAccount> {
        let clean_code = code.trim();
        if clean_code.is_empty() {
            return Err(CoreError::InvalidConfig(
                "Authorization code cannot be empty".to_string(),
            ));
        }

        let form_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &self.client_id)
            .append_pair("code", clean_code)
            .append_pair("grant_type", "authorization_code")
            .append_pair("redirect_uri", MS_REDIRECT_URI)
            .append_pair("scope", "XboxLive.signin offline_access")
            .finish();

        let oauth = self.exchange_oauth_token(&form_body).await?;
        self.complete_xbox_and_minecraft_chain(oauth, None, None)
            .await
    }

    /// Refreshes a Microsoft [`AuthAccount`]:
    /// - If `expires_at` still has more than 1800 seconds remaining, refreshes the player's
    ///   profile, skins, and capes via [`Self::get_profile`] (falling back to full token refresh
    ///   if the Bearer token was revoked).
    /// - Otherwise exchanges `refresh_token` with proper URL-encoding and runs the XBL/XSTS/MC chain.
    pub async fn refresh(&self, account: &AuthAccount) -> CoreResult<AuthAccount> {
        let now = unix_timestamp_secs();
        let expires_at = account
            .expires_at
            .or(account.meta.access_token_expires_in)
            .unwrap_or(0);

        if expires_at - now > 1800 && !account.access_token.trim().is_empty() {
            match self.get_profile(&account.access_token).await {
                Ok(profile) => {
                    let mut updated = account.clone();
                    updated.uuid = profile.id.clone();
                    updated.name = profile.name.clone();
                    updated.profile = Some(profile);
                    updated.expires_at = Some(expires_at);
                    return Ok(updated);
                }
                Err(_) if account.refresh_token.is_some() => {
                    // Access token expired/revoked early; fall through to refresh_token exchange
                }
                Err(e) => return Err(e),
            }
        }

        let refresh_token = account
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::Auth {
                reason: "missing_refresh_token".to_string(),
                message: "Cannot refresh Microsoft session: missing refresh_token".to_string(),
            })?;

        let form_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &self.client_id)
            .append_pair("refresh_token", refresh_token)
            .append_pair("grant_type", "refresh_token")
            .append_pair("scope", "XboxLive.signin offline_access")
            .finish();

        let oauth = self.exchange_oauth_token(&form_body).await?;
        self.complete_xbox_and_minecraft_chain(
            oauth,
            Some(account.client_token.as_str()),
            account.user_info.clone(),
        )
        .await
    }

    /// Fetches the authenticated player's Minecraft profile (`/minecraft/profile`) and downloads
    /// active/unlocked skin & cape PNG textures as `data:image/png;base64,...`.
    pub async fn get_profile(&self, mc_access_token: &str) -> CoreResult<MinecraftProfile> {
        let resp = self
            .http
            .get(format!("{}/minecraft/profile", self.endpoints.mc_services_base))
            .bearer_auth(mc_access_token.trim())
            .send()
            .await?;

        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

        if !status.is_success() || body.get("id").and_then(|v| v.as_str()).is_none() {
            let err_code = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("profile_not_found")
                .to_string();
            let err_msg = body
                .get("errorMessage")
                .and_then(|v| v.as_str())
                .unwrap_or("Failed to fetch Minecraft Java profile")
                .to_string();
            return Err(CoreError::Auth {
                reason: err_code,
                message: format!("{err_msg} (HTTP {status})"),
            });
        }

        let mut profile: MinecraftProfile = serde_json::from_value(body)?;

        for skin in &mut profile.skins {
            if !skin.url.trim().is_empty() {
                if let Some(b64) = self.fetch_texture_data_uri(&skin.url).await {
                    skin.base64 = Some(b64);
                }
            }
        }

        for cape in &mut profile.capes {
            if !cape.url.trim().is_empty() {
                if let Some(b64) = self.fetch_texture_data_uri(&cape.url).await {
                    cape.base64 = Some(b64);
                }
            }
        }

        Ok(profile)
    }

    /// Uploads a PNG skin to `POST https://api.minecraftservices.com/minecraft/profile/skins`
    /// (`variant` = `"classic"` or `"slim"`, `file` = PNG) and returns the refreshed [`MinecraftProfile`].
    pub async fn upload_skin(
        &self,
        mc_access_token: &str,
        png_bytes: &[u8],
        variant: SkinVariant,
    ) -> CoreResult<MinecraftProfile> {
        if png_bytes.is_empty() {
            return Err(CoreError::InvalidConfig(
                "Skin PNG bytes cannot be empty".to_string(),
            ));
        }

        let file_part = Part::bytes(png_bytes.to_vec())
            .file_name("skin.png")
            .mime_str("image/png")
            .map_err(|e| CoreError::InvalidConfig(e.to_string()))?;

        let form = Form::new()
            .text("variant", variant.as_api_str().to_string())
            .part("file", file_part);

        let resp = self
            .http
            .post(format!(
                "{}/minecraft/profile/skins",
                self.endpoints.mc_services_base
            ))
            .bearer_auth(mc_access_token.trim())
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(CoreError::Auth {
                reason: "skin_upload_failed".to_string(),
                message: format!("Minecraft Services skin upload failed ({status}): {text}"),
            });
        }

        self.get_profile(mc_access_token).await
    }

    /// Equips (`Some(cape_id)` -> `PUT .../capes/active`) or unequips (`None` -> `DELETE .../capes/active`)
    /// an official Minecraft cape, then returns the refreshed [`MinecraftProfile`].
    pub async fn set_active_cape(
        &self,
        mc_access_token: &str,
        cape_id: Option<&str>,
    ) -> CoreResult<MinecraftProfile> {
        let url = format!(
            "{}/minecraft/profile/capes/active",
            self.endpoints.mc_services_base
        );

        let clean_cape = cape_id.map(str::trim).filter(|s| !s.is_empty());
        let resp = if let Some(id) = clean_cape {
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct EquipCape<'a> {
                cape_id: &'a str,
            }

            self.http
                .put(&url)
                .bearer_auth(mc_access_token.trim())
                .json(&EquipCape { cape_id: id })
                .send()
                .await?
        } else {
            self.http
                .delete(&url)
                .bearer_auth(mc_access_token.trim())
                .send()
                .await?
        };

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(CoreError::Auth {
                reason: "cape_update_failed".to_string(),
                message: format!("Minecraft Services cape update failed ({status}): {text}"),
            });
        }

        self.get_profile(mc_access_token).await
    }

    async fn exchange_oauth_token(&self, form_body: &str) -> CoreResult<MsOAuthTokenResponse> {
        let resp = self
            .http
            .post(&self.endpoints.oauth_token)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body.to_string())
            .send()
            .await?;

        let status = resp.status();
        let raw: serde_json::Value = resp.json().await?;

        if !status.is_success() || raw.get("error").is_some() {
            let reason = raw
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("oauth2_error")
                .to_string();
            let message = raw
                .get("error_description")
                .and_then(|v| v.as_str())
                .unwrap_or("Microsoft Live OAuth2 token exchange failed")
                .to_string();
            return Err(CoreError::Auth { reason, message });
        }

        serde_json::from_value(raw).map_err(CoreError::Json)
    }

    async fn complete_xbox_and_minecraft_chain(
        &self,
        oauth: MsOAuthTokenResponse,
        existing_client_token: Option<&str>,
        existing_user_info: Option<crate::auth::UserInfo>,
    ) -> CoreResult<AuthAccount> {
        // 1. Xbox Live User Authenticate (`d=<token>`, with raw `<token>` fallback for custom Azure AD apps)
        let xbl = match self
            .authenticate_xbox_live(&format!("d={}", oauth.access_token))
            .await
        {
            Ok(res) => res,
            Err(_) if self.client_id != DEFAULT_MS_CLIENT_ID => {
                self.authenticate_xbox_live(&oauth.access_token).await?
            }
            Err(e) => return Err(e),
        };

        let uhs = xbl
            .display_claims
            .xui
            .first()
            .and_then(|c| c.uhs.as_deref())
            .ok_or_else(|| CoreError::Auth {
                reason: "xbl_missing_uhs".to_string(),
                message: "Xbox Live response did not contain user hash (uhs)".to_string(),
            })?
            .to_string();

        // 2. XSTS Authorize for Minecraft Services (`rp://api.minecraftservices.com/`)
        let xsts_mc = self
            .authorize_xsts(&xbl.token, "rp://api.minecraftservices.com/")
            .await?;

        // 3. XSTS Authorize for Xbox Live (`http://xboxlive.com`) to retrieve XUID / Gamertag / AgeGroup
        let xbox_info = match self.authorize_xsts(&xbl.token, "http://xboxlive.com").await {
            Ok(xsts_xbl) => xsts_xbl.display_claims.xui.first().and_then(|claim| {
                claim.xid.as_ref().map(|xuid| XboxInfo {
                    xuid: xuid.clone(),
                    gamertag: claim.gtg.clone(),
                    age_group: claim.agg.clone(),
                })
            }),
            Err(_) => None,
        };

        // 4. Minecraft Services `login_with_xbox`
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct McXboxLoginReq {
            identity_token: String,
        }

        let mc_login_resp = self
            .http
            .post(format!(
                "{}/authentication/login_with_xbox",
                self.endpoints.mc_services_base
            ))
            .json(&McXboxLoginReq {
                identity_token: format!("XBL3.0 x={uhs};{}", xsts_mc.token),
            })
            .send()
            .await?;

        let mc_status = mc_login_resp.status();
        let mc_raw: serde_json::Value = mc_login_resp.json().await?;

        if !mc_status.is_success() || mc_raw.get("access_token").is_none() {
            let err_msg = mc_raw
                .get("errorMessage")
                .or_else(|| mc_raw.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("Minecraft Services Xbox login failed")
                .to_string();
            return Err(CoreError::Auth {
                reason: "mc_login_failed".to_string(),
                message: err_msg,
            });
        }

        let mc_access_token = mc_raw["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let expires_in = mc_raw
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(86_400);
        let expires_at = unix_timestamp_secs() + expires_in;

        // 5. Check entitlements AND `/minecraft/profile` (supporting Xbox Game Pass accounts whose
        // `entitlements.items` may be empty when `/minecraft/profile` returns 200 OK!)
        let owns_in_store = self.check_mcstore_entitlements(&mc_access_token).await;

        let profile = match self.get_profile(&mc_access_token).await {
            Ok(profile) => profile,
            Err(profile_err) => {
                if !owns_in_store {
                    return Err(CoreError::Auth {
                        reason: "no_minecraft_license".to_string(),
                        message: "This Microsoft account does not own Minecraft: Java Edition (and has no active Xbox Game Pass profile).".to_string(),
                    });
                }
                return Err(profile_err);
            }
        };

        let client_token = existing_client_token
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string());

        Ok(AuthAccount {
            access_token: mc_access_token,
            client_token,
            refresh_token: oauth.refresh_token,
            expires_at: Some(expires_at),
            uuid: profile.id.clone(),
            name: profile.name.clone(),
            user_properties: "{}".to_string(),
            meta: AccountMeta {
                online: true,
                account_type: "Microsoft".to_string(),
                offline: Some(false),
                access_token_expires_in: Some(expires_at),
                demo: Some(false),
            },
            user_info: existing_user_info,
            profile: Some(profile),
            xbox: xbox_info,
        })
    }

    async fn authenticate_xbox_live(&self, rps_ticket: &str) -> CoreResult<XboxTokenResponse> {
        let payload = serde_json::json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": rps_ticket,
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        });

        let resp = self
            .http
            .post(&self.endpoints.xbl_authenticate)
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(CoreError::Auth {
                reason: "xbl_auth_failed".to_string(),
                message: format!("Xbox Live authentication failed ({status}): {text}"),
            });
        }

        serde_json::from_str(&text).map_err(CoreError::Json)
    }

    async fn authorize_xsts(
        &self,
        xbl_token: &str,
        relying_party: &str,
    ) -> CoreResult<XboxTokenResponse> {
        let payload = serde_json::json!({
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [xbl_token],
            },
            "RelyingParty": relying_party,
            "TokenType": "JWT"
        });

        let resp = self
            .http
            .post(&self.endpoints.xsts_authorize)
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            if let Ok(err_json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(xerr) = err_json.get("XErr").and_then(|v| v.as_u64()) {
                    return Err(map_xsts_error(
                        xerr,
                        err_json.get("Message").and_then(|v| v.as_str()),
                    ));
                }
            }
            return Err(CoreError::Auth {
                reason: "xsts_auth_failed".to_string(),
                message: format!("XSTS authorization failed ({status}): {text}"),
            });
        }

        serde_json::from_str(&text).map_err(CoreError::Json)
    }

    async fn check_mcstore_entitlements(&self, mc_access_token: &str) -> bool {
        let resp = match self
            .http
            .get(format!(
                "{}/entitlements/mcstore",
                self.endpoints.mc_services_base
            ))
            .bearer_auth(mc_access_token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => return false,
        };

        let Ok(json) = resp.json::<serde_json::Value>().await else {
            return false;
        };

        json.get("items")
            .and_then(|v| v.as_array())
            .map(|items| {
                items.iter().any(|item| {
                    matches!(
                        item.get("name").and_then(|n| n.as_str()),
                        Some("product_minecraft")
                            | Some("game_minecraft")
                            | Some("product_minecraft_bedrock")
                    )
                })
            })
            .unwrap_or(false)
    }

    async fn fetch_texture_data_uri(&self, url: &str) -> Option<String> {
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        if bytes.is_empty() {
            return None;
        }
        Some(format!(
            "data:image/png;base64,{}",
            BASE64_STANDARD.encode(&bytes)
        ))
    }
}

/// Maps Xbox Live XSTS `XErr` numeric codes to explicit human-readable diagnostics.
pub fn map_xsts_error(xerr: u64, server_message: Option<&str>) -> CoreError {
    let message = match xerr {
        2148916227 => {
            "This account has been banned from Xbox Live.".to_string()
        }
        2148916233 => {
            "This Microsoft account does not have an Xbox Live profile yet. Please sign in once at https://www.xbox.com to create your Gamertag.".to_string()
        }
        2148916235 => {
            "Xbox Live is unavailable or restricted in your country/region.".to_string()
        }
        2148916236 | 2148916237 => {
            "This Microsoft account requires adult age verification on https://www.xbox.com before signing in.".to_string()
        }
        2148916238 => {
            "This is a child/teen account (under 18) and must be added to a Microsoft Family group by an adult with multiplayer and third-party access enabled.".to_string()
        }
        other => match server_message.filter(|s| !s.trim().is_empty()) {
            Some(msg) => format!("Xbox Live XSTS error {other}: {msg}"),
            None => format!("Xbox Live XSTS authorization failed with code {other}"),
        },
    };

    CoreError::XboxXsts { xerr, message }
}

fn unix_timestamp_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Deserialize)]
struct MsOAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct XboxTokenResponse {
    #[serde(rename = "Token")]
    token: String,
    #[serde(rename = "DisplayClaims")]
    display_claims: XboxDisplayClaims,
}

#[derive(Debug, Deserialize)]
struct XboxDisplayClaims {
    #[serde(default)]
    xui: Vec<XboxXuiClaim>,
}

#[derive(Debug, Deserialize)]
struct XboxXuiClaim {
    #[serde(default)]
    uhs: Option<String>,
    #[serde(default)]
    xid: Option<String>,
    #[serde(default)]
    gtg: Option<String>,
    #[serde(default)]
    agg: Option<String>,
}
