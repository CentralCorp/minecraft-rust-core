use std::cmp::Ordering;
use std::path::Path;

use minecraft_rust_core::minecraft::libraries::collect_vanilla_libraries;
use minecraft_rust_core::minecraft::manifest::apply_linux_arm_lwjgl_overrides;
use minecraft_rust_core::utils::maven::{compare_maven_versions, MavenArtifact};
use minecraft_rust_core::utils::os_rules::{
    collect_arch_specific_native_families, evaluate_rules, is_library_allowed, MojangRule,
    RuleContext, TargetArch, TargetOs,
};

#[test]
fn test_maven_coordinate_parsing_and_extensions() {
    // 1. Standard 3-part coordinate
    let asm = MavenArtifact::parse("org.ow2.asm:asm:9.7").unwrap();
    assert_eq!(asm.group, "org.ow2.asm");
    assert_eq!(asm.artifact, "asm");
    assert_eq!(asm.version, "9.7");
    assert_eq!(asm.classifier, None);
    assert_eq!(asm.extension, "jar");
    assert_eq!(asm.relative_path(), "org/ow2/asm/asm/9.7/asm-9.7.jar");

    // 2. 4-part coordinate with classifier
    let forge_univ = MavenArtifact::parse("net.minecraftforge:forge:1.20.1-47.3.0:universal").unwrap();
    assert_eq!(forge_univ.classifier.as_deref(), Some("universal"));
    assert_eq!(forge_univ.extension, "jar");
    assert_eq!(
        forge_univ.relative_path(),
        "net/minecraftforge/forge/1.20.1-47.3.0/forge-1.20.1-47.3.0-universal.jar"
    );

    // 3. Explicit `@zip` / `@txt` extension without classifier and with classifier
    let mcp = MavenArtifact::parse("de.oceanlabs.mcp:mcp_config:1.20.1-20230612.114412@zip").unwrap();
    assert_eq!(mcp.classifier, None);
    assert_eq!(mcp.extension, "zip");
    assert_eq!(
        mcp.relative_path(),
        "de/oceanlabs/mcp/mcp_config/1.20.1-20230612.114412/mcp_config-1.20.1-20230612.114412.zip"
    );

    let mappings = MavenArtifact::parse(
        "[net.minecraft:client:1.20.1-20230612.114412:mappings@txt]",
    )
    .unwrap();
    assert_eq!(mappings.classifier.as_deref(), Some("mappings"));
    assert_eq!(mappings.extension, "txt");
    assert_eq!(
        mappings.relative_path(),
        "net/minecraft/client/1.20.1-20230612.114412/client-1.20.1-20230612.114412-mappings.txt"
    );

    // 4. CRITICAL AUDIT BUG FIX: `@jar` + `-clientdata.lzma` must NEVER corrupt `universal.jar`!
    let neoforge_univ =
        MavenArtifact::parse("net.neoforged:neoforge:21.1.72:universal@jar").unwrap();
    assert_eq!(
        neoforge_univ.relative_path(),
        "net/neoforged/neoforge/21.1.72/neoforge-21.1.72-universal.jar"
    );

    let clientdata_lzma = neoforge_univ.for_clientdata_lzma();
    assert_eq!(
        clientdata_lzma.relative_path(),
        "net/neoforged/neoforge/21.1.72/neoforge-21.1.72-clientdata.lzma"
    );
    assert_ne!(
        neoforge_univ.relative_path(),
        clientdata_lzma.relative_path()
    );
}

#[test]
fn test_compare_maven_versions_semver_and_non_semver() {
    assert_eq!(compare_maven_versions("3.3.1", "3.3.2"), Ordering::Less);
    assert_eq!(compare_maven_versions("3.3.3", "3.3.2"), Ordering::Greater);
    assert_eq!(compare_maven_versions("9.7", "9.3"), Ordering::Greater);
    assert_eq!(
        compare_maven_versions("21.0-jre", "32.1.2-jre"),
        Ordering::Less
    );
    assert_eq!(
        compare_maven_versions("47.1.99", "47.1.106"),
        Ordering::Less
    );
    assert_eq!(
        compare_maven_versions("21.1.72-beta", "21.1.72"),
        Ordering::Less
    );
    assert_eq!(compare_maven_versions("1.20.1", "1.20.1"), Ordering::Equal);
}

