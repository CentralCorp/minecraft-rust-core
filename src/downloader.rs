use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use md5::Md5;
use sha1::{Digest, Sha1};
use tokio::fs::{self, File};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Semaphore;

use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::utils::hash_cache::{clear_readonly_if_exists, FileHashCache};
use crate::utils::zip_extract::ensure_executable;

const DEFAULT_MAX_RETRIES: usize = 3;
const WRITE_BUF_CAPACITY: usize = 64 * 1024;

/// Expected cryptographic checksum verified before renaming `.part` -> final destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedHash {
    Sha1(String),
    Md5(String),
}

/// Description of a single file to download atomically.
#[derive(Debug, Clone)]
pub struct DownloadTask {
    pub url: String,
    pub destination: PathBuf,
    pub size: Option<u64>,
    pub expected_hash: Option<ExpectedHash>,
    pub category: String,
    pub executable: bool,
}

impl DownloadTask {
    pub fn new(url: impl Into<String>, destination: PathBuf, category: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            destination,
            size: None,
            expected_hash: None,
            category: category.into(),
            executable: false,
        }
    }
}

/// Bounded-concurrency async downloader using `tokio::sync::Semaphore`, `64 KiB` buffered atomic
/// `.part` files, streaming SHA-1/MD5 verification, HTTP `2xx` validation, deduplication by
/// destination path, incremental [`FileHashCache`] updates, and rolling speed / ETA telemetry.
#[derive(Debug, Clone)]
pub struct Downloader {
    http: reqwest::Client,
    concurrency: usize,
    max_retries: usize,
    chunk_timeout: Duration,
    hash_cache: Option<FileHashCache>,
}

impl Default for Downloader {
    fn default() -> Self {
        Self::new(12, 10_000)
    }
}

impl Downloader {
    /// Creates a new [`Downloader`] with bounded concurrency (`1..=64`) and connect/read idle timeout.
    pub fn new(concurrency: usize, timeout_ms: u64) -> Self {
        let clamped_concurrency = concurrency.clamp(1, 64);
        let connect_timeout = Duration::from_millis(timeout_ms.max(2_000));
        let chunk_timeout = connect_timeout.max(Duration::from_secs(30));

        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(chunk_timeout)
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(60))
            .user_agent(concat!("minecraft-rust-core/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| crate::shared_http_client());

        Self {
            http,
            concurrency: clamped_concurrency,
            max_retries: DEFAULT_MAX_RETRIES,
            chunk_timeout,
            hash_cache: None,
        }
    }

    /// Creates a [`Downloader`] reusing an existing [`reqwest::Client`] connection pool.
    pub fn with_client(http: reqwest::Client, concurrency: usize, max_retries: u32) -> Self {
        Self {
            http,
            concurrency: concurrency.clamp(1, 64),
            max_retries: (max_retries as usize).max(1),
            chunk_timeout: Duration::from_secs(30),
            hash_cache: None,
        }
    }

    /// Overrides the per-chunk stream idle timeout in milliseconds.
    pub fn with_chunk_timeout(mut self, timeout_ms: u64) -> Self {
        self.chunk_timeout = Duration::from_millis(timeout_ms.max(2_000));
        self
    }

    /// Attaches a [`FileHashCache`] so every verified download immediately records its
    /// `(size, mtime_secs, mtime_nanos) -> sha1` entry without re-hashing on the next launch.
    pub fn with_hash_cache(mut self, hash_cache: FileHashCache) -> Self {
        self.hash_cache = Some(hash_cache);
        self
    }

    /// Returns a [`Downloader`] configured to bypass environment HTTP proxies (useful for local servers/tests).
    pub fn with_no_proxy(mut self) -> Self {
        let timeout = self.chunk_timeout;
        self.http = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(60))
            .user_agent(concat!("minecraft-rust-core/", env!("CARGO_PKG_VERSION")))
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        self
    }

    /// Returns a reference to the underlying `reqwest::Client`.
    pub fn client(&self) -> &reqwest::Client {
        &self.http
    }

