use std::borrow::Borrow;
use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::BufReader;
use tokio::process::Command;

use crate::bundle::{BundleItem, BundleManager};
use crate::config::LaunchOptions;
use crate::downloader::Downloader;
use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::java::resolve_java_runtime;
use crate::loader::install_loader;
use crate::minecraft::arguments::build_launch_arguments;
use crate::minecraft::assets::{collect_vanilla_assets_parsed, copy_legacy_assets_from_parsed};
use crate::minecraft::libraries::{collect_vanilla_libraries, extract_natives_to};
use crate::minecraft::manifest::resolve_vanilla_version;
use crate::utils::hash_cache::FileHashCache;
use crate::utils::os_rules::RuleContext;
use crate::utils::zip_extract::ensure_executable;
use crate::utils::{exit_status_code, read_lossy_line};

/// Fully prepared JVM invocation specification returned by [`Launcher::prepare`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedLaunch {
    pub java_path: PathBuf,
    pub jvm_args: Vec<String>,
    pub main_class: String,
    pub game_args: Vec<String>,
    pub cwd: PathBuf,
}

impl PreparedLaunch {
    /// Returns the complete ordered argument vector (`jvm_args` + `main_class` + `game_args`)
    /// passed directly to `java`.
    pub fn full_args(&self) -> Vec<String> {
        let mut all = Vec::with_capacity(self.jvm_args.len() + 1 + self.game_args.len());
        all.extend(self.jvm_args.iter().cloned());
        all.push(self.main_class.clone());
        all.extend(self.game_args.iter().cloned());
        all
    }
}

/// High-level multi-instance Minecraft launcher orchestrating Java resolution, Vanilla manifest &
/// asset verification, mod-loader installation (`Forge`, `NeoForge`, `Fabric`, `LegacyFabric`, `Quilt`),
/// isolated instance bundle management, and non-blocking JVM process execution.
#[derive(Debug, Clone)]
pub struct Launcher {
    options: LaunchOptions,
    http: reqwest::Client,
}

impl Launcher {
    /// Creates a new [`Launcher`] bound to `options`, reusing [`crate::shared_http_client()`].
    pub fn new(options: LaunchOptions) -> Self {
        Self::with_client(options, crate::shared_http_client())
    }

    /// Creates a new [`Launcher`] bound to `options` and a caller-provided [`reqwest::Client`].
    pub fn with_client(options: LaunchOptions, http: reqwest::Client) -> Self {
        Self { options, http }
    }

    /// Builder-style override for the [`reqwest::Client`] used during [`Self::prepare`].
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Returns a reference to the underlying [`LaunchOptions`].
    pub fn options(&self) -> &LaunchOptions {
        &self.options
    }

    /// Returns a reference to the [`reqwest::Client`] used by this [`Launcher`].
    pub fn client(&self) -> &reqwest::Client {
        &self.http
    }

