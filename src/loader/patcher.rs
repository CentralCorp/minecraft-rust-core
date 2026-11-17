use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::utils::maven::MavenArtifact;
use crate::utils::zip_extract::{
    read_jar_main_class, read_zip_entry_bytes, sanitize_archive_entry_path,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallDataEntry {
    #[serde(default)]
    pub client: String,
    #[serde(default)]
    pub server: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessorSpec {
    #[serde(default)]
    pub sides: Option<Vec<String>>,
    pub jar: String,
    #[serde(default)]
    pub classpath: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub outputs: Option<HashMap<String, String>>,
}

impl ProcessorSpec {
    /// Returns `true` when this processor applies to the `"client"` side.
    pub fn applies_to_client(&self) -> bool {
        match self.sides.as_ref() {
            None => true,
            Some(sides) if sides.is_empty() => true,
            Some(sides) => sides.iter().any(|s| s.eq_ignore_ascii_case("client")),
        }
    }
}

/// Execution context for resolving Forge & NeoForge `install_profile.json` placeholders and running processors.
#[derive(Debug, Clone)]
pub struct PatchContext<'a> {
    pub root_dir: &'a Path,
    pub libraries_dir: &'a Path,
    pub loader_dir: &'a Path,
    pub installer_jar: &'a Path,
    pub mc_jar: &'a Path,
    pub mc_json: &'a Path,
    pub java_bin: &'a Path,
    pub base_loader_artifact: Option<&'a MavenArtifact>,
}

/// Resolves a single `install_profile.json` token (`args` element or `data[key].client` value):
/// - `'literal'` -> unquoted literal string (never treated as a file path!)
/// - `[group:artifact:version[:classifier][@ext]]` -> `<libraries_dir>/<maven_relative_path>`
/// - `/data/...` -> extracts entry from `installer_jar` into `<loader_dir>/extracted/...` and returns its path
/// - `{PLACEHOLDER}` -> built-in (`{SIDE}`, `{MINECRAFT_JAR}`, `{MINECRAFT_VERSION}`, `{ROOT}`,
///   `{INSTALLER}` = actual installer `.jar` path, `{LIBRARY_DIR}`) or `data` map lookup.
pub fn resolve_processor_token(
    raw: &str,
    data_map: &HashMap<String, InstallDataEntry>,
    ctx: &PatchContext<'_>,
) -> CoreResult<String> {
    resolve_token_inner(raw, data_map, ctx, 0)
}

fn resolve_token_inner(
    raw: &str,
    data_map: &HashMap<String, InstallDataEntry>,
    ctx: &PatchContext<'_>,
    depth: usize,
) -> CoreResult<String> {
    if depth > 8 {
        return Err(CoreError::PatchError(format!(
            "Circular placeholder reference while resolving '{raw}'"
        )));
    }

    let trimmed = raw.trim();

    // 1. Single-quoted literal `'...'`
    if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        return Ok(trimmed[1..trimmed.len() - 1].to_string());
    }

    // 2. Exact `{KEY}` placeholder
    if trimmed.len() >= 3
        && trimmed.starts_with('{')
        && trimmed.ends_with('}')
        && !trimmed[1..trimmed.len() - 1].contains('{')
    {
        let key = &trimmed[1..trimmed.len() - 1];
        if let Some(builtin) = resolve_builtin_placeholder(key, ctx) {
            return Ok(builtin);
        }
        if let Some(entry) = data_map.get(key) {
            let resolved = resolve_token_inner(&entry.client, data_map, ctx, depth + 1)?;
            // If `BINPATCH` extracted `/data/client.lzma`, also mirror to `<libraries>/<loader>-clientdata.lzma`
            // when `base_loader_artifact` is known, without overwriting `universal.jar`!
            if key == "BINPATCH" {
                if let Some(base_art) = ctx.base_loader_artifact {
                    let lzma_dest = base_art
                        .for_clientdata_lzma()
                        .to_library_path(ctx.libraries_dir);
                    let src_path = Path::new(&resolved);
                    if src_path.exists() && src_path != lzma_dest {
                        if let Some(parent) = lzma_dest.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::copy(src_path, &lzma_dest);
                    }
                }
            }
            return Ok(resolved);
        }
        return Ok(trimmed.to_string());
    }

    // 3. Embedded `{KEY}` placeholders inside a composite argument (e.g. `-DlibraryDirectory={LIBRARY_DIR}`)
    let expanded = if trimmed.contains('{') && trimmed.contains('}') {
        expand_inline_placeholders(trimmed, data_map, ctx, depth + 1)?
    } else {
        trimmed.to_string()
    };

    // 4. Bracketed Maven coordinate `[group:artifact:version...]`
    if expanded.starts_with('[') && expanded.ends_with(']') {
        let artifact = MavenArtifact::parse(&expanded)?;
        let full_path = artifact.to_library_path(ctx.libraries_dir);
        if let Some(parent) = full_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        return Ok(full_path.to_string_lossy().to_string());
    }

    // 5. Archive-internal resource `/data/...` inside the downloaded installer JAR
    if expanded.starts_with('/') && expanded.len() > 1 {
        if let Some(extracted_path) = extract_installer_data_entry(&expanded, ctx)? {
            return Ok(extracted_path.to_string_lossy().to_string());
        }
    }

    Ok(expanded)
}

