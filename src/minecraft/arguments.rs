use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::LaunchOptions;
use crate::error::{CoreError, CoreResult};
use crate::minecraft::manifest::is_legacy_assets;
use crate::utils::maven::{compare_maven_versions, MavenArtifact};
use crate::utils::os_rules::{
    collect_arch_specific_native_families, evaluate_rules, is_library_allowed, MojangRule,
    RuleContext, TargetArch, TargetOs,
};

/// Installed mod-loader profile data merged into Vanilla launch arguments.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoaderProfileData {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "mainClass")]
    pub main_class: Option<String>,
    #[serde(default, rename = "minecraftArguments")]
    pub minecraft_arguments: Option<String>,
    #[serde(default)]
    pub arguments: Option<RawArgumentsSection>,
    #[serde(default)]
    pub libraries: Vec<serde_json::Value>,
    #[serde(default)]
    pub is_old_forge: bool,
    #[serde(default)]
    pub custom_jar_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawArgumentsSection {
    #[serde(default)]
    pub game: Vec<serde_json::Value>,
    #[serde(default)]
    pub jvm: Vec<serde_json::Value>,
}

/// Final assembled launch command arguments before process spawn.
#[derive(Debug, Clone)]
pub struct BuiltArguments {
    pub jvm_args: Vec<String>,
    pub main_class: String,
    pub game_args: Vec<String>,
    pub classpath_entries: Vec<PathBuf>,
}

/// Builds the complete `jvm_args` (including deduplicated `-cp`), `main_class`, and `game_args`
/// for launching Vanilla or Modded Minecraft (`1.7.10` through `1.21+` and `26.x+`).
pub fn build_launch_arguments(
    options: &LaunchOptions,
    mc_version_id: &str,
    version_json: &serde_json::Value,
    loader_profile: Option<&LoaderProfileData>,
) -> CoreResult<BuiltArguments> {
    let mut rule_ctx = RuleContext::current(options.intel_enabled_mac);
    let has_custom_res =
        options.screen.width.unwrap_or(0) > 0 && options.screen.height.unwrap_or(0) > 0;
    rule_ctx = rule_ctx.with_feature("has_custom_resolution", has_custom_res);
    rule_ctx = rule_ctx.with_feature(
        "is_demo_user",
        options.authenticator.meta.demo.unwrap_or(false),
    );

    let quick_play_supported = supports_quick_play(mc_version_id, version_json);
    if let Some(ref ac) = options.auto_connect {
        let (norm_host, _) = ac.normalized();
        if !norm_host.is_empty() && quick_play_supported {
            // Note: Do NOT enable `has_quick_plays_support` here — Mojang's manifest binds that
            // feature to `["--quickPlayPath", "${quickPlayPath}"]` (log file telemetry), NOT
            // `--quickPlayMultiplayer`!
            rule_ctx = rule_ctx.with_feature("is_quick_play_multiplayer", true);
        }
    }

    let classpath_entries = build_deduplicated_classpath(
        &options.path,
        options.resolved_mcp_path().as_deref(),
        mc_version_id,
        version_json,
        loader_profile,
        &rule_ctx,
    )?;

    let cp_separator = classpath_separator();
    let cp_joined = classpath_entries
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(cp_separator);

    let main_class = loader_profile
        .and_then(|lp| lp.main_class.as_deref())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| version_json.get("mainClass").and_then(|m| m.as_str()))
        .ok_or_else(|| {
            CoreError::InvalidConfig(format!(
                "Missing mainClass in Minecraft {mc_version_id} manifest"
            ))
        })?
        .to_string();

    let jvm_args = build_jvm_arguments(
        options,
        mc_version_id,
        version_json,
        loader_profile,
        &cp_joined,
        &rule_ctx,
    );

    let game_args = build_game_arguments(
        options,
        mc_version_id,
        version_json,
        loader_profile,
        &rule_ctx,
        quick_play_supported,
    );

    Ok(BuiltArguments {
        jvm_args,
        main_class,
        game_args,
        classpath_entries,
    })
}

