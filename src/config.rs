//! TOML configuration. The Notion token is NEVER stored here — it comes from the
//! NOTION_TOKEN environment variable (systemd EnvironmentFile / LoadCredential).

use std::path::PathBuf;

use serde::Deserialize;
use tracing::warn;

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
    /// One or many mappings. TOML accepts either a single `[mapping]` table or repeated
    /// `[[mapping]]` tables; untagged, so the table-vs-sequence shape picks the variant.
    pub mapping: RawMappings,
    /// Optional `[webhook]` table. Absent means the receiver never starts: the daemon
    /// polls exactly as it did before webhooks existed.
    #[serde(default)]
    pub webhook: RawWebhook,
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

/// The optional `[webhook]` table. Struct-level `#[serde(default)]` makes every key
/// optional (falling back to `Default`), so an empty table -- or none at all -- is valid
/// and `enabled = false` is the resting state. `deny_unknown_fields` turns a typo like
/// `secrets_file` into a loud parse error instead of a silently ignored key.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawWebhook {
    /// Off by default; the receiver only binds when this is true.
    pub enabled: bool,
    /// Loopback by default: the intended deployment terminates TLS in a tunnel
    /// (cloudflared) and forwards here, so there's no reason to bind a public interface.
    pub bind: String,
    pub port: u16,
    /// The single path the receiver answers on; anything else 404s.
    pub path: String,
    /// Optional file holding the signing secret (Notion's verification_token). Read only
    /// when $NOTION_WEBHOOK_SECRET is unset. Absent is fine: the one-time handshake
    /// delivers the token and the receiver persists it under the state dir.
    pub secret_file: Option<PathBuf>,
    /// How slow the poller may fall back to once webhook pushes cover the common case.
    /// Parsed now so the schema is stable; not yet wired into the poll loop.
    pub fallback_poll_secs: u64,
}

impl Default for RawWebhook {
    fn default() -> Self {
        RawWebhook {
            enabled: false,
            bind: "127.0.0.1".to_string(),
            port: 8080,
            path: "/notion-webhook".to_string(),
            secret_file: None,
            fallback_poll_secs: 900,
        }
    }
}

/// The optional per-directory override file (`<local_root>/.notion-sync.toml`). Only
/// the dir-scoped keys live here; deny_unknown_fields rejects anything that would
/// repoint the mapping or touch secrets, so a typo fails loudly instead of silently
/// doing nothing. A missing key inherits from the central config.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RawPerDirConfig {
    /// Extends the central mapping's ignore list (additive, not a replacement).
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Overrides the central max_file_bytes for this directory only.
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
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
    /// Effective cap for this mapping: the per-dir `.notion-sync.toml` value if it set
    /// one, else the central `max_file_bytes`.
    pub max_file_bytes: u64,
}

