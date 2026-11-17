use std::cmp::Ordering;
use std::fmt;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::error::{CoreError, CoreResult};

/// Default Maven repository mirrors searched when resolving Forge/NeoForge/Modded artifacts.
pub const DEFAULT_MAVEN_MIRRORS: &[&str] = &[
    "https://maven.minecraftforge.net",
    "https://maven.neoforged.net/releases",
    "https://libraries.minecraft.net",
    "https://repo1.maven.org/maven2",
    "https://maven.creeperhost.net",
];

/// Parsed Gradle/Maven artifact coordinate:
/// `group:artifact:version[:classifier][@extension]`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MavenArtifact {
    pub group: String,
    pub artifact: String,
    pub version: String,
    pub classifier: Option<String>,
    pub extension: String,
}

impl MavenArtifact {
    /// Parses a Maven coordinate such as:
    /// - `"org.ow2.asm:asm:9.7"`
    /// - `"net.minecraftforge:forge:1.20.1-47.3.0:universal"`
    /// - `"de.oceanlabs.mcp:mcp_config:1.20.1-20230612.114412@zip"`
    /// - `"net.neoforged:neoforge:21.1.72:universal@jar"`
    /// - `"[net.minecraft:client:1.20.1-20230612.114412:srg]"`
    pub fn parse(raw: &str) -> CoreResult<Self> {
        let trimmed = raw.trim();
        let unbracketed = trimmed
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(trimmed)
            .trim();

        if unbracketed.is_empty() {
            return Err(CoreError::InvalidConfig(
                "Maven coordinate cannot be empty".to_string(),
            ));
        }

        let (coord_part, explicit_ext) = match unbracketed.rsplit_once('@') {
            Some((left, right)) if !right.is_empty() && !right.contains(':') => {
                (left, Some(right.trim_start_matches('.').to_string()))
            }
            _ => (unbracketed, None),
        };

        let parts: Vec<&str> = coord_part.split(':').collect();
        if parts.len() < 3 {
            return Err(CoreError::InvalidConfig(format!(
                "Invalid Maven coordinate '{raw}': expected at least group:artifact:version"
            )));
        }

        let group = parts[0].trim();
        let artifact = parts[1].trim();
        let version = parts[2].trim();

        if group.is_empty() || artifact.is_empty() || version.is_empty() {
            return Err(CoreError::InvalidConfig(format!(
                "Invalid Maven coordinate '{raw}': empty group, artifact, or version"
            )));
        }

        let classifier = if parts.len() >= 4 {
            let joined = parts[3..].join(":");
            let c = joined.trim();
            if c.is_empty() {
                None
            } else {
                Some(c.to_string())
            }
        } else {
            None
        };

        Ok(Self {
            group: group.to_string(),
            artifact: artifact.to_string(),
            version: version.to_string(),
            classifier,
            extension: explicit_ext.unwrap_or_else(|| "jar".to_string()),
        })
    }

    /// Returns a copy with `classifier` replaced.
    pub fn with_classifier(&self, classifier: Option<&str>) -> Self {
        let mut copy = self.clone();
        copy.classifier = classifier
            .map(|c| c.trim().trim_start_matches('-').to_string())
            .filter(|c| !c.is_empty());
        copy
    }

    /// Returns a copy with `extension` replaced (e.g. `"lzma"` or `".lzma"`).
    pub fn with_extension(&self, ext: &str) -> Self {
        let mut copy = self.clone();
        let clean = ext.trim().trim_start_matches('.');
        if !clean.is_empty() {
            copy.extension = clean.to_string();
        }
        copy
    }

    /// Returns a copy overriding optional classifier suffix and/or extension without ever
    /// corrupting `@jar` coordinates (fixes the legacy `universal.jar` overwrite bug).
    pub fn with_suffix_and_ext(&self, classifier_override: Option<&str>, ext_override: Option<&str>) -> Self {
        let mut copy = self.clone();
        if let Some(c) = classifier_override {
            let clean = c.trim().trim_start_matches('-');
            if !clean.is_empty() {
                copy.classifier = Some(clean.to_string());
            }
        }
        if let Some(e) = ext_override {
            let clean = e.trim().trim_start_matches('.');
            if !clean.is_empty() {
                copy.extension = clean.to_string();
            }
        }
        copy
    }

    /// Produces the canonical `-clientdata.lzma` artifact for Forge/NeoForge binary patches
    /// (`<artifact>-<version>-clientdata.lzma`), regardless of whether the base coordinate
    /// had `:universal` or `@jar`.
    pub fn for_clientdata_lzma(&self) -> Self {
        self.with_suffix_and_ext(Some("clientdata"), Some("lzma"))
    }

    /// Slash-separated group path (`"net.minecraftforge"` -> `"net/minecraftforge"`).
    pub fn group_path(&self) -> String {
        self.group.replace('.', "/")
    }

    /// Relative Maven directory (`"net/minecraftforge/forge/1.20.1-47.3.0"`).
    pub fn directory(&self) -> String {
        format!("{}/{}/{}", self.group_path(), self.artifact, self.version)
    }

    /// Artifact file name (`"forge-1.20.1-47.3.0-universal.jar"`).
    pub fn file_name(&self) -> String {
        match self.classifier.as_deref() {
            Some(c) if !c.is_empty() => {
                format!("{}-{}-{}.{}", self.artifact, self.version, c, self.extension)
            }
            _ => format!("{}-{}.{}", self.artifact, self.version, self.extension),
        }
    }