#[test]
fn test_mojang_os_arch_and_feature_rules() {
    // Multi-rule allow all except osx (used by Linux/Windows-only libraries)
    let rules_json = serde_json::json!([
        { "action": "allow" },
        { "action": "disallow", "os": { "name": "osx" } }
    ]);
    let rules: Vec<MojangRule> = serde_json::from_value(rules_json).unwrap();

    let linux_ctx = RuleContext::new(TargetOs::Linux, TargetArch::X86_64, false);
    let mac_ctx = RuleContext::new(TargetOs::MacOs, TargetArch::Arm64, false);
    assert!(evaluate_rules(&rules, &linux_ctx));
    assert!(!evaluate_rules(&rules, &mac_ctx));

    // Feature + arch rule
    let feature_rule_json = serde_json::json!([
        {
            "action": "allow",
            "os": { "name": "windows", "arch": "x86" },
            "features": { "has_custom_resolution": true }
        }
    ]);
    let feature_rules: Vec<MojangRule> = serde_json::from_value(feature_rule_json).unwrap();

    let win_x86_with_res = RuleContext::new(TargetOs::Windows, TargetArch::X86, false)
        .with_feature("has_custom_resolution", true);
    let win_x64_with_res = RuleContext::new(TargetOs::Windows, TargetArch::X86_64, false)
        .with_feature("has_custom_resolution", true);
    let win_x86_no_res = RuleContext::new(TargetOs::Windows, TargetArch::X86, false);

    assert!(evaluate_rules(&feature_rules, &win_x86_with_res));
    assert!(!evaluate_rules(&feature_rules, &win_x64_with_res));
    assert!(!evaluate_rules(&feature_rules, &win_x86_no_res));
}