#[derive(Clone)]
struct SelectedLib {
    order_idx: usize,
    version: String,
    from_loader: bool,
    resolved_path: PathBuf,
}

fn is_non_classpath_extension(ext: &str) -> bool {
    matches!(
        ext.trim().trim_start_matches('.').to_ascii_lowercase().as_str(),
        "txt" | "lzma" | "tsrg" | "pom"
    )
}

fn is_non_classpath_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(is_non_classpath_extension)
        .unwrap_or(false)
}

fn upsert_classpath_slot(
    slot_map: &mut HashMap<String, SelectedLib>,
    order_idx: usize,
    from_loader: bool,
    artifact: &MavenArtifact,
    resolved_path: PathBuf,
) {
    if is_non_classpath_extension(&artifact.extension) || is_non_classpath_path(&resolved_path) {
        return;
    }

    let dedup_key = artifact.dedup_key();
    match slot_map.get_mut(&dedup_key) {
        None => {
            slot_map.insert(
                dedup_key,
                SelectedLib {
                    order_idx,
                    version: artifact.version.clone(),
                    from_loader,
                    resolved_path,
                },
            );
        }
        Some(existing) => {
            let cmp = compare_maven_versions(&artifact.version, &existing.version);
            if cmp == Ordering::Greater
                || (cmp == Ordering::Equal && from_loader && !existing.from_loader)
            {
                existing.version = artifact.version.clone();
                existing.from_loader = from_loader;
                existing.resolved_path = resolved_path;
            }
        }
    }
}

