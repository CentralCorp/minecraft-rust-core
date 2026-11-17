use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::bundle::{BundleItem, BundleManager};
use crate::config::LaunchOptions;
use crate::downloader::Downloader;
use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::java::resolve_java_runtime;
use crate::loader::install_loader;
use crate::minecraft::arguments::build_launch_arguments;
use crate::minecraft::assets::{collect_vanilla_assets, copy_legacy_assets_if_needed};
use crate::minecraft::libraries::{collect_vanilla_libraries, extract_natives};
use crate::minecraft::manifest::resolve_vanilla_version;
use crate::utils::os_rules::RuleContext;
use crate::utils::zip_extract::ensure_executable;

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
}

impl Launcher {
    /// Creates a new [`Launcher`] bound to `options`.
    pub fn new(options: LaunchOptions) -> Self {
        Self { options }
    }

    /// Returns a reference to the underlying [`LaunchOptions`].
    pub fn options(&self) -> &LaunchOptions {
        &self.options
    }

    /// Resolves Java, verifies/downloads Minecraft, assets, libraries, mod loader, and custom
    /// instance bundle files, runs `check_files` if `verify: true`, extracts natives, and
    /// assembles final JVM & game arguments.
    pub async fn prepare(&self, on_event: EventCallback) -> CoreResult<PreparedLaunch> {
        if self.options.path.as_os_str().is_empty() {
            return Err(CoreError::InvalidConfig(
                "LaunchOptions.path cannot be empty".to_string(),
            ));
        }

        let game_dir = self.options.game_dir();
        fs::create_dir_all(&self.options.path).await?;
        fs::create_dir_all(self.options.path.join("assets")).await?;
        fs::create_dir_all(self.options.path.join("libraries")).await?;
        fs::create_dir_all(self.options.path.join("versions")).await?;
        fs::create_dir_all(self.options.path.join("runtime")).await?;
        fs::create_dir_all(&game_dir).await?;

        let downloader = Downloader::new(
            self.options.clamped_concurrency(),
            self.options.timeout_ms,
        );
        let bundle_manager = BundleManager::from_options(&self.options);

        // 1. Resolve Vanilla version manifest (with automatic offline local cache fallback + ARM LWJGL fixes)
        let resolved_mc = resolve_vanilla_version(
            &self.options.path,
            &self.options.version,
            downloader.client(),
        )
        .await?;
        let mc_version_id = resolved_mc.version_id;
        let version_json = resolved_mc.version_json;

        // 2. Resolve Java Runtime (custom path -> Mojang JRE manifest -> Azul Zulu API)
        let resolved_java = resolve_java_runtime(
            &self.options,
            &version_json,
            &downloader,
            &on_event,
        )
        .await?;

        // 3. Collect Vanilla libraries, client JAR, assets, and optional custom instance distribution
        let rule_ctx = RuleContext::current(self.options.intel_enabled_mac);
        let vanilla_libs = collect_vanilla_libraries(
            &self.options.path,
            &mc_version_id,
            &version_json,
            &rule_ctx,
        )?;

        let vanilla_assets = collect_vanilla_assets(
            &self.options.path,
            &version_json,
            downloader.client(),
        )
        .await?;

        let custom_dist_items = bundle_manager
            .fetch_custom_distribution(downloader.client(), self.options.url.as_deref())
            .await?;

        let mut all_bundle_items = Vec::with_capacity(
            resolved_java.bundle_files.len()
                + vanilla_libs.bundle_items.len()
                + vanilla_assets.len()
                + custom_dist_items.len(),
        );
        all_bundle_items.extend(resolved_java.bundle_files);
        all_bundle_items.extend(vanilla_libs.bundle_items);
        all_bundle_items.extend(vanilla_assets);
        all_bundle_items.extend(custom_dist_items.iter().cloned());

        // 4. Verify local hashes & download missing/corrupt files atomically before running loader processors
        let pending_downloads = bundle_manager
            .check_bundle(&all_bundle_items, &on_event)
            .await?;
        if !pending_downloads.is_empty() {
            downloader.download_many(pending_downloads, &on_event).await?;
        }

        ensure_executable(&resolved_java.binary_path)?;

        // 5. Run safe `verify` cleaner inside `game_dir` when enabled
        if self.options.verify {
            let mut valid_instance_items = custom_dist_items;
            if let Some(mcp_path) = self.options.resolved_mcp_path() {
                valid_instance_items.push(BundleItem {
                    path: mcp_path,
                    url: None,
                    sha1: None,
                    size: None,
                    category: "MCP".to_string(),
                    inline_content: None,
                    executable: false,
                });
            }
            bundle_manager.check_files(&valid_instance_items).await?;
        }

        // 6. Extract native libraries & populate legacy `<path>/resources` if needed
        extract_natives(
            &self.options.path,
            &mc_version_id,
            &vanilla_libs.native_jars,
            &on_event,
        )
        .await?;

        copy_legacy_assets_if_needed(&self.options.path, &game_dir, &version_json).await?;

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
        let full_args = prepared.full_args();

        let redacted_cmd = format_redacted_command(
            &prepared.java_path,
            &full_args,
            &self.options,
        );
        on_event(LaunchEvent::Data {
            line: format!("[minecraft-rust-core] Launching: {redacted_cmd}"),
        });

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

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        tokio::spawn(async move {
            let cb_out = on_event.clone();
            let out_task = tokio::spawn(async move {
                if let Some(out) = stdout {
                    let mut reader = BufReader::new(out).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        cb_out(LaunchEvent::Data { line });
                    }
                }
            });

            let cb_err = on_event.clone();
            let err_task = tokio::spawn(async move {
                if let Some(err) = stderr {
                    let mut reader = BufReader::new(err).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        cb_err(LaunchEvent::Data { line });
                    }
                }
            });

            let status_res = child.wait().await;
            let _ = out_task.await;
            let _ = err_task.await;

            let exit_code = match status_res {
                Ok(status) => status.code().unwrap_or(0),
                Err(err) => {
                    on_event(LaunchEvent::Error {
                        message: format!("Error waiting on Minecraft process (PID {pid}): {err}"),
                    });
                    -1
                }
            };

            on_event(LaunchEvent::Close { exit_code });
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
    let sensitive_flags = [
        "--accessToken",
        "--uuid",
        "--xuid",
        "--clientId",
    ];

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
