use std::net::IpAddr;
use std::time::{Duration, Instant};

use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::TokioAsyncResolver;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{CoreError, CoreResult};

const DEFAULT_PING_TIMEOUT_MS: u64 = 3_000;
const MAX_STATUS_PACKET_BYTES: usize = 4 * 1024 * 1024;
/// Minecraft Server List Ping protocol version (`-1` = unspecified/any version per protocol spec).
const PING_PROTOCOL_VERSION: i32 = -1;

/// Normalized Minecraft Java Server List Ping response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatusResponse {
    pub online: bool,
    pub latency_ms: u64,
    pub version: String,
    pub players_online: u32,
    pub players_max: u32,
    pub motd: String,
    pub favicon: Option<String>,
}

/// Async TCP Minecraft Server List Ping (`0x00` Handshake + `0x00` Status Request) client
/// with automatic `_minecraft._tcp.{host}` DNS SRV resolution and exact UTF-8 VarInt framing.
#[derive(Debug, Clone)]
pub struct ServerStatusClient {
    host: String,
    port: u16,
    timeout: Duration,
    protocol_version: i32,
}

impl ServerStatusClient {
    /// Creates a new [`ServerStatusClient`] targeting `host:port` (default timeout: `3000ms`).
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            timeout: Duration::from_millis(DEFAULT_PING_TIMEOUT_MS),
            protocol_version: PING_PROTOCOL_VERSION,
        }
    }

    /// Sets the connection and response read timeout in milliseconds.
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout = Duration::from_millis(timeout_ms.max(200));
        self
    }

    /// Overrides the Minecraft protocol version sent in the Handshake packet.
    pub fn with_protocol_version(mut self, protocol_version: i32) -> Self {
        self.protocol_version = protocol_version;
        self
    }

    /// Performs optional DNS SRV lookup (`_minecraft._tcp.{host}`) when `port == 25565` and `host`
    /// is a domain name, connects over TCP, exchanges the Minecraft Status Handshake + Request,
    /// and parses the complete status JSON payload (including multi-packet base64 favicons).
    pub async fn ping(&self) -> CoreResult<ServerStatusResponse> {
        let trimmed_host = self.host.trim();
        if trimmed_host.is_empty() {
            return Err(CoreError::InvalidConfig(
                "Server host cannot be empty".to_string(),
            ));
        }

        let (connect_host, connect_port) = self.resolve_srv_target(trimmed_host).await;
        let addr_str = format!("{connect_host}:{connect_port}");

        let start_connect = Instant::now();
        let mut stream = tokio::time::timeout(self.timeout, TcpStream::connect(&addr_str))
            .await
            .map_err(|_| {
                CoreError::StatusError(format!(
                    "Timed out connecting to Minecraft server {addr_str}"
                ))
            })?
            .map_err(|e| {
                CoreError::StatusError(format!("Failed to connect to {addr_str}: {e}"))
            })?;

        let _ = stream.set_nodelay(true);

        // Build Handshake (0x00) + Status Request (0x00) frames
        let request_bytes =
            build_status_handshake_and_request(&connect_host, connect_port, self.protocol_version);

        let rtt_start = Instant::now();
        tokio::time::timeout(self.timeout, stream.write_all(&request_bytes))
            .await
            .map_err(|_| CoreError::StatusError(format!("Timed out sending handshake to {addr_str}")))?
            .map_err(CoreError::Io)?;

        // Read until full framed packet `reading_buf.len() >= packet_len + header_offset` arrives
        let json_str = tokio::time::timeout(self.timeout, read_status_response_json(&mut stream))
            .await
            .map_err(|_| {
                CoreError::StatusError(format!(
                    "Timed out reading status packet from {addr_str}"
                ))
            })??;

        // Measure exact ping/pong RTT if server keeps socket open, falling back to status RTT
        let fallback_latency = rtt_start
            .elapsed()
            .as_millis()
            .max(start_connect.elapsed().as_millis().min(1)) as u64;
        let latency_ms = measure_optional_ping_pong(&mut stream)
            .await
            .unwrap_or_else(|| rtt_start.elapsed().as_millis() as u64)
            .max(fallback_latency.min(rtt_start.elapsed().as_millis() as u64));

        let _ = stream.shutdown().await;

        parse_status_json(&json_str, latency_ms)
    }

    async fn resolve_srv_target(&self, host: &str) -> (String, u16) {
        if self.port != 25565
            || host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok()
        {
            return (host.to_string(), self.port);
        }

        let srv_timeout = Duration::from_millis(1_500).min(self.timeout);
        let lookup_fut = async {
            let resolver = TokioAsyncResolver::tokio_from_system_conf()
                .unwrap_or_else(|_| {
                    TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default())
                });
            let query = format!("_minecraft._tcp.{}.", host.trim_end_matches('.'));
            let srv_lookup = resolver.srv_lookup(query).await.ok()?;

            let mut records: Vec<_> = srv_lookup.iter().collect();
            records.sort_by(|a, b| {
                a.priority()
                    .cmp(&b.priority())
                    .then_with(|| b.weight().cmp(&a.weight()))
            });

            let best = records.first()?;
            let target = best.target().to_utf8();
            let clean_target = target.trim_end_matches('.').trim();
            if clean_target.is_empty() {
                None
            } else {
                Some((clean_target.to_string(), best.port()))
            }
        };

        match tokio::time::timeout(srv_timeout, lookup_fut).await {
            Ok(Some(resolved)) => resolved,
            _ => (host.to_string(), self.port),
        }
    }
}