/// Deduplicates combined Loader + Vanilla libraries by `(group, artifact, classifier, extension)`,
/// keeping the highest version via [`compare_maven_versions`] across ALL platforms and MC versions!
pub fn build_deduplicated_classpath(
    root_dir: &Path,
    mcp_jar: Option<&Path>,
    mc_version_id: &str,
    version_json: &serde_json::Value,
    loader_profile: Option<&LoaderProfileData>,
    rule_ctx: &RuleContext,
) -> CoreResult<Vec<PathBuf>> {
    let vanilla_libs = version_json
        .get("libraries")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();

    let mut combined: Vec<(serde_json::Value, bool)> =
        Vec::with_capacity(vanilla_libs.len() + loader_profile.map(|lp| lp.libraries.len()).unwrap_or(0));
    if let Some(lp) = loader_profile {
        for lib in &lp.libraries {
            combined.push((lib.clone(), true));
        }
    }
    for lib in vanilla_libs {
        combined.push((lib, false));
    }

    let all_names: Vec<String> = combined
        .iter()
        .filter_map(|(v, _)| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect();
    let arch_specific_families =
        collect_arch_specific_native_families(all_names.iter().map(String::as_str), rule_ctx.os);

    let mut slot_map: HashMap<String, SelectedLib> = HashMap::new();
    let libraries_root = root_dir.join("libraries");

    for (idx, (lib_val, from_loader)) in combined.iter().enumerate() {
        let Some(coord) = lib_val.get("name").and_then(|n| n.as_str()) else {
            continue;
        };

        // Forge 1.19+ conflict guard from upstream launcher
        if *from_loader && coord.starts_with("org.apache.logging.log4j:log4j-slf4j2-impl") {
            continue;
        }

        let rules: Option<Vec<MojangRule>> = lib_val
            .get("rules")
            .cloned()
            .and_then(|r| serde_json::from_value(r).ok());

        if !is_library_allowed(
            rules.as_deref(),
            Some(coord),
            rule_ctx,
            &arch_specific_families,
        ) {
            continue;
        }

        let artifact = match MavenArtifact::parse(coord) {
            Ok(a) => a,
            Err(_) => continue,
        };

        if is_non_classpath_extension(&artifact.extension) {
            continue;
        }

        let downloads_obj = lib_val.get("downloads");
        let artifact_dl = downloads_obj.and_then(|d| d.get("artifact"));
        let natives_obj = lib_val.get("natives").and_then(|n| n.as_object());

        // 1. Base artifact: ALWAYS include when `downloads.artifact` is present (even if `natives`
        //    is ALSO present on the same library object, e.g. `com.mojang:text2speech` in 1.12.2–1.16.5!)
        //    OR when `natives` is absent.
        if natives_obj.is_none() || artifact_dl.is_some() {
            let explicit_rel = artifact_dl
                .and_then(|a| a.get("path"))
                .and_then(|p| p.as_str())
                .filter(|rel| !rel.trim().is_empty());

            let resolved_path = match explicit_rel {
                Some(rel) => libraries_root.join(rel),
                None => artifact.to_library_path(&libraries_root),
            };

            upsert_classpath_slot(
                &mut slot_map,
                idx * 2,
                *from_loader,
                &artifact,
                resolved_path,
            );
        }

        // 2. Legacy `natives` classifier JAR: independently add `artifact.with_classifier(...)`
        //    if `natives` maps `rule_ctx.os` to a classifier!
        if let Some(natives_map) = natives_obj {
            let os_key = rule_ctx.os.mojang_name();
            let tpl = natives_map.get(os_key).or_else(|| match rule_ctx.os {
                TargetOs::MacOs => natives_map.get("macos"),
                _ => None,
            });
            if let Some(tpl_str) = tpl.and_then(|v| v.as_str()) {
                let classifier =
                    tpl_str.replace("${arch}", rule_ctx.effective_arch().bitness_str());
                let native_artifact = artifact.with_classifier(Some(&classifier));
                let explicit_native_rel = downloads_obj
                    .and_then(|d| d.get("classifiers"))
                    .and_then(|c| c.get(&classifier))
                    .and_then(|ci| ci.get("path"))
                    .and_then(|p| p.as_str())
                    .filter(|rel| !rel.trim().is_empty());

                let native_path = match explicit_native_rel {
                    Some(rel) => libraries_root.join(rel),
                    None => native_artifact.to_library_path(&libraries_root),
                };

                upsert_classpath_slot(
                    &mut slot_map,
                    idx * 2 + 1,
                    *from_loader,
                    &native_artifact,
                    native_path,
                );
            }
        }
    }

    let mut ordered: Vec<SelectedLib> = slot_map.into_values().collect();
    ordered.sort_by_key(|item| item.order_idx);

    let mut final_paths = Vec::with_capacity(ordered.len() + 1);
    let mut seen_exact = HashSet::with_capacity(ordered.len() + 1);

    for item in ordered {
        let key = normalize_cp_key(&item.resolved_path);
        if seen_exact.insert(key) {
            final_paths.push(item.resolved_path);
        }
    }

    // Append primary game JAR at the end of `-cp`
    let primary_jar = if let Some(lp) = loader_profile {
        if lp.is_old_forge {
            lp.custom_jar_path.clone()
        } else {
            None
        }
    } else {
        None
    }
    .or_else(|| mcp_jar.map(Path::to_path_buf))
    .unwrap_or_else(|| {
        root_dir
            .join("versions")
            .join(mc_version_id)
            .join(format!("{mc_version_id}.jar"))
    });

    let primary_key = normalize_cp_key(&primary_jar);
    if seen_exact.insert(primary_key) {
        final_paths.push(primary_jar);
    }

    Ok(final_paths)
}

fn uses_alternative_gc(custom_jvm_tokens: &[String]) -> bool {
    custom_jvm_tokens.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-XX:+UseZGC"
                | "-XX:+UseShenandoahGC"
                | "-XX:+UseParallelGC"
                | "-XX:+UseConcMarkSweepGC"
                | "-XX:+UseSerialGC"
                | "-XX:-UseG1GC"
        )
    })
}

fn is_paired_jvm_flag(token: &str) -> bool {
    matches!(
        token,
        "--add-opens"
            | "--add-exports"
            | "--add-modules"
            | "--add-reads"
            | "--patch-module"
            | "--limit-modules"
            | "--upgrade-module-path"
            | "-p"
            | "--module-path"
            | "-cp"
            | "-classpath"
    )
}

fn contains_contiguous_pair(args: &[String], flag: &str, val: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == val)
}

