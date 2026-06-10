//! Integration-webhook receiver: receive, verify (HMAC-SHA256), then dispatch into the
//! engine -- a content update pulls the page to disk, a delete mirrors the trash locally.
//! The poller stays the catch-all fallback for anything this doesn't handle.
//!
//! Notion only delivers to public HTTPS, so the intended deployment terminates TLS in a
//! tunnel (cloudflared) and forwards to this loopback listener. Delivery is at-most-once
//! and best-effort -- events can be dropped, retried, reordered, or aggregated -- so this
//! is a latency optimization layered on top of the poller, never a replacement for it.
//!
//! Two request shapes land on the one configured path:
//!   * the one-time verification handshake: `{"verification_token": "..."}`, unsigned.
//!     We persist the token (it doubles as the HMAC key) and 200. You then paste it into
//!     the integration's Webhooks tab to finish verification.
//!   * real events, signed with `X-Notion-Signature: sha256=<hex>` over the raw body,
//!     keyed by the verification_token. We verify in constant time and 401 on mismatch.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{watch, RwLock};
use tracing::{debug, error, info, warn};

use super::engine::{Discovery, Engine};
use crate::config::Webhook;

/// Shared receiver state. The signing secret can change at runtime (the handshake
/// installs it), so it lives behind an RwLock the per-connection handlers share.
struct Receiver {
    path: String,
    secret: RwLock<Option<String>>,
    secret_store_path: PathBuf,
    /// The sync engine, so a verified event can be dispatched straight into the same
    /// pull/delete paths the poller uses. Per-path locks inside the engine serialize a
    /// webhook-driven pull against a concurrent poll of the same node.
    engine: Arc<Engine>,
}

pub async fn run(engine: Arc<Engine>, cfg: Webhook, mut shutdown: watch::Receiver<bool>) {
    let addr: SocketAddr = match format!("{}:{}", cfg.bind, cfg.port).parse() {
        Ok(a) => a,
        Err(e) => {
            error!(bind = %cfg.bind, port = cfg.port, error = %e, "invalid webhook bind address; receiver not started");
            return;
        }
    };

    // A bind failure is non-fatal to the daemon: the poller still syncs, so log loudly
    // and bow out instead of taking the whole process down over a busy port.
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = %addr, error = %e, "webhook listener failed to bind; continuing poller-only");
            return;
        }
    };

    // Prefer a configured secret; otherwise adopt one persisted by an earlier handshake
    // so a restart doesn't force re-verification.
    let mut secret = cfg.secret.clone();
    if secret.is_none() {
        secret = load_persisted_secret(&cfg.secret_store_path);
        if secret.is_some() {
            info!("loaded persisted webhook signing secret from a previous handshake");
        }
    }
    if secret.is_none() {
        warn!("webhook receiver has no signing secret yet; it will accept Notion's one-time verification handshake, then verify every event after that");
    }

    let recv = Arc::new(Receiver {
        path: cfg.path.clone(),
        secret: RwLock::new(secret),
        secret_store_path: cfg.secret_store_path.clone(),
        engine,
    });

    info!(addr = %addr, path = %cfg.path, "webhook receiver listening");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("webhook receiver shutting down");
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!(error = %e, "webhook accept failed");
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let recv = recv.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let recv = recv.clone();
                        async move { Ok::<_, Infallible>(handle(recv, req).await) }
                    });
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        debug!(error = %e, "webhook connection closed with error");
                    }
                });
            }
        }
    }
}

async fn handle(recv: Arc<Receiver>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    if req.method() != Method::POST || req.uri().path() != recv.path {
        return text(StatusCode::NOT_FOUND, "not found");
    }

    // Capture the signature header before the body consumes the request.
    let signature = req
        .headers()
        .get("X-Notion-Signature")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let raw = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            warn!(error = %e, "failed to read webhook request body");
            return text(StatusCode::BAD_REQUEST, "bad body");
        }
    };

    // The one-time handshake is unsigned by design: it's how we learn the signing key.
    if let Ok(hs) = serde_json::from_slice::<Handshake>(&raw) {
        match persist_secret(&recv.secret_store_path, &hs.verification_token) {
            Ok(()) => info!(
                store = %recv.secret_store_path.display(),
                "received webhook verification_token; persisted it -- paste it into the integration's Webhooks tab and click Verify"
            ),
            Err(e) => warn!(
                error = %e,
                "received webhook verification_token but could not persist it; verification works this run but won't survive a restart"
            ),
        }
        *recv.secret.write().await = Some(hs.verification_token);
        return text(StatusCode::OK, "ok");
    }

    // Everything else must be signed. No secret yet => we can't trust anything; refuse.
    let secret = recv.secret.read().await.clone();
    let Some(secret) = secret else {
        warn!("rejecting webhook event: no signing secret yet (verification handshake hasn't completed)");
        return text(StatusCode::UNAUTHORIZED, "not verified");
    };

    if !verify_signature(&raw, signature.as_deref(), &secret) {
        warn!("rejecting webhook: missing or invalid X-Notion-Signature");
        return text(StatusCode::UNAUTHORIZED, "bad signature");
    }

    // Ack fast, then dispatch. Notion's delivery is best-effort with a tight ack window
    // and retries, so a slow pull (network I/O) must never hold the response open and
    // trigger a redelivery. Parse + verify cost is trivial and stays inline; the actual
    // pull/delete is handed to a task so we can 200 right away. Per-path locks in the
    // engine serialize that task against a concurrent poll of the same node.
    match serde_json::from_slice::<WebhookEvent>(&raw) {
        Ok(ev) => {
            let engine = recv.engine.clone();
            tokio::spawn(async move { dispatch(&engine, &ev).await });
        }
        Err(e) => warn!(error = %e, "verified webhook but could not parse its event body"),
    }
    text(StatusCode::OK, "ok")
}

