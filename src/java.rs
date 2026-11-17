use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::fs;

use crate::bundle::BundleItem;
use crate::config::LaunchOptions;
use crate::downloader::{DownloadTask, Downloader, ExpectedHash};
use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::utils::os_rules::{TargetArch, TargetOs};
use crate::utils::zip_extract::{
    ensure_executable, extract_tar_gz_archive, extract_zip_archive, sanitize_archive_entry_path,
};

const MOJANG_JAVA_MANIFEST_URL: &str =
    "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const AZUL_ZULU_API_URL: &str = "https://api.azul.com/metadata/v1/zulu/packages/";

/// Result of Java runtime resolution.
#[derive(Debug, Clone)]
pub struct ResolvedJava {
    /// Path to the `java` (or `java.exe` / `javaw.exe`) binary.
    pub binary_path: PathBuf,
    /// Individual Mojang runtime manifest files to verify/download via [`crate::bundle::BundleManager`],
    /// if resolved via Mojang's curated manifest.
    pub bundle_files: Vec<BundleItem>,
}

/// Resolves or downloads an appropriate Java runtime for the target Minecraft version:
/// 1. Uses explicit `options.java.path` if configured.
/// 2. Queries Mojang's official Java Runtime manifest (`windows-x64/x86/arm64`,
///    `mac-os/mac-os-arm64` respecting `intel_enabled_mac`, `linux/linux-i386`) when no custom
///    Java major version override is requested.
/// 3. Falls back to Azul Zulu's Metadata API (`.zip` / `.tar.gz` extraction, macOS `.jre/Contents/Home/bin/java`
///    resolution, and Unix `0o755` executable bits).
pub async fn resolve_java_runtime(
    options: &LaunchOptions,
    mc_version_json: &serde_json::Value,
    downloader: &Downloader,
    on_event: &EventCallback,
) -> CoreResult<ResolvedJava> {
    // 1. Explicit custom Java path (or `"system"` when `embedded_java == false`)
    if let Some(ref custom_path) = options.java.path {
        if custom_path == Path::new("system") {
            if let Some(sys_bin) = locate_system_java_binary() {
                return Ok(ResolvedJava {
                    binary_path: sys_bin,
                    bundle_files: Vec::new(),
                });
            }
            // Host has no system Java on JAVA_HOME / PATH -> fall back seamlessly to embedded JRE
        } else {
            let resolved_bin = locate_java_binary_in_custom_path(custom_path)?;
            return Ok(ResolvedJava {
                binary_path: resolved_bin,
                bundle_files: Vec::new(),
            });
        }
    }

    let forced_major = options
        .java
        .version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // 2. Mojang official curated Java Runtime Manifest (when no forced version is set)
    if forced_major.is_none() {
        let component = mc_version_json
            .get("javaVersion")
            .and_then(|jv| jv.get("component"))
            .and_then(|c| c.as_str())
            .unwrap_or("jre-legacy");

        if let Some(arch_os_key) =
            mojang_runtime_platform_key(TargetOs::current(), TargetArch::current(), options.intel_enabled_mac)
        {
            match try_resolve_mojang_jre(
                &options.path,
                arch_os_key,
                component,
                downloader.client(),
            )
            .await
            {
                Ok(Some(resolved)) => return Ok(resolved),
                Ok(None) | Err(_) => {
                    // Offline check: reuse an already-installed Mojang JRE in `<path>/runtime/` if present
                    if let Some(local_bin) = find_cached_mojang_jre(&options.path, arch_os_key) {
                        return Ok(ResolvedJava {
                            binary_path: local_bin,
                            bundle_files: Vec::new(),
                        });
                    }
                }
            }
        }
    }

    // 3. Fallback to Azul Zulu API
    let major_version = forced_major
        .and_then(|v| v.parse::<u32>().ok())
        .or_else(|| {
            mc_version_json
                .get("javaVersion")
                .and_then(|jv| jv.get("majorVersion"))
                .and_then(|m| m.as_u64())
                .map(|m| m as u32)
        })
        .unwrap_or(8);

    resolve_azul_zulu_jre(options, major_version, downloader, on_event).await
}

/// Maps `(TargetOs, TargetArch, intel_enabled_mac)` to Mojang's Java Runtime manifest platform key.
pub fn mojang_runtime_platform_key(
    os: TargetOs,
    arch: TargetArch,
    intel_enabled_mac: bool,
) -> Option<&'static str> {
    match (os, arch) {
        (TargetOs::Windows, TargetArch::X86_64) => Some("windows-x64"),
        (TargetOs::Windows, TargetArch::X86) => Some("windows-x86"),
        (TargetOs::Windows, TargetArch::Arm64) => Some("windows-arm64"),
        (TargetOs::MacOs, TargetArch::X86_64) => Some("mac-os"),
        (TargetOs::MacOs, TargetArch::Arm64) => {
            if intel_enabled_mac {
                Some("mac-os")
            } else {
                Some("mac-os-arm64")
            }
        }
        (TargetOs::Linux, TargetArch::X86_64) => Some("linux"),
        (TargetOs::Linux, TargetArch::X86) => Some("linux-i386"),
        _ => None,
    }
}

