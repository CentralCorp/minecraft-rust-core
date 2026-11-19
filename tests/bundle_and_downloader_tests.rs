use std::sync::{Arc, Mutex};

use sha1::{Digest, Sha1};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use minecraft_rust_core::bundle::{BundleItem, BundleManager};
use minecraft_rust_core::downloader::{DownloadTask, Downloader, ExpectedHash};
use minecraft_rust_core::error::CoreError;
use minecraft_rust_core::events::{noop_event_callback, EventCallback, LaunchEvent};

fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[tokio::test]
async fn test_bundle_jar_disable_recursive_ignored_and_instance_cleaner() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let game_dir = root.join("instances").join("central-pvp");
    tokio::fs::create_dir_all(&game_dir).await.unwrap();

    // Configure recursive `ignored`: `"config"` (entire directory tree) + `"options.txt"` (file)
    let ignored = vec!["config".to_string(), "options.txt".to_string()];
    let manager = BundleManager::new(root.clone(), game_dir.clone(), true, &ignored);

    // 1. User disabled `mods/minimaps.jar` -> renamed to `mods/minimaps.jar-disable`
    let mods_dir = game_dir.join("mods");
    tokio::fs::create_dir_all(&mods_dir).await.unwrap();
    let disabled_mod = mods_dir.join("minimaps.jar-disable");
    tokio::fs::write(&disabled_mod, b"DISABLED_MOD_BYTES")
        .await
        .unwrap();

    // 2. User customized `config/sub/hud.toml` and `options.txt` (hashes differ from remote manifest!)
    let custom_cfg = game_dir.join("config/sub/hud.toml");
    tokio::fs::create_dir_all(custom_cfg.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&custom_cfg, b"user_modified = true")
        .await
        .unwrap();
    let options_txt = game_dir.join("options.txt");
    tokio::fs::write(&options_txt, b"fov:1.0")
        .await
        .unwrap();

    // 3. Unauthorized mod `mods/xray.jar` (should be removed by `check_files`!)
    let xray_mod = mods_dir.join("xray.jar");
    tokio::fs::write(&xray_mod, b"UNAUTHORIZED_MOD")
        .await
        .unwrap();

    // 4. Protected player data inside instance (`saves`, `screenshots`, `logs`, `launcher_config`)
    let world_level = game_dir.join("saves/MyWorld/level.dat");
    tokio::fs::create_dir_all(world_level.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&world_level, b"WORLD_DATA").await.unwrap();

    let mods_cfg = game_dir.join("launcher_config/mods_config.json");
    tokio::fs::create_dir_all(mods_cfg.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&mods_cfg, b"{}").await.unwrap();

    // Remote distribution manifest items:
    // - `mods/jei.jar` (missing locally -> MUST be queued for download)
    // - `mods/minimaps.jar` (disabled via `.jar-disable` -> MUST NOT be re-downloaded!)
    // - `config/sub/hud.toml` (in `ignored` dir `"config"` and already exists -> MUST NOT be overwritten!)
    // - `config/new_default.toml` (in `ignored` dir `"config"` but missing -> SHOULD be downloaded on first install!)
    // - `options.txt` (in `ignored` and already exists -> MUST NOT be overwritten!)
    let jei_path = mods_dir.join("jei.jar");
    let new_cfg_path = game_dir.join("config/new_default.toml");
    let manifest_items = vec![
        BundleItem::download(
            jei_path.clone(),
            "https://cdn.example.com/mods/jei.jar",
            Some(sha1_hex(b"JEI_BYTES")),
            Some(9),
            "mods",
        ),
        BundleItem::download(
            mods_dir.join("minimaps.jar"),
            "https://cdn.example.com/mods/minimaps.jar",
            Some(sha1_hex(b"MINIMAPS_REMOTE")),
            Some(15),
            "mods",
        ),
        BundleItem::download(
            custom_cfg.clone(),
            "https://cdn.example.com/config/sub/hud.toml",
            Some(sha1_hex(b"user_modified = false")),
            Some(21),
            "config",
        ),
        BundleItem::download(
            new_cfg_path.clone(),
            "https://cdn.example.com/config/new_default.toml",
            Some(sha1_hex(b"default = 1")),
            Some(11),
            "config",
        ),
        BundleItem::download(
            options_txt.clone(),
            "https://cdn.example.com/options.txt",
            Some(sha1_hex(b"fov:0.0")),
            Some(7),
            "Instance",
        ),
    ];

    let pending = manager
        .check_bundle(&manifest_items, &noop_event_callback())
        .await
        .unwrap();

    let pending_dests: Vec<_> = pending.iter().map(|t| t.destination.clone()).collect();
    assert_eq!(pending_dests.len(), 2);
    assert!(pending_dests.contains(&jei_path));
    assert!(pending_dests.contains(&new_cfg_path));

    // Simulate `jei.jar` having been downloaded before `check_files` runs
    tokio::fs::write(&jei_path, b"JEI_BYTES").await.unwrap();

    let removed = manager.check_files(&manifest_items).await.unwrap();
    assert!(removed.contains(&xray_mod), "Unauthorized xray.jar must be deleted");
    assert!(!xray_mod.exists());

    // Verify every legitimate/disabled/ignored/protected file is untouched!
    assert!(jei_path.exists());
    assert!(disabled_mod.exists());
    assert_eq!(
        tokio::fs::read(&custom_cfg).await.unwrap(),
        b"user_modified = true"
    );
    assert_eq!(tokio::fs::read(&options_txt).await.unwrap(), b"fov:1.0");
    assert!(world_level.exists());
    assert!(mods_cfg.exists());
}