/// What a verified event asks us to do locally. A separate, pure mapping so the routing
/// is unit-testable without standing up an Engine, and so an unrecognized event type
/// falls through to `Ignore` (the poller catches it) instead of a blind pull.
#[derive(Clone, Copy)]
enum Action {
    Pull,
    Delete,
    Ignore,
}

/// Map an event type to a local action. Deliberately conservative: page creates and
/// content updates route to Pull (a tracked page pulls, an untracked one is a discovery
/// candidate), deletes mirror the trash, and everything else (moves, property-only edits,
/// undeletes, comments, database/data_source events) is left to the poller.
fn action_for(event_type: &str) -> Action {
    match event_type {
        "page.created" | "page.content_updated" => Action::Pull,
        "page.deleted" => Action::Delete,
        _ => Action::Ignore,
    }
}

/// True only if there is at least one author and every author is our own bot. Notion
/// aggregates events, so `authors` can list several people; a single human co-author in
/// the window means a real edit we must not treat as our own echo.
fn authored_only_by(authors: &[Author], bot_user_id: &str) -> bool {
    !authors.is_empty() && authors.iter().all(|a| a.id == bot_user_id)
}

/// Apply a verified event to the local mirror, reusing the engine's existing pull/delete/
/// discovery paths. The entity id is matched against tracked pages: a tracked page pulls
/// (or, for a delete, mirrors the trash); an untracked page that was created or updated is
/// handed to discovery, which adopts it only if it places under a tracked parent chain and
/// reads back as a faithful code body. Anything else stays the poller's job.
async fn dispatch(engine: &Engine, ev: &WebhookEvent) {
    let action = action_for(&ev.ty);
    if matches!(action, Action::Ignore) {
        debug!(ty = %ev.ty, entity = %ev.entity.id, "webhook event needs no local action; poller remains the catch-all");
        return;
    }

    let node = {
        let st = engine.state.lock().await;
        st.get_by_page_id(&ev.entity.id).ok().flatten()
    };

    match (action, node) {
        // A tracked page that was trashed: mirror the delete locally.
        (Action::Delete, Some(node)) => {
            info!(rel_path = %node.rel_path, "webhook: remote page trashed; applying remote delete locally");
            if let Err(e) = engine.handle_remote_delete(&node).await {
                warn!(rel_path = %node.rel_path, error = %e, "webhook-driven remote delete failed");
            }
        }
        // A delete for a page we never tracked is a no-op.
        (Action::Delete, None) => {
            debug!(ty = %ev.ty, entity = %ev.entity.id, "webhook delete for an untracked page; nothing to do");
        }
        // A tracked page changed: pull, reusing the poller's content echo guard. Same rule
        // (engine::remote_body_matches_last_sync is the shared half): an edit attributed
        // solely to our bot whose remote body still hashes to what we last synced is our
        // own push coming back -- skip it. A human co-author, or a diverged body, is real.
        (Action::Pull, Some(node)) => {
            if authored_only_by(&ev.authors, &engine.bot_user_id)
                && engine.remote_body_matches_last_sync(&node).await
            {
                debug!(rel_path = %node.rel_path, "webhook: skipping self-authored echo (content matches last sync)");
                return;
            }
            info!(rel_path = %node.rel_path, "webhook: external Notion edit; pulling");
            if let Err(e) = engine.pull_page(&node).await {
                warn!(rel_path = %node.rel_path, error = %e, "webhook-driven pull failed");
            }
        }
        // An untracked page created/updated under our tree: try to discover it. Skip the
        // probe when the event names a non-page entity, or is attributed solely to our own
        // bot (our just-pushed page, which the push itself will track -- adopting it here
        // would race that push).
        (Action::Pull, None) => {
            if !ev.entity.ty.is_empty() && ev.entity.ty != "page" {
                debug!(ty = %ev.ty, entity = %ev.entity.id, entity_type = %ev.entity.ty, "webhook event for a non-page entity; leaving to the poller");
                return;
            }
            if authored_only_by(&ev.authors, &engine.bot_user_id) {
                debug!(ty = %ev.ty, entity = %ev.entity.id, "webhook: untracked page authored only by us; the in-flight push will track it");
                return;
            }
            match engine.discover_remote_page(&ev.entity.id).await {
                Ok(Discovery::Created) => {}
                Ok(_) => {
                    debug!(ty = %ev.ty, entity = %ev.entity.id, "webhook: untracked page not mirrored (outside the tree or no code body yet)")
                }
                Err(e) => {
                    warn!(entity = %ev.entity.id, error = %e, "webhook-driven discovery failed")
                }
            }
        }
        (Action::Ignore, _) => {}
    }
}

