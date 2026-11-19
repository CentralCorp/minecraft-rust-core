use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::auth::AuthAccount;
use crate::error::CoreError;

fn default_timeout_ms() -> u64 {
    10_000
}

fn default_version() -> String {
    "latest_release".to_string()
}

fn default_concurrent_downloads() -> usize {
    12
}

fn default_loader_build() -> String {
    "latest".to_string()
}

fn default_java_type() -> String {
    "jre".to_string()
}

fn default_min_memory() -> String {
    "1G".to_string()
}

fn default_max_memory() -> String {
    "2G".to_string()
}

fn default_mc_port() -> u16 {
    25565
}

/// Supported Minecraft mod loader families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoaderType {
    Forge,
    NeoForge,
    Fabric,
    LegacyFabric,
    Quilt,
}

impl LoaderType {
    /// Canonical lowercase identifier used in APIs and directory paths.
    pub fn as_str(&self) -> &'static str {
        match self {
            LoaderType::Forge => "forge",
            LoaderType::NeoForge => "neoforge",
            LoaderType::Fabric => "fabric",
            LoaderType::LegacyFabric => "legacyfabric",
            LoaderType::Quilt => "quilt",
        }
    }
}

impl fmt::Display for LoaderType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LoaderType {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized: String = s
            .trim()
            .chars()
            .filter(|c| *c != '-' && *c != '_' && *c != ' ')
            .flat_map(|c| c.to_lowercase())
            .collect();

        match normalized.as_str() {
            "forge" => Ok(LoaderType::Forge),
            "neoforge" => Ok(LoaderType::NeoForge),
            "fabric" | "fabricmc" => Ok(LoaderType::Fabric),
            "legacyfabric" => Ok(LoaderType::LegacyFabric),
            "quilt" | "quiltmc" => Ok(LoaderType::Quilt),
            other => Err(CoreError::InvalidConfig(format!(
                "Unsupported mod loader type: '{other}'"
            ))),
        }
    }
}

impl Serialize for LoaderType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LoaderType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        LoaderType::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

fn deserialize_opt_loader_type<'de, D>(deserializer: D) -> Result<Option<LoaderType>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(ref s) if s.trim().is_empty() || s.trim().eq_ignore_ascii_case("none") || s.trim().eq_ignore_ascii_case("vanilla") => {
            Ok(None)
        }
        Some(s) => LoaderType::from_str(&s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn deserialize_loader_build<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Ok(default_loader_build()),
    }
}

fn deserialize_java_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_lowercase()),
        _ => Ok(default_java_type()),
    }
}

fn deserialize_opt_string_or_int<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        Str(String),
        Int(u64),
    }

    let opt = Option::<StringOrNumber>::deserialize(deserializer)?;
    Ok(match opt {
        Some(StringOrNumber::Str(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Some(StringOrNumber::Int(n)) => Some(n.to_string()),
        None => None,
    })
}

fn normalize_jvm_memory_string(raw: &str, default_val: fn() -> String) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_val();
    }

    let without_b = trimmed
        .strip_suffix(['b', 'B'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed);

    let last_char = without_b.chars().last().unwrap_or(' ');
    if matches!(last_char, 'g' | 'G' | 'm' | 'M' | 'k' | 'K') {
        let unit = last_char.to_ascii_uppercase();
        let num_part = without_b[..without_b.len() - last_char.len_utf8()].trim();
        if num_part.is_empty() {
            return default_val();
        }
        if num_part.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = num_part.parse::<u64>() {
                if n == 0 {
                    return default_val();
                }
                return format!("{n}{unit}");
            }
        }
        if let Ok(f) = num_part.parse::<f64>() {
            if f.is_finite() && f > 0.0 {
                return match unit {
                    'G' => format!("{}M", (f * 1024.0).round() as u64),
                    'M' => format!("{}M", f.round() as u64),
                    'K' => format!("{}K", f.round() as u64),
                    _ => default_val(),
                };
            }
        }
        return default_val();
    }

    if without_b.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(n) = without_b.parse::<u64>() {
            return if n == 0 {
                default_val()
            } else if n < 128 {
                format!("{n}G")
            } else {
                format!("{n}M")
            };
        }
    }

    if let Ok(f) = without_b.parse::<f64>() {
        if f.is_finite() && f > 0.0 {
            return if f < 128.0 {
                format!("{}M", (f * 1024.0).round() as u64)
            } else {
                format!("{}M", f.round() as u64)
            };
        }
    }

    default_val()
}

