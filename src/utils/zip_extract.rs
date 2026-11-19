use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::Archive;
use zip::ZipArchive;

use crate::error::CoreResult;
use crate::utils::hash_cache::clear_readonly_if_exists;

const ARCHIVE_IO_BUF_SIZE: usize = 64 * 1024;

/// Sanitizes an archive entry path to prevent ZipSlip / path-traversal attacks.
/// Returns `None` if the entry contains parent-directory (`..`) traversal, null bytes, or an empty relative path.
pub fn sanitize_archive_entry_path(raw: &str) -> Option<PathBuf> {
    if raw.contains('\0') {
        return None;
    }

    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let mut clean = PathBuf::new();
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::Normal(seg) => clean.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if clean.as_os_str().is_empty() {
        None
    } else {
        Some(clean)
    }
}

fn copy_reader_to_atomic_dest(
    reader: &mut impl Read,
    out_path: &Path,
    copy_buf: &mut [u8],
) -> CoreResult<()> {
    let file_name = out_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("entry");
    let part_path = out_path.with_file_name(format!(
        ".{file_name}.{}.part",
        std::process::id()
    ));

    let write_res = (|| -> CoreResult<()> {
        let out_file = File::create(&part_path)?;
        let mut writer = BufWriter::with_capacity(ARCHIVE_IO_BUF_SIZE, out_file);
        loop {
            let n = reader.read(copy_buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&copy_buf[..n])?;
        }
        writer.flush()?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&part_path);
        return Err(err);
    }

    clear_readonly_if_exists(out_path);
    #[cfg(windows)]
    if out_path.exists() {
        let _ = fs::remove_file(out_path);
    }

    if let Err(err) = fs::rename(&part_path, out_path) {
        let _ = fs::remove_file(&part_path);
        return Err(err.into());
    }

    Ok(())
}

/// Extracts all files from a `.zip` / `.jar` archive into `dest_dir` safely using `64 KiB`
/// buffered I/O, deduplicated parent directory creation, and atomic `.part` + `rename` writes
/// (blocking call; run inside `tokio::task::spawn_blocking` for large archives).
pub fn extract_zip_archive(
    archive_path: &Path,
    dest_dir: &Path,
    skip_meta_inf: bool,
) -> CoreResult<Vec<PathBuf>> {
    let file = File::open(archive_path)?;
    let reader = BufReader::with_capacity(ARCHIVE_IO_BUF_SIZE, file);
    let mut zip = ZipArchive::new(reader)?;
    fs::create_dir_all(dest_dir)?;

    let mut created_dirs: HashSet<PathBuf> = HashSet::new();
    created_dirs.insert(dest_dir.to_path_buf());
    let mut copy_buf = vec![0u8; ARCHIVE_IO_BUF_SIZE];
    let mut extracted = Vec::new();

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let raw_name = entry.name().to_string();

        if skip_meta_inf && raw_name.replace('\\', "/").starts_with("META-INF") {
            continue;
        }

        let Some(rel_path) = sanitize_archive_entry_path(&raw_name) else {
            continue;
        };

        let out_path = dest_dir.join(&rel_path);
        if entry.is_dir() || raw_name.ends_with('/') {
            if created_dirs.insert(out_path.clone()) {
                fs::create_dir_all(&out_path)?;
            }
            continue;
        }

        if let Some(parent) = out_path.parent() {
            if created_dirs.insert(parent.to_path_buf()) {
                fs::create_dir_all(parent)?;
            }
        }

        #[cfg(unix)]
        if entry.is_symlink() {
            let mut target_str = String::new();
            entry.read_to_string(&mut target_str)?;
            let link_target = PathBuf::from(target_str.trim());
            if !link_target
                .components()
                .any(|c| matches!(c, Component::ParentDir))
                || is_safe_relative_symlink(&out_path, &link_target, dest_dir)
            {
                let _ = fs::remove_file(&out_path);
                let _ = std::os::unix::fs::symlink(&link_target, &out_path);
                extracted.push(out_path);
                continue;
            }
        }

        let unix_mode = entry.unix_mode();
        copy_reader_to_atomic_dest(&mut entry, &out_path, &mut copy_buf)?;

        apply_extracted_permissions(&out_path, &rel_path, unix_mode)?;
        extracted.push(out_path);
    }

    Ok(extracted)
}

