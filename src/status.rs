use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

use crate::error::{CoreError, CoreResult};

const DEFAULT_PING_TIMEOUT_MS: u64 = 3_000;
const MAX_STATUS_PACKET_BYTES: usize = 4 * 1024 * 1024;
/// Minecraft Server List Ping protocol version (`-1` = unspecified/any version per protocol spec).
const PING_PROTOCOL_VERSION: i32 = -1;
const DNS_TYPE_SRV: u16 = 33;
const DNS_CLASS_IN: u16 = 1;
const MAX_DNS_POINTER_JUMPS: usize = 16;

/// Parsed DNS `SRV` resource record (`_minecraft._tcp.<domain>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrvRecord {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    pub target: String,
}

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
/// with lightweight UDP `_minecraft._tcp.{host}` DNS SRV resolution and exact UTF-8 VarInt framing.
#[derive(Debug, Clone)]
pub struct ServerStatusClient {
    host: String,
    port: u16,
    timeout: Duration,
    protocol_version: i32,
    custom_srv_nameserver: Option<SocketAddr>,
}

impl ServerStatusClient {
    /// Creates a new [`ServerStatusClient`] targeting `host:port` (default timeout: `3000ms`).
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            timeout: Duration::from_millis(DEFAULT_PING_TIMEOUT_MS),
            protocol_version: PING_PROTOCOL_VERSION,
            custom_srv_nameserver: None,
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

    /// Overrides the DNS nameserver used for `_minecraft._tcp.{host}` SRV resolution.
    pub fn with_srv_nameserver(mut self, nameserver: SocketAddr) -> Self {
        self.custom_srv_nameserver = Some(nameserver);
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
        let bare_host = connect_host
            .trim_start_matches('[')
            .trim_end_matches(']');
        let addr_str = if bare_host.contains(':') {
            format!("[{bare_host}]:{connect_port}")
        } else {
            format!("{bare_host}:{connect_port}")
        };

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
            build_status_handshake_and_request(bare_host, connect_port, self.protocol_version);

        let rtt_start = Instant::now();
        tokio::time::timeout(self.timeout, stream.write_all(&request_bytes))
            .await
            .map_err(|_| {
                CoreError::StatusError(format!("Timed out sending handshake to {addr_str}"))
            })?
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
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if self.port != 25565
            || bare.eq_ignore_ascii_case("localhost")
            || bare.parse::<IpAddr>().is_ok()
        {
            return (host.to_string(), self.port);
        }

        let qname = format!("_minecraft._tcp.{}", bare.trim_end_matches('.'));
        let srv_timeout = Duration::from_millis(1_200).min(self.timeout);
        let nameservers = collect_dns_nameservers(self.custom_srv_nameserver);

        let lookup_fut = async {
            for ns in nameservers {
                if let Ok(records) = query_dns_srv_udp(&qname, ns).await {
                    if let Some(best) = records.first() {
                        return Some((best.target.clone(), best.port));
                    }
                    // Authoritative empty / NXDOMAIN response — no need to query backup resolvers
                    return None;
                }
            }
            None
        };

        match tokio::time::timeout(srv_timeout, lookup_fut).await {
            Ok(Some(resolved)) => resolved,
            _ => (host.to_string(), self.port),
        }
    }
}

/// Builds a minimal RFC 1035 / RFC 2782 UDP DNS query packet for `QTYPE = SRV (33)`, `QCLASS = IN (1)`.
pub fn build_dns_srv_query(qname: &str, tx_id: u16) -> CoreResult<Vec<u8>> {
    let trimmed = qname.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        return Err(CoreError::InvalidConfig(
            "DNS SRV query name cannot be empty".to_string(),
        ));
    }

    let mut packet = Vec::with_capacity(64 + trimmed.len());
    // 12-byte RFC 1035 Header: ID, Flags (RD=1 -> 0x0100), QDCOUNT=1, AN/NS/AR=0
    packet.extend_from_slice(&tx_id.to_be_bytes());
    packet.extend_from_slice(&0x0100u16.to_be_bytes());
    packet.extend_from_slice(&1u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());

    let mut wire_name_len = 1usize; // trailing 0x00 root label
    for label in trimmed.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return Err(CoreError::InvalidConfig(format!(
                "Invalid DNS label length in '{qname}'"
            )));
        }
        wire_name_len += 1 + bytes.len();
        if wire_name_len > 255 {
            return Err(CoreError::InvalidConfig(format!(
                "DNS query name exceeds 255 bytes: '{qname}'"
            )));
        }
        packet.push(bytes.len() as u8);
        packet.extend_from_slice(bytes);
    }
    packet.push(0x00); // Root label terminator

    packet.extend_from_slice(&DNS_TYPE_SRV.to_be_bytes());
    packet.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
    Ok(packet)
}