/// Resolved webhook settings. `Some` only when `[webhook] enabled = true`; `None` is the
/// poller-only resting state, so an existing config without a `[webhook]` table behaves
/// exactly as before.
#[derive(Debug, Clone)]
pub struct Webhook {
    pub bind: String,
    pub port: u16,
    pub path: String,
    pub fallback_poll_secs: u64,
    /// Signing secret (the verification_token), resolved at load from
    /// $NOTION_WEBHOOK_SECRET or `secret_file`. `None` means we haven't been handed one
    /// yet; the receiver accepts the one-time handshake, persists the token to
    /// `secret_store_path`, and verifies every event after that.
    pub secret: Option<String>,
    /// Where a handshake-delivered token is persisted so it survives a restart
    /// ($XDG_STATE_HOME/notion-sync/webhook_secret, beside state.db and the object store).
    pub secret_store_path: PathBuf,
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
    /// Webhook receiver settings, or `None` when `[webhook]` is absent or disabled.
    pub webhook: Option<Webhook>,
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
            // A missing/unreadable root used to be a hard error here, to fail fast on a
            // typo'd path. But on a late-mount or impermanence host (skadi) a root that
            // isn't there *yet* at boot turned that fail-fast into a systemd crash-loop.
            // So warn and keep the mapping instead: reconcile, the poller health-check,
            // and the watcher each already skip a missing root per mapping, and keeping it
            // in the config means the CLI subcommands can still resolve its paths.
            if !rm.local_root.is_dir() {
                warn!(
                    mapping = %name,
                    root = %rm.local_root.display(),
                    "local_root is missing or not a directory; keeping the mapping, but it won't sync until the path exists (typo, unmounted volume, or a dir that isn't created yet?)"
                );
            }
            // Per-directory overrides live in <local_root>/.notion-sync.toml. Optional:
            // a mapping with no such file is exactly the old central-only behavior.
            // ignore is additive (central baseline + this dir's extras); max_file_bytes
            // is a straight override. Registry/secret keys in that file are rejected at
            // parse time, so a mapping can't be repointed from inside its own tree.
            let per_dir = load_per_dir(&rm.local_root)?;
            let mut ignore = rm.ignore;
            ignore.extend(per_dir.ignore);
            let max_file_bytes = per_dir.max_file_bytes.unwrap_or(raw.max_file_bytes);
            mappings.push(Mapping {
                name,
                local_root: rm.local_root,
                parent_page_id: rm.parent_page_id,
                ignore,
                max_file_bytes,
            });
        }

        let token = load_token(raw.token_file.as_deref())?;
        let webhook = resolve_webhook(raw.webhook)?;

        Ok(Config {
            notion_version: raw.notion_version,
            poll_interval_secs: raw.poll_interval_secs,
            debounce_ms: raw.debounce_ms,
            max_file_bytes: raw.max_file_bytes,
            mappings,
            token,
            token_file: raw.token_file,
            webhook,
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

/// Load `<local_root>/.notion-sync.toml` if present. A missing file is fine (the mapping
/// just uses central defaults). A present-but-unreadable or malformed file is an error,
/// because silently falling back to central defaults is how you end up mirroring
/// something you meant to ignore and only notice later.
fn load_per_dir(local_root: &std::path::Path) -> Result<RawPerDirConfig, ConfigError> {
    let path = local_root.join(".notion-sync.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RawPerDirConfig::default()),
        Err(e) => return Err(ConfigError::Read(format!("{}: {e}", path.display()))),
    };
    toml::from_str(&text).map_err(|e| {
        ConfigError::Parse(format!(
            "{}: {e} (per-directory config accepts only `ignore` and `max_file_bytes`)",
            path.display()
        ))
    })
}

// The daemon's own state dir and per-directory config file are never mirrored, whatever
// the user's ignore list says, so a config edit can't accidentally drag the machinery
// into Notion. Syncthing treats .stignore the same way.
const ALWAYS_IGNORE: [&str; 2] = [".notion-sync", ".notion-sync.toml"];

// Hand-rolled to avoid a glob-crate dependency: exact match, leading "*.ext", or
// trailing "build*", tested against every path component.
pub fn is_ignored(rel_path: &std::path::Path, patterns: &[String]) -> bool {
    let components: Vec<String> = rel_path
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_string()))
        .collect();
    for comp in &components {
        if ALWAYS_IGNORE.contains(&comp.as_str()) {
            return true;
        }
    }
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

/// Turn the raw `[webhook]` table into resolved settings, or `None` when disabled.
///
/// Strict on shape (bind must be an IP, port and path must be sane) but deliberately
/// lenient on the secret: an enabled receiver with no secret yet is a valid state,
/// because Notion's one-time handshake is what delivers the signing token. The receiver
/// accepts that single unsigned handshake, persists the token, and verifies everything
/// afterward.
fn resolve_webhook(raw: RawWebhook) -> Result<Option<Webhook>, ConfigError> {
    if !raw.enabled {
        return Ok(None);
    }
    if raw.bind.parse::<std::net::IpAddr>().is_err() {
        return Err(ConfigError::Invalid(format!(
            "webhook.bind must be an IP address (e.g. \"127.0.0.1\"), got {:?}",
            raw.bind
        )));
    }
    if raw.port == 0 {
        return Err(ConfigError::Invalid("webhook.port must be non-zero".into()));
    }
    if !raw.path.starts_with('/') {
        return Err(ConfigError::Invalid(format!(
            "webhook.path must start with '/', got {:?}",
            raw.path
        )));
    }
    if raw.fallback_poll_secs == 0 {
        return Err(ConfigError::Invalid(
            "webhook.fallback_poll_secs must be > 0".into(),
        ));
    }
    let secret = load_webhook_secret(raw.secret_file.as_deref())?;
    Ok(Some(Webhook {
        bind: raw.bind,
        port: raw.port,
        path: raw.path,
        fallback_poll_secs: raw.fallback_poll_secs,
        secret,
        secret_store_path: default_webhook_secret_path(),
    }))
}

/// Resolve the webhook signing secret without erroring when it simply isn't set yet.
/// Precedence: $NOTION_WEBHOOK_SECRET, then `secret_file`. A configured-but-unreadable or
/// empty `secret_file` IS an error (a typo'd path must not silently fall back to an
/// unverified endpoint); an unset secret returns `None` so the handshake can bootstrap.
fn load_webhook_secret(
    secret_file: Option<&std::path::Path>,
) -> Result<Option<String>, ConfigError> {
    if let Ok(s) = std::env::var("NOTION_WEBHOOK_SECRET") {
        if !s.trim().is_empty() {
            return Ok(Some(s.trim().to_string()));
        }
    }
    if let Some(path) = secret_file {
        let s = std::fs::read_to_string(path).map_err(|e| {
            ConfigError::Invalid(format!(
                "webhook.secret_file {} could not be read: {e}",
                path.display()
            ))
        })?;
        let s = s.trim().to_string();
        if s.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "webhook.secret_file {} is empty",
                path.display()
            )));
        }
        return Ok(Some(s));
    }
    Ok(None)
}