/// Extracts a `.tar.gz` / `.tgz` archive (such as Azul Zulu JREs on Linux/macOS) into `dest_dir`
/// safely with `64 KiB` buffered I/O, anti-traversal protection, symlink handling, and Unix executable bits.
pub fn extract_tar_gz_archive(archive_path: &Path, dest_dir: &Path) -> CoreResult<Vec<PathBuf>> {
    let file = File::open(archive_path)?;
    let buf_reader = BufReader::with_capacity(ARCHIVE_IO_BUF_SIZE, file);
    let gz = GzDecoder::new(buf_reader);
    let mut archive = Archive::new(gz);
    fs::create_dir_all(dest_dir)?;

    let mut created_dirs: HashSet<PathBuf> = HashSet::new();
    created_dirs.insert(dest_dir.to_path_buf());
    let mut copy_buf = vec![0u8; ARCHIVE_IO_BUF_SIZE];
    let mut extracted = Vec::new();

    for entry_res in archive.entries()? {
        let mut entry = entry_res?;
        let raw_path = entry.path()?.to_string_lossy().to_string();
        let Some(rel_path) = sanitize_archive_entry_path(&raw_path) else {
            continue;
        };

        let out_path = dest_dir.join(&rel_path);
        let entry_type = entry.header().entry_type();

        if entry_type.is_dir() {
            if created_dirs.insert(out_path.clone()) {
                fs::create_dir_all(&out_path)?;
            }
            continue;
        }

        if let Some(parent) = out_path.parent() {
            if created_dirs.insert(parent.to_path_buf()) {
                fs::create_dir_all(parent)?;
            }
        }

        #[cfg(unix)]
        if entry_type.is_symlink() {
            if let Some(link_name) = entry.link_name()? {
                let target = link_name.into_owned();
                if is_safe_relative_symlink(&out_path, &target, dest_dir) {
                    let _ = fs::remove_file(&out_path);
                    let _ = std::os::unix::fs::symlink(&target, &out_path);
                    extracted.push(out_path);
                }
            }
            continue;
        }

        if entry_type.is_file() {
            let mode = entry.header().mode().ok();
            copy_reader_to_atomic_dest(&mut entry, &out_path, &mut copy_buf)?;

            apply_extracted_permissions(&out_path, &rel_path, mode)?;
            extracted.push(out_path);
        }
    }

    Ok(extracted)
}

/// Reads a single entry from a `.zip` / `.jar` archive into memory, returning `Ok(None)` if absent.
pub fn read_zip_entry_bytes(archive_path: &Path, entry_name: &str) -> CoreResult<Option<Vec<u8>>> {
    let file = File::open(archive_path)?;
    let reader = BufReader::with_capacity(ARCHIVE_IO_BUF_SIZE, file);
    let mut zip = ZipArchive::new(reader)?;
    let normalized = entry_name.trim_start_matches('/').replace('\\', "/");

    for candidate in [entry_name, normalized.as_str()] {
        if let Ok(mut entry) = zip.by_name(candidate) {
            if entry.is_dir() {
                return Ok(None);
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes)?;
            return Ok(Some(bytes));
        }
    }

    Ok(None)
}

