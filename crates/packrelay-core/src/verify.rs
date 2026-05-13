// Verify subcommand — re-check an already-installed pack against the
// sidecar manifest we wrote at install time.
//
// Useful for: detecting bit-rot, confirming a manual edit didn't
// silently corrupt a file, and as a smoke test that the launcher's
// install loop produces consistent state on disk.

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::manifest::Manifest;

pub async fn run(dest: &Path) -> Result<()> {
    let sidecar = dest.join("_packrelay-manifest.json");
    let raw = fs::read_to_string(&sidecar)
        .await
        .with_context(|| format!("reading {}", sidecar.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&raw).with_context(|| "parsing sidecar manifest")?;

    println!(
        "[verify] {} v{} ({} files)",
        manifest.display_name,
        manifest.version,
        manifest.files.len()
    );

    let mut failures = Vec::<String>::new();
    for file in &manifest.files {
        let normalized = file.path.replace('\\', "/");
        let target = dest.join(&normalized);
        match check_file(&target, &file.sha256, file.size).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("  FAIL {}: {e:#}", file.path);
                failures.push(file.path.clone());
            }
        }
    }

    if failures.is_empty() {
        println!(
            "[verify] all {} files match the manifest.",
            manifest.files.len()
        );
        Ok(())
    } else {
        Err(anyhow!(
            "{} file(s) failed verification.",
            failures.len()
        ))
    }
}

async fn check_file(path: &Path, expected_sha: &str, expected_size: u64) -> Result<()> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("stat {}", path.display()))?;
    if metadata.len() != expected_size {
        anyhow::bail!(
            "size mismatch (got {}, expected {expected_size})",
            metadata.len()
        );
    }

    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let digest = hex::encode(hasher.finalize());
    if digest != expected_sha {
        anyhow::bail!(
            "sha256 mismatch (got {digest}, expected {expected_sha})"
        );
    }
    Ok(())
}