    /// Resolves Java, verifies/downloads Minecraft, assets, libraries, mod loader, and custom
    /// instance bundle files, runs `check_files` if `verify: true`, extracts per-instance natives,
    /// and assembles final JVM & game arguments.
    pub async fn prepare(&self, on_event: EventCallback) -> CoreResult<PreparedLaunch> {
        if self.options.path.as_os_str().is_empty() {
            return Err(CoreError::InvalidConfig(
                "LaunchOptions.path cannot be empty".to_string(),
            ));
        }

        let game_dir = self.options.game_dir();
        tokio::try_join!(
            fs::create_dir_all(self.options.path.join("assets")),
            fs::create_dir_all(self.options.path.join("libraries")),
            fs::create_dir_all(self.options.path.join("versions")),
            fs::create_dir_all(self.options.path.join("runtime")),
            fs::create_dir_all(&game_dir),
        )?;

        let hash_cache = FileHashCache::load(&self.options.path).await;
        let downloader = Downloader::with_client(
            self.http.clone(),
            self.options.clamped_concurrency(),
            3,
        )
        .with_chunk_timeout(self.options.timeout_ms.max(30_000))
        .with_hash_cache(hash_cache.clone());
        let bundle_manager =
            BundleManager::from_options(&self.options).with_hash_cache(hash_cache.clone());

        // 1 & 2 & 3. Run Vanilla discovery (manifest -> concurrent Java + asset index + libraries)
        //            in parallel with custom instance distribution fetch (`tokio::try_join!`)!
        let vanilla_discovery = async {
            let resolved_mc = resolve_vanilla_version(
                &self.options.path,
                &self.options.version,
                downloader.client(),
            )
            .await?;
            let mc_version_id = resolved_mc.version_id;
            let version_json = resolved_mc.version_json;

            let rule_ctx = RuleContext::current(self.options.intel_enabled_mac);
            let vanilla_libs = collect_vanilla_libraries(
                &self.options.path,
                &mc_version_id,
                &version_json,
                &rule_ctx,
            )?;

            let (resolved_java, collected_assets) = tokio::try_join!(
                resolve_java_runtime(&self.options, &version_json, &downloader, &on_event),
                collect_vanilla_assets_parsed(
                    &self.options.path,
                    &version_json,
                    downloader.client()
                ),
            )?;

            Ok::<_, CoreError>((
                mc_version_id,
                version_json,
                vanilla_libs,
                resolved_java,
                collected_assets,
            ))
        };

        let custom_discovery = bundle_manager
            .fetch_custom_distribution_packaged(downloader.client(), self.options.url.as_deref());

        let (
            (mc_version_id, version_json, vanilla_libs, resolved_java, collected_assets),
            custom_dist,
        ) = tokio::try_join!(vanilla_discovery, custom_discovery)?;

        let mut all_bundle_items = Vec::with_capacity(
            resolved_java.bundle_files.len()
                + vanilla_libs.bundle_items.len()
                + collected_assets.items.len()
                + custom_dist.items.len(),
        );
        all_bundle_items.extend(resolved_java.bundle_files);
        all_bundle_items.extend(vanilla_libs.bundle_items);
        all_bundle_items.extend(collected_assets.items);
        all_bundle_items.extend(custom_dist.items.iter().cloned());

        // 4. Verify local hashes via FileHashCache & selectively download direct files + dirty
        //    folder ZIP packages atomically before running loader processors
        bundle_manager
            .sync_bundle(
                &downloader,
                &all_bundle_items,
                &custom_dist.packages,
                &on_event,
            )
            .await?;

        ensure_executable(&resolved_java.binary_path)?;

        // 5. Run safe `verify` cleaner inside `game_dir` when enabled
        if self.options.verify {
            let mut valid_instance_items = custom_dist.items;
            if let Some(mcp_path) = self.options.resolved_mcp_path() {
                valid_instance_items.push(BundleItem {
                    path: mcp_path,
                    url: None,
                    sha1: None,
                    size: None,
                    category: "MCP".to_string(),
                    inline_content: None,
                    executable: false,
                    package_id: None,
                });
            }
            bundle_manager.check_files(&valid_instance_items).await?;
        }

        // 6. Extract native libraries to isolated `<game_dir>/natives` (or `<root>/versions/<id>/natives`)
        //    with `.natives.json` warm-launch stamp & populate legacy `<path>/resources` if needed
        let natives_dir = self.options.natives_dir(&mc_version_id);
        extract_natives_to(&natives_dir, &vanilla_libs.native_jars, &on_event).await?;

        copy_legacy_assets_from_parsed(
            &self.options.path,
            &game_dir,
            &version_json,
            collected_assets.parsed_index.as_ref(),
        )
        .await?;

        // 7. Install & patch mod loader (`Forge`, `NeoForge`, `Fabric`, `LegacyFabric`, `Quilt`) if enabled
        let loader_profile = install_loader(
            &self.options,
            &mc_version_id,
            &resolved_java.binary_path,
            &downloader,
            &bundle_manager,
            &on_event,
        )
        .await?;

        hash_cache.save().await?;

        // 8. Build final deduplicated classpath, JVM arguments, mainClass, and game arguments
        let built = build_launch_arguments(
            &self.options,
            &mc_version_id,
            &version_json,
            loader_profile.as_ref(),
        )?;

        Ok(PreparedLaunch {
            java_path: resolved_java.binary_path,
            jvm_args: built.jvm_args,
            main_class: built.main_class,
            game_args: built.game_args,
            cwd: game_dir,
        })
    }

    /// Prepares and spawns the Minecraft JVM process in `prepared.cwd` (`<path>/instances/<slug>`
    /// or `<path>`), redacts credentials in emitted logs, streams stdout/stderr lines on
    /// [`LaunchEvent::Data`] and exit code on [`LaunchEvent::Close`], and returns the child OS PID.
    pub async fn launch(&self, on_event: EventCallback) -> CoreResult<u32> {
        let prepared = self.prepare(on_event.clone()).await?;
        self.spawn_prepared(&prepared, on_event).await
    }

    /// Spawns an already-[`PreparedLaunch`] Minecraft JVM process, enabling callers (such as
    /// desktop launchers) to run post-extraction hooks (e.g. optional mod reconciliation) right
    /// after [`Self::prepare`] and before starting Java.
    pub async fn spawn_prepared(
        &self,
        prepared: &PreparedLaunch,
        on_event: EventCallback,
    ) -> CoreResult<u32> {
        self.spawn_prepared_with_on_spawn(prepared, on_event, |_| {})
            .await
    }

