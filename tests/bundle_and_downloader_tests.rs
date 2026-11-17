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
