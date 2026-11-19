//! # `minecraft-rust-core`
//!
//! Async, multi-instance Rust library for launching **Minecraft: Java Edition** (`1.7.10` through `1.21+`)
//! across Windows, macOS, and Linux (`x86_64`, `x86`, `aarch64`, `arm`).
//!
//! ## Core Capabilities
//! - **Authentication (`auth`)**:
//!   - [`auth::microsoft::MicrosoftClient`] (OAuth2 -> Xbox Live -> XSTS with `XErr` diagnostics -> Minecraft Services + Game Pass support + skin upload & active cape management)
//!   - [`auth::azauth::AzAuthClient`] (Azuriom AzAuth login, 2FA challenge support, token verify, logout, and skin upload with full subfolder base URL support)
//!   - [`auth::mojang`] (deterministic Offline UUIDv3 login + legacy Yggdrasil client)
//! - **Multi-Instance Isolation (`config`, `bundle`, `launch`)**:
//!   - Shared root `<path>/assets`, `<path>/libraries`, `<path>/versions`, and `<path>/runtime`
//!   - Isolated instance directory `<path>/instances/<slug>` and per-instance/build loader directory `<path>/loader/<slug>/<loader>-<mc>-<build>`
//! - **Mod Loaders (`loader`)**:
//!   - `Forge` (`1.7.10` – `1.21+`), `NeoForge` (`1.20.1` legacy & `1.20.2` – `1.21+`), `Fabric` (Meta v2), `LegacyFabric` (Meta v2), and `Quilt` (Meta v3)
//! - **Server Ping (`status`)**:
//!   - [`status::ServerStatusClient`] with DNS SRV (`_minecraft._tcp.{host}`) resolution and exact UTF-8 byte-length VarInt framing.

pub mod auth;
pub mod bundle;
pub mod config;
pub mod downloader;
pub mod error;
pub mod events;
pub mod java;
pub mod launch;
pub mod loader;
pub mod minecraft;
pub mod status;
pub mod utils;

// Ergonomic crate-root re-exports
pub use auth::{
    azauth::{AzAuthClient, AzAuthLoginResult},
    microsoft::{MicrosoftClient, DEFAULT_MS_CLIENT_ID, MS_REDIRECT_URI},
    mojang::{offline_login, YggdrasilClient},
    AccountMeta, AuthAccount, CapeInfo, MinecraftProfile, RoleInfo, SkinInfo, SkinVariant,
    UserInfo, XboxInfo,
};
pub use bundle::{BundleItem, BundleManager, CustomDistribution, PackageDescriptor};
pub use config::{
    JavaOptions, LaunchOptions, LoaderOptions, LoaderType, MemoryOptions, ScreenOptions,
    ServerConnect,
};
pub use downloader::{DownloadTask, Downloader, ExpectedHash};
pub use error::{CoreError, CoreResult};
pub use events::{noop_event_callback, EventCallback, LaunchEvent};
pub use launch::{Launcher, PreparedLaunch};
pub use status::{ServerStatusClient, ServerStatusResponse};
pub use utils::hash_cache::FileHashCache;

/// Returns a process-wide shared [`reqwest::Client`] pooling TLS sessions and HTTP keep-alive
/// connections across authentication, manifest discovery, server metadata, and file downloads.
pub fn shared_http_client() -> reqwest::Client {
    static SHARED_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    SHARED_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(30))
                .tcp_nodelay(true)
                .pool_idle_timeout(std::time::Duration::from_secs(60))
                .user_agent(concat!("minecraft-rust-core/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}
