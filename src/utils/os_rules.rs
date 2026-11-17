use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::utils::maven::MavenArtifact;

/// Normalized operating system used in Mojang version manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetOs {
    Windows,
    MacOs,
    Linux,
}

impl TargetOs {
    /// Detects the host OS at runtime.
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            TargetOs::Windows
        } else if cfg!(target_os = "macos") {
            TargetOs::MacOs
        } else {
            TargetOs::Linux
        }
    }

    /// Canonical Mojang OS name (`"windows"`, `"osx"`, `"linux"`).
    pub fn mojang_name(&self) -> &'static str {
        match self {
            TargetOs::Windows => "windows",
            TargetOs::MacOs => "osx",
            TargetOs::Linux => "linux",
        }
    }

    pub fn matches_rule_name(&self, name: &str) -> bool {
        let lower = name.trim().to_ascii_lowercase();
        match self {
            TargetOs::Windows => lower == "windows" || lower == "win32",
            TargetOs::MacOs => lower == "osx" || lower == "macos" || lower == "darwin",
            TargetOs::Linux => lower == "linux",
        }
    }
}

/// Normalized CPU architecture used in Mojang rules, LWJGL native classifiers, and JRE lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetArch {
    X86_64,
    X86,
    Arm64,
    Arm32,
}

impl TargetArch {
    /// Detects the host CPU architecture at runtime.
    pub fn current() -> Self {
        if cfg!(target_arch = "x86_64") {
            TargetArch::X86_64
        } else if cfg!(target_arch = "x86") {
            TargetArch::X86
        } else if cfg!(target_arch = "aarch64") {
            TargetArch::Arm64
        } else if cfg!(target_arch = "arm") {
            TargetArch::Arm32
        } else {
            TargetArch::X86_64
        }
    }

    /// `"32"` or `"64"` replacement for legacy `${arch}` native classifiers.
    pub fn bitness_str(&self) -> &'static str {
        match self {
            TargetArch::X86 | TargetArch::Arm32 => "32",
            TargetArch::X86_64 | TargetArch::Arm64 => "64",
        }
    }

    pub fn matches_rule_arch(&self, rule_arch: &str) -> bool {
        let lower = rule_arch.trim().to_ascii_lowercase();
        match self {
            TargetArch::X86 => lower == "x86" || lower == "i386" || lower == "i686" || lower == "x32",
            TargetArch::X86_64 => lower == "x86_64" || lower == "amd64" || lower == "x64",
            TargetArch::Arm64 => lower == "arm64" || lower == "aarch64",
            TargetArch::Arm32 => lower == "arm" || lower == "arm32" || lower == "aarch32",
        }
    }
}

/// Evaluation context for Mojang `rules` (OS, CPU architecture, Rosetta mode, and feature flags).
#[derive(Debug, Clone)]
pub struct RuleContext {
    pub os: TargetOs,
    pub arch: TargetArch,
    pub intel_enabled_mac: bool,
    pub features: HashMap<String, bool>,
}

impl RuleContext {
    /// Creates a [`RuleContext`] for the current host machine.
    pub fn current(intel_enabled_mac: bool) -> Self {
        Self {
            os: TargetOs::current(),
            arch: TargetArch::current(),
            intel_enabled_mac,
            features: HashMap::new(),
        }
    }

    /// Creates an explicit [`RuleContext`] (useful for deterministic cross-platform testing).
    pub fn new(os: TargetOs, arch: TargetArch, intel_enabled_mac: bool) -> Self {
        Self {
            os,
            arch,
            intel_enabled_mac,
            features: HashMap::new(),
        }
    }

    /// Sets a feature flag (e.g. `is_demo_user`, `has_custom_resolution`, `is_quick_play_multiplayer`).
    pub fn with_feature(mut self, key: impl Into<String>, enabled: bool) -> Self {
        self.features.insert(key.into(), enabled);
        self
    }

    /// Effective architecture accounting for `intel_enabled_mac` (Rosetta 2 x86_64 mode on Apple Silicon).
    pub fn effective_arch(&self) -> TargetArch {
        if self.os == TargetOs::MacOs && self.arch == TargetArch::Arm64 && self.intel_enabled_mac {
            TargetArch::X86_64
        } else {
            self.arch
        }
    }
}

/// OS selector inside a [`MojangRule`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OsRuleCondition {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// Standard Mojang manifest rule (`allow` / `disallow`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangRule {
    pub action: String,
    #[serde(default)]
    pub os: Option<OsRuleCondition>,
    #[serde(default)]
    pub features: Option<HashMap<String, bool>>,
}