async fn try_resolve_mojang_jre(
    root_path: &Path,
    arch_os: &str,
    component: &str,
    http: &reqwest::Client,
) -> CoreResult<Option<ResolvedJava>> {
    let all_json: serde_json::Value = http
        .get(MOJANG_JAVA_MANIFEST_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let Some(entry) = all_json
        .get(arch_os)
        .and_then(|plat| plat.get(component))
        .and_then(|arr| arr.as_array())
        .and_then(|arr| arr.first())
    else {
        return Ok(None);
    };

    let Some(version_name) = entry
        .get("version")
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
    else {
        return Ok(None);
    };

    let Some(manifest_url) = entry
        .get("manifest")
        .and_then(|m| m.get("url"))
        .and_then(|u| u.as_str())
    else {
        return Ok(None);
    };

    let manifest_json: serde_json::Value = http
        .get(manifest_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let Some(files_map) = manifest_json.get("files").and_then(|f| f.as_object()) else {
        return Ok(None);
    };

    // Identify prefix before `bin/java` or `bin/java.exe` / `bin/javaw.exe` (e.g. `jre.bundle/Contents/Home/` on macOS)
    let exe_candidates: &[&str] = if cfg!(windows) {
        &["bin/java.exe", "bin/javaw.exe"]
    } else {
        &["bin/java"]
    };

    let mut strip_prefix = String::new();
    let mut chosen_exe_rel: Option<String> = None;

    'outer: for exe_suffix in exe_candidates {
        for key in files_map.keys() {
            let norm = key.replace('\\', "/");
            if norm.ends_with(exe_suffix) {
                strip_prefix = norm[..norm.len() - exe_suffix.len()].to_string();
                chosen_exe_rel = Some((*exe_suffix).to_string());
                break 'outer;
            }
        }
    }

    let Some(exe_rel) = chosen_exe_rel else {
        return Ok(None);
    };

    let safe_ver = version_name.replace(['/', '\\', ':', ' '], "_");
    let runtime_base = root_path
        .join("runtime")
        .join(format!("jre-{safe_ver}-{arch_os}"));

    let mut bundle_files = Vec::with_capacity(files_map.len());
    #[allow(unused_mut)]
    let mut pending_symlinks: Vec<(PathBuf, String)> = Vec::new();

    for (raw_rel, info) in files_map {
        let norm_rel = raw_rel.replace('\\', "/");
        let stripped = norm_rel
            .strip_prefix(&strip_prefix)
            .unwrap_or(&norm_rel);
        let Some(clean_rel) = sanitize_archive_entry_path(stripped) else {
            continue;
        };

        let entry_type = info.get("type").and_then(|t| t.as_str()).unwrap_or("file");
        let dest = runtime_base.join(&clean_rel);

        match entry_type {
            "directory" => {
                let _ = fs::create_dir_all(&dest).await;
            }
            "link" => {
                if let Some(target) = info.get("target").and_then(|t| t.as_str()) {
                    pending_symlinks.push((dest, target.to_string()));
                }
            }
            _ => {
                let Some(raw_dl) = info.get("downloads").and_then(|d| d.get("raw")) else {
                    continue;
                };
                let Some(url) = raw_dl.get("url").and_then(|u| u.as_str()) else {
                    continue;
                };
                let sha1 = raw_dl
                    .get("sha1")
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
                let size = raw_dl.get("size").and_then(|s| s.as_u64());
                let executable = info
                    .get("executable")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false)
                    || stripped.starts_with("bin/")
                    || stripped.ends_with("jspawnhelper");

                bundle_files.push(BundleItem {
                    path: dest,
                    url: Some(url.to_string()),
                    sha1,
                    size,
                    category: "Java".to_string(),
                    inline_content: None,
                    executable,
                });
            }
        }
    }

    #[cfg(unix)]
    for (link_path, target_str) in pending_symlinks {
        let target_pb = PathBuf::from(&target_str);
        if !target_pb.is_absolute() {
            if let Some(parent) = link_path.parent() {
                let _ = fs::create_dir_all(parent).await;
            }
            let _ = std::fs::remove_file(&link_path);
            let _ = std::os::unix::fs::symlink(&target_pb, &link_path);
        }
    }

    Ok(Some(ResolvedJava {
        binary_path: runtime_base.join(exe_rel),
        bundle_files,
    }))
}