/// Encodes a signed 32-bit integer using Minecraft's 7-bit VarInt encoding (1 to 5 bytes).
pub fn encode_varint(value: i32, out: &mut Vec<u8>) {
    let mut uval = value as u32;
    loop {
        if (uval & !0x7F) == 0 {
            out.push(uval as u8);
            return;
        }
        out.push(((uval & 0x7F) as u8) | 0x80);
        uval >>= 7;
    }
}

/// Attempts to decode a Minecraft VarInt from the beginning of `buf`:
/// - Returns `Ok(Some((value, bytes_consumed)))` when a complete VarInt is present.
/// - Returns `Ok(None)` when `buf` ends mid-VarInt and more TCP bytes are required.
/// - Returns `Err` if the VarInt exceeds 5 bytes (malformed packet).
pub fn try_decode_varint(buf: &[u8]) -> CoreResult<Option<(i32, usize)>> {
    let mut result: u32 = 0;
    for (idx, &byte) in buf.iter().enumerate() {
        if idx >= 5 {
            return Err(CoreError::StatusError(
                "Malformed Minecraft VarInt (> 5 bytes)".to_string(),
            ));
        }
        result |= ((byte & 0x7F) as u32) << (idx * 7);
        if (byte & 0x80) == 0 {
            return Ok(Some((result as i32, idx + 1)));
        }
    }
    Ok(None)
}

/// Encodes a UTF-8 string prefixed by its exact UTF-8 byte length as a VarInt.
pub fn encode_mc_string(s: &str, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    encode_varint(bytes.len() as i32, out);
    out.extend_from_slice(bytes);
}

/// Wraps a raw packet body with its VarInt byte-length header.
pub fn frame_mc_packet(payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(payload.len() + 5);
    encode_varint(payload.len() as i32, &mut framed);
    framed.extend_from_slice(payload);
    framed
}

/// Builds the concatenated Handshake (`0x00`) + Status Request (`0x00`) TCP byte stream.
pub fn build_status_handshake_and_request(
    host: &str,
    port: u16,
    protocol_version: i32,
) -> Vec<u8> {
    let mut handshake_body = Vec::with_capacity(host.len() + 16);
    encode_varint(0x00, &mut handshake_body); // Packet ID: Handshake
    encode_varint(protocol_version, &mut handshake_body);
    encode_mc_string(host, &mut handshake_body);
    handshake_body.extend_from_slice(&port.to_be_bytes());
    encode_varint(1, &mut handshake_body); // Next State: 1 (Status)

    let mut wire = frame_mc_packet(&handshake_body);
    // Status Request packet: length = 1, packet_id = 0x00
    wire.extend_from_slice(&[0x01, 0x00]);
    wire
}

