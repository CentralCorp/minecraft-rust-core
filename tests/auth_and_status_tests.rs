use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use minecraft_rust_core::auth::azauth::{AzAuthClient, AzAuthLoginResult};
use minecraft_rust_core::auth::microsoft::{
    MicrosoftClient, MicrosoftEndpoints, DEFAULT_MS_CLIENT_ID,
};
use minecraft_rust_core::auth::mojang::{offline_login, offline_uuid};
use minecraft_rust_core::auth::SkinVariant;
use minecraft_rust_core::config::{
    JavaOptions, LaunchOptions, LoaderOptions, MemoryOptions, ScreenOptions,
};
use minecraft_rust_core::error::CoreError;
use minecraft_rust_core::events::{EventCallback, LaunchEvent};
use minecraft_rust_core::launch::{format_redacted_command, Launcher};
use minecraft_rust_core::status::{
    encode_mc_string, encode_varint, frame_mc_packet, try_decode_varint,
    try_extract_status_json_from_buffer, ServerStatusClient,
};

// Minimal 1x1 transparent PNG
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

async fn read_full_http_request(socket: &mut tokio::net::TcpStream) -> (String, String, Vec<u8>) {
    let mut raw = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;

    loop {
        let n = socket.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);

        if header_end.is_none() {
            if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let end = pos + 4;
                header_end = Some(end);
                let headers_str = String::from_utf8_lossy(&raw[..end]);
                for line in headers_str.lines() {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            content_length = v.trim().parse::<usize>().unwrap_or(0);
                        }
                    }
                }
            }
        }

        if let Some(end) = header_end {
            if raw.len() >= end + content_length {
                break;
            }
        }
    }

    let end = header_end.unwrap_or(raw.len());
    let headers_str = String::from_utf8_lossy(&raw[..end]).to_string();
    let first_line = headers_str.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let body = raw[end..].to_vec();
    (method, path, body)
}

fn http_json_response(status_line: &str, json_body: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(json_body).unwrap();
    let header = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(&body);
    out
}

#[test]
fn test_offline_uuidv3_matches_java_name_uuid_from_bytes() {
    // Exact known Java `UUID.nameUUIDFromBytes("OfflinePlayer:Notch".getBytes(UTF_8))` = `b50ad385-829d-3141-a216-7e7d7539ba7f`
    let notch_uuid = offline_uuid("Notch");
    assert_eq!(
        notch_uuid.hyphenated().to_string(),
        "b50ad385-829d-3141-a216-7e7d7539ba7f"
    );

    let acc = offline_login("Notch");
    assert_eq!(acc.uuid, "b50ad385829d3141a2167e7d7539ba7f");
    assert_eq!(acc.name, "Notch");
    assert!(!acc.meta.online);
    assert_eq!(acc.meta.offline, Some(true));
    assert_eq!(acc.meta.account_type, "Mojang");
}

#[test]
fn test_microsoft_redirect_url_extraction_and_client_id_fallback() {
    let c_none = MicrosoftClient::new(None);
    assert_eq!(c_none.client_id(), DEFAULT_MS_CLIENT_ID);

    let c_empty = MicrosoftClient::new(Some("   "));
    assert_eq!(c_empty.client_id(), DEFAULT_MS_CLIENT_ID);

    let auth_url = c_none.authorization_url();
    assert!(auth_url.contains("client_id=00000000402b5328"));
    assert!(auth_url.contains("response_type=code"));

    // Intermediate login URL -> `None`
    assert_eq!(
        MicrosoftClient::extract_code_from_redirect_url(
            "https://login.live.com/oauth20_authorize.srf?client_id=00000000402b5328"
        ),
        None
    );

    // Success redirect -> `Some(Ok(code))`
    assert_eq!(
        MicrosoftClient::extract_code_from_redirect_url(
            "https://login.live.com/oauth20_desktop.srf?code=M.C507_BL2.2.059abc&lc=1036"
        ),
        Some(Ok("M.C507_BL2.2.059abc".to_string()))
    );

    // Error redirect -> `Some(Err(desc))`
    assert_eq!(
        MicrosoftClient::extract_code_from_redirect_url(
            "https://login.live.com/oauth20_desktop.srf?error=access_denied&error_description=The+user+canceled+authentication"
        ),
        Some(Err("The user canceled authentication".to_string()))
    );
}