#[tokio::test]
async fn test_root_cleaner_protects_shared_caches_and_player_data() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    // Root mode WITHOUT isolated `instance` (`has_instance = false`)
    let manager = BundleManager::new(root.clone(), root.clone(), false, &[]);

    let save_file = root.join("saves/Survival/level.dat");
    let shader_pack = root.join("shaderpacks/Complementary.zip");
    let shared_lib = root.join("libraries/org/ow2/asm/asm/9.7/asm-9.7.jar");
    let rogue_file = root.join("mods/cheat.jar");

    for p in [&save_file, &shader_pack, &shared_lib, &rogue_file] {
        tokio::fs::create_dir_all(p.parent().unwrap()).await.unwrap();
        tokio::fs::write(p, b"test").await.unwrap();
    }

    manager.check_files(&[]).await.unwrap();

    assert!(!rogue_file.exists(), "Rogue mod in root mods/ should be cleaned");
    assert!(save_file.exists(), "saves/ must be protected at root");
    assert!(shader_pack.exists(), "shaderpacks/ must be protected at root");
    assert!(shared_lib.exists(), "libraries/ must be protected at root");
}

#[tokio::test]
async fn test_downloader_atomic_part_deduplication_and_sha1_verification() {
    let payload_ok = b"HELLO_MINECRAFT_RUST_CORE_PAYLOAD_1234567890";
    let ok_sha1 = sha1_hex(payload_ok);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Spawn minimal HTTP/1.1 mock server serving `/ok`, `/corrupt`, and `/missing`
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let path = first_line.split_whitespace().nth(1).unwrap_or("/");

                let response = match path {
                    "/ok" => {
                        let body = b"HELLO_MINECRAFT_RUST_CORE_PAYLOAD_1234567890";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes()
                        .into_iter()
                        .chain(body.iter().copied())
                        .collect::<Vec<u8>>()
                    }
                    "/corrupt" => {
                        let body = b"TAMPERED_BYTES";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes()
                        .into_iter()
                        .chain(body.iter().copied())
                        .collect::<Vec<u8>>()
                    }
                    _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found".to_vec(),
                };

                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    let downloader = Downloader::new(4, 3_000).with_no_proxy();
    let tmp = TempDir::new().unwrap();

    // 1. Test concurrent deduplication + SHA-1 verification + Progress events
    let dest_ok = tmp.path().join("assets/objects/ab/valid.bin");
    let mut task1 = DownloadTask::new(
        format!("http://{addr}/ok"),
        dest_ok.clone(),
        "Assets",
    );
    task1.expected_hash = Some(ExpectedHash::Sha1(ok_sha1.clone()));
    task1.size = Some(payload_ok.len() as u64);

    // Queue duplicate task pointing to identical destination to confirm zero `.part` collision!
    let events_seen = Arc::new(Mutex::new(Vec::new()));
    let events_clone = events_seen.clone();
    let cb: EventCallback = Arc::new(move |ev| {
        events_clone.lock().unwrap().push(ev);
    });

    downloader
        .download_many(vec![task1.clone(), task1], &cb)
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(&dest_ok).await.unwrap(), payload_ok);
    let part_file = dest_ok.with_file_name(".valid.bin.part");
    assert!(!part_file.exists(), ".part temporary file must be renamed cleanly");
    assert!(events_seen
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, LaunchEvent::Progress { .. })));

    // 2. Test SHA-1 mismatch rejects file AND deletes `.part` file without creating corrupt destination!
    let dest_bad = tmp.path().join("libraries/tampered.jar");
    let mut bad_task = DownloadTask::new(
        format!("http://{addr}/corrupt"),
        dest_bad.clone(),
        "Libraries",
    );
    bad_task.expected_hash = Some(ExpectedHash::Sha1(ok_sha1));

    let err = downloader
        .download_single(&bad_task, &noop_event_callback())
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::HashMismatch { .. }));
    assert!(!dest_bad.exists(), "Corrupt destination must NEVER be created");
    assert!(
        !dest_bad.with_file_name(".tampered.jar.part").exists(),
        ".part file must be cleaned up on hash mismatch"
    );

    // 3. Test HTTP 404 returns `CoreError::DownloadFailed`
    let dest_404 = tmp.path().join("libraries/missing.jar");
    let task_404 = DownloadTask::new(format!("http://{addr}/missing"), dest_404.clone(), "Libraries");
    let err_404 = downloader
        .download_single(&task_404, &noop_event_callback())
        .await
        .unwrap_err();
    assert!(matches!(
        err_404,
        CoreError::DownloadFailed {
            status: Some(404),
            ..
        }
    ));
    assert!(!dest_404.exists());
}