/// Smart-normalizes custom `jvm_args` / `game_args` entries:
/// if an entry starts with `--` (or `-p `) followed by whitespace, has NO `=` before the first
/// whitespace, and contains NO quotes (`"` or `'`), splits it on the first whitespace into
/// `[flag, rest]` so `--add-opens java.base/java.lang=ALL-UNNAMED` or `--width 1920` work even
/// when entered on a single line, while keeping `-Dkey=path with spaces` intact!
pub fn normalize_custom_arg_tokens(entries: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(entries.len());

    for raw in entries {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }

        if !trimmed.contains('"') && !trimmed.contains('\'') {
            if let Some(ws_idx) = trimmed.find(char::is_whitespace) {
                let head = &trimmed[..ws_idx];
                let rest = trimmed[ws_idx..].trim();
                if !rest.is_empty()
                    && (head.starts_with("--") || head == "-p")
                    && !head.contains('=')
                {
                    out.push(head.to_string());
                    out.push(rest.to_string());
                    continue;
                }
            }
        }

        out.push(trimmed.to_string());
    }

    out
}

fn build_jvm_arguments(
    options: &LaunchOptions,
    mc_version_id: &str,
    version_json: &serde_json::Value,
    loader_profile: Option<&LoaderProfileData>,
    cp_joined: &str,
    rule_ctx: &RuleContext,
) -> Vec<String> {
    let natives_dir = options
        .natives_dir(mc_version_id)
        .to_string_lossy()
        .to_string();
    let libraries_dir = options.path.join("libraries").to_string_lossy().to_string();
    let cp_sep = classpath_separator();

    let custom_jvm_tokens = normalize_custom_arg_tokens(&options.jvm_args);
    let alt_gc = uses_alternative_gc(&custom_jvm_tokens);

    let mut jvm_args = vec![
        format!("-Xms{}", options.memory.min),
        format!("-Xmx{}", options.memory.max),
        "-XX:+UnlockExperimentalVMOptions".to_string(),
    ];

    if !alt_gc {
        jvm_args.push("-XX:G1NewSizePercent=20".to_string());
        jvm_args.push("-XX:G1ReservePercent=20".to_string());
        jvm_args.push("-XX:MaxGCPauseMillis=50".to_string());
        jvm_args.push("-XX:G1HeapRegionSize=32M".to_string());
    }

    jvm_args.extend([
        "-Dfml.ignoreInvalidMinecraftCertificates=true".to_string(),
        format!("-Djna.tmpdir={natives_dir}"),
        format!("-Dorg.lwjgl.system.SharedLibraryExtractPath={natives_dir}"),
        format!("-Dio.netty.native.workdir={natives_dir}"),
        format!("-Djava.library.path={natives_dir}"),
    ]);

    // OS-specific JVM flags
    match rule_ctx.os {
        TargetOs::Windows => {
            jvm_args.push(
                "-XX:HeapDumpPath=MojangTricksIntelDriversForPerformance_javaw.exe_minecraft.exe.heapdump"
                    .to_string(),
            );
        }
        TargetOs::MacOs => {
            jvm_args.push("-XstartOnFirstThread".to_string());
            if let Some(icon_arg) = resolve_macos_dock_icon(&options.path, version_json) {
                jvm_args.push("-Xdock:name=Minecraft".to_string());
                jvm_args.push(format!("-Xdock:icon={icon_arg}"));
            }
        }
        TargetOs::Linux => {
            if rule_ctx.effective_arch() == TargetArch::X86 {
                jvm_args.push("-Xss1M".to_string());
            }
        }
    }

    if options.bypass_offline {
        jvm_args.push("-Dminecraft.api.auth.host=https://nope.invalid/".to_string());
        jvm_args.push("-Dminecraft.api.account.host=https://nope.invalid/".to_string());
        jvm_args.push("-Dminecraft.api.session.host=https://nope.invalid/".to_string());
        jvm_args.push("-Dminecraft.api.services.host=https://nope.invalid/".to_string());
    }

    // Custom user/instance JVM args (preserving paired flags like repeated `--add-opens A --add-opens B`)
    let mut custom_iter = custom_jvm_tokens.into_iter().peekable();
    while let Some(tok) = custom_iter.next() {
        if is_paired_jvm_flag(&tok) {
            if let Some(val) = custom_iter.next() {
                if !contains_contiguous_pair(&jvm_args, &tok, &val) {
                    jvm_args.push(tok);
                    jvm_args.push(val);
                }
            } else {
                jvm_args.push(tok);
            }
        } else if !jvm_args.iter().any(|existing| existing == &tok) {
            jvm_args.push(tok);
        }
    }

    // Modern Vanilla `arguments.jvm` rules (e.g. additional platform flags, without duplicating `-cp`)
    let jvm_placeholders: [(&str, &str); 7] = [
        ("${natives_directory}", &natives_dir),
        ("${launcher_name}", "minecraft-rust-core"),
        ("${launcher_version}", env!("CARGO_PKG_VERSION")),
        ("${classpath}", cp_joined),
        ("${classpath_separator}", cp_sep),
        ("${library_directory}", &libraries_dir),
        ("${version_name}", mc_version_id),
    ];

    let mut cp_injected = false;

    if let Some(vanilla_jvm) = version_json
        .get("arguments")
        .and_then(|a| a.get("jvm"))
        .and_then(|j| j.as_array())
    {
        let evaluated = strip_unresolved_placeholders(expand_argument_entries(
            vanilla_jvm,
            rule_ctx,
            &jvm_placeholders,
        ));
        let mut iter = evaluated.into_iter().peekable();
        while let Some(arg) = iter.next() {
            if arg == "-cp" || arg == "-classpath" {
                if let Some(cp_val) = iter.next() {
                    jvm_args.push("-cp".to_string());
                    jvm_args.push(cp_val);
                    cp_injected = true;
                }
                continue;
            }
            if arg.starts_with("-Djava.library.path=")
                || arg.starts_with("-Djna.tmpdir=")
                || arg.starts_with("-Dorg.lwjgl.system.SharedLibraryExtractPath=")
                || arg.starts_with("-Dio.netty.native.workdir=")
                || arg == "-XstartOnFirstThread"
                || arg.starts_with("-XX:HeapDumpPath=")
            {
                continue;
            }
            jvm_args.push(arg);
        }
    }

    if !cp_injected {
        jvm_args.push("-cp".to_string());
        jvm_args.push(cp_joined.to_string());
    }

    // Append Mod Loader JVM arguments (Forge 1.17+ / NeoForge 1.20.2+ `-p <module_path>`, `--add-opens`, etc.)
    if let Some(lp) = loader_profile {
        if let Some(ref args_sec) = lp.arguments {
            let loader_jvm = strip_unresolved_placeholders(expand_argument_entries(
                &args_sec.jvm,
                rule_ctx,
                &jvm_placeholders,
            ));
            jvm_args.extend(loader_jvm);
        }
    }

    jvm_args
}

