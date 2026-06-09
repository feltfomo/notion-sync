//! TOML configuration. The Notion token is NEVER stored here — it comes from the
//! NOTION_TOKEN environment variable (systemd EnvironmentFile / LoadCredential).

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct RawConfig {
    #[serde(default = "default_version")]
    pub notion_version: String,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,
    #[serde(default = "default_policy")]
    pub conflict_policy: String,
    #[serde(default = "default_max_bytes")]
    pub max_file_bytes: u64,
    /// Optional path to read the Notion token from. Used only when $NOTION_TOKEN is
    /// unset/empty (sops / systemd LoadCredential friendly).
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// One or many mappings. TOML accepts either a single `[mapping]` table (the
    /// legacy single-directory form) or repeated `[[mapping]]` tables (one per
    /// directory). Untagged, so a sequence-vs-table shape disambiguates the two.
    pub mapping: RawMappings,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RawMappings {
    Many(Vec<RawMapping>),
    One(RawMapping),
}

#[derive(Debug, Deserialize)]
pub struct RawMapping {
    /// Optional label. Defaults to the final component of `local_root`. It names this
    /// mapping's subtree in state.db (paths are stored as `<name>/<rel>`) and never
    /// reaches Notion, so two mappings may point at differently-named roots but must
    /// not share a name.
    #[serde(default)]
    pub name: Option<String>,
    pub local_root: PathBuf,
    pub parent_page_id: String,
    #[serde(default)]
    pub ignore: Vec<String>,
}

fn default_version() -> String {
    "2022-06-28".to_string()
}
fn default_poll() -> u64 {
    45
}
fn default_debounce() -> u64 {
    1000
}
fn default_policy() -> String {
    "local-wins".to_string()
}
fn default_max_bytes() -> u64 {
    5_000_000
}

/// One local-root -> Notion-parent mapping. A config holds one or more. `name` is the
/// stable label that namespaces this mapping's rows in state.db; it is unique across
/// mappings and is never shown in Notion.
#[derive(Debug, Clone)]
pub struct Mapping {
    pub name: String,
    pub local_root: PathBuf,
    pub parent_page_id: String,
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub notion_version: String,
    pub poll_interval_secs: u64,
    pub debounce_ms: u64,
    pub max_file_bytes: u64,
    /// One or more directory mappings. Never empty after a successful load.
    pub mappings: Vec<Mapping>,
    /// Resolved from $NOTION_TOKEN (preferred) or `token_file`.
    pub token: String,
    /// Where the token came from, retained so the daemon can re-read it on a 401
    /// without a restart.
    pub token_file: Option<PathBuf>,
}

impl Config {
    /// The mapping with this name, if any.
    pub fn mapping_by_name(&self, name: &str) -> Option<&Mapping> {
        self.mappings.iter().find(|m| m.name == name)
    }

    /// The mapping that owns a namespaced rel_path (its first path segment is the
    /// mapping name).
    pub fn mapping_for_path(&self, rel: &str) -> Option<&Mapping> {
        let name = rel.split('/').next().unwrap_or("");
        self.mapping_by_name(name)
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read(String),
    Parse(String),
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Read(e) => write!(f, "cannot read config: {e}"),
            ConfigError::Parse(e) => write!(f, "cannot parse config: {e}"),
            ConfigError::Invalid(e) => write!(f, "invalid config: {e}"),
        }
    }
}
impl std::error::Error for ConfigError {}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read(e.to_string()))?;
        let raw: RawConfig =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;

        // v1 accepts only local-wins.
        if raw.conflict_policy != "local-wins" {
            return Err(ConfigError::Invalid(format!(
                "conflict_policy must be \"local-wins\" in v1, got {:?}",
                raw.conflict_policy
            )));
        }
        if !(750..=2000).contains(&raw.debounce_ms) {
            return Err(ConfigError::Invalid(format!(
                "debounce_ms must be within [750, 2000], got {}",
                raw.debounce_ms
            )));
        }
        if raw.poll_interval_secs == 0 {
            return Err(ConfigError::Invalid(
                "poll_interval_secs must be > 0".into(),
            ));
        }
        let raw_mappings = match raw.mapping {
            RawMappings::Many(v) => v,
            RawMappings::One(m) => vec![m],
        };
        if raw_mappings.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [[mapping]] is required".into(),
            ));
        }

        let mut mappings: Vec<Mapping> = Vec::with_capacity(raw_mappings.len());
        let mut seen = std::collections::HashSet::new();
        for rm in raw_mappings {
            let name = derive_mapping_name(rm.name, &rm.local_root)?;
            if !seen.insert(name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate mapping name {name:?}; mapping names must be unique"
                )));
            }
            // Each mapping is independently guarded against the empty/missing-tree
            // mass-delete footgun at reconcile time, but reject an outright
            // non-directory here so a typo fails fast instead of silently skipping a
            // mapping on every pass.
            if !rm.local_root.is_dir() {
                return Err(ConfigError::Invalid(format!(
                    "local_root for mapping {name:?} is not a directory: {}",
                    rm.local_root.display()
                )));
            }
            mappings.push(Mapping {
                name,
                local_root: rm.local_root,
                parent_page_id: rm.parent_page_id,
                ignore: rm.ignore,
            });
        }

        let token = load_token(raw.token_file.as_deref())?;

        Ok(Config {
            notion_version: raw.notion_version,
            poll_interval_secs: raw.poll_interval_secs,
            debounce_ms: raw.debounce_ms,
            max_file_bytes: raw.max_file_bytes,
            mappings,
            token,
            token_file: raw.token_file,
        })
    }
}

