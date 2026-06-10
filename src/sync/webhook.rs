//! Integration-webhook receiver. Phase 1: receive, verify, log. No engine dispatch yet.
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

use crate::config::Webhook;

/// Shared receiver state. The signing secret can change at runtime (the handshake
/// installs it), so it lives behind an RwLock the per-connection handlers share.
struct Receiver {
    path: String,
    secret: RwLock<Option<String>>,
    secret_store_path: PathBuf,
}

pub async fn run(cfg: Webhook, mut shutdown: watch::Receiver<bool>) {
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
    });

    info!(addr = %addr, path = %cfg.path, "webhook receiver listening (receive/verify/log only)");

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

    // Phase 1 stops here: log the event and 200. Engine dispatch is a later phase.
    match serde_json::from_slice::<WebhookEvent>(&raw) {
        Ok(ev) => {
            let authors: Vec<&str> = ev.authors.iter().map(|a| a.id.as_str()).collect();
            info!(
                ty = %ev.ty,
                entity = %ev.entity.id,
                entity_type = %ev.entity.ty,
                ?authors,
                "webhook event verified (logging only)"
            );
        }
        Err(e) => warn!(error = %e, "verified webhook but could not parse its event body"),
    }
    text(StatusCode::OK, "ok")
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
}