#[derive(Debug, Deserialize)]
struct ZuluPackage {
    name: String,
    download_url: String,
    #[serde(default)]
    sha256_hash: Option<String>,
}

async fn resolve_azul_zulu_jre(
    options: &LaunchOptions,
    major_version: u32,
    downloader: &Downloader,
    on_event: &EventCallback,
) -> CoreResult<ResolvedJava> {
    let os = TargetOs::current();
    let arch = if os == TargetOs::MacOs
        && TargetArch::current() == TargetArch::Arm64
        && options.intel_enabled_mac
    {
        TargetArch::X86_64
    } else {
        TargetArch::current()
    };

    let zulu_os = match os {
        TargetOs::Windows => "windows",
        TargetOs::MacOs => "macos",
        TargetOs::Linux => "linux",
    };
    let zulu_arch = match arch {
        TargetArch::X86_64 => "x64",
        TargetArch::X86 => "x86",
        TargetArch::Arm64 => "aarch64",
        TargetArch::Arm32 => "arm",
    };

    let runtime_dir = options
        .path
        .join("runtime")
        .join(format!("zulu-{major_version}-{zulu_os}-{zulu_arch}"));

    if let Some(existing_bin) = find_java_binary_in_dir(&runtime_dir) {
        ensure_executable(&existing_bin)?;
        return Ok(ResolvedJava {
            binary_path: existing_bin,
            bundle_files: Vec::new(),
        });
    }

    let archive_types: &[&str] = match os {
        TargetOs::Windows => &["zip"],
        TargetOs::MacOs => &["tar.gz", "zip"],
        TargetOs::Linux => &["tar.gz", "zip"],
    };

    let pkg_type = if options.java.image_type.trim().is_empty() {
        "jre"
    } else {
        options.java.image_type.trim()
    };

    let mut chosen_pkg: Option<(ZuluPackage, &'static str)> = None;
    for &archive_ext in archive_types {
        let query_url = format!(
            "{AZUL_ZULU_API_URL}?java_version={major_version}&os={zulu_os}&arch={zulu_arch}&archive_type={archive_ext}&java_package_type={pkg_type}&latest=true&release_status=ga&availability_types=CA"
        );
        let resp = downloader.client().get(&query_url).send().await?;
        if !resp.status().is_success() {
            continue;
        }
        let pkgs: Vec<ZuluPackage> = resp.json().await.unwrap_or_default();
        if let Some(first) = pkgs.into_iter().next() {
            chosen_pkg = Some((first, archive_ext));
            break;
        }
    }

    let Some((pkg, archive_ext)) = chosen_pkg else {
        return Err(CoreError::JavaNotFound(format!(
            "No Azul Zulu {pkg_type} package found for Java {major_version} on {zulu_os}/{zulu_arch}"
        )));
    };

    fs::create_dir_all(&runtime_dir).await?;
    let archive_path = runtime_dir.join(&pkg.name);

    if !archive_path.exists() {
        let task = DownloadTask {
            url: pkg.download_url.clone(),
            destination: archive_path.clone(),
            size: None,
            expected_hash: None,
            category: pkg.name.clone(),
            executable: false,
        };
        downloader.download_single(&task, on_event).await?;
    }

    if let Some(expected_sha256) = pkg.sha256_hash.as_ref().filter(|s| !s.trim().is_empty()) {
        let actual = crate::utils::hash::sha256_file(&archive_path).await?;
        if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
            let _ = fs::remove_file(&archive_path).await;
            return Err(CoreError::HashMismatch {
                path: archive_path.display().to_string(),
                expected: expected_sha256.clone(),
                actual,
            });
        }
    }

    on_event(LaunchEvent::Extract {
        message: format!("Extracting Java runtime {}...", pkg.name),
    });

    let archive_clone = archive_path.clone();
    let dest_clone = runtime_dir.clone();
    let is_targz = archive_ext == "tar.gz"
        || pkg.name.ends_with(".tar.gz")
        || pkg.name.ends_with(".tgz");

    tokio::task::spawn_blocking(move || {
        if is_targz {
            extract_tar_gz_archive(&archive_clone, &dest_clone)
        } else {
            extract_zip_archive(&archive_clone, &dest_clone, true)
        }
    })
    .await
    .map_err(|e| CoreError::JavaNotFound(format!("Java extraction task panicked: {e}")))??;

    let _ = fs::remove_file(&archive_path).await;

    // Preserve ExpectedHash import used in tests/callers
    let _ = ExpectedHash::Sha1;

    let java_bin = find_java_binary_in_dir(&runtime_dir).ok_or_else(|| {
        CoreError::JavaNotFound(format!(
            "Extracted Azul Zulu archive into '{}', but could not locate bin/java executable",
            runtime_dir.display()
        ))
    })?;

    ensure_executable(&java_bin)?;

    Ok(ResolvedJava {
        binary_path: java_bin,
        bundle_files: Vec::new(),
    })
}