fn resolve_builtin_placeholder(key: &str, ctx: &PatchContext<'_>) -> Option<String> {
    match key {
        "SIDE" => Some("client".to_string()),
        "MINECRAFT_JAR" => Some(ctx.mc_jar.to_string_lossy().to_string()),
        "MINECRAFT_VERSION" => Some(ctx.mc_json.to_string_lossy().to_string()),
        "ROOT" => Some(ctx.root_dir.to_string_lossy().to_string()),
        // CRITICAL FIX: `{INSTALLER}` is the actual installer `.jar` file path, NOT `{ROOT}`!
        "INSTALLER" => Some(ctx.installer_jar.to_string_lossy().to_string()),
        "LIBRARY_DIR" => Some(ctx.libraries_dir.to_string_lossy().to_string()),
        _ => None,
    }
}

fn expand_inline_placeholders(
    input: &str,
    data_map: &HashMap<String, InstallDataEntry>,
    ctx: &PatchContext<'_>,
    depth: usize,
) -> CoreResult<String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut key = String::new();
            let mut closed = false;
            for next_ch in chars.by_ref() {
                if next_ch == '}' {
                    closed = true;
                    break;
                }
                key.push(next_ch);
            }
            if closed && !key.is_empty() {
                if let Some(builtin) = resolve_builtin_placeholder(&key, ctx) {
                    out.push_str(&builtin);
                } else if let Some(entry) = data_map.get(&key) {
                    let resolved = resolve_token_inner(&entry.client, data_map, ctx, depth)?;
                    out.push_str(&resolved);
                } else {
                    out.push('{');
                    out.push_str(&key);
                    out.push('}');
                }
            } else {
                out.push('{');
                out.push_str(&key);
            }
        } else {
            out.push(ch);
        }
    }

    Ok(out)
}

fn extract_installer_data_entry(
    entry_path: &str,
    ctx: &PatchContext<'_>,
) -> CoreResult<Option<PathBuf>> {
    if !ctx.installer_jar.exists() {
        return Ok(None);
    }

    let Some(rel_path) = sanitize_archive_entry_path(entry_path) else {
        return Ok(None);
    };
    let dest = ctx.loader_dir.join("extracted").join(&rel_path);
    if dest.exists() {
        return Ok(Some(dest));
    }

    let Some(bytes) = read_zip_entry_bytes(ctx.installer_jar, entry_path)? else {
        return Ok(None);
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, bytes)?;
    Ok(Some(dest))
}