fn is_repeatable_game_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--tweakClass" | "--fml.mods" | "--fml.mavenRoots" | "--addMods"
    )
}

fn merge_legacy_argument_string(target: &mut Vec<String>, extra_str: &str) {
    let tokens: Vec<&str> = extra_str.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if tok.starts_with('-')
            && !tok.contains('=')
            && i + 1 < tokens.len()
            && !tokens[i + 1].starts_with('-')
        {
            let val = tokens[i + 1];
            if is_repeatable_game_flag(tok) {
                if !contains_contiguous_pair(target, tok, val) {
                    target.push(tok.to_string());
                    target.push(val.to_string());
                }
            } else if !target.iter().any(|existing| existing == tok) {
                target.push(tok.to_string());
                target.push(val.to_string());
            }
            i += 2;
        } else {
            if !target.iter().any(|existing| existing == tok) {
                target.push(tok.to_string());
            }
            i += 1;
        }
    }
}

fn build_game_arguments(
    options: &LaunchOptions,
    mc_version_id: &str,
    version_json: &serde_json::Value,
    loader_profile: Option<&LoaderProfileData>,
    rule_ctx: &RuleContext,
    quick_play_supported: bool,
) -> Vec<String> {
    let mut raw_args: Vec<String> = Vec::new();

    if let Some(legacy_str) = version_json.get("minecraftArguments").and_then(|m| m.as_str()) {
        // Legacy MC 1.7.10 -> 1.12.2: if Legacy Forge/LiteLoader provides `minecraftArguments`
        // (including multiple `--tweakClass` flags), preserve pairs cleanly!
        if let Some(loader_mc_args) = loader_profile.and_then(|lp| lp.minecraft_arguments.as_deref())
        {
            raw_args.extend(loader_mc_args.split_whitespace().map(str::to_string));
            merge_legacy_argument_string(&mut raw_args, legacy_str);
        } else {
            raw_args.extend(legacy_str.split_whitespace().map(str::to_string));
        }
    } else if let Some(game_arr) = version_json
        .get("arguments")
        .and_then(|a| a.get("game"))
        .and_then(|g| g.as_array())
    {
        raw_args.extend(expand_argument_entries(game_arr, rule_ctx, &[]));
    }

    // Append modern Mod Loader game arguments (e.g. `--launchTarget forgeclient`, `--fml.forgeVersion`, etc.)
    if let Some(lp) = loader_profile {
        if lp.minecraft_arguments.is_none() || version_json.get("minecraftArguments").is_none() {
            if let Some(ref loader_mc_str) = lp.minecraft_arguments {
                merge_legacy_argument_string(&mut raw_args, loader_mc_str);
            }
        }
        if let Some(ref args_sec) = lp.arguments {
            let extra = expand_argument_entries(&args_sec.game, rule_ctx, &[]);
            raw_args.extend(extra);
        }
    }

    let auth = &options.authenticator;
    let user_type = if mc_version_id.starts_with("1.16")
        && (auth.meta.account_type.eq_ignore_ascii_case("microsoft")
            || auth.meta.account_type.eq_ignore_ascii_case("xbox"))
    {
        "Xbox".to_string()
    } else if auth.meta.account_type.eq_ignore_ascii_case("microsoft")
        || auth.meta.account_type.eq_ignore_ascii_case("xbox")
    {
        "msa".to_string()
    } else if auth.meta.account_type.eq_ignore_ascii_case("mojang") {
        "mojang".to_string()
    } else {
        auth.meta.account_type.clone()
    };

    let version_name = loader_profile
        .and_then(|lp| lp.id.as_deref())
        .unwrap_or(mc_version_id)
        .to_string();
    let asset_index_id = version_json
        .get("assetIndex")
        .and_then(|ai| ai.get("id"))
        .and_then(|id| id.as_str())
        .or_else(|| version_json.get("assets").and_then(|a| a.as_str()))
        .unwrap_or(mc_version_id)
        .to_string();

    let game_dir_str = options.game_dir().to_string_lossy().to_string();
    let assets_root_str = if is_legacy_assets(version_json) {
        options.path.join("resources").to_string_lossy().to_string()
    } else {
        options.path.join("assets").to_string_lossy().to_string()
    };
    let version_type = version_json
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("release")
        .to_string();
    let xuid = auth
        .xbox
        .as_ref()
        .map(|x| x.xuid.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&auth.access_token)
        .to_string();
    let client_id = if !auth.client_token.is_empty() {
        auth.client_token.clone()
    } else {
        auth.access_token.clone()
    };
    let quick_play_mp = options
        .auto_connect
        .as_ref()
        .map(|ac| ac.quick_play_address())
        .unwrap_or_default();
    let width_str = options.screen.width.unwrap_or(854).to_string();
    let height_str = options.screen.height.unwrap_or(480).to_string();

    let replacements: [(&str, &str); 17] = [
        ("${auth_access_token}", &auth.access_token),
        ("${auth_session}", &auth.access_token),
        ("${auth_player_name}", &auth.name),
        ("${auth_uuid}", &auth.uuid),
        ("${auth_xuid}", &xuid),
        ("${user_properties}", &auth.user_properties),
        ("${user_type}", &user_type),
        ("${version_name}", &version_name),
        ("${assets_index_name}", &asset_index_id),
        ("${game_directory}", &game_dir_str),
        ("${assets_root}", &assets_root_str),
        ("${game_assets}", &assets_root_str),
        ("${version_type}", &version_type),
        ("${clientid}", &client_id),
        ("${resolution_width}", &width_str),
        ("${resolution_height}", &height_str),
        ("${quickPlayMultiplayer}", &quick_play_mp),
    ];

    let substituted: Vec<String> = raw_args
        .into_iter()
        .map(|arg| apply_placeholders(&arg, &replacements))
        .collect();
    let mut resolved_args = strip_unresolved_placeholders(substituted);

    // Inject `--width` / `--height` if configured and not already emitted via `has_custom_resolution` rule
    if let (Some(w), Some(h)) = (options.screen.width, options.screen.height) {
        if w > 0 && h > 0 && !resolved_args.iter().any(|a| a == "--width") {
            resolved_args.push("--width".to_string());
            resolved_args.push(w.to_string());
            resolved_args.push("--height".to_string());
            resolved_args.push(h.to_string());
        }
    }

    // Inject `--fullscreen` if requested
    if options.screen.fullscreen && !resolved_args.iter().any(|a| a == "--fullscreen") {
        resolved_args.push("--fullscreen".to_string());
    }

    // Inject multiplayer auto-connect (`--quickPlayMultiplayer host:port` on 1.20+ or `--server` / `--port` on < 1.20)
    if let Some(ref ac) = options.auto_connect {
        let (norm_host, norm_port) = ac.normalized();
        if !norm_host.is_empty() {
            if quick_play_supported {
                if !resolved_args.iter().any(|a| a == "--quickPlayMultiplayer") {
                    resolved_args.push("--quickPlayMultiplayer".to_string());
                    resolved_args.push(ac.quick_play_address());
                }
            } else if !resolved_args.iter().any(|a| a == "--server") {
                resolved_args.push("--server".to_string());
                resolved_args.push(norm_host);
                resolved_args.push("--port".to_string());
                resolved_args.push(norm_port.to_string());
            }
        }
    }

    // Append extra custom game arguments (smart-splitting single-line `--flag value` entries)
    resolved_args.extend(normalize_custom_arg_tokens(&options.game_args));

    resolved_args
}

