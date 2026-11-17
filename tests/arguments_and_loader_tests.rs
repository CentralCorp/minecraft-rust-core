use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use minecraft_rust_core::auth::mojang::offline_login;
use minecraft_rust_core::config::{
    JavaOptions, LaunchOptions, LoaderOptions, LoaderType, MemoryOptions, ScreenOptions,
    ServerConnect,
};
use minecraft_rust_core::loader::neoforge::{
    neoforge_version_prefix, select_modern_neoforge_build, select_neoforge_1_20_1_build,
};
use minecraft_rust_core::loader::patcher::{
    is_already_patched, resolve_processor_token, InstallDataEntry, PatchContext, ProcessorSpec,
};
use minecraft_rust_core::minecraft::arguments::{
    build_launch_arguments, LoaderProfileData, RawArgumentsSection,
};
use minecraft_rust_core::utils::maven::MavenArtifact;
use minecraft_rust_core::utils::zip_extract::{parse_manifest_main_class, read_jar_main_class};

fn make_base_options(root: PathBuf) -> LaunchOptions {
    LaunchOptions {
        url: None,
        authenticator: offline_login("CentralTester"),
        timeout_ms: 5_000,
        path: root,
        version: "1.20.4".to_string(),
        instance: None,
        detached: false,
        intel_enabled_mac: false,
        concurrent_downloads: 4,
        bypass_offline: false,
        loader: LoaderOptions::default(),
        mcp: None,
        verify: false,
        ignored: Vec::new(),
        jvm_args: Vec::new(),
        game_args: Vec::new(),
        auto_connect: None,
        java: JavaOptions::default(),
        screen: ScreenOptions::default(),
        memory: MemoryOptions {
            min: "2G".to_string(),
            max: "4G".to_string(),
        },
    }
}

#[test]
fn test_loader_type_case_insensitive_deserialization() {
    for (raw, expected) in [
        ("\"forge\"", LoaderType::Forge),
        ("\"FORGE\"", LoaderType::Forge),
        ("\"neoForge\"", LoaderType::NeoForge),
        ("\"neoforge\"", LoaderType::NeoForge),
        ("\"Neo-Forge\"", LoaderType::NeoForge),
        ("\"fabric\"", LoaderType::Fabric),
        ("\"LegacyFabric\"", LoaderType::LegacyFabric),
        ("\"legacy-fabric\"", LoaderType::LegacyFabric),
        ("\"legacyfabric\"", LoaderType::LegacyFabric),
        ("\"Quilt\"", LoaderType::Quilt),
    ] {
        let parsed: LoaderType = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed, expected);
    }

    let opt_json = serde_json::json!({
        "type": "neoForge",
        "build": "21.1.72",
        "enable": true
    });
    let loader_opts: LoaderOptions = serde_json::from_value(opt_json).unwrap();
    assert_eq!(loader_opts.loader_type, Some(LoaderType::NeoForge));
    assert_eq!(loader_opts.build, "21.1.72");
    assert!(loader_opts.enable);
}

#[test]
fn test_multi_instance_game_dir_and_loader_dir_isolation() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    let mut inst_a = make_base_options(root.clone());
    inst_a.instance = Some("pvp-faction".to_string());
    inst_a.loader = LoaderOptions {
        path: None,
        loader_type: Some(LoaderType::Forge),
        build: "47.3.0".to_string(),
        enable: true,
    };

    let mut inst_b = make_base_options(root.clone());
    inst_b.instance = Some("create-modpack".to_string());
    inst_b.loader = LoaderOptions {
        path: None,
        loader_type: Some(LoaderType::NeoForge),
        build: "21.1.72".to_string(),
        enable: true,
    };

    assert_eq!(inst_a.game_dir(), root.join("instances").join("pvp-faction"));
    assert_eq!(inst_b.game_dir(), root.join("instances").join("create-modpack"));
    assert_ne!(inst_a.loader_dir("1.20.1"), inst_b.loader_dir("1.21.1"));
    assert!(inst_a
        .loader_dir("1.20.1")
        .starts_with(root.join("loader").join("pvp-faction")));
    assert!(inst_b
        .loader_dir("1.21.1")
        .starts_with(root.join("loader").join("create-modpack")));
}