fn parse_memory_value<'de, D>(deserializer: D, default_val: fn() -> String) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MemValue {
        Str(String),
        Int(u64),
        Float(f64),
    }

    let opt = Option::<MemValue>::deserialize(deserializer)?;
    Ok(match opt {
        Some(MemValue::Str(s)) => normalize_jvm_memory_string(&s, default_val),
        Some(MemValue::Int(0)) | None => default_val(),
        Some(MemValue::Int(n)) if n < 128 => format!("{n}G"),
        Some(MemValue::Int(n)) => format!("{n}M"),
        Some(MemValue::Float(f)) if f.is_finite() && f > 0.0 => {
            if f < 128.0 {
                format!("{}M", (f * 1024.0).round() as u64)
            } else {
                format!("{}M", f.round() as u64)
            }
        }
        Some(MemValue::Float(_)) => default_val(),
    })
}

fn deserialize_min_memory<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    parse_memory_value(deserializer, default_min_memory)
}

fn deserialize_max_memory<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    parse_memory_value(deserializer, default_max_memory)
}

/// Mod loader installation & launch configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(
        rename = "type",
        alias = "loader_type",
        default,
        deserialize_with = "deserialize_opt_loader_type"
    )]
    pub loader_type: Option<LoaderType>,
    #[serde(
        default = "default_loader_build",
        deserialize_with = "deserialize_loader_build"
    )]
    pub build: String,
    #[serde(default)]
    pub enable: bool,
}

impl Default for LoaderOptions {
    fn default() -> Self {
        Self {
            path: None,
            loader_type: None,
            build: default_loader_build(),
            enable: false,
        }
    }
}

/// Java runtime resolution & download configuration.
#[derive(Debug, Clone, Serialize)]
pub struct JavaOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "type", default = "default_java_type")]
    pub image_type: String,
}

impl Default for JavaOptions {
    fn default() -> Self {
        Self {
            path: None,
            version: None,
            image_type: default_java_type(),
        }
    }
}

impl<'de> Deserialize<'de> for JavaOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum PathOrBool {
            Bool(bool),
            Str(String),
        }

        #[derive(Deserialize)]
        struct RawJavaObj {
            #[serde(default)]
            path: Option<PathOrBool>,
            #[serde(default, deserialize_with = "deserialize_opt_string_or_int")]
            version: Option<String>,
            #[serde(
                rename = "type",
                alias = "image_type",
                default = "default_java_type",
                deserialize_with = "deserialize_java_type"
            )]
            image_type: String,
            #[serde(default, alias = "config")]
            embedded: Option<bool>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum BoolOrObj {
            Bool(bool),
            Obj(RawJavaObj),
        }

        match Option::<BoolOrObj>::deserialize(deserializer)? {
            None | Some(BoolOrObj::Bool(true)) => Ok(Self::default()),
            Some(BoolOrObj::Bool(false)) => Ok(Self {
                path: Some(PathBuf::from("system")),
                version: None,
                image_type: default_java_type(),
            }),
            Some(BoolOrObj::Obj(raw)) => {
                let explicit_path = match raw.path {
                    Some(PathOrBool::Str(s)) => {
                        let trimmed = s.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(PathBuf::from(trimmed))
                        }
                    }
                    Some(PathOrBool::Bool(b)) => {
                        let _ = b;
                        None
                    }
                    None => None,
                };
                let resolved_path = match (explicit_path, raw.embedded) {
                    (Some(p), _) => Some(p),
                    (None, Some(false)) => Some(PathBuf::from("system")),
                    (None, _) => None,
                };
                Ok(Self {
                    path: resolved_path,
                    version: raw.version,
                    image_type: raw.image_type,
                })
            }
        }
    }
}

/// JVM heap memory bounds (`-Xms` and `-Xmx`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryOptions {
    #[serde(default = "default_min_memory", deserialize_with = "deserialize_min_memory")]
    pub min: String,
    #[serde(default = "default_max_memory", deserialize_with = "deserialize_max_memory")]
    pub max: String,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        Self {
            min: default_min_memory(),
            max: default_max_memory(),
        }
    }
}

