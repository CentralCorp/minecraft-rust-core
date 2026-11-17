use serde::Serialize;

/// Unified result alias for all `minecraft-rust-core` operations.
pub type CoreResult<T> = Result<T, CoreError>;

/// Comprehensive error type for authentication, networking, file verification,
/// Java resolution, mod loader installation, server ping, and game process execution.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("ZIP archive error: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("Authentication error ({reason}): {message}")]
    Auth { reason: String, message: String },

    #[error("Xbox Live / XSTS error (XErr {xerr}): {message}")]
    XboxXsts { xerr: u64, message: String },

    #[error("Operation cancelled: {0}")]
    Cancelled(String),

    #[error("Integrity check failed for '{path}': expected {expected}, got {actual}")]
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("Download failed for '{url}' (HTTP {status:?}): {message}")]
    DownloadFailed {
        url: String,
        status: Option<u16>,
        message: String,
    },

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Minecraft version not found: {0}")]
    VersionNotFound(String),

    #[error("Java runtime error: {0}")]
    JavaNotFound(String),

    #[error("Mod loader error: {0}")]
    LoaderError(String),

    #[error("Mod loader processor patch error: {0}")]
    PatchError(String),

    #[error("Server status ping error: {0}")]
    StatusError(String),

    #[error("Game process error: {0}")]
    ProcessError(String),

    #[error("{0}")]
    Other(String),
}

impl CoreError {
    /// Machine-readable error category code.
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::Io(_) => "io_error",
            CoreError::Http(_) => "http_error",
            CoreError::Json(_) => "json_error",
            CoreError::Url(_) => "url_error",
            CoreError::Zip(_) => "zip_error",
            CoreError::Auth { .. } => "auth_error",
            CoreError::XboxXsts { .. } => "xbox_xsts_error",
            CoreError::Cancelled(_) => "cancelled",
            CoreError::HashMismatch { .. } => "hash_mismatch",
            CoreError::DownloadFailed { .. } => "download_failed",
            CoreError::InvalidConfig(_) => "invalid_config",
            CoreError::VersionNotFound(_) => "version_not_found",
            CoreError::JavaNotFound(_) => "java_not_found",
            CoreError::LoaderError(_) => "loader_error",
            CoreError::PatchError(_) => "patch_error",
            CoreError::StatusError(_) => "status_error",
            CoreError::ProcessError(_) => "process_error",
            CoreError::Other(_) => "other_error",
        }
    }

    /// Returns the specific authentication reason slug if this is a `CoreError::Auth`.
    pub fn auth_reason(&self) -> Option<&str> {
        match self {
            CoreError::Auth { reason, .. } => Some(reason.as_str()),
            _ => None,
        }
    }
}

impl Serialize for CoreError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
