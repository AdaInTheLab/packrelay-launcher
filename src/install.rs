// Install loop — the core of the launcher.
//
// Mirrors scripts/install-pack.sh but with proper parallelism: a
// bounded buffer_unordered stream pumps `concurrency` file
// downloads in flight at once, each one streaming bytes through the
// SHA-256 hasher and onto disk in a single pass.
//
// On any verification failure (size mismatch, hash mismatch, network
// error) the run aborts with an actionable message. No partial-state
// repair yet — rerun to start fresh. Resume support lands when we
// have an "update existing install" command.

use anyhow::{anyhow, Context, Result};
use futures::stream::{self, StreamExt, TryStreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::client::Client;
use crate::manifest::{FileEntry, Manifest};

pub async fn run(
    client: &Client,
    slug: &str,
    dest: &Path,
    concurrency: usize,
) -> Result<()> {
    println!("[install] fetching manifest for '{slug}'...");
    let (manifest_raw, manifest) = client.fetch_manifest(slug).await?;

    println!(
        "[install] manifest: {} v{} ({} files, {:.1} MB)",
        manifest.display_name,
        manifest.version,
        manifest.files.len(),
        manifest_total_bytes(&manifest) as f64 / (1024.0 * 1024.0),
    );

    fs::create_dir_all(dest)
        .await
        .with_context(|| format!("creating dest {}", dest.display()))?;

    let total_bytes = manifest_total_bytes(&manifest);
    let bar = ProgressBar::new(total_bytes);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] [{wide_bar:.cyan/blue}] \
             {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12} eta {eta:>4}",
        )
        .unwrap()
        .progress_chars("##-"),
    );
    let bar = Arc::new(bar);

    // Parallel download stream. The closure captures small &
    // references; reqwest::Client is Arc-internal so cloning is cheap.
    let http = client.http().clone();
    let dest_arc: Arc<PathBuf> = Arc::new(dest.to_path_buf());

    let results: Vec<Result<()>> = stream::iter(manifest.files.iter().enumerate().map(
        |(idx, file)| {
            let http = http.clone();
            let dest = dest_arc.clone();
            let bar = bar.clone();
            let url = client.file_url(&file.sha256);
            let file = file.clone();
            async move {
                download_and_verify(&http, &url, &file, dest.as_ref(), &bar)
                    .await
                    .with_context(|| format!("file #{} ({})", idx + 1, file.path))
            }
        },
    ))
    .buffer_unordered(concurrency)
    .collect()
    .await;

    bar.finish_and_clear();

    // Surface every failure at once — useful when a flaky network
    // takes out multiple files in a row.
    let failures: Vec<_> = results.into_iter().filter_map(|r| r.err()).collect();
    if !failures.is_empty() {
        for err in &failures {
            eprintln!("[install]  - {err:#}");
        }
        return Err(anyhow!(
            "{} of {} file(s) failed verification.",
            failures.len(),
            manifest.files.len()
        ));
    }

    // Sidecar: preserve the EXACT manifest bytes the server returned
    // so a future signature verifier can re-check against them. A
    // re-serialization through serde would change byte order /
    // whitespace and break that property.
    let sidecar = dest.join("_packrelay-manifest.json");
    fs::write(&sidecar, manifest_raw.as_bytes())
        .await
        .with_context(|| format!("writing {}", sidecar.display()))?;

    println!(
        "[install] verified {} files, {:.1} MB. Installed into {}",
        manifest.files.len(),
        total_bytes as f64 / (1024.0 * 1024.0),
        dest.display()
    );
    Ok(())
}

fn manifest_total_bytes(m: &Manifest) -> u64 {
    m.files.iter().map(|f| f.size).sum()
}

async fn download_and_verify(
    http: &reqwest::Client,
    url: &str,
    file: &FileEntry,
    dest: &Path,
    bar: &ProgressBar,
) -> Result<()> {
    // Belt-and-suspenders against a malformed manifest. The server's
    // Zod schema already rejects absolute paths and ".." segments,
    // but we double-check here so a hypothetical server bug can't
    // write outside dest.
    if file.path.starts_with('/')
        || file.path.starts_with('\\')
        || file
            .path
            .split(['/', '\\'])
            .any(|seg| seg == ".." || seg == ".")
    {
        anyhow::bail!("unsafe path in manifest: {}", file.path);
    }

    let normalized = file.path.replace('\\', "/");
    let target = dest.join(&normalized);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating parent {}", parent.display()))?;
    }

    let res = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !res.status().is_success() {
        anyhow::bail!(
            "download failed for {}: HTTP {}",
            file.path,
            res.status()
        );
    }

    let mut hasher = Sha256::new();
    let mut out = fs::File::create(&target)
        .await
        .with_context(|| format!("creating {}", target.display()))?;

    // Stream bytes through the hasher and out to disk in one pass.
    // Avoids ever materializing the full file in memory — important
    // for the multi-MB texture entries in real packs.
    let mut total: u64 = 0;
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.try_next().await.with_context(|| {
        format!("reading {} from network", file.path)
    })? {
        hasher.update(&chunk);
        out.write_all(&chunk).await?;
        total += chunk.len() as u64;
        bar.inc(chunk.len() as u64);
    }
    out.flush().await?;

    if total != file.size {
        anyhow::bail!(
            "size mismatch for {}: got {}, expected {}",
            file.path,
            total,
            file.size
        );
    }

    let digest = hex::encode(hasher.finalize());
    if digest != file.sha256 {
        anyhow::bail!(
            "sha256 mismatch for {}: got {}, expected {}",
            file.path,
            digest,
            file.sha256
        );
    }

    Ok(())
}

// MultiProgress import kept available for future GUI-bridging usage
// (per-file bars in the eventual Tauri shell). Silenced for now so
// the unused-import lint stays quiet.
#[allow(dead_code)]
fn _multi_progress_anchor() -> MultiProgress {
    MultiProgress::new()
}