#[tokio::test]
async fn test_microsoft_full_chain_gamepass_xerr_refresh_and_skins_capes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let base_for_server = base.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let base_url = base_for_server.clone();
            tokio::spawn(async move {
                let (method, path, body) = read_full_http_request(&mut socket).await;
                let body_str = String::from_utf8_lossy(&body);

                let resp = match (method.as_str(), path.as_str()) {
                    ("POST", "/oauth20_token.srf") => {
                        if body_str.contains("code=TRIGGER_CHILD_XERR") {
                            http_json_response(
                                "200 OK",
                                &serde_json::json!({
                                    "access_token": "ms_child_tok",
                                    "refresh_token": "ref+tok/with=special",
                                    "expires_in": 3600
                                }),
                            )
                        } else if body_str.contains("grant_type=refresh_token") {
                            // Verify `+`, `/`, `=` in `refresh_token` were URL-encoded properly!
                            assert!(
                                body_str.contains("refresh_token=ref%2Btok%2Fwith%3Dspecial"),
                                "Refresh token must be URL-encoded, got: {body_str}"
                            );
                            http_json_response(
                                "200 OK",
                                &serde_json::json!({
                                    "access_token": "ms_refreshed_tok",
                                    "refresh_token": "ref+tok/with=special",
                                    "expires_in": 3600
                                }),
                            )
                        } else {
                            http_json_response(
                                "200 OK",
                                &serde_json::json!({
                                    "access_token": "ms_live_tok",
                                    "refresh_token": "ref+tok/with=special",
                                    "expires_in": 3600
                                }),
                            )
                        }
                    }
                    ("POST", "/user/authenticate") => {
                        let xbl_tok = if body_str.contains("ms_child_tok") {
                            "xbl_child"
                        } else {
                            "xbl_valid"
                        };
                        http_json_response(
                            "200 OK",
                            &serde_json::json!({
                                "Token": xbl_tok,
                                "DisplayClaims": { "xui": [{ "uhs": "uhs_999" }] }
                            }),
                        )
                    }
                    ("POST", "/xsts/authorize") => {
                        if body_str.contains("xbl_child") {
                            http_json_response(
                                "401 Unauthorized",
                                &serde_json::json!({
                                    "XErr": 2148916238u64,
                                    "Message": "Child account requires family group"
                                }),
                            )
                        } else if body_str.contains("http://xboxlive.com") {
                            http_json_response(
                                "200 OK",
                                &serde_json::json!({
                                    "Token": "xsts_xbl_tok",
                                    "DisplayClaims": {
                                        "xui": [{
                                            "uhs": "uhs_999",
                                            "xid": "25354123456789",
                                            "gtg": "RiptiazGT",
                                            "agg": "Adult"
                                        }]
                                    }
                                }),
                            )
                        } else {
                            http_json_response(
                                "200 OK",
                                &serde_json::json!({
                                    "Token": "xsts_mc_tok",
                                    "DisplayClaims": { "xui": [{ "uhs": "uhs_999" }] }
                                }),
                            )
                        }
                    }
                    ("POST", "/authentication/login_with_xbox") => http_json_response(
                        "200 OK",
                        &serde_json::json!({
                            "access_token": "mc_jwt_access_token",
                            "expires_in": 86400
                        }),
                    ),
                    // Simulate Xbox Game Pass user where `entitlements/mcstore` returns `items: []`
                    // while `/minecraft/profile` returns `200 OK`!
                    ("GET", "/entitlements/mcstore") => {
                        http_json_response("200 OK", &serde_json::json!({ "items": [] }))
                    }
                    ("GET", "/minecraft/profile") => http_json_response(
                        "200 OK",
                        &serde_json::json!({
                            "id": "069a79f444e94726a5befca90e38aaf5",
                            "name": "GamePassHero",
                            "skins": [{
                                "id": "skin-1",
                                "state": "ACTIVE",
                                "url": format!("{base_url}/textures/skin.png"),
                                "variant": "SLIM"
                            }],
                            "capes": [{
                                "id": "cape-migrator",
                                "state": "ACTIVE",
                                "url": format!("{base_url}/textures/cape.png"),
                                "alias": "Migrator"
                            }]
                        }),
                    ),
                    ("GET", "/textures/skin.png") | ("GET", "/textures/cape.png") => {
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            TINY_PNG.len()
                        );
                        let mut out = hdr.into_bytes();
                        out.extend_from_slice(TINY_PNG);
                        out
                    }
                    ("POST", "/minecraft/profile/skins") => {
                        assert!(body_str.contains("slim"));
                        http_json_response("200 OK", &serde_json::json!({}))
                    }
                    ("PUT", "/minecraft/profile/capes/active") => {
                        assert!(body_str.contains("cape-migrator"));
                        http_json_response("200 OK", &serde_json::json!({}))
                    }
                    ("DELETE", "/minecraft/profile/capes/active") => {
                        http_json_response("200 OK", &serde_json::json!({}))
                    }
                    _ => http_json_response("404 Not Found", &serde_json::json!({ "error": "not_found" })),
                };

                let _ = socket.write_all(&resp).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    let endpoints = MicrosoftEndpoints {
        oauth_authorize: format!("{base}/oauth20_authorize.srf"),
        oauth_token: format!("{base}/oauth20_token.srf"),
        xbl_authenticate: format!("{base}/user/authenticate"),
        xsts_authorize: format!("{base}/xsts/authorize"),
        mc_services_base: base.clone(),
    };
    let client = MicrosoftClient::with_endpoints(None, endpoints);

    // 1. Authenticate Game Pass user (empty `entitlements.items` + valid `/minecraft/profile`)
    let mut account = client.authenticate_with_code("VALID_CODE").await.unwrap();
    assert_eq!(account.name, "GamePassHero");
    assert_eq!(account.uuid, "069a79f444e94726a5befca90e38aaf5");
    assert!(account.meta.online);
    assert_eq!(account.meta.account_type, "Microsoft");
    assert_eq!(account.xbox.as_ref().unwrap().gamertag.as_deref(), Some("RiptiazGT"));
    assert!(account.profile.as_ref().unwrap().skins[0]
        .base64
        .as_deref()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    assert!(account.profile.as_ref().unwrap().capes[0]
        .base64
        .as_deref()
        .unwrap()
        .starts_with("data:image/png;base64,"));

    // 2. Test XSTS `XErr = 2148916238` child account diagnostic
    let xerr = client
        .authenticate_with_code("TRIGGER_CHILD_XERR")
        .await
        .unwrap_err();
    match xerr {
        CoreError::XboxXsts { xerr: code, message } => {
            assert_eq!(code, 2148916238);
            assert!(message.to_lowercase().contains("family"));
        }
        other => panic!("Expected CoreError::XboxXsts, got {other:?}"),
    }

    // 3. Test `refresh` when token is expired -> URL-encodes `refresh_token` (`ref+tok/with=special`)
    account.expires_at = Some(0);
    let refreshed = client.refresh(&account).await.unwrap();
    assert_eq!(refreshed.name, "GamePassHero");

    // 4. Test `upload_skin` & `set_active_cape`
    let prof_after_skin = client
        .upload_skin(&refreshed.access_token, TINY_PNG, SkinVariant::Slim)
        .await
        .unwrap();
    assert_eq!(prof_after_skin.name, "GamePassHero");

    let prof_after_cape = client
        .set_active_cape(&refreshed.access_token, Some("cape-migrator"))
        .await
        .unwrap();
    assert_eq!(prof_after_cape.capes[0].id, "cape-migrator");

    let prof_after_hide = client
        .set_active_cape(&refreshed.access_token, None)
        .await
        .unwrap();
    assert_eq!(prof_after_hide.name, "GamePassHero");
}

#[tokio::test]
async fn test_azauth_subfolder_url_preservation_2fa_and_skin_upload() {
    // 1. Subfolder URL preservation check (CRITICAL AUDIT BUG FIX!)
    let sub_client = AzAuthClient::new("https://play.centralcorp.fr/azuriom/").unwrap();
    assert_eq!(
        sub_client.auth_base_url(),
        "https://play.centralcorp.fr/azuriom/api/auth"
    );
    assert_eq!(
        sub_client.skin_base_url(),
        "https://play.centralcorp.fr/azuriom/api/skin-api/skins"
    );

    // 2. Mock Azuriom server mounted under `/azuriom` subfolder
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (method, path, body) = read_full_http_request(&mut socket).await;
                let body_str = String::from_utf8_lossy(&body);

                let resp = match (method.as_str(), path.as_str()) {
                    ("POST", "/azuriom/api/auth/authenticate") => {
                        if !body_str.contains("\"code\"") {
                            http_json_response(
                                "200 OK",
                                &serde_json::json!({
                                    "status": "pending",
                                    "reason": "2fa"
                                }),
                            )
                        } else {
                            http_json_response(
                                "200 OK",
                                &serde_json::json!({
                                    "access_token": "az_secret_tok_12345",
                                    "uuid": "11223344556677889900aabbccddeeff",
                                    "username": "AzPlayer",
                                    "id": 42,
                                    "banned": false,
                                    "monnaie": 150.5,
                                    "verified": true,
                                    "maintenance_bypass": true,
                                    "role": { "name": "Admin", "color": "#ff0000" }
                                }),
                            )
                        }
                    }
                    ("POST", "/azuriom/api/auth/verify") => http_json_response(
                        "200 OK",
                        &serde_json::json!({
                            "access_token": "az_secret_tok_12345",
                            "uuid": "11223344556677889900aabbccddeeff",
                            "username": "AzPlayer",
                            "id": 42,
                            "money": 150.5
                        }),
                    ),
                    ("GET", "/azuriom/api/skin-api/skins/42") => {
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            TINY_PNG.len()
                        );
                        let mut out = hdr.into_bytes();
                        out.extend_from_slice(TINY_PNG);
                        out
                    }
                    ("POST", "/azuriom/api/skin-api/skins/update") => {
                        assert!(body_str.contains("az_secret_tok_12345"));
                        http_json_response("200 OK", &serde_json::json!({ "status": "success" }))
                    }
                    ("POST", "/azuriom/api/auth/logout") => {
                        http_json_response("200 OK", &serde_json::json!({ "status": "success" }))
                    }
                    _ => http_json_response(
                        "404 Not Found",
                        &serde_json::json!({ "status": "error", "message": format!("Unexpected path {path}") }),
                    ),
                };

                let _ = socket.write_all(&resp).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    let az = AzAuthClient::new(&format!("http://{addr}/azuriom")).unwrap();

    // Step A: Login without 2FA -> `Requires2fa`
    let step1 = az.login("AzPlayer", "hunter2", None).await.unwrap();
    assert!(matches!(step1, AzAuthLoginResult::Requires2fa));

    // Step B: Login with 2FA code -> `Success`
    let step2 = az.login("AzPlayer", "hunter2", Some("123456")).await.unwrap();
    let AzAuthLoginResult::Success { account } = step2 else {
        panic!("Expected AzAuthLoginResult::Success");
    };
    assert_eq!(account.name, "AzPlayer");
    assert_eq!(account.meta.account_type, "AZauth");
    let ui = account.user_info.as_ref().unwrap();
    assert_eq!(ui.money, 150.5);
    assert!(ui.maintenance_bypass);
    assert_eq!(ui.role.as_ref().unwrap().name, "Admin");

    // Step C: Verify, Upload Skin, Logout
    let verified = az.verify(&account.access_token).await.unwrap();
    assert_eq!(verified.uuid, "11223344556677889900aabbccddeeff");
    az.upload_skin(&account.access_token, TINY_PNG).await.unwrap();
    assert!(az.logout(&account.access_token).await.unwrap());
}