fn build_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, content) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

#[tokio::test]
async fn test_folder_zip_packages_selective_delta_download_and_optional_mods() {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let game_dir = root.join("instances").join("survival");

    let config_v1 = b"renderDistance = 12";
    let config_v2 = b"renderDistance = 16";
    let jei_v1 = b"JEI_MOD_V1";
    let jei_v2 = b"JEI_MOD_V2";
    let minimap_v1 = b"MINIMAP_OPTIONAL_V1";
    let minimap_v2 = b"MINIMAP_OPTIONAL_V2";
    let options_default = b"fov:0.0";
    let servers_v1 = b"SERVERS_DAT_V1";
    let servers_v2 = b"SERVERS_DAT_V2";

    let zip_config_v1 = build_zip_bytes(&[("config/forge-client.toml", config_v1)]);
    let zip_config_v2 = build_zip_bytes(&[("config/forge-client.toml", config_v2)]);
    let zip_mods_v1 = build_zip_bytes(&[
        ("mods/jei.jar", jei_v1),
        ("mods/minimap.jar", minimap_v1),
    ]);
    let zip_mods_v2 = build_zip_bytes(&[
        ("mods/jei.jar", jei_v2),
        ("mods/minimap.jar", minimap_v2),
    ]);
    let zip_root_v1 = build_zip_bytes(&[
        ("options.txt", options_default),
        ("servers.dat", servers_v1),
    ]);
    let zip_root_v2 = build_zip_bytes(&[
        ("options.txt", options_default),
        ("servers.dat", servers_v2),
    ]);

    let config_hits = Arc::new(AtomicUsize::new(0));
    let mods_hits = Arc::new(AtomicUsize::new(0));
    let root_hits = Arc::new(AtomicUsize::new(0));
    let saw_accept_header = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let manifest_state = Arc::new(Mutex::new(String::new()));
    let files_map: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));

    // Populate Phase 1 (v1) server state
    {
        let mut fm = files_map.lock().unwrap();
        fm.insert("/pkg/config.zip".to_string(), zip_config_v1.clone());
        fm.insert("/pkg/mods.zip".to_string(), zip_mods_v1.clone());
        fm.insert("/pkg/root.zip".to_string(), zip_root_v1.clone());
        fm.insert("/raw/fallback.cfg".to_string(), b"FALLBACK_OK".to_vec());
    }

    let make_manifest = |base_url: &str,
                         cfg_bytes: &[u8],
                         cfg_zip: &[u8],
                         jei_bytes: &[u8],
                         mini_bytes: &[u8],
                         mods_zip: &[u8],
                         srv_bytes: &[u8],
                         root_zip: &[u8]| {
        serde_json::json!({
            "version": 2,
            "packages": [
                {
                    "id": "config-p1",
                    "folder": "config",
                    "size": cfg_zip.len(),
                    "hash": sha1_hex(cfg_zip),
                    "url": format!("{base_url}/pkg/config.zip"),
                    "files_count": 1
                },
                {
                    "id": "mods-p1",
                    "folder": "mods",
                    "size": mods_zip.len(),
                    "hash": sha1_hex(mods_zip),
                    "url": format!("{base_url}/pkg/mods.zip"),
                    "files_count": 2
                },
                {
                    "id": "root-p1",
                    "folder": "root",
                    "size": root_zip.len(),
                    "hash": sha1_hex(root_zip),
                    "url": format!("{base_url}/pkg/root.zip"),
                    "files_count": 2
                }
            ],
            "files": [
                {
                    "path": "config/forge-client.toml",
                    "size": cfg_bytes.len(),
                    "hash": sha1_hex(cfg_bytes),
                    "url": format!("{base_url}/raw/config.toml"),
                    "package": "config-p1"
                },
                {
                    "path": "mods/jei.jar",
                    "size": jei_bytes.len(),
                    "hash": sha1_hex(jei_bytes),
                    "url": format!("{base_url}/raw/jei.jar"),
                    "package": "mods-p1"
                },
                {
                    "path": "mods/minimap.jar",
                    "size": mini_bytes.len(),
                    "hash": sha1_hex(mini_bytes),
                    "url": format!("{base_url}/raw/minimap.jar"),
                    "package": "mods-p1"
                },
                {
                    "path": "options.txt",
                    "size": options_default.len(),
                    "hash": sha1_hex(options_default),
                    "url": format!("{base_url}/raw/options.txt"),
                    "package": "root-p1"
                },
                {
                    "path": "servers.dat",
                    "size": srv_bytes.len(),
                    "hash": sha1_hex(srv_bytes),
                    "url": format!("{base_url}/raw/servers.dat"),
                    "package": "root-p1"
                }
            ]
        })
        .to_string()
    };

    *manifest_state.lock().unwrap() = make_manifest(
        &base,
        config_v1,
        &zip_config_v1,
        jei_v1,
        minimap_v1,
        &zip_mods_v1,
        servers_v1,
        &zip_root_v1,
    );

    let m_state = Arc::clone(&manifest_state);
    let f_map = Arc::clone(&files_map);
    let c_hits = Arc::clone(&config_hits);
    let md_hits = Arc::clone(&mods_hits);
    let r_hits = Arc::clone(&root_hits);
    let hdr_hits = Arc::clone(&saw_accept_header);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let m_state = Arc::clone(&m_state);
            let f_map = Arc::clone(&f_map);
            let c_hits = Arc::clone(&c_hits);
            let md_hits = Arc::clone(&md_hits);
            let r_hits = Arc::clone(&r_hits);
            let hdr_hits = Arc::clone(&hdr_hits);

            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                if req.to_ascii_lowercase().contains("x-accept-packages: 1") {
                    hdr_hits.fetch_add(1, Ordering::SeqCst);
                }
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");

                if path == "/files" {
                    let body = m_state.lock().unwrap().clone();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    return;
                }

                if path == "/pkg/config.zip" {
                    c_hits.fetch_add(1, Ordering::SeqCst);
                } else if path == "/pkg/mods.zip" {
                    md_hits.fetch_add(1, Ordering::SeqCst);
                } else if path == "/pkg/root.zip" {
                    r_hits.fetch_add(1, Ordering::SeqCst);
                }

                let maybe_bytes = f_map.lock().unwrap().get(path).cloned();
                if let Some(bytes) = maybe_bytes {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&bytes).await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            });
        }
    });

    let downloader = Downloader::new(4, 5_000).with_no_proxy();
    let manager = BundleManager::new(
        root.clone(),
        game_dir.clone(),
        true,
        &["options.txt".to_string()],
    );

    // 1. Fresh install: all 3 folder ZIP packages downloaded & extracted cleanly
    let dist1 = manager
        .fetch_custom_distribution_packaged(downloader.client(), Some(&format!("{base}/files")))
        .await
        .unwrap();
    assert_eq!(dist1.packages.len(), 3);
    assert_eq!(dist1.items.len(), 5);
    assert!(saw_accept_header.load(Ordering::SeqCst) >= 1);

    let extract_events = Arc::new(Mutex::new(Vec::new()));
    let ev_clone = Arc::clone(&extract_events);
    let cb: EventCallback = Arc::new(move |ev| {
        if matches!(ev, LaunchEvent::Extract { .. } | LaunchEvent::Error { .. }) {
            ev_clone.lock().unwrap().push(ev);
        }
    });

    manager
        .sync_bundle(&downloader, &dist1.items, &dist1.packages, &cb)
        .await
        .unwrap();

    assert_eq!(config_hits.load(Ordering::SeqCst), 1);
    assert_eq!(mods_hits.load(Ordering::SeqCst), 1);
    assert_eq!(root_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        tokio::fs::read(game_dir.join("config/forge-client.toml"))
            .await
            .unwrap(),
        config_v1
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("mods/jei.jar")).await.unwrap(),
        jei_v1
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("mods/minimap.jar"))
            .await
            .unwrap(),
        minimap_v1
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("options.txt")).await.unwrap(),
        options_default
    );
    assert!(
        !root.join("loader/.pkg_cache").join("survival").exists(),
        "Temporary ZIP cache must be cleaned up after extraction"
    );

    // 2. Player disables optional mod `minimap.jar` -> `minimap.jar-disable` and edits `options.txt`
    tokio::fs::rename(
        game_dir.join("mods/minimap.jar"),
        game_dir.join("mods/minimap.jar-disable"),
    )
    .await
    .unwrap();
    tokio::fs::write(game_dir.join("options.txt"), b"fov:1.25")
        .await
        .unwrap();

    // Re-sync with zero remote changes -> MUST download 0 ZIP packages!
    manager
        .sync_bundle(&downloader, &dist1.items, &dist1.packages, &cb)
        .await
        .unwrap();
    assert_eq!(config_hits.load(Ordering::SeqCst), 1, "No config ZIP re-download");
    assert_eq!(mods_hits.load(Ordering::SeqCst), 1, "Disabled optional mod must not trigger mods ZIP re-download");
    assert_eq!(root_hits.load(Ordering::SeqCst), 1, "Modified ignored options.txt must not trigger root ZIP re-download");

    // 3. Server modifies ONLY `config/forge-client.toml` -> ONLY `config-p1` ZIP is downloaded!
    files_map
        .lock()
        .unwrap()
        .insert("/pkg/config.zip".to_string(), zip_config_v2.clone());
    *manifest_state.lock().unwrap() = make_manifest(
        &base,
        config_v2,
        &zip_config_v2,
        jei_v1,
        minimap_v1,
        &zip_mods_v1,
        servers_v1,
        &zip_root_v1,
    );

    let dist2 = manager
        .fetch_custom_distribution_packaged(downloader.client(), Some(&format!("{base}/files")))
        .await
        .unwrap();
    manager
        .sync_bundle(&downloader, &dist2.items, &dist2.packages, &cb)
        .await
        .unwrap();

    assert_eq!(
        config_hits.load(Ordering::SeqCst),
        2,
        "Modified config ZIP must be downloaded"
    );
    assert_eq!(
        mods_hits.load(Ordering::SeqCst),
        1,
        "Untouched mods ZIP must NOT be downloaded"
    );
    assert_eq!(
        root_hits.load(Ordering::SeqCst),
        1,
        "Untouched root ZIP must NOT be downloaded"
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("config/forge-client.toml"))
            .await
            .unwrap(),
        config_v2
    );

    // 4. Server updates `mods` ZIP (`jei_v2` + `minimap_v2`) AND `servers.dat` (`root` ZIP):
    //    - `jei.jar` updates to `jei_v2`
    //    - `minimap.jar` MUST NOT be re-created; `minimap.jar-disable` receives `minimap_v2` while staying disabled!
    //    - `options.txt` MUST keep player's `fov:1.25` while `servers.dat` updates to `servers_v2`!
    {
        let mut fm = files_map.lock().unwrap();
        fm.insert("/pkg/mods.zip".to_string(), zip_mods_v2.clone());
        fm.insert("/pkg/root.zip".to_string(), zip_root_v2.clone());
    }
    *manifest_state.lock().unwrap() = make_manifest(
        &base,
        config_v2,
        &zip_config_v2,
        jei_v2,
        minimap_v2,
        &zip_mods_v2,
        servers_v2,
        &zip_root_v2,
    );

    let dist3 = manager
        .fetch_custom_distribution_packaged(downloader.client(), Some(&format!("{base}/files")))
        .await
        .unwrap();
    manager
        .sync_bundle(&downloader, &dist3.items, &dist3.packages, &cb)
        .await
        .unwrap();

    assert_eq!(config_hits.load(Ordering::SeqCst), 2, "Config ZIP untouched in step 4");
    assert_eq!(mods_hits.load(Ordering::SeqCst), 2, "Mods ZIP downloaded once in step 4");
    assert_eq!(root_hits.load(Ordering::SeqCst), 2, "Root ZIP downloaded once in step 4");

    assert_eq!(
        tokio::fs::read(game_dir.join("mods/jei.jar")).await.unwrap(),
        jei_v2
    );
    assert!(
        !game_dir.join("mods/minimap.jar").exists(),
        "Disabled optional mod must NEVER be re-enabled by ZIP extraction"
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("mods/minimap.jar-disable"))
            .await
            .unwrap(),
        minimap_v2,
        "Disabled optional mod .jar-disable receives updated bytes cleanly"
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("options.txt")).await.unwrap(),
        b"fov:1.25",
        "Ignored options.txt must NOT be overwritten by root ZIP extraction"
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("servers.dat")).await.unwrap(),
        servers_v2
    );

    // 5. Verify resilient fallback to per-file URL when a package `.zip` returns HTTP 404,
    //    without emitting spurious `LaunchEvent::Error`!
    let fallback_dest = game_dir.join("extra/fallback.cfg");
    let broken_pkg = minecraft_rust_core::bundle::PackageDescriptor {
        id: "extra-p1".to_string(),
        folder: "extra".to_string(),
        url: format!("{base}/pkg/missing-404.zip"),
        sha1: None,
        size: Some(100),
        files_count: Some(1),
    };
    let fallback_item = BundleItem::download(
        fallback_dest.clone(),
        format!("{base}/raw/fallback.cfg"),
        Some(sha1_hex(b"FALLBACK_OK")),
        Some(11),
        "extra",
    )
    .with_package_id("extra-p1");

    manager
        .sync_bundle(&downloader, &[fallback_item], &[broken_pkg], &cb)
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read(&fallback_dest).await.unwrap(),
        b"FALLBACK_OK"
    );
    assert!(
        !extract_events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, LaunchEvent::Error { .. })),
        "Successful per-file fallback must not emit LaunchEvent::Error"
    );
}

