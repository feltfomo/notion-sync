//! Entrypoint. Parses the CLI, then either boots the sync daemon (`run`, the default
//! when no subcommand is given) or hands off to a `cli` subcommand
//! (backup/restore/history/log/diff/show/untrash/gc).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use tokio::sync::{watch, Mutex};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use notion_sync::api::{NotionClient, RateLimiter};
use notion_sync::cli::{Cli, Command};
use notion_sync::config::Config;
use notion_sync::state::State;
use notion_sync::sync::{poller, reconcile, watcher, Engine};

#[tokio::main]
async fn main() {
    // journald-friendly: no ANSI, structured fields. RUST_LOG overrides level.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_target(false)
        .init();

    let cli = Cli::parse();
    if let Err(e) = real_main(cli).await {
        error!(error = %e, "fatal");
        std::process::exit(1);
    }
}

async fn real_main(cli: Cli) -> Result<(), String> {
    let config_path = resolve_config_path(cli.config);
    match cli.command {
        // Daemon is the default when no subcommand is supplied.
        None | Some(Command::Run) => run_daemon(&config_path).await,
        Some(command) => {
            // The non-daemon subcommands need an engine (state + store + api) but not
            // the watcher/poller, and no whoami round-trip (only `untrash` calls out).
            let engine = build_engine(&config_path, String::new()).await?;
            notion_sync::cli::dispatch(engine, command).await
        }
    }
}

/// Build the shared engine: config, API client, state.db, object store. Does NOT call
/// whoami; the daemon passes its bot id in, subcommands pass an empty string.
async fn build_engine(config_path: &Path, bot_user_id: String) -> Result<Arc<Engine>, String> {
    let cfg = Config::load(config_path).map_err(|e| e.to_string())?;
    let limiter = Arc::new(RateLimiter::notion_default());
    let api = Arc::new(
        NotionClient::new(cfg.token.clone(), cfg.notion_version.clone(), limiter)
            .map_err(|e| e.to_string())?,
    );
    let state = Arc::new(Mutex::new(
        State::open_default().map_err(|e| e.to_string())?,
    ));
    Ok(Arc::new(Engine {
        cfg: cfg.clone(),
        api,
        state,
        locks: notion_sync::sync::locks::PathLocks::new(),
        store: notion_sync::sync::snapshot::ObjectStore::new(&cfg.local_root),
        bot_user_id,
        self_writes: Mutex::new(HashMap::new()),
    }))
}

async fn run_daemon(config_path: &Path) -> Result<(), String> {
    if !ensure_config_exists(config_path)? {
        info!(
            path = %config_path.display(),
            "wrote a starter config; edit local_root + parent_page_id, export $NOTION_TOKEN, then re-run"
        );
        return Ok(());
    }
    let cfg = Config::load(config_path).map_err(|e| e.to_string())?;
    info!(local_root = %cfg.local_root.display(), parent = %cfg.parent_page_id, "loaded config");

    // Shared token bucket: the watcher and poller draw from the SAME limiter.
    let limiter = Arc::new(RateLimiter::notion_default());
    let api = Arc::new(
        NotionClient::new(cfg.token.clone(), cfg.notion_version.clone(), limiter)
            .map_err(|e| e.to_string())?,
    );

    // Identify ourselves once for echo-loop suppression.
    let bot_user_id = api
        .whoami()
        .await
        .map_err(|e| format!("failed GET /v1/users/me (token valid?): {e}"))?;
    info!(bot_user_id, "authenticated");

    let state = Arc::new(Mutex::new(
        State::open_default().map_err(|e| e.to_string())?,
    ));

    let engine = Arc::new(Engine {
        cfg: cfg.clone(),
        api,
        state,
        locks: notion_sync::sync::locks::PathLocks::new(),
        store: notion_sync::sync::snapshot::ObjectStore::new(&cfg.local_root),
        bot_user_id,
        self_writes: Mutex::new(HashMap::new()),
    });

    // Startup reconciliation (adopt existing pages, converge disk/state/Notion).
    if let Err(e) = reconcile::run(engine.clone()).await {
        error!(error = %e, "reconciliation failed; continuing into steady state");
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    spawn_signal_handler(shutdown_tx.clone());

    let watcher_task = {
        let engine = engine.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move { watcher::run(engine, rx).await })
    };
    let poller_task = {
        let engine = engine.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move { poller::run(engine, rx).await })
    };

    let _ = tokio::join!(watcher_task, poller_task);
    info!("shutdown complete");
    Ok(())
}

fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".config")
        });
    base.join("notion-sync").join("config.toml")
}

/// First-run scaffolding. If there's no config yet, create the parent dir, drop in a
/// copy of the bundled example, and return `false` so the caller can point the user
/// at what to edit instead of dying with a cryptic "cannot read config" on a fresh
/// install. Returns `true` when a config already exists and we should just run.
fn ensure_config_exists(path: &std::path::Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(true);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create config dir {}: {e}", dir.display()))?;
    }
    std::fs::write(path, include_str!("../config.example.toml"))
        .map_err(|e| format!("cannot write starter config {}: {e}", path.display()))?;
    Ok(false)
}

#[cfg(unix)]
fn spawn_signal_handler(shutdown_tx: watch::Sender<bool>) {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => info!("received SIGTERM"),
            _ = int.recv() => info!("received SIGINT"),
        }
        let _ = shutdown_tx.send(true);
    });
}

#[cfg(not(unix))]
fn spawn_signal_handler(shutdown_tx: watch::Sender<bool>) {
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = shutdown_tx.send(true);
    });
}