/// Window dimensions and fullscreen launch configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScreenOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default)]
    pub fullscreen: bool,
}

/// Optional multiplayer server auto-connect endpoint on game startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerConnect {
    pub host: String,
    pub port: u16,
}

impl ServerConnect {
    /// Normalizes `host` and `port`:
    /// - Splits an embedded `host:port` suffix when `host` is not a raw unbracketed IPv6 literal
    ///   (e.g. `"play.example.com:25566"` with default `port: 25565` -> `("play.example.com", 25566)`).
    /// - Unbrackets `"[::1]:25570"` -> `("::1", 25570)` and `"[::1]"` -> `("::1", port)`.
    pub fn normalized(&self) -> (String, u16) {
        normalize_host_and_port(&self.host, self.port)
    }

    /// Formats the endpoint for MC `1.20+` `--quickPlayMultiplayer` (`host:port`, or `[ipv6]:port`
    /// when `host` is an IPv6 literal).
    pub fn quick_play_address(&self) -> String {
        let (host, port) = self.normalized();
        if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        }
    }
}

impl<'de> Deserialize<'de> for ServerConnect {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawServerConnect {
            #[serde(alias = "ip", alias = "server", alias = "address")]
            host: String,
            #[serde(default)]
            port: Option<u16>,
        }

        let raw = RawServerConnect::deserialize(deserializer)?;
        let (clean_host, embedded_port) = split_embedded_host_port(&raw.host);
        let port = match (raw.port, embedded_port) {
            (Some(explicit), Some(ep)) if explicit == default_mc_port() || explicit == 0 => ep,
            (Some(explicit), _) if explicit > 0 => explicit,
            (_, Some(ep)) => ep,
            _ => default_mc_port(),
        };

        Ok(Self {
            host: clean_host,
            port,
        })
    }
}

fn split_embedded_host_port(raw_host: &str) -> (String, Option<u16>) {
    let trimmed = raw_host.trim();
    if trimmed.is_empty() {
        return (String::new(), None);
    }

    // Bracketed IPv6 literal: `[::1]:25570` or `[::1]`
    if let Some(after_open) = trimmed.strip_prefix('[') {
        if let Some((ipv6_body, remainder)) = after_open.split_once(']') {
            let clean_ipv6 = ipv6_body.trim().to_string();
            let rem = remainder.trim();
            if let Some(port_str) = rem.strip_prefix(':') {
                if let Ok(p) = port_str.trim().parse::<u16>() {
                    if p > 0 {
                        return (clean_ipv6, Some(p));
                    }
                }
            }
            return (clean_ipv6, None);
        }
    }

    // Single colon `hostname:port` or `127.0.0.1:25566` (multiple colons = raw IPv6 literal like `::1`)
    let colon_count = trimmed.bytes().filter(|&b| b == b':').count();
    if colon_count == 1 {
        if let Some((h, p_str)) = trimmed.rsplit_once(':') {
            let clean_h = h.trim();
            if !clean_h.is_empty() {
                if let Ok(p) = p_str.trim().parse::<u16>() {
                    if p > 0 {
                        return (clean_h.to_string(), Some(p));
                    }
                }
            }
        }
    }

    (trimmed.to_string(), None)
}

fn normalize_host_and_port(raw_host: &str, raw_port: u16) -> (String, u16) {
    let (clean_host, embedded_port) = split_embedded_host_port(raw_host);
    let port = match embedded_port {
        Some(ep) if raw_port == default_mc_port() || raw_port == 0 => ep,
        _ if raw_port == 0 => default_mc_port(),
        _ => raw_port,
    };
    (clean_host, port)
}

/// Complete multi-instance launch configuration passed to [`crate::launch::Launcher`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchOptions {
    #[serde(default)]
    pub url: Option<String>,
    pub authenticator: AuthAccount,
    #[serde(default = "default_timeout_ms", alias = "timeout")]
    pub timeout_ms: u64,
    pub path: PathBuf,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub detached: bool,
    #[serde(default, alias = "intelEnabledMac")]
    pub intel_enabled_mac: bool,
    #[serde(
        default = "default_concurrent_downloads",
        alias = "downloadFileMultiple"
    )]
    pub concurrent_downloads: usize,
    #[serde(default, alias = "bypassOffline")]
    pub bypass_offline: bool,
    #[serde(default)]
    pub loader: LoaderOptions,
    #[serde(default)]
    pub mcp: Option<String>,
    #[serde(default)]
    pub verify: bool,
    #[serde(default)]
    pub ignored: Vec<String>,
    #[serde(default, alias = "JVM_ARGS")]
    pub jvm_args: Vec<String>,
    #[serde(default, alias = "GAME_ARGS")]
    pub game_args: Vec<String>,
    #[serde(default, alias = "autoConnect")]
    pub auto_connect: Option<ServerConnect>,
    #[serde(default)]
    pub java: JavaOptions,
    #[serde(default)]
    pub screen: ScreenOptions,
    #[serde(default)]
    pub memory: MemoryOptions,
}

