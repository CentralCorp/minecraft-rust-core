use std::path::Path;

use md5::Md5;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::error::CoreResult;

const STREAM_BUF_SIZE: usize = 64 * 1024;

/// Computes the lowercase hex SHA-1 digest of a file asynchronously without buffering the entire file in memory.
pub async fn sha1_file(path: &Path) -> CoreResult<String> {
    hash_file_with::<Sha1>(path).await
}

/// Computes the lowercase hex MD5 digest of a file asynchronously.
pub async fn md5_file(path: &Path) -> CoreResult<String> {
    hash_file_with::<Md5>(path).await
}

/// Computes the lowercase hex SHA-256 digest of a file asynchronously.
pub async fn sha256_file(path: &Path) -> CoreResult<String> {
    hash_file_with::<Sha256>(path).await
}

/// Computes the lowercase hex SHA-1 digest of an in-memory byte slice.
pub fn sha1_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Computes the lowercase hex MD5 digest of an in-memory byte slice.
pub fn md5_bytes(bytes: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Computes the lowercase hex SHA-256 digest of an in-memory byte slice.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

async fn hash_file_with<D: Digest + Default>(path: &Path) -> CoreResult<String> {
    let mut file = File::open(path).await?;
    let mut hasher = D::default();
    let mut buf = vec![0u8; STREAM_BUF_SIZE];

    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hex::encode(hasher.finalize()))
}