fn contains_unresolved_placeholder(s: &str) -> bool {
    let Some(start) = s.find("${") else {
        return false;
    };
    s[start + 2..].contains('}')
}

/// Pair-aware pass removing unreplaced `${...}` tokens: if a value argument still contains an
/// unreplaced `${...}` placeholder, drops both that token AND any immediately preceding standalone
/// `-flag` / `--flag` token (preventing dangling flags like `--quickPlayPath` before `--quickPlayMultiplayer`).
fn strip_unresolved_placeholders(tokens: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());

    for tok in tokens {
        if contains_unresolved_placeholder(&tok) {
            if !tok.starts_with('-') {
                if let Some(prev) = out.last() {
                    if prev.starts_with('-') && !prev.contains('=') {
                        out.pop();
                    }
                }
            }
            continue;
        }
        out.push(tok);
    }

    out
}

fn expand_argument_entries(
    entries: &[serde_json::Value],
    rule_ctx: &RuleContext,
    placeholders: &[(&str, &str)],
) -> Vec<String> {
    let mut out = Vec::new();

    for entry in entries {
        match entry {
            serde_json::Value::String(s) => {
                out.push(apply_placeholders(s, placeholders));
            }
            serde_json::Value::Object(obj) => {
                if let Some(rules_val) = obj.get("rules") {
                    if let Ok(rules) = serde_json::from_value::<Vec<MojangRule>>(rules_val.clone())
                    {
                        if !evaluate_rules(&rules, rule_ctx) {
                            continue;
                        }
                    }
                }

                if let Some(val) = obj.get("value") {
                    match val {
                        serde_json::Value::String(s) => {
                            out.push(apply_placeholders(s, placeholders));
                        }
                        serde_json::Value::Array(arr) => {
                            for item in arr {
                                if let Some(s) = item.as_str() {
                                    out.push(apply_placeholders(s, placeholders));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    out
}

fn apply_placeholders(input: &str, replacements: &[(&str, &str)]) -> String {
    if !input.contains("${") {
        return input.to_string();
    }
    let mut result = input.to_string();
    for (key, val) in replacements {
        if result.contains(key) {
            result = result.replace(key, val);
        }
    }
    result
}

/// Determines whether a Minecraft version supports `1.20+` `--quickPlayMultiplayer`:
/// 1. Checks `arguments.game` rules/values for `is_quick_play_multiplayer` first.
/// 2. Falls back to version comparison ONLY when `mc_version_id` starts with `"1."`
///    (`minor >= 20`, handling `"1.20-pre1"` / `"1.20-rc1"`) or year-based `>= 26.` releases
///    (never misclassifying pre-1.20 snapshots like `"19w14b"`!).
pub fn supports_quick_play(mc_version_id: &str, version_json: &serde_json::Value) -> bool {
    if let Some(game_arr) = version_json
        .get("arguments")
        .and_then(|a| a.get("game"))
        .and_then(|g| g.as_array())
    {
        for entry in game_arr {
            if let Some(obj) = entry.as_object() {
                if let Some(rules) = obj.get("rules").and_then(|r| r.as_array()) {
                    for rule in rules {
                        if rule
                            .get("features")
                            .and_then(|f| f.get("is_quick_play_multiplayer"))
                            .is_some()
                        {
                            return true;
                        }
                    }
                }
                if let Some(val) = obj.get("value") {
                    match val {
                        serde_json::Value::String(s)
                            if s == "--quickPlayMultiplayer"
                                || s.contains("quickPlayMultiplayer") =>
                        {
                            return true;
                        }
                        serde_json::Value::Array(arr)
                            if arr.iter().any(|v| {
                                v.as_str()
                                    .map(|s| s.contains("quickPlayMultiplayer"))
                                    .unwrap_or(false)
                            }) =>
                        {
                            return true;
                        }
                        _ => {}
                    }
                }
            } else if let Some(s) = entry.as_str() {
                if s.contains("quickPlayMultiplayer") {
                    return true;
                }
            }
        }
    }

    if version_json.get("minecraftArguments").is_some() && version_json.get("arguments").is_none() {
        return false;
    }

    let trimmed = mc_version_id.trim();
    if let Some(rest) = trimmed.strip_prefix("1.") {
        let minor_digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(minor) = minor_digits.parse::<u32>() {
            return minor >= 20;
        }
        return false;
    }

    if let Some((year_part, after_dot)) = trimmed.split_once('.') {
        if !after_dot.is_empty() && year_part.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(year) = year_part.parse::<u32>() {
                return year >= 26;
            }
        }
    }

    false
}

fn resolve_macos_dock_icon(root_dir: &Path, version_json: &serde_json::Value) -> Option<String> {
    let assets_id = version_json
        .get("assetIndex")
        .and_then(|ai| ai.get("id"))
        .and_then(|id| id.as_str())
        .or_else(|| version_json.get("assets").and_then(|a| a.as_str()))?;
    let index_path = root_dir
        .join("assets")
        .join("indexes")
        .join(format!("{assets_id}.json"));
    let content = std::fs::read_to_string(index_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let objects = json.get("objects")?;
    let icon_obj = objects
        .get("icons/minecraft.icns")
        .or_else(|| objects.get("minecraft/icons/minecraft.icns"))?;
    let hash = icon_obj.get("hash")?.as_str()?;
    if hash.len() < 2 {
        return None;
    }
    let icon_path = root_dir
        .join("assets")
        .join("objects")
        .join(&hash[..2])
        .join(hash);
    Some(icon_path.to_string_lossy().to_string())
}

pub fn classpath_separator() -> &'static str {
    if cfg!(windows) {
        ";"
    } else {
        ":"
    }
}

fn normalize_cp_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}