impl LaunchOptions {
    /// Returns a sanitized, non-empty instance slug if multi-instance isolation is enabled.
    pub fn instance_slug(&self) -> Option<String> {
        let raw = self.instance.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let sanitized = sanitize_path_segment(raw);
        if sanitized.is_empty() {
            None
        } else {
            Some(sanitized)
        }
    }

    /// Returns the isolated game working directory (`<path>/instances/<slug>` when `instance`
    /// is set, or `<path>` otherwise).
    pub fn game_dir(&self) -> PathBuf {
        match self.instance_slug() {
            Some(slug) => self.path.join("instances").join(slug),
            None => self.path.clone(),
        }
    }

    /// Returns the isolated native shared library directory:
    /// - `<game_dir>/natives` when `instance` is set (preventing `os error 32` / `SIGBUS` when
    ///   running two instances simultaneously), or
    /// - `<path>/versions/<resolved_mc_version>/natives` otherwise.
    pub fn natives_dir(&self, resolved_mc_version: &str) -> PathBuf {
        if self.instance_slug().is_some() {
            self.game_dir().join("natives")
        } else {
            self.path
                .join("versions")
                .join(resolved_mc_version)
                .join("natives")
        }
    }

    /// Returns the isolated loader working directory (`<path>/loader/<slug_or_build>`),
    /// ensuring two instances or loader builds never overwrite each other's configs.
    pub fn loader_dir(&self, resolved_mc_version: &str) -> PathBuf {
        if let Some(ref custom) = self.loader.path {
            let trimmed = custom.trim().trim_start_matches("./").trim_matches('/');
            if !trimmed.is_empty() && trimmed != "loader" {
                return self.path.join(trimmed);
            }
        }

        let loader_name = self
            .loader
            .loader_type
            .map(|t| t.as_str())
            .unwrap_or("vanilla");
        let build_seg = sanitize_path_segment(&self.loader.build);
        let ver_seg = sanitize_path_segment(resolved_mc_version);

        match self.instance_slug() {
            Some(slug) => self
                .path
                .join("loader")
                .join(slug)
                .join(format!("{loader_name}-{ver_seg}-{build_seg}")),
            None => self
                .path
                .join("loader")
                .join(format!("{loader_name}-{ver_seg}-{build_seg}")),
        }
    }

    /// Resolves `mcp` path relative to the instance game directory (or root path).
    pub fn resolved_mcp_path(&self) -> Option<PathBuf> {
        let raw = self.mcp.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        let candidate = PathBuf::from(raw);
        if candidate.is_absolute() {
            Some(candidate)
        } else {
            Some(self.game_dir().join(candidate))
        }
    }

    /// Returns `concurrent_downloads` clamped to `[1, 64]`.
    pub fn clamped_concurrency(&self) -> usize {
        self.concurrent_downloads.clamp(1, 64)
    }
}