#[test]
fn test_mc_1_19_plus_native_classifier_arch_disambiguation() {
    let lib_coords = [
        "org.lwjgl:lwjgl:3.3.3",
        "org.lwjgl:lwjgl:3.3.3:natives-windows",
        "org.lwjgl:lwjgl:3.3.3:natives-windows-x86",
        "org.lwjgl:lwjgl:3.3.3:natives-windows-arm64",
        "org.lwjgl:lwjgl:3.3.3:natives-macos",
        "org.lwjgl:lwjgl:3.3.3:natives-macos-arm64",
        "org.lwjgl:lwjgl:3.3.3:natives-linux",
    ];

    let win_families = collect_arch_specific_native_families(lib_coords.iter().copied(), TargetOs::Windows);
    let win_rules: Vec<MojangRule> =
        serde_json::from_value(serde_json::json!([{ "action": "allow", "os": { "name": "windows" } }])).unwrap();

    // Windows x64 -> ONLY `:natives-windows`
    let win_x64 = RuleContext::new(TargetOs::Windows, TargetArch::X86_64, false);
    assert!(is_library_allowed(Some(&win_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-windows"), &win_x64, &win_families));
    assert!(!is_library_allowed(Some(&win_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-windows-x86"), &win_x64, &win_families));
    assert!(!is_library_allowed(Some(&win_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-windows-arm64"), &win_x64, &win_families));

    // Windows x86 -> ONLY `:natives-windows-x86`
    let win_x86 = RuleContext::new(TargetOs::Windows, TargetArch::X86, false);
    assert!(!is_library_allowed(Some(&win_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-windows"), &win_x86, &win_families));
    assert!(is_library_allowed(Some(&win_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-windows-x86"), &win_x86, &win_families));

    // Windows arm64 -> ONLY `:natives-windows-arm64`
    let win_arm64 = RuleContext::new(TargetOs::Windows, TargetArch::Arm64, false);
    assert!(!is_library_allowed(Some(&win_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-windows"), &win_arm64, &win_families));
    assert!(is_library_allowed(Some(&win_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-windows-arm64"), &win_arm64, &win_families));

    // macOS Apple Silicon vs `intel_enabled_mac: true`
    let mac_families = collect_arch_specific_native_families(lib_coords.iter().copied(), TargetOs::MacOs);
    let mac_rules: Vec<MojangRule> =
        serde_json::from_value(serde_json::json!([{ "action": "allow", "os": { "name": "osx" } }])).unwrap();

    let mac_native_arm = RuleContext::new(TargetOs::MacOs, TargetArch::Arm64, false);
    assert!(!is_library_allowed(Some(&mac_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-macos"), &mac_native_arm, &mac_families));
    assert!(is_library_allowed(Some(&mac_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-macos-arm64"), &mac_native_arm, &mac_families));

    let mac_rosetta_intel = RuleContext::new(TargetOs::MacOs, TargetArch::Arm64, true);
    assert!(is_library_allowed(Some(&mac_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-macos"), &mac_rosetta_intel, &mac_families));
    assert!(!is_library_allowed(Some(&mac_rules), Some("org.lwjgl:lwjgl:3.3.3:natives-macos-arm64"), &mac_rosetta_intel, &mac_families));
}

#[test]
fn test_linux_arm_lwjgl_overrides_and_tinyfd_native_path_fix() {
    // 1. MC 1.20.1 uses LWJGL 3.3.1 (< 3.3.3): ARM64 override SHOULD apply and `lwjgl-tinyfd` native path MUST NOT collide with base jar!
    let mut mc_1_20_1 = serde_json::json!({
        "id": "1.20.1",
        "libraries": [
            {
                "name": "com.mojang:logging:1.1.1",
                "downloads": {
                    "artifact": {
                        "path": "com/mojang/logging/1.1.1/logging-1.1.1.jar",
                        "url": "https://libraries.minecraft.net/com/mojang/logging/1.1.1/logging-1.1.1.jar"
                    }
                }
            },
            {
                "name": "org.lwjgl:lwjgl:3.3.1",
                "downloads": {
                    "artifact": {
                        "path": "org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1.jar",
                        "url": "https://libraries.minecraft.net/org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1.jar"
                    }
                }
            }
        ]
    });

    let applied = apply_linux_arm_lwjgl_overrides(&mut mc_1_20_1, TargetArch::Arm64).unwrap();
    assert!(applied);

    // Check `lwjgl-tinyfd` in both 3.3.1 and 3.3.2 for both Arm64 and Arm32!
    for ver in ["3.3.1", "3.3.2"] {
        for arch in [TargetArch::Arm64, TargetArch::Arm32] {
            let mut doc = serde_json::json!({
                "id": "test",
                "libraries": [{ "name": format!("org.lwjgl:lwjgl:{ver}") }]
            });
            assert!(apply_linux_arm_lwjgl_overrides(&mut doc, arch).unwrap());

            let ctx = RuleContext::new(TargetOs::Linux, arch, false);
            let collected = collect_vanilla_libraries(Path::new("/tmp/mc-root"), "test", &doc, &ctx).unwrap();

            let tinyfd_paths: Vec<String> = collected
                .bundle_items
                .iter()
                .map(|i| i.path.to_string_lossy().replace('\\', "/"))
                .filter(|p| p.contains("lwjgl-tinyfd"))
                .collect();

            assert_eq!(tinyfd_paths.len(), 2, "Expected base jar + natives-linux jar for {ver} on {arch:?}");
            assert!(tinyfd_paths.iter().any(|p| p.ends_with(&format!("lwjgl-tinyfd-{ver}.jar"))));
            assert!(tinyfd_paths.iter().any(|p| p.ends_with(&format!("lwjgl-tinyfd-{ver}-natives-linux.jar"))));
        }
    }

    // 2. MC 1.21+ uses LWJGL 3.3.3 (>= 3.3.3): Mojang ships official `natives-linux-arm64`, so override MUST NOT replace libraries!
    let mut mc_1_21 = serde_json::json!({
        "id": "1.21.1",
        "libraries": [
            { "name": "org.lwjgl:lwjgl:3.3.3" },
            { "name": "org.lwjgl:lwjgl:3.3.3:natives-linux-arm64" }
        ]
    });
    let applied_1_21 = apply_linux_arm_lwjgl_overrides(&mut mc_1_21, TargetArch::Arm64).unwrap();
    assert!(!applied_1_21);
    assert_eq!(mc_1_21["libraries"].as_array().unwrap().len(), 2);
}