#[tokio::test]
async fn test_file_hash_cache_persistence_zero_read_hit_invalidation_and_asset_fast_path() {
    use minecraft_rust_core::FileHashCache;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let game_dir = root.join("instances").join("perf-test");
    tokio::fs::create_dir_all(game_dir.join("mods")).await.unwrap();

    let mod_path = game_dir.join("mods/sodium.jar");
    let mod_v1 = b"SODIUM_MOD_V1_PAYLOAD";
    let sha1_v1 = sha1_hex(mod_v1);
    tokio::fs::write(&mod_path, mod_v1).await.unwrap();

    // 1. Cold check_bundle hashes `sodium.jar` and persists `.file_hash_cache_v1.json`
    let cache = FileHashCache::load(&root).await;
    assert!(cache.is_empty());

    let mgr = BundleManager::new(root.clone(), game_dir.clone(), false, &[])
        .with_hash_cache(cache.clone());
    let item_v1 = BundleItem::download(
        mod_path.clone(),
        "https://cdn.example.com/mods/sodium.jar",
        Some(sha1_v1.clone()),
        Some(mod_v1.len() as u64),
        "mods",
    );

    let pending_cold = mgr
        .check_bundle(std::slice::from_ref(&item_v1), &noop_event_callback())
        .await
        .unwrap();
    assert!(pending_cold.is_empty());
    cache.save().await.unwrap();
    assert!(FileHashCache::cache_file_path(&root).is_file());

    // 2. Reload persisted cache from disk and verify warm 0-read hit
    let reloaded = FileHashCache::load(&root).await;
    assert_eq!(
        reloaded.get_if_fresh_path(&mod_path).as_deref(),
        Some(sha1_v1.as_str())
    );

    // 3. Remote update changes `expected_sha1` while local file is untouched -> MUST queue download!
    let item_remote_v2 = BundleItem::download(
        mod_path.clone(),
        "https://cdn.example.com/mods/sodium.jar",
        Some(sha1_hex(b"SODIUM_MOD_V2_NEW_RELEASE")),
        Some(mod_v1.len() as u64),
        "mods",
    );
    let mgr_reloaded = BundleManager::new(root.clone(), game_dir.clone(), false, &[])
        .with_hash_cache(reloaded.clone());
    let pending_remote_update = mgr_reloaded
        .check_bundle(&[item_remote_v2], &noop_event_callback())
        .await
        .unwrap();
    assert_eq!(pending_remote_update.len(), 1);

    // 4. Local file size/content changes on disk -> cached entry invalidated and re-hashed!
    tokio::fs::write(&mod_path, b"CORRUPTED_LOCAL_MOD_DIFFERENT_SIZE")
        .await
        .unwrap();
    assert!(reloaded.get_if_fresh_path(&mod_path).is_none());
    let pending_corrupt = mgr_reloaded
        .check_bundle(&[item_v1], &noop_event_callback())
        .await
        .unwrap();
    assert_eq!(pending_corrupt.len(), 1);

    // 5. Mojang content-addressed asset (`assets/objects/<xx>/<40-hex-sha1>`):
    //    - `verify == false` + matching `size` -> 0 file opens (treated as valid)
    //    - `verify == true` -> opens & hashes file, catching tampered bytes of identical length!
    let real_asset_bytes = b"MOJANG_SOUND_OGG_BYTES_12345";
    let asset_sha1 = sha1_hex(real_asset_bytes);
    let asset_path = root
        .join("assets/objects")
        .join(&asset_sha1[..2])
        .join(&asset_sha1);
    tokio::fs::create_dir_all(asset_path.parent().unwrap())
        .await
        .unwrap();
    // Write same-length tampered bytes!
    let tampered_same_len = vec![b'X'; real_asset_bytes.len()];
    tokio::fs::write(&asset_path, &tampered_same_len)
        .await
        .unwrap();

    let asset_item = BundleItem::download(
        asset_path.clone(),
        "https://resources.download.minecraft.net/fake",
        Some(asset_sha1),
        Some(real_asset_bytes.len() as u64),
        "Assets",
    );

    let fast_mgr = BundleManager::new(root.clone(), game_dir.clone(), false, &[])
        .with_hash_cache(FileHashCache::in_memory());
    assert!(
        fast_mgr
            .check_bundle(std::slice::from_ref(&asset_item), &noop_event_callback())
            .await
            .unwrap()
            .is_empty(),
        "verify=false content-addressed asset fast-path checks size only"
    );

    let strict_mgr = BundleManager::new(root.clone(), game_dir.clone(), true, &[])
        .with_verify_mode(true)
        .with_hash_cache(FileHashCache::in_memory());
    assert_eq!(
        strict_mgr
            .check_bundle(&[asset_item], &noop_event_callback())
            .await
            .unwrap()
            .len(),
        1,
        "verify=true must hash content-addressed asset and catch tampered bytes"
    );
}

