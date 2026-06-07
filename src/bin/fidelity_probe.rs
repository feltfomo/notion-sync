//! Round-trips an adversarial UTF-8 payload through the real /v1/blocks endpoint via
//! the daemon's own encode/reassemble path, exiting non-zero on any mutation. This is
//! the authoritative write-back gate, not a sandbox approximation.
//!
//! NOTION_TOKEN=... fidelity-probe --parent-page-id <PAGE_ID> [--version 2022-06-28]

use std::sync::Arc;

use notion_sync::api::{NotionClient, RateLimiter};
use notion_sync::chunk;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_ansi(false).init();
    if let Err(e) = run().await {
        eprintln!("FIDELITY PROBE FAILED: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let parent = arg("--parent-page-id").ok_or("missing --parent-page-id")?;
    let version = arg("--version").unwrap_or_else(|| "2022-06-28".to_string());
    let token = std::env::var("NOTION_TOKEN").map_err(|_| "NOTION_TOKEN not set")?;

    let limiter = Arc::new(RateLimiter::notion_default());
    let api = NotionClient::new(token, version, limiter).map_err(|e| e.to_string())?;

    // Adversarial payload: tabs, mixed indentation, trailing whitespace, an internal
    // blank line, multibyte chars across a chunk boundary, and a final newline.
    let mut payload = String::new();
    payload.push_str("fn main() {\n");
    payload.push_str("\tlet x = 1;   \n"); // trailing spaces
    payload.push_str("\t\tlet y = 2;\n"); // double tab
    payload.push_str("    let z = 3;\n"); // four-space indent
    payload.push_str("\n"); // internal blank line
    payload.push_str("\t// emoji \u{1F680} CJK \u{4F60}\u{597D} euro \u{20AC} accented \u{00E9}\n");
    // Force a >2000-char item so multibyte boundary handling is exercised.
    payload.push_str(&"\u{1F600}".repeat(2100));
    payload.push_str("\n}\n");

    println!("creating probe page under {parent} ...");
    let page = api.create_page(&parent, "Fidelity Probe", vec![]).await.map_err(|e| e.to_string())?;

    // Encode + append exactly as the daemon would.
    let blocks = chunk::encode(&payload, "rust");
    for batch in chunk::batch_blocks(&blocks) {
        api.append_children(&page.id, batch).await.map_err(|e| e.to_string())?;
    }

    let children = api.list_children(&page.id).await.map_err(|e| e.to_string())?;
    let per_block: Vec<Vec<String>> = children
        .iter()
        .filter(|b| b.ty == "code")
        .map(|b| {
            b.code
                .as_ref()
                .map(|c| c.rich_text.iter().map(|r| r.plain_text.clone()).collect())
                .unwrap_or_default()
        })
        .collect();
    let readback = chunk::reassemble(&per_block);

    // Trash the probe page (best effort).
    let _ = api.update_page(&page.id, None, None, Some(true)).await;

    if readback == payload {
        println!(
            "FIDELITY OK: {} bytes round-tripped byte-identical (blake3 {})",
            payload.len(),
            notion_sync::hashutil::hash_str(&payload)
        );
        Ok(())
    } else {
        let (i, a, b) = first_diff(&payload, &readback);
        Err(format!(
            "MUTATION DETECTED at byte {i}: expected {a:?}, got {b:?}. \
             Write-back is UNSAFE until the chunker compensates deterministically."
        ))
    }
}

fn first_diff(a: &str, b: &str) -> (usize, Option<char>, Option<char>) {
    let mut ai = a.chars();
    let mut bi = b.chars();
    let mut idx = 0;
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) if x == y => idx += x.len_utf8(),
            (x, y) => return (idx, x, y),
        }
    }
}

fn arg(flag: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    None
}