    /// Spawns an already-[`PreparedLaunch`] Minecraft JVM process (accepting either an owned
    /// [`PreparedLaunch`] or `&PreparedLaunch`), invoking `on_spawn(pid)` **immediately after
    /// `cmd.spawn()` succeeds and BEFORE emitting `Launching:` or spawning the `child.wait()`
    /// background task** so callers can register the PID atomically before any `Close` event can fire.
    pub async fn spawn_prepared_with_on_spawn(
        &self,
        prepared: impl Borrow<PreparedLaunch>,
        callback: EventCallback,
        on_spawn: impl FnOnce(u32) + Send + 'static,
    ) -> CoreResult<u32> {
        let prepared = prepared.borrow();
        let full_args = prepared.full_args();
        let redacted_cmd =
            format_redacted_command(&prepared.java_path, &full_args, &self.options);

        let mut cmd = Command::new(&prepared.java_path);
        cmd.args(&full_args)
            .current_dir(&prepared.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            const DETACHED_PROCESS: u32 = 0x00000008;
            let flags = if self.options.detached {
                CREATE_NO_WINDOW | DETACHED_PROCESS
            } else {
                CREATE_NO_WINDOW
            };
            cmd.creation_flags(flags);
        }

        #[cfg(unix)]
        if self.options.detached {
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| {
            CoreError::ProcessError(format!(
                "Failed to spawn Minecraft Java process '{}': {e}",
                prepared.java_path.display()
            ))
        })?;

        let pid = child.id().ok_or_else(|| {
            CoreError::ProcessError(
                "Minecraft Java process exited before an OS PID could be obtained".to_string(),
            )
        })?;

        // 1. Register PID with caller synchronously BEFORE background wait task can emit `Close`!
        on_spawn(pid);

        // 2. Emit redacted launch line AFTER `cmd.spawn()` has succeeded
        callback(LaunchEvent::Data {
            line: format!("[minecraft-rust-core] Launching: {redacted_cmd}"),
        });

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        tokio::spawn(async move {
            let cb_out = callback.clone();
            let out_task = tokio::spawn(async move {
                if let Some(out) = stdout {
                    let mut reader = BufReader::new(out);
                    let mut buf = Vec::with_capacity(1024);
                    while let Ok(Some(line)) = read_lossy_line(&mut reader, &mut buf).await {
                        cb_out(LaunchEvent::Data { line });
                    }
                }
            });

            let cb_err = callback.clone();
            let err_task = tokio::spawn(async move {
                if let Some(err) = stderr {
                    let mut reader = BufReader::new(err);
                    let mut buf = Vec::with_capacity(1024);
                    while let Ok(Some(line)) = read_lossy_line(&mut reader, &mut buf).await {
                        cb_err(LaunchEvent::Data { line });
                    }
                }
            });

            let status_res = child.wait().await;
            let _ = out_task.await;
            let _ = err_task.await;

            let exit_code = match status_res {
                Ok(status) => exit_status_code(&status),
                Err(err) => {
                    callback(LaunchEvent::Error {
                        message: format!("Error waiting on Minecraft process (PID {pid}): {err}"),
                    });
                    -1
                }
            };

            callback(LaunchEvent::Close { exit_code });
        });

        Ok(pid)
    }
}

/// Formats a diagnostic command line with all sensitive authentication tokens & identifiers redacted.
pub fn format_redacted_command(
    java_path: &std::path::Path,
    full_args: &[String],
    options: &LaunchOptions,
) -> String {
    let sensitive_flags = ["--accessToken", "--uuid", "--xuid", "--clientId"];

    let mut secret_values: Vec<&str> = Vec::new();
    let access_tok = options.authenticator.access_token.trim();
    if !access_tok.is_empty() && access_tok != "0" && access_tok != "null" && access_tok.len() > 4 {
        secret_values.push(access_tok);
    }
    let client_tok = options.authenticator.client_token.trim();
    if !client_tok.is_empty() && client_tok.len() > 4 {
        secret_values.push(client_tok);
    }
    if let Some(ref refresh_tok) = options.authenticator.refresh_token {
        let trimmed = refresh_tok.trim();
        if !trimmed.is_empty() && trimmed.len() > 4 {
            secret_values.push(trimmed);
        }
    }
    if !options.authenticator.uuid.trim().is_empty() {
        secret_values.push(options.authenticator.uuid.trim());
    }
    if let Some(ref xb) = options.authenticator.xbox {
        let xuid = xb.xuid.trim();
        if !xuid.is_empty() {
            secret_values.push(xuid);
        }
    }

    let mut parts = Vec::with_capacity(full_args.len() + 1);
    parts.push(format!("\"{}\"", java_path.display()));

    let mut redact_next = false;
    for arg in full_args {
        if redact_next {
            parts.push("[REDACTED]".to_string());
            redact_next = false;
            continue;
        }
        if sensitive_flags.contains(&arg.as_str()) {
            parts.push(arg.clone());
            redact_next = true;
            continue;
        }

        let mut sanitized = arg.clone();
        for secret in &secret_values {
            if sanitized.contains(secret) {
                sanitized = sanitized.replace(secret, "[REDACTED]");
            }
        }
        parts.push(sanitized);
    }

    parts.join(" ")
}