#[tokio::test]
async fn test_isolated_natives_stamp_warm_skip_and_cleaner_protection() {
    use minecraft_rust_core::minecraft::libraries::extract_natives_to;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let game_dir = root.join("instances").join("instance-one");
    let natives_dir = game_dir.join("natives");

    let native_jar = root.join("libraries/org/lwjgl/lwjgl-natives-linux.jar");
    tokio::fs::create_dir_all(native_jar.parent().unwrap())
        .await
        .unwrap();
    let zip_bytes = build_zip_bytes(&[
        ("liblwjgl.so", b"FAKE_ELF_SHARED_OBJECT"),
        ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\n"),
    ]);
    tokio::fs::write(&native_jar, &zip_bytes).await.unwrap();

    let extract_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_ref = Arc::clone(&extract_count);
    let cb: EventCallback = Arc::new(move |ev| {
        if matches!(ev, LaunchEvent::Extract { .. }) {
            count_ref.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    // Cold extract -> writes `liblwjgl.so` + `.natives.json`, skips `META-INF`
    extract_natives_to(&natives_dir, std::slice::from_ref(&native_jar), &cb)
        .await
        .unwrap();
    assert_eq!(extract_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(natives_dir.join("liblwjgl.so").is_file());
    assert!(natives_dir.join(".natives.json").is_file());
    assert!(!natives_dir.join("META-INF").exists());

    // Warm extract -> `.natives.json` stamp matches `(path, size, mtime)`, 0 re-extractions!
    extract_natives_to(&natives_dir, std::slice::from_ref(&native_jar), &cb)
        .await
        .unwrap();
    assert_eq!(
        extract_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "Warm launch must skip re-extracting unchanged native libraries"
    );

    // `BundleManager::check_files` MUST protect `<game_dir>/natives`!
    let manager = BundleManager::new(root, game_dir, true, &[]);
    manager.check_files(&[]).await.unwrap();
    assert!(
        natives_dir.join("liblwjgl.so").is_file(),
        "Isolated instance natives/ directory must never be deleted by check_files"
    );
}

#[tokio::test]
async fn test_per_package_fallback_preserves_succeeded_packages() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let game_dir = root.join("instances").join("per-pkg");

    let good_payload = b"GOOD_MOD_FROM_ZIP_ONLY";
    let broken_payload = b"RECOVERED_VIA_PER_FILE_FALLBACK";
    let good_zip = build_zip_bytes(&[("mods/good.jar", good_payload)]);

    let good_raw_hits = Arc::new(AtomicUsize::new(0));
    let broken_raw_hits = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let g_hits = Arc::clone(&good_raw_hits);
    let b_hits = Arc::clone(&broken_raw_hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let good_zip_clone = good_zip.clone();
            let g_hits = Arc::clone(&g_hits);
            let b_hits = Arc::clone(&b_hits);
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");

                let send_ok = |body: &[u8]| {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let mut v = head.into_bytes();
                    v.extend_from_slice(body);
                    v
                };

                let resp = match path {
                    "/pkg/good.zip" => send_ok(&good_zip_clone),
                    "/pkg/broken.zip" => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                    "/raw/good.jar" => {
                        g_hits.fetch_add(1, Ordering::SeqCst);
                        send_ok(good_payload)
                    }
                    "/raw/broken.cfg" => {
                        b_hits.fetch_add(1, Ordering::SeqCst);
                        send_ok(broken_payload)
                    }
                    _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec(),
                };
                let _ = socket.write_all(&resp).await;
            });
        }
    });

    let downloader = Downloader::new(4, 3_000).with_no_proxy();
    let manager = BundleManager::new(root, game_dir.clone(), true, &[]);

    let packages = vec![
        minecraft_rust_core::bundle::PackageDescriptor {
            id: "good-p1".to_string(),
            folder: "mods".to_string(),
            url: format!("{base}/pkg/good.zip"),
            sha1: None,
            size: None,
            files_count: Some(1),
        },
        minecraft_rust_core::bundle::PackageDescriptor {
            id: "broken-p1".to_string(),
            folder: "config".to_string(),
            url: format!("{base}/pkg/broken.zip"),
            sha1: None,
            size: None,
            files_count: Some(1),
        },
    ];

    let items = vec![
        BundleItem::download(
            game_dir.join("mods/good.jar"),
            format!("{base}/raw/good.jar"),
            Some(sha1_hex(good_payload)),
            Some(good_payload.len() as u64),
            "mods",
        )
        .with_package_id("good-p1"),
        BundleItem::download(
            game_dir.join("config/broken.cfg"),
            format!("{base}/raw/broken.cfg"),
            Some(sha1_hex(broken_payload)),
            Some(broken_payload.len() as u64),
            "config",
        )
        .with_package_id("broken-p1"),
    ];

    manager
        .sync_bundle(&downloader, &items, &packages, &noop_event_callback())
        .await
        .unwrap();

    assert_eq!(
        good_raw_hits.load(Ordering::SeqCst),
        0,
        "Succeeded package must NOT fall back to per-file download when sibling package fails!"
    );
    assert_eq!(
        broken_raw_hits.load(Ordering::SeqCst),
        1,
        "Failed package must fall back cleanly to per-file download"
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("mods/good.jar")).await.unwrap(),
        good_payload
    );
    assert_eq!(
        tokio::fs::read(game_dir.join("config/broken.cfg"))
            .await
            .unwrap(),
        broken_payload
    );
}