/// Pick a mapping's name: the explicit `name`, else the final component of
/// `local_root`. The result must be a single non-empty path segment because it becomes
/// the first segment of every namespaced rel_path in state.db.
fn derive_mapping_name(
    name: Option<String>,
    local_root: &std::path::Path,
) -> Result<String, ConfigError> {
    let name = match name {
        Some(n) => n,
        None => local_root
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "mapping for {} needs an explicit name (one can't be derived from that path)",
                    local_root.display()
                ))
            })?,
    };
    if name.is_empty() || name.contains('/') {
        return Err(ConfigError::Invalid(format!(
            "mapping name {name:?} must be non-empty and must not contain '/'"
        )));
    }
    Ok(name)
}

// Hand-rolled to avoid a glob-crate dependency: exact match, leading "*.ext", or
// trailing "build*", tested against every path component.
pub fn is_ignored(rel_path: &std::path::Path, patterns: &[String]) -> bool {
    let components: Vec<String> = rel_path
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_string()))
        .collect();
    for pat in patterns {
        for comp in &components {
            if glob_match(pat, comp) {
                return true;
            }
        }
    }
    false
}

fn glob_match(pattern: &str, name: &str) -> bool {
    // `*foo*` => contains. Must be checked BEFORE the single-ended cases, otherwise
    // the leading-`*` branch wins and matches against the literal trailing `*`.
    if pattern.len() >= 2 && pattern.starts_with('*') && pattern.ends_with('*') {
        return name.contains(&pattern[1..pattern.len() - 1]);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}

/// Resolve the Notion integration token. Precedence: $NOTION_TOKEN, then the optional
/// `token_file` path. Kept standalone (and `pub`) so the daemon can re-read the token
/// on a 401 without restarting.
pub fn load_token(token_file: Option<&std::path::Path>) -> Result<String, ConfigError> {
    if let Ok(tok) = std::env::var("NOTION_TOKEN") {
        if !tok.trim().is_empty() {
            return Ok(tok.trim().to_string());
        }
    }
    if let Some(path) = token_file {
        let tok = std::fs::read_to_string(path).map_err(|e| {
            ConfigError::Invalid(format!(
                "NOTION_TOKEN is unset and token_file {} could not be read: {e}",
                path.display()
            ))
        })?;
        let tok = tok.trim().to_string();
        if tok.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "token_file {} is empty",
                path.display()
            )));
        }
        return Ok(tok);
    }
    Err(ConfigError::Invalid(
        "No Notion token found. Set $NOTION_TOKEN (or `token_file` in config): create an \
         integration at https://www.notion.so/my-integrations, share the parent page with \
         it, export the token (export NOTION_TOKEN=ntn_...), then see the README Quickstart."
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn glob_exact_leading_trailing_and_both_ended() {
        assert!(glob_match("target", "target"));
        assert!(!glob_match("target", "targets"));
        assert!(glob_match("*.lock", "Cargo.lock"));
        assert!(!glob_match("*.lock", "Cargo.toml"));
        assert!(glob_match("build*", "build.rs"));
        // both-ended 'contains' must not be shadowed by the leading-'*' branch.
        assert!(glob_match("*node*", "node_modules"));
        assert!(glob_match("*node*", "my-node-thing"));
        assert!(!glob_match("*node*", "package.json"));
    }

    #[test]
    fn is_ignored_matches_any_component() {
        let pats = vec![
            ".git".to_string(),
            "target".to_string(),
            "*.lock".to_string(),
        ];
        assert!(is_ignored(Path::new("target/debug/foo"), &pats));
        assert!(is_ignored(Path::new("a/b/Cargo.lock"), &pats));
        assert!(is_ignored(Path::new(".git/HEAD"), &pats));
        assert!(!is_ignored(Path::new("src/main.rs"), &pats));
    }

    #[test]
    fn mapping_name_defaults_to_last_path_component() {
        let n = derive_mapping_name(None, Path::new("/home/me/Projects/myapp")).unwrap();
        assert_eq!(n, "myapp");
        let explicit = derive_mapping_name(Some("docs".into()), Path::new("/var/data")).unwrap();
        assert_eq!(explicit, "docs");
    }

    #[test]
    fn mapping_name_rejects_empty_or_slashed() {
        assert!(derive_mapping_name(Some("".into()), Path::new("/x")).is_err());
        assert!(derive_mapping_name(Some("a/b".into()), Path::new("/x")).is_err());
    }

    #[test]
    fn mapping_for_path_routes_by_first_segment() {
        let cfg = Config {
            notion_version: "v".into(),
            poll_interval_secs: 45,
            debounce_ms: 1000,
            max_file_bytes: 1,
            mappings: vec![
                Mapping {
                    name: "app".into(),
                    local_root: std::path::PathBuf::from("/a"),
                    parent_page_id: "pa".into(),
                    ignore: vec![],
                },
                Mapping {
                    name: "docs".into(),
                    local_root: std::path::PathBuf::from("/d"),
                    parent_page_id: "pd".into(),
                    ignore: vec![],
                },
            ],
            token: "t".into(),
            token_file: None,
        };
        assert_eq!(cfg.mapping_for_path("docs/readme.md").unwrap().name, "docs");
        assert_eq!(cfg.mapping_for_path("app/src/main.rs").unwrap().name, "app");
        assert!(cfg.mapping_for_path("unknown/x").is_none());
    }
}