/// Decodes a framed Status Response packet from `reading_buf` if all bytes have arrived:
/// checks `reading_buf.len() >= packet_len + header_offset` (fixing the legacy `length - offset`
/// truncation bug!).
pub fn try_extract_status_json_from_buffer(reading_buf: &[u8]) -> CoreResult<Option<String>> {
    let Some((packet_len_i32, header_offset)) = try_decode_varint(reading_buf)? else {
        return Ok(None);
    };

    if packet_len_i32 <= 0 || (packet_len_i32 as usize) > MAX_STATUS_PACKET_BYTES {
        return Err(CoreError::StatusError(format!(
            "Invalid Minecraft status packet length: {packet_len_i32}"
        )));
    }

    let packet_len = packet_len_i32 as usize;
    let total_frame_len = header_offset + packet_len;
    if reading_buf.len() < total_frame_len {
        return Ok(None);
    }

    let packet_slice = &reading_buf[header_offset..total_frame_len];
    let Some((packet_id, id_len)) = try_decode_varint(packet_slice)? else {
        return Err(CoreError::StatusError(
            "Truncated packet ID inside Minecraft status frame".to_string(),
        ));
    };

    if packet_id != 0x00 {
        return Err(CoreError::StatusError(format!(
            "Unexpected Minecraft status packet ID: 0x{packet_id:02x}"
        )));
    }

    let after_id = &packet_slice[id_len..];
    let Some((str_byte_len_i32, str_len_header)) = try_decode_varint(after_id)? else {
        return Err(CoreError::StatusError(
            "Truncated JSON string length inside Minecraft status packet".to_string(),
        ));
    };

    if str_byte_len_i32 < 0 {
        return Err(CoreError::StatusError(
            "Negative JSON string length in Minecraft status packet".to_string(),
        ));
    }

    let str_byte_len = str_byte_len_i32 as usize;
    let str_start = str_len_header;
    let str_end = str_start + str_byte_len;
    if after_id.len() < str_end {
        return Err(CoreError::StatusError(format!(
            "Minecraft status JSON length ({str_byte_len}) exceeds packet payload ({})",
            after_id.len().saturating_sub(str_start)
        )));
    }

    let json_text = std::str::from_utf8(&after_id[str_start..str_end])
        .map_err(|e| CoreError::StatusError(format!("Invalid UTF-8 in status JSON: {e}")))?
        .to_string();

    Ok(Some(json_text))
}

async fn read_status_response_json(stream: &mut TcpStream) -> CoreResult<String> {
    let mut reading_buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];

    loop {
        if let Some(json) = try_extract_status_json_from_buffer(&reading_buf)? {
            return Ok(json);
        }

        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(CoreError::StatusError(
                "Connection closed by server before complete status packet was received".to_string(),
            ));
        }

        reading_buf.extend_from_slice(&chunk[..n]);
        if reading_buf.len() > MAX_STATUS_PACKET_BYTES + 16 {
            return Err(CoreError::StatusError(
                "Server status response exceeded 4 MiB safety limit".to_string(),
            ));
        }
    }
}

async fn measure_optional_ping_pong(stream: &mut TcpStream) -> Option<u64> {
    let mut ping_payload = Vec::with_capacity(9);
    encode_varint(0x01, &mut ping_payload);
    ping_payload.extend_from_slice(&0x0102_0304_0506_0708_i64.to_be_bytes());
    let framed = frame_mc_packet(&ping_payload);

    let ping_start = Instant::now();
    let ping_timeout = Duration::from_millis(400);
    tokio::time::timeout(ping_timeout, stream.write_all(&framed))
        .await
        .ok()?
        .ok()?;

    let mut pong_buf = [0u8; 16];
    let n = tokio::time::timeout(ping_timeout, stream.read(&mut pong_buf))
        .await
        .ok()?
        .ok()?;
    if n >= 9 {
        Some(ping_start.elapsed().as_millis() as u64)
    } else {
        None
    }
}

pub(crate) fn parse_status_json(json_str: &str, latency_ms: u64) -> CoreResult<ServerStatusResponse> {
    let root: serde_json::Value = serde_json::from_str(json_str)?;

    let version = root
        .get("version")
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("Unknown")
        .to_string();

    let players_online = root
        .get("players")
        .and_then(|p| p.get("online"))
        .and_then(|o| o.as_u64())
        .unwrap_or(0) as u32;

    let players_max = root
        .get("players")
        .and_then(|p| p.get("max"))
        .and_then(|m| m.as_u64())
        .unwrap_or(0) as u32;

    let motd = root
        .get("description")
        .map(flatten_chat_component)
        .unwrap_or_default()
        .trim()
        .to_string();

    let favicon = root
        .get("favicon")
        .and_then(|f| f.as_str())
        .map(|s| s.replace(['\n', '\r'], ""))
        .filter(|s| !s.is_empty());

    Ok(ServerStatusResponse {
        online: true,
        latency_ms,
        version,
        players_online,
        players_max,
        motd,
        favicon,
    })
}

fn flatten_chat_component(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(flatten_chat_component)
            .collect::<Vec<_>>()
            .join(""),
        serde_json::Value::Object(map) => {
            let mut out = String::new();
            if let Some(text) = map.get("text").and_then(|t| t.as_str()) {
                out.push_str(text);
            } else if let Some(tr) = map.get("translate").and_then(|t| t.as_str()) {
                out.push_str(tr);
            }
            if let Some(extra) = map.get("extra") {
                out.push_str(&flatten_chat_component(extra));
            }
            out
        }
        _ => String::new(),
    }
}
