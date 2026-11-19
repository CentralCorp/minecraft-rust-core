use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

pub mod hash;
pub mod hash_cache;
pub mod maven;
pub mod os_rules;
pub mod zip_extract;

/// Maximum bytes read into memory per line segment before flushing (`64 KiB`).
pub(crate) const MAX_LOG_LINE_BYTES: u64 = 64 * 1024;

/// Percent-encodes all non-unreserved bytes (`A-Z`, `a-z`, `0-9`, `-`, `_`, `.`, `~`) per RFC 3986.
pub(crate) fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Reads the next `\n`-delimited chunk (capped at `64 KiB` per read) from `reader` and decodes it
/// with `String::from_utf8_lossy` so non-UTF-8 `CP1252`/`CP850` bytes on localized systems never
/// abort log streaming.
pub(crate) async fn read_lossy_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    buf.clear();
    let mut limited = tokio::io::AsyncReadExt::take(&mut *reader, MAX_LOG_LINE_BYTES);
    let n = limited.read_until(b'\n', buf).await?;
    if n == 0 {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(buf).into_owned()))
}

/// Extracts the process exit code, returning `128 + signal` on Unix when the child was terminated
/// by a signal (`SIGSEGV`, `SIGKILL`, `SIGTERM`, etc.) instead of `0` or `-1`.
pub(crate) fn exit_status_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    -1
}