/// Parses a DNS UDP response packet and extracts `SRV` records sorted by `(priority ASC, weight DESC)`.
pub fn parse_dns_srv_response(packet: &[u8], expected_tx_id: u16) -> CoreResult<Vec<SrvRecord>> {
    if packet.len() < 12 {
        return Err(CoreError::StatusError(
            "Truncated DNS header (< 12 bytes)".to_string(),
        ));
    }

    let tx_id = u16::from_be_bytes([packet[0], packet[1]]);
    if tx_id != expected_tx_id {
        return Err(CoreError::StatusError(format!(
            "DNS transaction ID mismatch (expected 0x{expected_tx_id:04x}, got 0x{tx_id:04x})"
        )));
    }

    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if (flags & 0x8000) == 0 {
        return Err(CoreError::StatusError(
            "DNS packet is not a response (QR=0)".to_string(),
        ));
    }

    let rcode = flags & 0x000F;
    if rcode != 0 {
        // NXDOMAIN (3) or server refusal -> treat as no SRV records cleanly
        return Ok(Vec::new());
    }

    let qdcount = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let ancount = u16::from_be_bytes([packet[6], packet[7]]) as usize;

    let mut offset = 12usize;
    for _ in 0..qdcount {
        let (_, after_qname) = decode_dns_name(packet, offset)?;
        if after_qname + 4 > packet.len() {
            return Err(CoreError::StatusError(
                "Truncated DNS question section".to_string(),
            ));
        }
        offset = after_qname + 4;
    }

    let mut records = Vec::new();
    for _ in 0..ancount {
        let (_, rr_hdr) = decode_dns_name(packet, offset)?;
        if rr_hdr + 10 > packet.len() {
            return Err(CoreError::StatusError(
                "Truncated DNS resource record header".to_string(),
            ));
        }

        let rtype = u16::from_be_bytes([packet[rr_hdr], packet[rr_hdr + 1]]);
        let rclass = u16::from_be_bytes([packet[rr_hdr + 2], packet[rr_hdr + 3]]);
        let rdlength = u16::from_be_bytes([packet[rr_hdr + 8], packet[rr_hdr + 9]]) as usize;
        let rdata_start = rr_hdr + 10;
        let rdata_end = rdata_start.saturating_add(rdlength);
        if rdata_end > packet.len() {
            return Err(CoreError::StatusError(
                "Truncated DNS resource record RDATA".to_string(),
            ));
        }

        if rtype == DNS_TYPE_SRV && (rclass & 0x7FFF) == DNS_CLASS_IN && rdlength >= 7 {
            let priority = u16::from_be_bytes([packet[rdata_start], packet[rdata_start + 1]]);
            let weight = u16::from_be_bytes([packet[rdata_start + 2], packet[rdata_start + 3]]);
            let port = u16::from_be_bytes([packet[rdata_start + 4], packet[rdata_start + 5]]);
            let (raw_target, target_end) = decode_dns_name(packet, rdata_start + 6)?;
            if target_end > rdata_end {
                return Err(CoreError::StatusError(
                    "DNS SRV target exceeded RDATA bounds".to_string(),
                ));
            }
            let clean_target = raw_target.trim_end_matches('.').trim().to_string();
            if !clean_target.is_empty() && port > 0 {
                records.push(SrvRecord {
                    priority,
                    weight,
                    port,
                    target: clean_target,
                });
            }
        }

        offset = rdata_end;
    }

    records.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.weight.cmp(&a.weight))
    });

    Ok(records)
}