/// Extracts all entries under `prefix` (e.g. `"maven/"`) from `archive_path` into `dest_dir`,
/// stripping `prefix` from the output relative paths.
///
/// Multi-instance safe: skips overwriting existing files whose byte size already matches
/// `entry.size()`, and writes missing/mismatched files via atomic `.part` + `rename`!
pub fn extract_zip_prefix_to_dir(
    archive_path: &Path,
    prefix: &str,
    dest_dir: &Path,
) -> CoreResult<Vec<PathBuf>> {
    let file = File::open(archive_path)?;
    let reader = BufReader::with_capacity(ARCHIVE_IO_BUF_SIZE, file);
    let mut zip = ZipArchive::new(reader)?;
    let clean_prefix = prefix.trim_start_matches('/').replace('\\', "/");

    let mut created_dirs: HashSet<PathBuf> = HashSet::new();
    let mut copy_buf = vec![0u8; ARCHIVE_IO_BUF_SIZE];
    let mut extracted = Vec::new();

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        let trimmed = name.trim_start_matches('/');
        let Some(stripped) = trimmed.strip_prefix(&clean_prefix) else {
            continue;
        };
        let Some(rel_path) = sanitize_archive_entry_path(stripped) else {
            continue;
        };

        let out_path = dest_dir.join(&rel_path);
        if let Ok(meta) = fs::metadata(&out_path) {
            if meta.is_file() && meta.len() == entry.size() {
                extracted.push(out_path);
                continue;
            }
        }

        if let Some(parent) = out_path.parent() {
            if created_dirs.insert(parent.to_path_buf()) {
                fs::create_dir_all(parent)?;
            }
        }

        copy_reader_to_atomic_dest(&mut entry, &out_path, &mut copy_buf)?;
        extracted.push(out_path);
    }

    Ok(extracted)
}

/// Parses `Main-Class` from a JAR `META-INF/MANIFEST.MF` string, supporting:
/// - `\r\n`, `\n`, and `\r` line endings
/// - RFC 822 / JAR specification 72-byte line continuations (lines starting with a single space `' '`)
/// - Case-insensitive `Main-Class:` header matching
pub fn parse_manifest_main_class(manifest_text: &str) -> Option<String> {
    let normalized = manifest_text.replace("\r\n", "\n").replace('\r', "\n");
    let mut unfolded = String::with_capacity(normalized.len());

    let mut lines = normalized.split('\n');
    if let Some(first) = lines.next() {
        unfolded.push_str(first);
    }
    for line in lines {
        if let Some(continuation) = line.strip_prefix(' ') {
            unfolded.push_str(continuation);
        } else {
            unfolded.push('\n');
            unfolded.push_str(line);
        }
    }

    for line in unfolded.lines() {
        if let Some((header, value)) = line.split_once(':') {
            if header.trim().eq_ignore_ascii_case("Main-Class") {
                let class_name = value.trim();
                if !class_name.is_empty() {
                    return Some(class_name.to_string());
                }
            }
        }
    }

    None
}

/// Opens `jar_path` and extracts the `Main-Class` attribute from `META-INF/MANIFEST.MF`.
pub fn read_jar_main_class(jar_path: &Path) -> CoreResult<Option<String>> {
    let Some(bytes) = read_zip_entry_bytes(jar_path, "META-INF/MANIFEST.MF")? else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(parse_manifest_main_class(&text))
}

/// Marks a file as executable (`0o755`) on Unix platforms; no-op on Windows.
pub fn ensure_executable(path: &Path) -> CoreResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            let mut perms = fs::metadata(path)?.permissions();
            let mode = perms.mode();
            if (mode & 0o111) != 0o111 {
                perms.set_mode(mode | 0o755);
                fs::set_permissions(path, perms)?;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn apply_extracted_permissions(
    out_path: &Path,
    rel_path: &Path,
    archive_mode: Option<u32>,
) -> CoreResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let in_bin_dir = rel_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|name| name == "bin" || name == "jspawnhelper")
            .unwrap_or(false)
            || rel_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n == "jspawnhelper" || n == "java")
                .unwrap_or(false);

        let is_exec = archive_mode.map(|m| (m & 0o111) != 0).unwrap_or(false) || in_bin_dir;
        if is_exec {
            let mode = archive_mode.map(|m| (m & 0o777) | 0o755).unwrap_or(0o755);
            fs::set_permissions(out_path, fs::Permissions::from_mode(mode))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (out_path, rel_path, archive_mode);
    }
    Ok(())
}

#[cfg(unix)]
fn is_safe_relative_symlink(link_location: &Path, target: &Path, root: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let Some(mut resolved) = link_location.parent().map(Path::to_path_buf) else {
        return false;
    };
    for comp in target.components() {
        match comp {
            Component::Normal(s) => resolved.push(s),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() || !resolved.starts_with(root) {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    resolved.starts_with(root)
}