#[tokio::test]
async fn test_varint_utf8_status_framing_and_live_tcp_ping() {
    // 1. VarInt roundtrip boundary tests
    for val in [0, 1, 127, 128, 16383, 16384, 2_097_151, -1] {
        let mut buf = Vec::new();
        encode_varint(val, &mut buf);
        let (decoded, used) = try_decode_varint(&buf).unwrap().unwrap();
        assert_eq!(decoded, val);
        assert_eq!(used, buf.len());
    }

    // 2. Multi-byte UTF-8 MOTD + >32KB base64 favicon (3-byte VarInt packet length header!)
    let large_favicon = format!("data:image/png;base64,{}", "A".repeat(40_000));
    let status_json = serde_json::json!({
        "version": { "name": "Paper 1.21.4", "protocol": 769 },
        "players": { "max": 250, "online": 87 },
        "description": { "text": "⚡ CentralCorp — Édition Légendaire ⚔" },
        "favicon": large_favicon
    })
    .to_string();

    let mut resp_payload = Vec::new();
    encode_varint(0x00, &mut resp_payload);
    encode_mc_string(&status_json, &mut resp_payload);
    let framed_packet = frame_mc_packet(&resp_payload);

    // Confirm `reading_buf.len() == framed_packet.len() - 1` returns `Ok(None)` (fixing the
    // legacy `length - offset` truncation bug where the last 2-3 bytes of JSON were cut off!)
    assert!(
        try_extract_status_json_from_buffer(&framed_packet[..framed_packet.len() - 1])
            .unwrap()
            .is_none()
    );
    let extracted = try_extract_status_json_from_buffer(&framed_packet)
        .unwrap()
        .unwrap();
    assert_eq!(extracted, status_json);

    // 3. Real TCP mock Minecraft server sending fragmented chunks + Ping/Pong
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut req_buf = [0u8; 512];
            let _ = sock.read(&mut req_buf).await;

            // Send framed status packet in 3 fragmented TCP writes!
            let split1 = 100;
            let split2 = 20_000;
            let _ = sock.write_all(&framed_packet[..split1]).await;
            let _ = sock.write_all(&framed_packet[split1..split2]).await;
            let _ = sock.write_all(&framed_packet[split2..]).await;

            // Echo Ping -> Pong (`0x01` + 8-byte payload)
            let mut ping_buf = [0u8; 16];
            if let Ok(n) = sock.read(&mut ping_buf).await {
                if n > 0 {
                    let _ = sock.write_all(&ping_buf[..n]).await;
                }
            }
        }
    });

    let status = ServerStatusClient::new("127.0.0.1", port)
        .with_timeout(3_000)
        .ping()
        .await
        .unwrap();

    assert!(status.online);
    assert_eq!(status.version, "Paper 1.21.4");
    assert_eq!(status.players_online, 87);
    assert_eq!(status.players_max, 250);
    assert_eq!(status.motd, "⚡ CentralCorp — Édition Légendaire ⚔");
    assert_eq!(status.favicon.as_deref(), Some(large_favicon.as_str()));
}

