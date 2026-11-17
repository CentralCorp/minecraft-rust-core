# `minecraft-rust-core`

Idiomatic, async, multi-instance Rust core library for launching **Minecraft: Java Edition** (`1.7.10` through `1.21+`) across Windows, macOS, and Linux (`x86_64`, `x86`, `aarch64`, `arm`).

Replaces the legacy TypeScript/Node.js `minecraft-java-core` package to power Tauri v2 desktop launchers with native concurrency, cryptographic file verification, multi-instance isolation, and zero-shell JVM process execution.

---

## Features

- **Multi-Provider Authentication (`auth`)**
  - **Microsoft Live (`auth::microsoft::MicrosoftClient`)**: Full OAuth2 authorization code & redirect interception -> Xbox Live (`user.auth.xboxlive.com`) -> XSTS (`rp://api.minecraftservices.com/` + `http://xboxlive.com`) with human-readable `XErr` diagnostics (`2148916233`, `2148916235`, `2148916236`/`2148916237`, `2148916238`) -> Minecraft Services (`login_with_xbox`), full **Xbox Game Pass** support, token refresh, skin upload (`classic` / `slim`), and active cape management.
  - **Azuriom AzAuth (`auth::azauth::AzAuthClient`)**: Full support for Azuriom installations at root or subfolder paths (e.g. `https://example.com/azuriom`), two-factor authentication (`AzAuthLoginResult::Requires2fa`), session verification, logout, and multipart PNG skin upload.
  - **Mojang / Offline (`auth::mojang`)**: Deterministic Java-compatible UUIDv3 offline accounts (`OfflinePlayer:<username>`) and Yggdrasil authentication client.
- **Multi-Instance Isolation (`config`, `bundle`, `launch`)**
  - Shared root caches: `<path>/assets`, `<path>/libraries`, `<path>/versions`, and `<path>/runtime` (deduplicating JREs, Vanilla assets, and Maven libraries across all instances).
  - Isolated instance directories: `<path>/instances/<slug>` (`--gameDir`, JVM `cwd`, custom distribution files, `.jar-disable` optional mods, recursive `ignored` files & folders, and safe `verify` file cleaning that protects `saves`, `screenshots`, `logs`, `crash-reports`, and `launcher_config`).
  - Isolated loader directories: `<path>/loader/<slug>/<loader>-<mc>-<build>` so concurrent or distinct modpack instances never overwrite each other's loader profiles or patch markers.
- **Full Mod Loader Support (`loader`)**
  - **Forge** (`1.7.10` – `1.21+`: legacy `versionInfo`, jar-mod overlay, and modern `install_profile.json` + `processors`)
  - **NeoForge** (`1.20.1` legacy `net.neoforged:forge` and `1.20.2` – `1.21+` modern `net.neoforged:neoforge`)
  - **Fabric** (Meta v2), **LegacyFabric** (Meta v2), and **Quilt** (Meta v3)
- **Java Runtime Resolution (`java`)**
  - Resolves custom `java.path`, Mojang's official Java Runtime manifest (`windows-x64/x86/arm64`, `mac-os/mac-os-arm64` + `intel_enabled_mac`, `linux/linux-i386`), and Azul Zulu Metadata API fallback (`.zip` / `.tar.gz` extraction with macOS `.jre/Contents/Home/bin/java` resolution and Unix `0o755` permissions).
- **Atomic Bounded Downloader (`downloader`)**
  - `tokio::sync::Semaphore` concurrency control, destination path deduplication, HTTP `2xx` enforcement, `.part` temporary files with streaming SHA-1 / MD5 verification before atomic rename, and rolling `Speed` (`bytes/sec`) & `Estimated` (`seconds`) telemetry.
- **Minecraft Server Ping (`status`)**
  - Async SLP (`0x00` Handshake + `0x00` Status Request + `0x01` Ping/Pong) with automatic `_minecraft._tcp.{host}` DNS SRV resolution and exact UTF-8 byte-length VarInt packet framing.

---

## Quick Start

### Add Dependency