#[test]
fn test_semver_classpath_deduplication_and_launch_arguments() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    let mut opts = make_base_options(root.clone());
    opts.instance = Some("survival-1204".to_string());
    opts.bypass_offline = true;
    opts.screen = ScreenOptions {
        width: Some(1920),
        height: Some(1080),
        fullscreen: true,
    };
    opts.auto_connect = Some(ServerConnect {
        host: "play.centralcorp.fr".to_string(),
        port: 25565,
    });

    let version_json = serde_json::json!({
        "id": "1.20.4",
        "type": "release",
        "assets": "12",
        "mainClass": "net.minecraft.client.main.Main",
        "arguments": {
            "game": [
                "--username", "${auth_player_name}",
                "--version", "${version_name}",
                "--gameDir", "${game_directory}",
                "--assetsDir", "${assets_root}",
                "--assetIndex", "${assets_index_name}",
                "--uuid", "${auth_uuid}",
                "--accessToken", "${auth_access_token}"
            ],
            "jvm": [
                "-Djava.library.path=${natives_directory}",
                "-cp", "${classpath}"
            ]
        },
        "libraries": [
            { "name": "org.ow2.asm:asm:9.3" },
            { "name": "com.google.guava:guava:21.0-jre" },
            { "name": "org.lwjgl:lwjgl:3.3.2" },
            { "name": "org.lwjgl:lwjgl:3.3.2:natives-linux" }
        ]
    });

    let loader_profile = LoaderProfileData {
        id: Some("fabric-loader-0.16.9-1.20.4".to_string()),
        main_class: Some("net.fabricmc.loader.impl.launch.knot.KnotClient".to_string()),
        minecraft_arguments: None,
        arguments: Some(RawArgumentsSection {
            game: Vec::new(),
            jvm: vec![serde_json::json!("-DFabricMcEmu= net.minecraft.client.main.Main ")],
        }),
        libraries: vec![
            serde_json::json!({ "name": "org.ow2.asm:asm:9.7" }),
            serde_json::json!({ "name": "com.google.guava:guava:32.1.2-jre" }),
            serde_json::json!({ "name": "net.fabricmc:fabric-loader:0.16.9" }),
        ],
        is_old_forge: false,
        custom_jar_path: None,
    };

    let built = build_launch_arguments(&opts, "1.20.4", &version_json, Some(&loader_profile)).unwrap();
    assert_eq!(
        built.main_class,
        "net.fabricmc.loader.impl.launch.knot.KnotClient"
    );

    let cp_strs: Vec<String> = built
        .classpath_entries
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();

    // `asm:9.7` MUST win over `asm:9.3`, and `guava:32.1.2-jre` MUST win over `guava:21.0-jre`!
    assert!(cp_strs.iter().any(|p| p.ends_with("asm-9.7.jar")));
    assert!(!cp_strs.iter().any(|p| p.ends_with("asm-9.3.jar")));
    assert!(cp_strs.iter().any(|p| p.ends_with("guava-32.1.2-jre.jar")));
    assert!(!cp_strs.iter().any(|p| p.ends_with("guava-21.0-jre.jar")));

    // Both `lwjgl-3.3.2.jar` AND `lwjgl-3.3.2-natives-linux.jar` have distinct classifiers and stay intact
    assert!(cp_strs.iter().any(|p| p.ends_with("lwjgl-3.3.2.jar")));
    if cfg!(target_os = "linux") {
        assert!(cp_strs.iter().any(|p| p.ends_with("lwjgl-3.3.2-natives-linux.jar")));
    }

    // Verify isolated `--gameDir` vs shared `--assetsDir`
    let game_dir_idx = built.game_args.iter().position(|a| a == "--gameDir").unwrap();
    assert_eq!(
        PathBuf::from(&built.game_args[game_dir_idx + 1]),
        root.join("instances").join("survival-1204")
    );
    let assets_dir_idx = built.game_args.iter().position(|a| a == "--assetsDir").unwrap();
    assert_eq!(
        PathBuf::from(&built.game_args[assets_dir_idx + 1]),
        root.join("assets")
    );

    // Verify `--fullscreen`, `--width`, `--height`, and 1.20+ `--quickPlayMultiplayer`
    assert!(built.game_args.contains(&"--fullscreen".to_string()));
    assert!(built.game_args.contains(&"--width".to_string()));
    assert!(built.game_args.contains(&"1920".to_string()));
    assert!(built.game_args.contains(&"--quickPlayMultiplayer".to_string()));
    assert!(built.game_args.contains(&"play.centralcorp.fr:25565".to_string()));

    // Verify `bypass_offline` JVM flags
    assert!(built
        .jvm_args
        .contains(&"-Dminecraft.api.auth.host=https://nope.invalid/".to_string()));
    assert!(built.jvm_args.contains(&"-Xms2G".to_string()));
    assert!(built.jvm_args.contains(&"-Xmx4G".to_string()));
}