fn decode_dns_name(packet: &[u8], start_offset: usize) -> CoreResult<(String, usize)> {
    let mut pos = start_offset;
    let mut consumed_end: Option<usize> = None;
    let mut jumps = 0usize;
    let mut total_len = 0usize;
    let mut labels: Vec<&str> = Vec::new();

    loop {
        if pos >= packet.len() {
            return Err(CoreError::StatusError(
                "Truncated DNS domain name".to_string(),
            ));
        }
        let len_byte = packet[pos];
        if len_byte == 0 {
            let end = consumed_end.unwrap_or(pos + 1);
            return Ok((labels.join("."), end));
        }

        if (len_byte & 0xC0) == 0xC0 {
            if pos + 1 >= packet.len() {
                return Err(CoreError::StatusError(
                    "Truncated DNS compression pointer".to_string(),
                ));
            }
            jumps += 1;
            if jumps > MAX_DNS_POINTER_JUMPS {
                return Err(CoreError::StatusError(
                    "DNS compression pointer loop detected".to_string(),
                ));
            }
            if consumed_end.is_none() {
                consumed_end = Some(pos + 2);
            }
            let ptr = (((len_byte & 0x3F) as usize) << 8) | (packet[pos + 1] as usize);
            if ptr >= packet.len() {
                return Err(CoreError::StatusError(
                    "DNS compression pointer out of bounds".to_string(),
                ));
            }
            pos = ptr;
            continue;
        }

        if (len_byte & 0xC0) != 0 {
            return Err(CoreError::StatusError(
                "Unsupported extended DNS label type".to_string(),
            ));
        }

        let label_len = len_byte as usize;
        let label_start = pos + 1;
        let label_end = label_start + label_len;
        if label_end > packet.len() {
            return Err(CoreError::StatusError("Truncated DNS label".to_string()));
        }

        total_len += label_len + 1;
        if total_len > 255 {
            return Err(CoreError::StatusError(
                "Decoded DNS name exceeds 255 bytes".to_string(),
            ));
        }

        let label = std::str::from_utf8(&packet[label_start..label_end])
            .map_err(|e| CoreError::StatusError(format!("Invalid UTF-8 in DNS label: {e}")))?;
        labels.push(label);
        pos = label_end;
    }
}

async fn query_dns_srv_udp(qname: &str, nameserver: SocketAddr) -> CoreResult<Vec<SrvRecord>> {
    let tx_id = next_dns_tx_id();
    let query_bytes = build_dns_srv_query(qname, tx_id)?;

    let bind_addr = if nameserver.is_ipv6() {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    };

    let socket = UdpSocket::bind(bind_addr).await?;
    let per_ns_timeout = Duration::from_millis(600);
    tokio::time::timeout(per_ns_timeout, socket.send_to(&query_bytes, nameserver))
        .await
        .map_err(|_| CoreError::StatusError("DNS SRV send timed out".to_string()))??;

    let mut resp_buf = [0u8; 1500];
    let (n, _) = tokio::time::timeout(per_ns_timeout, socket.recv_from(&mut resp_buf))
        .await
        .map_err(|_| CoreError::StatusError("DNS SRV recv timed out".to_string()))??;

    parse_dns_srv_response(&resp_buf[..n], tx_id)
}

fn collect_dns_nameservers(custom: Option<SocketAddr>) -> Vec<SocketAddr> {
    if let Some(ns) = custom {
        return vec![ns];
    }

    let mut out = Vec::with_capacity(3);
    if let Ok(resolv) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in resolv.lines() {
            let clean = line.split(['#', ';']).next().unwrap_or("").trim();
            let mut parts = clean.split_whitespace();
            if parts.next() == Some("nameserver") {
                if let Some(raw_ip) = parts.next() {
                    // Strip optional IPv6 zone index (`fe80::1%eth0`)
                    let ip_part = raw_ip.split('%').next().unwrap_or(raw_ip);
                    if let Ok(ip) = ip_part.parse::<IpAddr>() {
                        let addr = SocketAddr::new(ip, 53);
                        if !out.contains(&addr) {
                            out.push(addr);
                        }
                    }
                }
            }
            if out.len() >= 2 {
                break;
            }
        }
    }

    for fallback in [
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
    ] {
        if out.len() >= 3 {
            break;
        }
        if !out.contains(&fallback) {
            out.push(fallback);
        }
    }
    out
}

fn next_dns_tx_id() -> u16 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut x = nanos ^ ((std::process::id() as u64) << 16);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    let id = (x as u16) ^ ((x >> 16) as u16);
    if id == 0 { 1 } else { id }
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