    /// Full relative Maven path (`"net/minecraftforge/forge/1.20.1-47.3.0/forge-1.20.1-47.3.0-universal.jar"`).
    pub fn relative_path(&self) -> String {
        format!("{}/{}", self.directory(), self.file_name())
    }

    /// Full filesystem path inside a `libraries/` root directory.
    pub fn to_library_path(&self, libraries_root: &Path) -> PathBuf {
        libraries_root.join(self.relative_path())
    }

    /// Key identifying the artifact slot (`group:artifact:classifier:extension`) for
    /// SemVer classpath deduplication, preserving distinct classifiers (such as `:natives-linux`).
    pub fn dedup_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.group,
            self.artifact,
            self.classifier.as_deref().unwrap_or(""),
            self.extension
        )
    }
}

impl fmt::Display for MavenArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.group, self.artifact, self.version)?;
        if let Some(ref c) = self.classifier {
            write!(f, ":{c}")?;
        }
        if self.extension != "jar" {
            write!(f, "@{}", self.extension)?;
        }
        Ok(())
    }
}

/// Compares two Maven/SemVer version strings numerically by segment, preferring strict `semver`
/// when both parse cleanly and falling back to Maven token comparison so non-SemVer versions
/// (e.g. `21.0-jre`, `1.20.1-20230612.114412`, `3.3.3-snapshot`) never fail or get dropped.
pub fn compare_maven_versions(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }

    if let (Ok(va), Ok(vb)) = (Version::parse(a), Version::parse(b)) {
        return va.cmp(&vb);
    }

    let tokens_a = split_version_tokens(a);
    let tokens_b = split_version_tokens(b);

    for i in 0..tokens_a.len().max(tokens_b.len()) {
        match (tokens_a.get(i), tokens_b.get(i)) {
            (Some(ta), Some(tb)) if ta == tb => continue,
            (Some(VersionToken::Num(na)), Some(VersionToken::Num(nb))) => return na.cmp(nb),
            (Some(VersionToken::Num(_)), Some(VersionToken::Text(_))) => return Ordering::Greater,
            (Some(VersionToken::Text(_)), Some(VersionToken::Num(_))) => return Ordering::Less,
            (Some(VersionToken::Text(sa)), Some(VersionToken::Text(sb))) => {
                return qualifier_rank(sa)
                    .cmp(&qualifier_rank(sb))
                    .then_with(|| sa.cmp(sb));
            }
            (Some(VersionToken::Num(n)), None) => {
                if *n == 0 {
                    continue;
                }
                return Ordering::Greater;
            }
            (None, Some(VersionToken::Num(n))) => {
                if *n == 0 {
                    continue;
                }
                return Ordering::Less;
            }
            // In Maven/SemVer, a pre-release qualifier (e.g. -alpha, -beta, -rc) is older than release,
            // whereas a post-release qualifier (e.g. -jre, -final) is >= base.
            (Some(VersionToken::Text(s)), None) => {
                return if is_prerelease_qualifier(s) {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (None, Some(VersionToken::Text(s))) => {
                return if is_prerelease_qualifier(s) {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (None, None) => break,
        }
    }

    Ordering::Equal
}

#[derive(Debug, PartialEq, Eq)]
enum VersionToken {
    Num(u64),
    Text(String),
}

fn split_version_tokens(v: &str) -> Vec<VersionToken> {
    let mut out = Vec::new();
    let mut cur_num = String::new();
    let mut cur_txt = String::new();

    for ch in v.chars() {
        if ch.is_ascii_digit() {
            if !cur_txt.is_empty() {
                out.push(VersionToken::Text(cur_txt.to_ascii_lowercase()));
                cur_txt.clear();
            }
            cur_num.push(ch);
        } else if ch.is_ascii_alphabetic() {
            if !cur_num.is_empty() {
                if let Ok(n) = cur_num.parse::<u64>() {
                    out.push(VersionToken::Num(n));
                }
                cur_num.clear();
            }
            cur_txt.push(ch);
        } else {
            if !cur_num.is_empty() {
                if let Ok(n) = cur_num.parse::<u64>() {
                    out.push(VersionToken::Num(n));
                }
                cur_num.clear();
            }
            if !cur_txt.is_empty() {
                out.push(VersionToken::Text(cur_txt.to_ascii_lowercase()));
                cur_txt.clear();
            }
        }
    }

    if !cur_num.is_empty() {
        if let Ok(n) = cur_num.parse::<u64>() {
            out.push(VersionToken::Num(n));
        }
    }
    if !cur_txt.is_empty() {
        out.push(VersionToken::Text(cur_txt.to_ascii_lowercase()));
    }

    out
}

fn is_prerelease_qualifier(q: &str) -> bool {
    matches!(
        q,
        "alpha" | "a" | "beta" | "b" | "milestone" | "m" | "rc" | "cr" | "snapshot" | "pre"
    )
}

fn qualifier_rank(q: &str) -> i32 {
    match q {
        "alpha" | "a" => 1,
        "beta" | "b" => 2,
        "milestone" | "m" => 3,
        "rc" | "cr" | "pre" => 4,
        "snapshot" => 5,
        "ga" | "final" | "release" => 6,
        _ => 7,
    }
}