#[test]
fn test_manifest_mf_multiline_and_crlf_parsing() {
    // RFC 822 / JAR spec 72-byte wrapped `Main-Class` with CRLF line endings
    let wrapped_manifest = concat!(
        "Manifest-Version: 1.0\r\n",
        "Implementation-Title: installertools\r\n",
        "Main-Class: net.minecraftforge.installertools.Tas\r\n",
        " kMain\r\n",
        "Created-By: 17.0.8 (Eclipse Adoptium)\r\n"
    );
    assert_eq!(
        parse_manifest_main_class(wrapped_manifest).as_deref(),
        Some("net.minecraftforge.installertools.TaskMain")
    );

    // Verify reading directly from a real `.jar` archive
    let tmp = TempDir::new().unwrap();
    let jar_path = tmp.path().join("processor.jar");
    {
        let f = File::create(&jar_path).unwrap();
        let mut zip = ZipWriter::new(f);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(wrapped_manifest.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    assert_eq!(
        read_jar_main_class(&jar_path).unwrap().as_deref(),
        Some("net.minecraftforge.installertools.TaskMain")
    );
}

#[test]
fn test_patcher_token_resolution_installer_path_and_cache_check() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let libs_dir = root.join("libraries");
    let loader_dir = root.join("loader").join("forge-1.20.1-47.3.0");
    std::fs::create_dir_all(&libs_dir).unwrap();
    std::fs::create_dir_all(&loader_dir).unwrap();

    // Create a synthetic Forge installer JAR containing `/data/client.lzma`
    let installer_jar = loader_dir.join("forge-1.20.1-47.3.0-installer.jar");
    {
        let f = File::create(&installer_jar).unwrap();
        let mut zip = ZipWriter::new(f);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        zip.start_file("data/client.lzma", opts).unwrap();
        zip.write_all(b"LZMA_BINARY_PATCH_PAYLOAD").unwrap();
        zip.finish().unwrap();
    }

    // Also create `forge-1.20.1-47.3.0-universal.jar` so we verify `{BINPATCH}` NEVER overwrites it!
    let base_art = MavenArtifact::parse("net.minecraftforge:forge:1.20.1-47.3.0:universal").unwrap();
    let universal_jar_path = base_art.to_library_path(&libs_dir);
    std::fs::create_dir_all(universal_jar_path.parent().unwrap()).unwrap();
    std::fs::write(&universal_jar_path, b"ORIGINAL_UNIVERSAL_JAR").unwrap();

    let mc_jar = root.join("versions/1.20.1/1.20.1.jar");
    let mc_json = root.join("versions/1.20.1/1.20.1.json");
    let java_bin = PathBuf::from("/usr/bin/java");

    let ctx = PatchContext {
        root_dir: root,
        libraries_dir: &libs_dir,
        loader_dir: &loader_dir,
        installer_jar: &installer_jar,
        mc_jar: &mc_jar,
        mc_json: &mc_json,
        java_bin: &java_bin,
        base_loader_artifact: Some(&base_art),
    };

    let mut data_map = HashMap::new();
    data_map.insert(
        "BINPATCH".to_string(),
        InstallDataEntry {
            client: "/data/client.lzma".to_string(),
            server: "/data/server.lzma".to_string(),
        },
    );
    data_map.insert(
        "PATCHED".to_string(),
        InstallDataEntry {
            client: "[net.minecraftforge:forge:1.20.1-47.3.0:client]".to_string(),
            server: String::new(),
        },
    );
    data_map.insert(
        "PATCHED_SHA".to_string(),
        InstallDataEntry {
            client: "'0123456789abcdef0123456789abcdef01234567'".to_string(),
            server: String::new(),
        },
    );

    // 1. `{INSTALLER}` MUST resolve to the actual `.jar` file path, NOT `{ROOT}`!
    let resolved_installer = resolve_processor_token("{INSTALLER}", &data_map, &ctx).unwrap();
    assert_eq!(PathBuf::from(&resolved_installer), installer_jar);

    // 2. Single-quoted `'literal'` via `{PATCHED_SHA}` strips quotes cleanly
    let resolved_sha = resolve_processor_token("{PATCHED_SHA}", &data_map, &ctx).unwrap();
    assert_eq!(resolved_sha, "0123456789abcdef0123456789abcdef01234567");

    // 3. `{BINPATCH}` extracts `/data/client.lzma` AND mirrors `-clientdata.lzma` without touching `universal.jar`!
    let resolved_binpatch = resolve_processor_token("{BINPATCH}", &data_map, &ctx).unwrap();
    assert_eq!(
        std::fs::read(&resolved_binpatch).unwrap(),
        b"LZMA_BINARY_PATCH_PAYLOAD"
    );
    assert_eq!(
        std::fs::read(&universal_jar_path).unwrap(),
        b"ORIGINAL_UNIVERSAL_JAR"
    );
    let lzma_mirror = base_art.for_clientdata_lzma().to_library_path(&libs_dir);
    assert_eq!(
        std::fs::read(&lzma_mirror).unwrap(),
        b"LZMA_BINARY_PATCH_PAYLOAD"
    );

    // 4. `is_already_patched` checks `.patched.ok` + actual `--output` / `outputs` files, NOT `'literal'` args!
    let patched_jar_path = PathBuf::from(resolve_processor_token("{PATCHED}", &data_map, &ctx).unwrap());
    let proc_spec = ProcessorSpec {
        sides: Some(vec!["client".to_string()]),
        jar: "net.minecraftforge:binarypatcher:1.1.1".to_string(),
        classpath: Vec::new(),
        args: vec![
            "--clean".to_string(),
            "{MINECRAFT_JAR}".to_string(),
            "--output".to_string(),
            "{PATCHED}".to_string(),
            "--apply".to_string(),
            "{BINPATCH}".to_string(),
            "-- expected-sha".to_string(),
            "{PATCHED_SHA}".to_string(),
        ],
        outputs: Some(HashMap::from([(
            "{PATCHED}".to_string(),
            "{PATCHED_SHA}".to_string(),
        )])),
    };

    assert!(!is_already_patched(
        std::slice::from_ref(&proc_spec),
        &data_map,
        &ctx
    ));
    std::fs::write(&patched_jar_path, b"PATCHED_CLIENT_JAR").unwrap();
    std::fs::write(loader_dir.join(".patched.ok"), b"ok\n").unwrap();
    assert!(is_already_patched(&[proc_spec], &data_map, &ctx));
}

#[test]
fn test_neoforge_legacy_1_20_1_and_modern_version_selection() {
    assert_eq!(neoforge_version_prefix("1.20.1").as_deref(), Some("1.20.1-"));
    assert_eq!(neoforge_version_prefix("1.20.4").as_deref(), Some("20.4."));
    assert_eq!(neoforge_version_prefix("1.21").as_deref(), Some("21.0."));
    assert_eq!(neoforge_version_prefix("1.21.1").as_deref(), Some("21.1."));
    assert_eq!(neoforge_version_prefix("1.21.4").as_deref(), Some("21.4."));

    // Legacy 1.20.1 (`net.neoforged:forge`)
    let legacy_list = vec![
        "1.20.1-47.1.54".to_string(),
        "47.1.99".to_string(),
        "1.20.1-47.1.106".to_string(),
    ];
    assert_eq!(
        select_neoforge_1_20_1_build(&legacy_list, "latest", true, false).unwrap(),
        "1.20.1-47.1.106"
    );

    // Modern 1.21.1 (`net.neoforged:neoforge`) — `latest` vs `recommended` skipping `-beta`
    let modern_list = vec![
        "20.4.237".to_string(),
        "21.0.167".to_string(),
        "21.1.72".to_string(),
        "21.1.73-beta".to_string(),
    ];
    assert_eq!(
        select_modern_neoforge_build(&modern_list, "1.21.1", "21.1.", "latest", true, false)
            .unwrap(),
        "21.1.73-beta"
    );
    assert_eq!(
        select_modern_neoforge_build(&modern_list, "1.21.1", "21.1.", "recommended", false, true)
            .unwrap(),
        "21.1.72"
    );
}