/// Evaluates an ordered slice of [`MojangRule`] entries according to Mojang's specification:
/// - Empty rules slice -> `true` (allowed).
/// - Non-empty rules slice -> defaults to `false` unless an `allow` rule matches, and
///   subsequent matching `disallow` / `allow` rules override earlier ones in order.
pub fn evaluate_rules(rules: &[MojangRule], ctx: &RuleContext) -> bool {
    if rules.is_empty() {
        return true;
    }

    let mut allowed = false;
    let eff_arch = ctx.effective_arch();

    for rule in rules {
        let mut rule_matches = true;

        if let Some(ref os_cond) = rule.os {
            if let Some(ref req_name) = os_cond.name {
                if !ctx.os.matches_rule_name(req_name) {
                    rule_matches = false;
                }
            }
            if rule_matches {
                if let Some(ref req_arch) = os_cond.arch {
                    if !eff_arch.matches_rule_arch(req_arch) {
                        rule_matches = false;
                    }
                }
            }
        }

        if rule_matches {
            if let Some(ref req_features) = rule.features {
                for (feat_key, expected_val) in req_features {
                    let actual_val = ctx.features.get(feat_key).copied().unwrap_or(false);
                    if actual_val != *expected_val {
                        rule_matches = false;
                        break;
                    }
                }
            }
        }

        if rule_matches {
            if rule.action.eq_ignore_ascii_case("allow") {
                allowed = true;
            } else if rule.action.eq_ignore_ascii_case("disallow") {
                allowed = false;
            }
        }
    }

    allowed
}

/// Evaluates whether a MC 1.19+ Maven native classifier (e.g. `natives-windows-arm64`,
/// `natives-macos-arm64`, `natives-linux`) matches the target OS and architecture.
///
/// Fixing the MC 1.19+ bug where Mojang's `rules` only specified `os.name` without `os.arch`,
/// causing launchers that only checked `os.name` (or `rules[0]`) to inject x86, x64, and arm64
/// native JARs into `-cp` simultaneously!
pub fn matches_native_classifier(
    classifier: &str,
    ctx: &RuleContext,
    has_arch_specific_sibling: bool,
) -> bool {
    let lower = classifier.trim().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("natives-") else {
        return true;
    };

    let (os_part, arch_suffix) = if let Some(sfx) = rest.strip_prefix("windows") {
        (TargetOs::Windows, sfx.trim_start_matches('-'))
    } else if let Some(sfx) = rest.strip_prefix("macos") {
        (TargetOs::MacOs, sfx.trim_start_matches('-'))
    } else if let Some(sfx) = rest.strip_prefix("osx") {
        (TargetOs::MacOs, sfx.trim_start_matches('-'))
    } else if let Some(sfx) = rest.strip_prefix("linux") {
        (TargetOs::Linux, sfx.trim_start_matches('-'))
    } else {
        return false;
    };

    if os_part != ctx.os {
        return false;
    }

    let eff_arch = ctx.effective_arch();
    match arch_suffix {
        "" => {
            // Unsuffixed `natives-windows` / `natives-macos` / `natives-linux` targets x86_64,
            // unless no arch-specific sibling exists at all for this library on an older manifest.
            eff_arch == TargetArch::X86_64 || !has_arch_specific_sibling
        }
        "x86" | "i386" | "32" => eff_arch == TargetArch::X86,
        "x64" | "x86_64" | "amd64" | "64" => eff_arch == TargetArch::X86_64,
        "arm64" | "aarch64" => eff_arch == TargetArch::Arm64,
        "arm" | "arm32" | "aarch" | "aarch32" => eff_arch == TargetArch::Arm32,
        _ => false,
    }
}

/// Inspects a full list of library Maven names and returns the set of `"group:artifact"` keys
/// that provide an explicit architecture-suffixed native classifier for `target_os`
/// (such as `:natives-macos-arm64` or `:natives-windows-x86`).
pub fn collect_arch_specific_native_families<'a>(
    library_names: impl IntoIterator<Item = &'a str>,
    target_os: TargetOs,
) -> std::collections::HashSet<String> {
    let mut families = std::collections::HashSet::new();
    for raw_name in library_names {
        let Ok(art) = MavenArtifact::parse(raw_name) else {
            continue;
        };
        let Some(ref classifier) = art.classifier else {
            continue;
        };
        let lower = classifier.to_ascii_lowercase();
        let matches_os_with_arch = match target_os {
            TargetOs::Windows => lower.starts_with("natives-windows-"),
            TargetOs::MacOs => {
                lower.starts_with("natives-macos-") || lower.starts_with("natives-osx-")
            }
            TargetOs::Linux => lower.starts_with("natives-linux-"),
        };
        if matches_os_with_arch {
            families.insert(format!("{}:{}", art.group, art.artifact));
        }
    }
    families
}

/// Complete library eligibility check combining `MojangRule` evaluation and MC 1.19+ native
/// classifier architecture filtering.
pub fn is_library_allowed(
    rules: Option<&[MojangRule]>,
    maven_name: Option<&str>,
    ctx: &RuleContext,
    arch_specific_families: &std::collections::HashSet<String>,
) -> bool {
    if let Some(r) = rules {
        if !evaluate_rules(r, ctx) {
            return false;
        }
    }

    if let Some(name) = maven_name {
        if let Ok(art) = MavenArtifact::parse(name) {
            if let Some(ref classifier) = art.classifier {
                if classifier.to_ascii_lowercase().starts_with("natives-") {
                    let family = format!("{}:{}", art.group, art.artifact);
                    let has_sibling = arch_specific_families.contains(&family);
                    if !matches_native_classifier(classifier, ctx, has_sibling) {
                        return false;
                    }
                }
            }
        }
    }

    true
}
