pub mod azauth;
pub mod microsoft;
pub mod mojang;

use serde::{Deserialize, Deserializer, Serialize};

fn default_empty_props() -> String {
    "{}".to_string()
}

/// Known account provider types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountType {
    #[serde(rename = "AZauth", alias = "azauth")]
    AzAuth,
    #[serde(rename = "Microsoft", alias = "microsoft", alias = "Xbox", alias = "msa")]
    Microsoft,
    #[serde(rename = "Mojang", alias = "mojang")]
    Mojang,
    #[serde(rename = "Offline", alias = "offline")]
    Offline,
}

impl AccountType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AccountType::AzAuth => "AZauth",
            AccountType::Microsoft => "Microsoft",
            AccountType::Mojang => "Mojang",
            AccountType::Offline => "Offline",
        }
    }
}

/// Unified player authentication account compatible with IndexedDB persistence
/// across Azuriom (`AZauth`), `Microsoft`, and `Mojang` / Offline modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthAccount {
    pub access_token: String,
    pub client_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub uuid: String,
    pub name: String,
    #[serde(default = "default_empty_props")]
    pub user_properties: String,
    pub meta: AccountMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_info: Option<UserInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<MinecraftProfile>,
    #[serde(default, alias = "xboxAccount", skip_serializing_if = "Option::is_none")]
    pub xbox: Option<XboxInfo>,
}

/// Account session metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountMeta {
    #[serde(default)]
    pub online: bool,
    #[serde(rename = "type", alias = "account_type")]
    pub account_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token_expires_in: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demo: Option<bool>,
}

/// Extended backend player info (Azuriom or CentralPanel Microsoft sync).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserInfo {
    #[serde(default, deserialize_with = "deserialize_opt_string_id")]
    pub id: Option<String>,
    #[serde(default)]
    pub banned: bool,
    #[serde(default, alias = "monnaie", alias = "points")]
    pub money: f64,
    #[serde(default, deserialize_with = "deserialize_opt_role_info")]
    pub role: Option<RoleInfo>,
    #[serde(default, alias = "email_verified")]
    pub verified: bool,
    #[serde(default)]
    pub maintenance_bypass: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ban_reason: Option<String>,
}

/// Role / rank metadata returned by Azuriom or CentralPanel v2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleInfo {
    #[serde(default, deserialize_with = "deserialize_opt_u64_id")]
    pub id: Option<u64>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
}

/// Minecraft profile containing active and unlocked skins and capes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinecraftProfile {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub skins: Vec<SkinInfo>,
    #[serde(default)]
    pub capes: Vec<CapeInfo>,
}

/// Minecraft player skin model arm width variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkinVariant {
    #[default]
    #[serde(rename = "CLASSIC", alias = "classic", alias = "Classic")]
    Classic,
    #[serde(rename = "SLIM", alias = "slim", alias = "Slim")]
    Slim,
}

impl SkinVariant {
    /// Lowercase string expected by `POST /minecraft/profile/skins`.
    pub fn as_api_str(&self) -> &'static str {
        match self {
            SkinVariant::Classic => "classic",
            SkinVariant::Slim => "slim",
        }
    }
}

/// Player skin texture descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkinInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub url: String,
    #[serde(
        default,
        alias = "textureKey",
        skip_serializing_if = "Option::is_none"
    )]
    pub texture_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<SkinVariant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
}

/// Player cape texture descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapeInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
}

/// Xbox Live profile claims obtained during XSTS authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XboxInfo {
    pub xuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gamertag: Option<String>,
    #[serde(
        default,
        alias = "ageGroup",
        skip_serializing_if = "Option::is_none"
    )]
    pub age_group: Option<String>,
}

fn deserialize_opt_string_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IdValue {
        Str(String),
        Int(i64),
        Uint(u64),
    }

    let opt = Option::<IdValue>::deserialize(deserializer)?;
    Ok(match opt {
        Some(IdValue::Str(s)) if !s.is_empty() => Some(s),
        Some(IdValue::Str(_)) => None,
        Some(IdValue::Int(i)) => Some(i.to_string()),
        Some(IdValue::Uint(u)) => Some(u.to_string()),
        None => None,
    })
}

fn deserialize_opt_u64_id<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum U64OrStr {
        Uint(u64),
        Str(String),
    }

    let opt = Option::<U64OrStr>::deserialize(deserializer)?;
    Ok(match opt {
        Some(U64OrStr::Uint(n)) => Some(n),
        Some(U64OrStr::Str(s)) => s.trim().parse::<u64>().ok(),
        None => None,
    })
}

fn deserialize_opt_role_info<'de, D>(deserializer: D) -> Result<Option<RoleInfo>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RoleOrStr {
        Obj(RoleInfo),
        Name(String),
    }

    let opt = Option::<RoleOrStr>::deserialize(deserializer)?;
    Ok(match opt {
        Some(RoleOrStr::Obj(r)) => Some(r),
        Some(RoleOrStr::Name(name)) if !name.trim().is_empty() => Some(RoleInfo {
            id: None,
            name,
            color: None,
            background: None,
        }),
        _ => None,
    })
}