fn locate_java_binary_in_custom_path(custom: &Path) -> CoreResult<PathBuf> {
    if custom.is_file() {
        ensure_executable(custom)?;
        return Ok(custom.to_path_buf());
    }

    let bin_names: &[&str] = if cfg!(windows) {
        &["java.exe", "javaw.exe"]
    } else {
        &["java"]
    };

    for name in bin_names {
        let direct = custom.join(name);
        if direct.is_file() {
            ensure_executable(&direct)?;
            return Ok(direct);
        }
        let in_bin = custom.join("bin").join(name);
        if in_bin.is_file() {
            ensure_executable(&in_bin)?;
            return Ok(in_bin);
        }
        let macos_home = custom.join("Contents").join("Home").join("bin").join(name);
        if macos_home.is_file() {
            ensure_executable(&macos_home)?;
            return Ok(macos_home);
        }
    }

    // If path hasn't been created yet or is a system command name like `"java"`, allow it through
    if custom.components().count() == 1 {
        return Ok(custom.to_path_buf());
    }

    Err(CoreError::JavaNotFound(format!(
        "Custom Java binary not found at '{}'",
        custom.display()
    )))
}

fn locate_system_java_binary() -> Option<PathBuf> {
    if let Some(java_home) = std::env::var_os("JAVA_HOME") {
        let home_path = PathBuf::from(java_home);
        if let Some(bin) = find_java_binary_in_dir(&home_path) {
            let _ = ensure_executable(&bin);
            return Some(bin);
        }
    }

    let bin_names: &[&str] = if cfg!(windows) {
        &["javaw.exe", "java.exe"]
    } else {
        &["java"]
    };

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for exe in bin_names {
                let candidate = dir.join(exe);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

fn find_cached_mojang_jre(root_path: &Path, arch_os: &str) -> Option<PathBuf> {
    let runtime_root = root_path.join("runtime");
    let entries = std::fs::read_dir(&runtime_root).ok()?;
    let suffix = format!("-{arch_os}");

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("jre-") && name.ends_with(&suffix) {
            if let Some(bin) = find_java_binary_in_dir(&entry.path()) {
                return Some(bin);
            }
        }
    }
    None
}

/// Searches up to 4 directory levels under `root` for `bin/java` (`bin/java.exe` / `bin/javaw.exe`
/// on Windows) or macOS `.jre/Contents/Home/bin/java` (including text-file pseudo-symlinks in `.zip` distributions).
pub fn find_java_binary_in_dir(root: &Path) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }

    let exe_names: &[&str] = if cfg!(windows) {
        &["java.exe", "javaw.exe"]
    } else {
        &["java"]
    };

    for exe in exe_names {
        let direct = root.join("bin").join(exe);
        if direct.is_file() {
            return Some(direct);
        }
        let contents_home = root.join("Contents").join("Home").join("bin").join(exe);
        if contents_home.is_file() {
            return Some(contents_home);
        }
    }

    let read_dir = std::fs::read_dir(root).ok()?;
    for entry in read_dir.flatten() {
        let sub = entry.path();
        if !sub.is_dir() {
            continue;
        }

        // Check macOS Zulu `.zip` text-file symlink `bin` -> `zulu-XX.jre/Contents/Home/bin`
        let bin_candidate = sub.join("bin");
        if bin_candidate.is_file() {
            if let Ok(link_rel) = std::fs::read_to_string(&bin_candidate) {
                let target_bin = sub.join(link_rel.trim()).join("java");
                if target_bin.is_file() {
                    return Some(target_bin);
                }
            }
        }

        for exe in exe_names {
            let p1 = sub.join("bin").join(exe);
            if p1.is_file() {
                return Some(p1);
            }
            let p2 = sub.join("Contents").join("Home").join("bin").join(exe);
            if p2.is_file() {
                return Some(p2);
            }

            if let Ok(sub_entries) = std::fs::read_dir(&sub) {
                for sub_entry in sub_entries.flatten() {
                    let p3 = sub_entry
                        .path()
                        .join("Contents")
                        .join("Home")
                        .join("bin")
                        .join(exe);
                    if p3.is_file() {
                        return Some(p3);
                    }
                }
            }
        }
    }

    None
}