#[cfg(unix)]
#[tokio::test]
async fn test_launcher_offline_prepare_token_redaction_and_process_spawn() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    // Create a local cached Vanilla 1.20.4 manifest + dummy client JAR + asset index so `prepare` runs 100% offline
    let ver_dir = root.join("versions/1.20.4");
    let assets_idx_dir = root.join("assets/indexes");
    tokio::fs::create_dir_all(&ver_dir).await.unwrap();
    tokio::fs::create_dir_all(&assets_idx_dir).await.unwrap();

    let version_json = serde_json::json!({
        "id": "1.20.4",
        "type": "release",
        "assets": "12",
        "assetIndex": { "id": "12" },
        "mainClass": "net.minecraft.client.main.Main",
        "arguments": {
            "game": ["--username", "${auth_player_name}", "--accessToken", "${auth_access_token}", "--uuid", "${auth_uuid}"],
            "jvm": ["-cp", "${classpath}"]
        },
        "libraries": []
    });
    tokio::fs::write(
        ver_dir.join("1.20.4.json"),
        serde_json::to_vec_pretty(&version_json).unwrap(),
    )
    .await
    .unwrap();
    tokio::fs::write(ver_dir.join("1.20.4.jar"), b"DUMMY_MC_JAR")
        .await
        .unwrap();
    tokio::fs::write(assets_idx_dir.join("12.json"), br#"{"objects":{}}"#)
        .await
        .unwrap();

    // Create a mock `java` executable script that prints its cwd and exits 0
    let fake_java = root.join("mock-java.sh");
    tokio::fs::write(
        &fake_java,
        b"#!/usr/bin/env bash\necho \"MOCK_JVM_STARTED cwd=$(pwd)\"\nexit 0\n",
    )
    .await
    .unwrap();
    let mut perms = std::fs::metadata(&fake_java).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_java, perms).unwrap();

    let mut auth = offline_login("SecretPlayer");
    auth.access_token = "SUPER_SECRET_ACCESS_TOKEN_XYZ".to_string();
    auth.client_token = "SUPER_SECRET_CLIENT_TOKEN_ABC".to_string();

    let opts = LaunchOptions {
        url: None,
        authenticator: auth,
        timeout_ms: 2_000,
        path: root.clone(),
        version: "1.20.4".to_string(),
        instance: Some("faction-alpha".to_string()),
        detached: false,
        intel_enabled_mac: false,
        concurrent_downloads: 2,
        bypass_offline: false,
        loader: LoaderOptions::default(),
        mcp: None,
        verify: false,
        ignored: Vec::new(),
        jvm_args: Vec::new(),
        game_args: Vec::new(),
        auto_connect: None,
        java: JavaOptions {
            path: Some(fake_java.clone()),
            version: None,
            image_type: "jre".to_string(),
        },
        screen: ScreenOptions::default(),
        memory: MemoryOptions::default(),
    };

    let launcher = Launcher::new(opts.clone());
    let (close_tx, close_rx) = tokio::sync::oneshot::channel::<i32>();
    let close_tx_mutex = Arc::new(Mutex::new(Some(close_tx)));
    let data_lines = Arc::new(Mutex::new(Vec::<String>::new()));

    let lines_ref = data_lines.clone();
    let tx_ref = close_tx_mutex.clone();
    let cb: EventCallback = Arc::new(move |ev| match ev {
        LaunchEvent::Data { line } => {
            lines_ref.lock().unwrap().push(line);
        }
        LaunchEvent::Close { exit_code } => {
            if let Some(tx) = tx_ref.lock().unwrap().take() {
                let _ = tx.send(exit_code);
            }
        }
        _ => {}
    });

    let prepared = launcher.prepare(cb.clone()).await.unwrap();
    assert_eq!(prepared.cwd, root.join("instances/faction-alpha"));

    // Verify credential redaction NEVER leaks tokens or UUID!
    let redacted = format_redacted_command(&prepared.java_path, &prepared.full_args(), &opts);
    assert!(!redacted.contains("SUPER_SECRET_ACCESS_TOKEN_XYZ"));
    assert!(!redacted.contains("SUPER_SECRET_CLIENT_TOKEN_ABC"));
    assert!(!redacted.contains(&opts.authenticator.uuid));
    assert!(redacted.contains("[REDACTED]"));

    let pid = launcher.launch(cb).await.unwrap();
    assert!(pid > 0);

    let exit_code = tokio::time::timeout(std::time::Duration::from_secs(5), close_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit_code, 0);

    let captured = data_lines.lock().unwrap().join("\n");
    assert!(captured.contains("MOCK_JVM_STARTED"));
    assert!(captured.contains("instances/faction-alpha"));
    assert!(!captured.contains("SUPER_SECRET_ACCESS_TOKEN_XYZ"));
}
