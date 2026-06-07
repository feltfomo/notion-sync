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
    pub mapping: RawMapping,
}

#[derive(Debug, Deserialize)]
pub struct RawMapping {
    pub local_root: PathBuf,
    pub parent_page_id: String,
    #[serde(default)]
    pub ignore: Vec<String>,
}

fn default_version() -> String { "2022-06-28".to_string() }
fn default_poll() -> u64 { 45 }
fn default_debounce() -> u64 { 1000 }
fn default_policy() -> String { "local-wins".to_string() }
fn default_max_bytes() -> u64 { 5_000_000 }

#[derive(Debug, Clone)]
pub struct Config {
    pub notion_version: String,
    pub poll_interval_secs: u64,
    pub debounce_ms: u64,
    pub max_file_bytes: u64,
    pub local_root: PathBuf,
    pub parent_page_id: String,
    pub ignore: Vec<String>,
    /// Read from $NOTION_TOKEN, not from the file.
    pub token: String,
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
        let raw: RawConfig = toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;

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
            return Err(ConfigError::Invalid("poll_interval_secs must be > 0".into()));
        }
        if !raw.mapping.local_root.is_dir() {
            return Err(ConfigError::Invalid(format!(
                "local_root is not a directory: {}",
                raw.mapping.local_root.display()
            )));
        }
        let token = std::env::var("NOTION_TOKEN")
            .map_err(|_| ConfigError::Invalid("NOTION_TOKEN env var is not set".into()))?;
        if token.trim().is_empty() {
            return Err(ConfigError::Invalid("NOTION_TOKEN is empty".into()));
        }

        Ok(Config {
            notion_version: raw.notion_version,
            poll_interval_secs: raw.poll_interval_secs,
            debounce_ms: raw.debounce_ms,
            max_file_bytes: raw.max_file_bytes,
            local_root: raw.mapping.local_root,
            parent_page_id: raw.mapping.parent_page_id,
            ignore: raw.mapping.ignore,
            token,
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
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}