    /// Returns the configured maximum concurrent downloads (`1..=64`).
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Returns the attached [`FileHashCache`], if any.
    pub fn hash_cache(&self) -> Option<&FileHashCache> {
        self.hash_cache.as_ref()
    }

    /// Downloads a single file atomically to `destination`, emitting [`LaunchEvent::Progress`].
    pub async fn download_single(
        &self,
        task: &DownloadTask,
        on_event: &EventCallback,
    ) -> CoreResult<()> {
        let downloaded = Arc::new(AtomicU64::new(0));
        let last_emit_ms = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1)));
        let total_hint = task.size.unwrap_or(0);

        self.download_with_retries(task, downloaded, total_hint, last_emit_ms, false, on_event)
            .await?;

        if let Some(ref cache) = self.hash_cache {
            let _ = cache.save().await;
        }
        Ok(())
    }

    /// Downloads a batch of files concurrently, deduplicating identical destination paths and
    /// parent `create_dir_all` calls upfront.
    pub async fn download_many(
        &self,
        tasks: Vec<DownloadTask>,
        on_event: &EventCallback,
    ) -> CoreResult<()> {
        let deduped = deduplicate_tasks(tasks);
        if deduped.is_empty() {
            return Ok(());
        }

        // Deduplicate parent directory creation across the entire batch before spawning workers
        let mut unique_parents: HashSet<PathBuf> = HashSet::new();
        for task in &deduped {
            if let Some(parent) = task.destination.parent() {
                if !parent.as_os_str().is_empty() {
                    unique_parents.insert(parent.to_path_buf());
                }
            }
        }
        for parent in unique_parents {
            fs::create_dir_all(&parent).await?;
        }

        let known_total: u64 = deduped.iter().filter_map(|t| t.size).sum();
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let downloaded_total = Arc::new(AtomicU64::new(0));
        let total_bytes = Arc::new(AtomicU64::new(known_total));
        let last_progress_emit = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1)));

        let speed_handle = spawn_speed_reporter(
            Arc::clone(&downloaded_total),
            Arc::clone(&total_bytes),
            Arc::clone(on_event),
        );

        let mut join_set = tokio::task::JoinSet::new();

        for task in deduped {
            let sem = Arc::clone(&semaphore);
            let dl = self.clone();
            let dl_bytes = Arc::clone(&downloaded_total);
            let tot_bytes = Arc::clone(&total_bytes);
            let last_emit = Arc::clone(&last_progress_emit);
            let cb = Arc::clone(on_event);

            join_set.spawn(async move {
                let _permit = sem
                    .acquire_owned()
                    .await
                    .map_err(|e| CoreError::Other(format!("Download semaphore closed: {e}")))?;

                dl.download_with_retries_shared(
                    &task,
                    &dl_bytes,
                    &tot_bytes,
                    &last_emit,
                    true,
                    &cb,
                )
                .await
            });
        }

        let mut first_err: Option<CoreError> = None;
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    if first_err.is_none() {
                        on_event(LaunchEvent::Error {
                            message: err.to_string(),
                        });
                        first_err = Some(err);
                    }
                    join_set.abort_all();
                }
                Err(join_err) if !join_err.is_cancelled() => {
                    if first_err.is_none() {
                        first_err = Some(CoreError::Other(format!(
                            "Download task panicked: {join_err}"
                        )));
                    }
                }
                Err(_) => {}
            }
        }

        speed_handle.abort();

        if let Some(ref cache) = self.hash_cache {
            let _ = cache.save().await;
        }

        if let Some(err) = first_err {
            return Err(err);
        }

        let final_dl = downloaded_total.load(Ordering::Relaxed);
        let final_tot = total_bytes.load(Ordering::Relaxed).max(final_dl);
        on_event(LaunchEvent::Progress {
            downloaded: final_dl,
            total: final_tot,
            element: "complete".to_string(),
        });

        Ok(())
    }

    /// Internal helper used by pipelined package downloaders to share byte/progress counters
    /// across independent per-package tasks without aborting sibling packages on single-package failure.
    pub(crate) async fn download_task_shared(
        &self,
        task: &DownloadTask,
        downloaded_total: &Arc<AtomicU64>,
        total_bytes: &Arc<AtomicU64>,
        last_emit: &Arc<Mutex<Instant>>,
        parent_precreated: bool,
        on_event: &EventCallback,
    ) -> CoreResult<()> {
        self.download_with_retries_shared(
            task,
            downloaded_total,
            total_bytes,
            last_emit,
            parent_precreated,
            on_event,
        )
        .await
    }

    /// Probes `url` with an HTTP `HEAD` request (falling back to `GET Range: bytes=0-0` when
    /// Maven CDNs disallow `HEAD`) and returns `Some((url, size))` if `2xx`.
    pub async fn probe_url(&self, url: &str) -> Option<(String, u64)> {
        if let Ok(resp) = self.http.head(url).send().await {
            if resp.status().is_success() {
                let size = resp.content_length().unwrap_or(0);
                return Some((url.to_string(), size));
            }
        }

        if let Ok(resp) = self
            .http
            .get(url)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
        {
            if resp.status().is_success() || resp.status().as_u16() == 206 {
                let total_from_range = resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.rsplit_once('/'))
                    .and_then(|(_, tot)| tot.parse::<u64>().ok());
                let size = total_from_range.or_else(|| resp.content_length()).unwrap_or(0);
                return Some((url.to_string(), size));
            }
        }

        None
    }

    /// Searches a list of Maven mirrors in order and returns the first mirror serving `relative_maven_path`.
    pub async fn resolve_maven_mirror(
        &self,
        relative_maven_path: &str,
        mirrors: &[&str],
    ) -> Option<(String, u64)> {
        let clean_rel = relative_maven_path.trim_start_matches('/');
        for mirror in mirrors {
            let candidate = format!("{}/{}", mirror.trim_end_matches('/'), clean_rel);
            if let Some(hit) = self.probe_url(&candidate).await {
                return Some(hit);
            }
        }
        None
    }

    async fn download_with_retries(
        &self,
        task: &DownloadTask,
        downloaded: Arc<AtomicU64>,
        total_hint: u64,
        last_emit: Arc<Mutex<Instant>>,
        parent_precreated: bool,
        on_event: &EventCallback,
    ) -> CoreResult<()> {
        let total = Arc::new(AtomicU64::new(total_hint));
        self.download_with_retries_shared(
            task,
            &downloaded,
            &total,
            &last_emit,
            parent_precreated,
            on_event,
        )
        .await
    }

    async fn download_with_retries_shared(
        &self,
        task: &DownloadTask,
        downloaded_total: &Arc<AtomicU64>,
        total_bytes: &Arc<AtomicU64>,
        last_emit: &Arc<Mutex<Instant>>,
        parent_precreated: bool,
        on_event: &EventCallback,
    ) -> CoreResult<()> {
        let mut last_error: Option<CoreError> = None;
        let max_attempts = self.max_retries.max(1);

        for attempt in 1..=max_attempts {
            let bytes_added_this_attempt = AtomicU64::new(0);
            let added_total_this_attempt = AtomicU64::new(0);

            match self
                .execute_single_attempt(
                    task,
                    downloaded_total,
                    total_bytes,
                    &bytes_added_this_attempt,
                    &added_total_this_attempt,
                    last_emit,
                    parent_precreated,
                    on_event,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(err) => {
                    let partial = bytes_added_this_attempt.load(Ordering::Relaxed);
                    if partial > 0 {
                        downloaded_total.fetch_sub(partial, Ordering::Relaxed);
                    }
                    let extra_tot = added_total_this_attempt.load(Ordering::Relaxed);
                    if extra_tot > 0 {
                        total_bytes.fetch_sub(extra_tot, Ordering::Relaxed);
                    }

                    last_error = Some(err);
                    if attempt < max_attempts {
                        tokio::time::sleep(Duration::from_millis(250 * (attempt as u64))).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| CoreError::DownloadFailed {
            url: task.url.clone(),
            status: None,
            message: "Exhausted download retries".to_string(),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_single_attempt(
        &self,
        task: &DownloadTask,
        downloaded_total: &Arc<AtomicU64>,
        total_bytes: &Arc<AtomicU64>,
        bytes_added_this_attempt: &AtomicU64,
        added_total_this_attempt: &AtomicU64,
        last_emit: &Arc<Mutex<Instant>>,
        parent_precreated: bool,
        on_event: &EventCallback,
    ) -> CoreResult<()> {
        if !parent_precreated {
            if let Some(parent) = task.destination.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent).await?;
                }
            }
        }

        let part_path = build_part_path(&task.destination);
        let resp = self.http.get(&task.url).send().await?;
        let status = resp.status();

        if !status.is_success() {
            return Err(CoreError::DownloadFailed {
                url: task.url.clone(),
                status: Some(status.as_u16()),
                message: format!("Server returned non-2xx HTTP status {status}"),
            });
        }

        if task.size.unwrap_or(0) == 0 {
            if let Some(cl) = resp.content_length() {
                if cl > 0 {
                    total_bytes.fetch_add(cl, Ordering::Relaxed);
                    added_total_this_attempt.store(cl, Ordering::Relaxed);
                }
            }
        }

        let file = File::create(&part_path).await?;
        let mut writer = BufWriter::with_capacity(WRITE_BUF_CAPACITY, file);
        let need_sha1 = matches!(task.expected_hash, Some(ExpectedHash::Sha1(_)))
            || self.hash_cache.is_some();
        let mut sha1_hasher = need_sha1.then(Sha1::new);
        let mut md5_hasher =
            matches!(task.expected_hash, Some(ExpectedHash::Md5(_))).then(Md5::new);

        let mut stream = resp.bytes_stream();

        while let Some(chunk_res) = tokio::time::timeout(self.chunk_timeout, stream.next())
            .await
            .map_err(|_| {
                let part_cleanup = part_path.clone();
                std::mem::drop(std::fs::remove_file(part_cleanup));
                CoreError::DownloadFailed {
                    url: task.url.clone(),
                    status: None,
                    message: "Timed out reading response stream chunk".to_string(),
                }
            })?
        {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(e) => {
                    drop(writer);
                    let _ = fs::remove_file(&part_path).await;
                    return Err(CoreError::Http(e));
                }
            };

            if let Some(ref mut h) = sha1_hasher {
                h.update(&chunk);
            }
            if let Some(ref mut h) = md5_hasher {
                h.update(&chunk);
            }

            if let Err(io_err) = writer.write_all(&chunk).await {
                drop(writer);
                let _ = fs::remove_file(&part_path).await;
                return Err(CoreError::Io(io_err));
            }

            let len = chunk.len() as u64;
            bytes_added_this_attempt.fetch_add(len, Ordering::Relaxed);
            let cur_dl = downloaded_total.fetch_add(len, Ordering::Relaxed) + len;
            let cur_tot = total_bytes.load(Ordering::Relaxed).max(cur_dl);

            let should_emit = {
                if let Ok(mut guard) = last_emit.lock() {
                    if guard.elapsed() >= Duration::from_millis(45) {
                        *guard = Instant::now();
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if should_emit {
                on_event(LaunchEvent::Progress {
                    downloaded: cur_dl,
                    total: cur_tot,
                    element: task.category.clone(),
                });
            }
        }

        // Flush buffered writes and close file handle BEFORE atomic rename (no per-file fsync!)
        if let Err(e) = writer.flush().await {
            drop(writer);
            let _ = fs::remove_file(&part_path).await;
            return Err(CoreError::Io(e));
        }
        drop(writer);

        let computed_sha1 = sha1_hasher.map(|h| hex::encode(h.finalize()));

        // Verify cryptographic hash BEFORE atomic rename
        if let Some(ref expected) = task.expected_hash {
            match expected {
                ExpectedHash::Sha1(exp_hex) => {
                    let actual_hex = computed_sha1.clone().unwrap_or_default();
                    if !actual_hex.eq_ignore_ascii_case(exp_hex.trim()) {
                        let _ = fs::remove_file(&part_path).await;
                        return Err(CoreError::HashMismatch {
                            path: task.destination.display().to_string(),
                            expected: exp_hex.clone(),
                            actual: actual_hex,
                        });
                    }
                }
                ExpectedHash::Md5(exp_hex) => {
                    let actual_hex = hex::encode(md5_hasher.unwrap().finalize());
                    if !actual_hex.eq_ignore_ascii_case(exp_hex.trim()) {
                        let _ = fs::remove_file(&part_path).await;
                        return Err(CoreError::HashMismatch {
                            path: task.destination.display().to_string(),
                            expected: exp_hex.clone(),
                            actual: actual_hex,
                        });
                    }
                }
            }
        }

        clear_readonly_if_exists(&task.destination);
        #[cfg(windows)]
        if task.destination.exists() {
            let _ = fs::remove_file(&task.destination).await;
        }

        if let Err(rename_err) = fs::rename(&part_path, &task.destination).await {
            let _ = fs::remove_file(&part_path).await;
            return Err(CoreError::Io(rename_err));
        }

        if task.executable {
            ensure_executable(&task.destination)?;
        }

        if let (Some(ref cache), Some(sha1_hex)) = (&self.hash_cache, computed_sha1) {
            cache.record_path(&task.destination, sha1_hex);
        }

        let cur_dl = downloaded_total.load(Ordering::Relaxed);
        let cur_tot = total_bytes.load(Ordering::Relaxed).max(cur_dl);
        on_event(LaunchEvent::Progress {
            downloaded: cur_dl,
            total: cur_tot,
            element: task.category.clone(),
        });

        Ok(())
    }
}

pub(crate) fn spawn_speed_reporter(
    ticker_downloaded: Arc<AtomicU64>,
    ticker_total: Arc<AtomicU64>,
    ticker_cb: EventCallback,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.tick().await;
        let mut prev_bytes = 0u64;
        let mut prev_instant = Instant::now();
        let mut window: VecDeque<f64> = VecDeque::with_capacity(6);

        loop {
            interval.tick().await;
            let now = Instant::now();
            let dt = now.duration_since(prev_instant).as_secs_f64();
            if dt <= 0.0 {
                continue;
            }
            let cur_bytes = ticker_downloaded.load(Ordering::Relaxed);
            let delta = cur_bytes.saturating_sub(prev_bytes) as f64;
            prev_bytes = cur_bytes;
            prev_instant = now;

            if window.len() >= 5 {
                window.pop_front();
            }
            window.push_back(delta / dt);

            let avg_speed = window.iter().sum::<f64>() / (window.len() as f64);
            let speed_u64 = avg_speed.round().max(0.0) as u64;
            ticker_cb(LaunchEvent::Speed {
                bytes_per_sec: speed_u64,
            });

            let tot = ticker_total.load(Ordering::Relaxed);
            if avg_speed > 1.0 && tot > cur_bytes {
                let eta = ((tot - cur_bytes) as f64) / avg_speed;
                if eta.is_finite() {
                    ticker_cb(LaunchEvent::Estimated {
                        seconds_remaining: eta,
                    });
                }
            } else if tot > 0 && cur_bytes >= tot {
                ticker_cb(LaunchEvent::Estimated {
                    seconds_remaining: 0.0,
                });
            }
        }
    })
}

/// Deduplicates [`DownloadTask`] entries by normalized destination path so two concurrent tasks
/// never write to the same file simultaneously.
pub fn deduplicate_tasks(tasks: Vec<DownloadTask>) -> Vec<DownloadTask> {
    let mut seen = HashSet::with_capacity(tasks.len());
    let mut unique = Vec::with_capacity(tasks.len());

    for task in tasks {
        if task.url.trim().is_empty() {
            continue;
        }
        let key = normalize_dest_key(&task.destination);
        if seen.insert(key) {
            unique.push(task);
        }
    }

    unique
}

fn normalize_dest_key(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        raw.to_ascii_lowercase()
    } else {
        raw
    }
}

fn build_part_path(destination: &Path) -> PathBuf {
    let file_name = destination
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    destination.with_file_name(format!(".{file_name}.part"))
}