/// Constant-time HMAC-SHA256 check of the `sha256=<hex>` signature over the raw body.
fn verify_signature(body: &[u8], header: Option<&str>, secret: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let Some(hex) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Some(expected) = hex_decode(hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Decode a hex string to bytes. Hand-rolled to skip a `hex` crate for ~15 lines.
/// Returns None on odd length or any non-hex digit.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Atomically persist the signing token, owner-only (0600) on Unix so the key isn't
/// world-readable. temp-file + rename so a crash mid-write can't leave a half-written key.
fn persist_secret(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, token.trim().as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

fn load_persisted_secret(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn text(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"ok"))))
}

#[derive(Debug, Deserialize)]
struct Handshake {
    verification_token: String,
}

#[derive(Debug, Deserialize)]
struct WebhookEvent {
    #[serde(rename = "type", default)]
    ty: String,
    #[serde(default)]
    entity: Entity,
    #[serde(default)]
    authors: Vec<Author>,
}

#[derive(Debug, Deserialize, Default)]
struct Entity {
    #[serde(default)]
    id: String,
    #[serde(rename = "type", default)]
    ty: String,
}

#[derive(Debug, Deserialize, Default)]
struct Author {
    #[serde(default)]
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    fn sign(body: &[u8], secret: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let mut hex = String::new();
        for b in mac.finalize().into_bytes() {
            hex.push_str(&format!("{b:02x}"));
        }
        format!("sha256={hex}")
    }

    #[test]
    fn hex_decode_roundtrips_and_rejects_garbage() {
        assert_eq!(hex_decode("00ff10ab"), Some(vec![0x00, 0xff, 0x10, 0xab]));
        assert_eq!(hex_decode("AB"), Some(vec![0xab]));
        assert!(hex_decode("abc").is_none()); // odd length
        assert!(hex_decode("zz").is_none()); // non-hex
    }

    #[test]
    fn verify_signature_accepts_valid_and_rejects_tampering() {
        let secret = "verif_token_example";
        let body = br#"{"type":"page.content_updated","entity":{"id":"p1","type":"page"}}"#;
        let header = sign(body, secret);

        assert!(verify_signature(body, Some(&header), secret));
        // Wrong key, tampered body, wrong scheme, and a missing header all fail closed.
        assert!(!verify_signature(body, Some(&header), "wrong_token"));
        assert!(!verify_signature(b"tampered", Some(&header), secret));
        assert!(!verify_signature(body, Some("deadbeef"), secret));
        assert!(!verify_signature(body, None, secret));
    }

    #[test]
    fn events_do_not_masquerade_as_handshakes() {
        // The handshake has exactly the token; a real event lacks it, so the handshake
        // branch can't swallow an event (which would skip signature verification).
        let hs: Handshake = serde_json::from_slice(br#"{"verification_token":"tok_123"}"#).unwrap();
        assert_eq!(hs.verification_token, "tok_123");
        assert!(serde_json::from_slice::<Handshake>(
            br#"{"type":"page.content_updated","entity":{"id":"p1"}}"#
        )
        .is_err());
    }

    #[test]
    fn persisted_secret_roundtrips_and_ignores_blank() {
        let dir = std::env::temp_dir().join(format!(
            "notion-sync-wh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("webhook_secret");
        persist_secret(&path, "  tok_persisted\n").unwrap();
        assert_eq!(
            load_persisted_secret(&path).as_deref(),
            Some("tok_persisted")
        );

        persist_secret(&path, "   ").unwrap();
        assert!(load_persisted_secret(&path).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn action_routing_handles_pull_delete_and_falls_through_to_ignore() {
        assert!(matches!(action_for("page.created"), Action::Pull));
        assert!(matches!(action_for("page.content_updated"), Action::Pull));
        assert!(matches!(action_for("page.deleted"), Action::Delete));
        // Anything we don't explicitly act on stays with the poller -- never a blind pull.
        assert!(matches!(action_for("page.undeleted"), Action::Ignore));
        assert!(matches!(action_for("page.moved"), Action::Ignore));
        assert!(matches!(action_for("comment.created"), Action::Ignore));
        assert!(matches!(action_for(""), Action::Ignore));
    }

    #[test]
    fn self_author_check_requires_every_author_to_be_the_bot() {
        let bot = "bot-user-1";
        let only_us = [Author {
            id: "bot-user-1".into(),
        }];
        let with_human = [
            Author {
                id: "bot-user-1".into(),
            },
            Author {
                id: "human-9".into(),
            },
        ];
        assert!(authored_only_by(&only_us, bot));
        // A human co-author in the same aggregation window is a real edit: don't skip it.
        assert!(!authored_only_by(&with_human, bot));
        // No authors at all is not a self-edit; fall through to verify/pull.
        assert!(!authored_only_by(&[], bot));
    }
}
