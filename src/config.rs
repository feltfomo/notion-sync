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
    pub mapping: RawMapping,
}

#[derive(Debug, Deserialize)]
pub struct RawMapping {
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

#[derive(Debug, Clone)]
pub struct Config {
    pub notion_version: String,
    pub poll_interval_secs: u64,
    pub debounce_ms: u64,
    pub max_file_bytes: u64,
    pub local_root: PathBuf,
    pub parent_page_id: String,
    pub ignore: Vec<String>,
    /// Resolved from $NOTION_TOKEN (preferred) or `token_file`.
    pub token: String,
    /// Where the token came from, retained so the daemon can re-read it on a 401
    /// without a restart.
    pub token_file: Option<PathBuf>,
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
        if !raw.mapping.local_root.is_dir() {
            return Err(ConfigError::Invalid(format!(
                "local_root is not a directory: {}",
                raw.mapping.local_root.display()
            )));
        }
        let token = load_token(raw.token_file.as_deref())?;

        Ok(Config {
            notion_version: raw.notion_version,
            poll_interval_secs: raw.poll_interval_secs,
            debounce_ms: raw.debounce_ms,
            max_file_bytes: raw.max_file_bytes,
            local_root: raw.mapping.local_root,
            parent_page_id: raw.mapping.parent_page_id,
            ignore: raw.mapping.ignore,
            token,
            token_file: raw.token_file,
        })
    }
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
         it, export the token (export NOTION_TOKEN=secret_...), then see the README Quickstart."
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
        let pats = vec![".git".to_string(), "target".to_string(), "*.lock".to_string()];
        assert!(is_ignored(Path::new("target/debug/foo"), &pats));
        assert!(is_ignored(Path::new("a/b/Cargo.lock"), &pats));
        assert!(is_ignored(Path::new(".git/HEAD"), &pats));
        assert!(!is_ignored(Path::new("src/main.rs"), &pats));
    }
}