```toml
[dependencies]
minecraft-rust-core = { path = "../minecraft-java-core" }
```

### Launch an Isolated Instance

```rust
use std::path::PathBuf;
use std::sync::Arc;
use minecraft_rust_core::{
    auth::mojang::offline_login,
    config::{JavaOptions, LaunchOptions, LoaderOptions, LoaderType, MemoryOptions, ScreenOptions},
    events::LaunchEvent,
    launch::Launcher,
};

#[tokio::main]
async fn main() -> minecraft_rust_core::CoreResult<()> {
    let options = LaunchOptions {
        url: Some("https://panel.example.com/utils/instances/faction/files".into()),
        authenticator: offline_login("PlayerOne"),
        timeout_ms: 10_000,
        path: PathBuf::from("/path/to/.centralcorp"),
        version: "1.21.1".into(),
        instance: Some("faction".into()),
        detached: true,
        intel_enabled_mac: false,
        concurrent_downloads: 8,
        bypass_offline: false,
        loader: LoaderOptions {
            path: None,
            loader_type: Some(LoaderType::NeoForge),
            build: "recommended".into(),
            enable: true,
        },
        mcp: None,
        verify: true,
        ignored: vec!["config".into(), "options.txt".into()],
        jvm_args: vec![],
        game_args: vec![],
        auto_connect: None,
        java: JavaOptions::default(),
        screen: ScreenOptions::default(),
        memory: MemoryOptions {
            min: "2G".into(),
            max: "4G".into(),
        },
    };

    let launcher = Launcher::new(options);
    let pid = launcher
        .launch(Arc::new(|event| match event {
            LaunchEvent::Progress { downloaded, total, element } => {
                println!("[{element}] {downloaded}/{total} bytes");
            }
            LaunchEvent::Data { line } => println!("{line}"),
            LaunchEvent::Close { exit_code } => println!("Exited with code {exit_code}"),
            _ => {}
        }))
        .await?;

    println!("Spawned Minecraft instance with PID {pid}");
    Ok(())
}
```

---

## Module Hierarchy

| Module | Description |
| --- | --- |
| `error` | `CoreError` (`thiserror::Error` + `serde::Serialize`) and `CoreResult<T>` |
| `config` | `LaunchOptions`, `LoaderOptions`, `LoaderType`, `JavaOptions`, `MemoryOptions`, `ScreenOptions`, `ServerConnect` |
| `events` | `LaunchEvent` (`Check`, `Progress`, `Speed`, `Estimated`, `Extract`, `Patch`, `Data`, `Close`, `Error`) & `EventCallback` |
| `auth::microsoft` | `MicrosoftClient` (`authorization_url`, `extract_code_from_redirect_url`, `authenticate_with_code`, `refresh`, `get_profile`, `upload_skin`, `set_active_cape`) |
| `auth::azauth` | `AzAuthClient` (`login` with `AzAuthLoginResult`, `verify`, `logout`, `upload_skin`) |
| `auth::mojang` | `offline_login`, `offline_uuid`, and `YggdrasilClient` |
| `status` | `ServerStatusClient` & `ServerStatusResponse` (DNS SRV + SLP VarInt framing) |
| `downloader` | `Downloader`, `DownloadTask`, `ExpectedHash` |
| `bundle` | `BundleManager` & `BundleItem` (`fetch_custom_distribution`, `check_bundle`, `check_files`) |
| `java` | `resolve_java_runtime` (Custom -> Mojang JRE Manifest -> Azul Zulu API) |
| `minecraft` | `manifest`, `libraries`, `assets`, and `arguments` (SemVer classpath deduplication & ARM LWJGL overrides) |
| `loader` | `forge`, `neoforge`, `meta_loader` (`Fabric`, `LegacyFabric`, `Quilt`), and `patcher` |
| `launch` | `Launcher` (`prepare`, `launch`) & `PreparedLaunch` |

---

## Running Tests

```bash
cargo test
```

## License

Creative Commons Attribution-NonCommercial 4.0 International (`CC BY-NC 4.0`) — see [`LICENSE.md`](./LICENSE.md).