/// Sanitizes a single path segment so it cannot escape its parent directory.
pub(crate) fn sanitize_path_segment(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '-',
            _ => c,
        })
        .collect::<String>()
        .trim_matches('.')
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_memory_options_never_emits_float_gb_jvm_flags() {
        let m1: MemoryOptions = serde_json::from_value(json!({
            "min": "2.5G",
            "max": "4.5GB"
        }))
        .unwrap();
        assert_eq!(m1.min, "2560M");
        assert_eq!(m1.max, "4608M");

        let m2: MemoryOptions = serde_json::from_value(json!({
            "min": "2.5",
            "max": 6.5
        }))
        .unwrap();
        assert_eq!(m2.min, "2560M");
        assert_eq!(m2.max, "6656M");

        let m3: MemoryOptions = serde_json::from_value(json!({
            "min": 2048,
            "max": "4096"
        }))
        .unwrap();
        assert_eq!(m3.min, "2048M");
        assert_eq!(m3.max, "4096M");

        let m4: MemoryOptions = serde_json::from_value(json!({
            "min": 2,
            "max": "4G"
        }))
        .unwrap();
        assert_eq!(m4.min, "2G");
        assert_eq!(m4.max, "4G");
    }

    #[test]
    fn test_java_options_accepts_boolean_and_legacy_object_variants() {
        let j_true: JavaOptions = serde_json::from_value(json!(true)).unwrap();
        assert!(j_true.path.is_none());

        let j_false: JavaOptions = serde_json::from_value(json!(false)).unwrap();
        assert_eq!(j_false.path.as_deref(), Some(std::path::Path::new("system")));

        let j_legacy_idb: JavaOptions =
            serde_json::from_value(json!({ "path": false, "version": 17, "type": "jre" })).unwrap();
        assert!(j_legacy_idb.path.is_none());
        assert_eq!(j_legacy_idb.version.as_deref(), Some("17"));

        let j_embedded_false: JavaOptions =
            serde_json::from_value(json!({ "path": null, "embedded": false })).unwrap();
        assert_eq!(
            j_embedded_false.path.as_deref(),
            Some(std::path::Path::new("system"))
        );
    }

    #[test]
    fn test_launch_options_roundtrip_for_azauth_and_microsoft_accounts() {
        let azauth_opts: LaunchOptions = serde_json::from_value(json!({
            "url": "https://panel.example.com/utils/instances/survival/files",
            "authenticator": {
                "access_token": "az-token-123",
                "client_token": "069a79f444e94726a5befca90e38aaf5",
                "uuid": "069a79f444e94726a5befca90e38aaf5",
                "name": "Riptiaz",
                "user_properties": "{}",
                "meta": { "online": false, "type": "AZauth" },
                "user_info": {
                    "id": "1",
                    "banned": false,
                    "money": 500.0,
                    "role": { "name": "Admin", "color": "#ef4444" },
                    "verified": true,
                    "maintenance_bypass": true
                }
            },
            "path": "/tmp/centralcorp",
            "version": "1.20.1",
            "instance": "survival",
            "loader": { "type": "forge", "build": "47.2.0", "enable": true },
            "java": true,
            "memory": { "min": "2.5G", "max": "6G" }
        }))
        .unwrap();
        assert_eq!(azauth_opts.memory.min, "2560M");
        assert_eq!(azauth_opts.memory.max, "6G");
        assert_eq!(azauth_opts.authenticator.meta.account_type, "AZauth");
        assert!(azauth_opts.java.path.is_none());

        let ms_opts: LaunchOptions = serde_json::from_value(json!({
            "url": "https://panel.example.com/utils/instances/skyblock/files",
            "authenticator": {
                "access_token": "mc-jwt-xyz",
                "client_token": "abcdef0123456789abcdef0123456789",
                "refresh_token": "ms-refresh-xyz",
                "expires_at": 1999999999i64,
                "uuid": "abcdef0123456789abcdef0123456789",
                "name": "Notch",
                "user_properties": "{}",
                "meta": { "online": true, "type": "Microsoft" },
                "xbox": { "xuid": "2535412345678901", "gamertag": "Notch" },
                "profile": {
                    "id": "abcdef0123456789abcdef0123456789",
                    "name": "Notch",
                    "skins": [{ "state": "ACTIVE", "url": "https://textures.minecraft.net/texture/abc", "variant": "CLASSIC" }],
                    "capes": []
                }
            },
            "path": "/tmp/centralcorp",
            "version": "1.21.1",
            "instance": "skyblock",
            "loader": { "type": "neoforge", "build": "latest", "enable": true },
            "java": false,
            "auto_connect": { "host": "play.centralcorp.fr", "port": 25566 },
            "memory": { "min": 2.5, "max": "4096M" }
        }))
        .unwrap();
        assert_eq!(ms_opts.memory.min, "2560M");
        assert_eq!(ms_opts.memory.max, "4096M");
        assert_eq!(ms_opts.authenticator.meta.account_type, "Microsoft");
        assert_eq!(
            ms_opts.java.path.as_deref(),
            Some(std::path::Path::new("system"))
        );
    }
}