/// Checks whether all client `processors` have already been run successfully for this `loader_dir`
/// and all expected output artifacts still exist on disk.
pub fn is_already_patched(
    processors: &[ProcessorSpec],
    data_map: &HashMap<String, InstallDataEntry>,
    ctx: &PatchContext<'_>,
) -> bool {
    let marker = ctx.loader_dir.join(".patched.ok");
    if !marker.exists() {
        return false;
    }

    for proc_spec in processors {
        if !proc_spec.applies_to_client() {
            continue;
        }

        if let Some(ref outputs) = proc_spec.outputs {
            for key in outputs.keys() {
                if let Ok(resolved_out) = resolve_processor_token(key, data_map, ctx) {
                    if !Path::new(&resolved_out).exists() {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        }

        // Also verify any explicit `--output <file>` target produced by the processor
        let mut iter = proc_spec.args.iter();
        while let Some(arg) = iter.next() {
            if arg == "--output" || arg == "--out-jar" {
                if let Some(target_raw) = iter.next() {
                    if let Ok(resolved_target) = resolve_processor_token(target_raw, data_map, ctx)
                    {
                        if !Path::new(&resolved_target).exists() {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
            }
        }
    }

    true
}

/// Runs all client-side `processors` sequentially via `tokio::process::Command::new(java_bin)`
/// WITHOUT invoking a shell (`cmd`/`sh`), streaming output lines as [`LaunchEvent::Patch`].
pub async fn run_processors(
    processors: &[ProcessorSpec],
    data_map: &HashMap<String, InstallDataEntry>,
    ctx: &PatchContext<'_>,
    on_event: &EventCallback,
) -> CoreResult<()> {
    let client_processors: Vec<&ProcessorSpec> = processors
        .iter()
        .filter(|p| p.applies_to_client())
        .collect();

    if client_processors.is_empty() {
        return Ok(());
    }

    if is_already_patched(processors, data_map, ctx) {
        on_event(LaunchEvent::Patch {
            line: "Mod loader processors already verified and cached.".to_string(),
        });
        return Ok(());
    }

    fs::create_dir_all(ctx.loader_dir).await?;
    let marker = ctx.loader_dir.join(".patched.ok");
    if marker.exists() {
        let _ = fs::remove_file(&marker).await;
    }

    let cp_sep = if cfg!(windows) { ";" } else { ":" };
    let total = client_processors.len();

    for (idx, proc_spec) in client_processors.iter().enumerate() {
        let jar_artifact = MavenArtifact::parse(&proc_spec.jar)?;
        let jar_path = jar_artifact.to_library_path(ctx.libraries_dir);
        if !jar_path.exists() {
            return Err(CoreError::PatchError(format!(
                "Missing processor JAR '{}' at '{}'",
                proc_spec.jar,
                jar_path.display()
            )));
        }

        let main_class = read_jar_main_class(&jar_path)?.ok_or_else(|| {
            CoreError::PatchError(format!(
                "Processor JAR '{}' does not declare a Main-Class in META-INF/MANIFEST.MF",
                jar_path.display()
            ))
        })?;

        let mut cp_entries = Vec::with_capacity(proc_spec.classpath.len() + 1);
        cp_entries.push(jar_path.to_string_lossy().to_string());
        for dep in &proc_spec.classpath {
            let dep_art = MavenArtifact::parse(dep)?;
            let dep_path = dep_art.to_library_path(ctx.libraries_dir);
            let dep_str = dep_path.to_string_lossy().to_string();
            if !cp_entries.contains(&dep_str) {
                cp_entries.push(dep_str);
            }
        }
        let cp_joined = cp_entries.join(cp_sep);

        let mut resolved_args = Vec::with_capacity(proc_spec.args.len());
        let mut prev_was_output = false;
        for raw_arg in &proc_spec.args {
            let resolved = resolve_processor_token(raw_arg, data_map, ctx)?;
            if prev_was_output {
                if let Some(parent) = Path::new(&resolved).parent() {
                    let _ = fs::create_dir_all(parent).await;
                }
            }
            prev_was_output = resolved == "--output" || resolved == "--out-jar";
            resolved_args.push(resolved);
        }

        on_event(LaunchEvent::Patch {
            line: format!(
                "[{}/{}] Running processor {} ({})...",
                idx + 1,
                total,
                jar_artifact.artifact,
                main_class
            ),
        });

        let mut cmd = Command::new(ctx.java_bin);
        cmd.arg("-cp")
            .arg(&cp_joined)
            .arg(&main_class)
            .args(&resolved_args)
            .current_dir(ctx.loader_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn().map_err(|e| {
            CoreError::PatchError(format!(
                "Failed to spawn Java '{}' for processor '{}': {e}",
                ctx.java_bin.display(),
                proc_spec.jar
            ))
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let cb_out = on_event.clone();
        let out_handle = tokio::spawn(async move {
            let mut tail = Vec::new();
            if let Some(out) = stdout {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        cb_out(LaunchEvent::Patch {
                            line: trimmed.to_string(),
                        });
                        if tail.len() >= 15 {
                            tail.remove(0);
                        }
                        tail.push(trimmed.to_string());
                    }
                }
            }
            tail
        });

        let cb_err = on_event.clone();
        let err_handle = tokio::spawn(async move {
            let mut tail = Vec::new();
            if let Some(err) = stderr {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        cb_err(LaunchEvent::Patch {
                            line: trimmed.to_string(),
                        });
                        if tail.len() >= 15 {
                            tail.remove(0);
                        }
                        tail.push(trimmed.to_string());
                    }
                }
            }
            tail
        });

        let status = child.wait().await.map_err(|e| {
            CoreError::PatchError(format!(
                "Error waiting for processor '{}': {e}",
                proc_spec.jar
            ))
        })?;

        let out_tail = out_handle.await.unwrap_or_default();
        let err_tail = err_handle.await.unwrap_or_default();

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            let combined_tail = err_tail
                .into_iter()
                .chain(out_tail)
                .collect::<Vec<_>>()
                .join(" | ");
            return Err(CoreError::PatchError(format!(
                "Processor '{}' ({main_class}) exited with code {code}: {combined_tail}",
                proc_spec.jar
            )));
        }
    }

    fs::write(&marker, b"ok\n").await?;
    Ok(())
}