/// $XDG_STATE_HOME/notion-sync/webhook_secret (falling back to ~/.local/state), beside
/// state.db and the object store. Mirrors the resolution in state.rs/snapshot.rs; kept
/// local rather than shared because those module helpers are private and this is just a
/// different leaf under the same well-known dir.
fn default_webhook_secret_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local/state")
        });
    base.join("notion-sync").join("webhook_secret")
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
        "No Notion token found. Set $NOTION_TOKEN, or point `token_file` in the config at \
         a file holding the raw token. Under systemd (the NixOS module's `environmentFile`) \
         that file must contain a literal `NOTION_TOKEN=ntn_...` line: systemd silently \
         ignores a bare token with no `NOTION_TOKEN=`, which surfaces as this exact error \
         even though you did set one. First run? Create an integration at https://www.notion.so/my-integrations, \
         share the parent page with it, then see the README Quickstart."
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
    fn always_ignores_own_state_dir_and_config_even_with_empty_patterns() {
        let none: Vec<String> = vec![];
        assert!(is_ignored(Path::new(".notion-sync/objects/ab/x.gz"), &none));
        assert!(is_ignored(Path::new(".notion-sync.toml"), &none));
        assert!(is_ignored(Path::new("sub/.notion-sync.toml"), &none));
        assert!(!is_ignored(Path::new("src/main.rs"), &none));
    }

    #[test]
    fn per_dir_config_parses_allowed_keys_and_rejects_others() {
        let ok: RawPerDirConfig =
            toml::from_str("ignore = [\"result\", \"dist\"]\nmax_file_bytes = 1000").unwrap();
        assert_eq!(ok.ignore, vec!["result", "dist"]);
        assert_eq!(ok.max_file_bytes, Some(1000));
        // Registry/secret keys must not silently override the mapping.
        assert!(toml::from_str::<RawPerDirConfig>("parent_page_id = \"abc\"").is_err());
        assert!(toml::from_str::<RawPerDirConfig>("local_root = \"/x\"").is_err());
    }

    // The end-to-end merge: parsing is covered above, but this drives the real
    // Config::load file read so additive-ignore and the max_file_bytes override can't
    // silently regress.
    #[test]
    fn load_merges_per_dir_overrides_additively_and_leaves_others_central() {
        use std::io::Write;

        // Unique throwaway workspace so parallel test runs don't collide.
        let base = std::env::temp_dir().join(format!(
            "notion-sync-cfgtest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let with_override = base.join("proj");
        let without_override = base.join("notes");
        std::fs::create_dir_all(&with_override).unwrap();
        std::fs::create_dir_all(&without_override).unwrap();

        // Only the first mapping's root carries a per-directory file.
        std::fs::write(
            with_override.join(".notion-sync.toml"),
            "ignore = [\"build\", \"*.tmp\"]\nmax_file_bytes = 123\n",
        )
        .unwrap();

        // token_file keeps the test off $NOTION_TOKEN.
        let token = base.join("token");
        std::fs::write(&token, "ntn_test\n").unwrap();

        let cfg_path = base.join("config.toml");
        let mut f = std::fs::File::create(&cfg_path).unwrap();
        write!(
            f,
            "max_file_bytes = 5000000\n\
             token_file = {token:?}\n\n\
             [[mapping]]\n\
             local_root = {with_override:?}\n\
             parent_page_id = \"p1\"\n\
             ignore = [\".git\", \"target\"]\n\n\
             [[mapping]]\n\
             local_root = {without_override:?}\n\
             parent_page_id = \"p2\"\n\
             ignore = [\".git\"]\n",
        )
        .unwrap();
        drop(f);

        let cfg = Config::load(&cfg_path).expect("config should load");
        assert_eq!(cfg.mappings.len(), 2);

        let proj = cfg.mapping_by_name("proj").unwrap();
        // Central baseline kept; the per-dir entries are appended, not a replacement.
        assert_eq!(proj.ignore, vec![".git", "target", "build", "*.tmp"]);
        // Per-dir max_file_bytes overrides the central default.
        assert_eq!(proj.max_file_bytes, 123);

        let notes = cfg.mapping_by_name("notes").unwrap();
        // No per-dir file: central values pass through untouched.
        assert_eq!(notes.ignore, vec![".git"]);
        assert_eq!(notes.max_file_bytes, 5_000_000);

        let _ = std::fs::remove_dir_all(&base);
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
                    max_file_bytes: 5_000_000,
                },
                Mapping {
                    name: "docs".into(),
                    local_root: std::path::PathBuf::from("/d"),
                    parent_page_id: "pd".into(),
                    ignore: vec![],
                    max_file_bytes: 5_000_000,
                },
            ],
            token: "t".into(),
            token_file: None,
            webhook: None,
        };
        assert_eq!(cfg.mapping_for_path("docs/readme.md").unwrap().name, "docs");
        assert_eq!(cfg.mapping_for_path("app/src/main.rs").unwrap().name, "app");
        assert!(cfg.mapping_for_path("unknown/x").is_none());
    }

    #[test]
    fn webhook_absent_resolves_to_none() {
        // No [webhook] table at all: the field defaults in and resolve() yields None
        // (poller-only), so every pre-webhook config keeps working untouched.
        let raw: RawConfig =
            toml::from_str("[[mapping]]\nlocal_root = \"/tmp\"\nparent_page_id = \"p\"\n").unwrap();
        assert!(!raw.webhook.enabled);
        assert!(resolve_webhook(raw.webhook).unwrap().is_none());
    }

    #[test]
    fn webhook_enabled_resolves_with_defaults() {
        // Bare `enabled = true` is enough; the rest come from RawWebhook::default().
        let raw: RawWebhook = toml::from_str("enabled = true").unwrap();
        let w = resolve_webhook(raw).unwrap().expect("enabled => Some");
        assert_eq!(w.bind, "127.0.0.1");
        assert_eq!(w.port, 8080);
        assert_eq!(w.path, "/notion-webhook");
        assert_eq!(w.fallback_poll_secs, 900);
        assert!(w.secret_store_path.ends_with("notion-sync/webhook_secret"));
    }

    #[test]
    fn webhook_rejects_bad_bind_port_and_path() {
        let bad_bind = RawWebhook {
            enabled: true,
            bind: "not-an-ip".into(),
            ..RawWebhook::default()
        };
        assert!(resolve_webhook(bad_bind).is_err());

        let bad_port = RawWebhook {
            enabled: true,
            port: 0,
            ..RawWebhook::default()
        };
        assert!(resolve_webhook(bad_port).is_err());

        let bad_path = RawWebhook {
            enabled: true,
            path: "notion-webhook".into(),
            ..RawWebhook::default()
        };
        assert!(resolve_webhook(bad_path).is_err());
    }

    #[test]
    fn webhook_secret_file_is_read_trimmed_and_empty_is_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "notion-sync-wh-secret-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret");

        std::fs::write(&path, "  sek_123\n").unwrap();
        assert_eq!(
            load_webhook_secret(Some(&path)).unwrap().as_deref(),
            Some("sek_123")
        );

        // A configured-but-empty secret file is a hard error, not a silent None.
        std::fs::write(&path, "   \n").unwrap();
        assert!(load_webhook_secret(Some(&path)).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
