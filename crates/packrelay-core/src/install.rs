// Install loop — the core of the launcher.
//
// Reusable by both the CLI (with an indicatif progress bar) and the
// Tauri GUI (with frontend event emits). The shared install function
// emits ProgressEvents through a generic callback so the IO layer
// stays UI-agnostic.
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
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::blob_cache;
use crate::client::Client;
use crate::manifest::{FileEntry, Manifest};

/// Progress events emitted during an install run.
///
/// The Tauri GUI serializes these to JSON via `emit()`. The CLI maps
/// them onto an indicatif bar. Same shape for both surfaces.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ProgressEvent {
    /// Sent once at the start, after the manifest is fetched.
    Started {
        display_name: String,
        version: String,
        file_count: u32,
        total_bytes: u64,
    },
    /// Sent each time a chunk of bytes lands on disk. The delta is
    /// the chunk size — callers accumulate it themselves.
    Bytes { delta: u64 },
    /// Sent once a single file is fully written + hash-verified.
    FileDone { path: String },
    /// Sent once after every file has been verified and the sidecar
    /// manifest has been written.
    Done {
        file_count: u32,
        total_bytes: u64,
    },
}

/// Summary returned from a successful install. The caller can render
/// it however it likes — the CLI prints it; the GUI hands it back to
/// the React frontend.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    pub display_name: String,
    pub version: String,
    pub file_count: u32,
    pub total_bytes: u64,
    pub dest: String,
}

/// Optional cache + profile-mirror plumbing for install/update/
/// repair. When set, files land in the cache (deduplicated by
/// sha256) and are also linked into the active profile's mods/
/// directory so a future profile switch can capture the install
/// without a slow byte-by-byte copy. When unset, behaviour
/// matches the original install — write to dest, no cache, no
/// mirror.
#[derive(Clone, Debug, Default)]
pub struct InstallContext {
    pub cache_root: Option<PathBuf>,
    pub profile_mods: Option<PathBuf>,
}

/// Install a pack into `dest`. Spawns up to `concurrency` parallel
/// downloads from the cloud's per-file endpoint, hash-verifies each
/// against the manifest, and writes a sidecar manifest at the end.
///
/// `ctx` opts the install into the content-addressed cache and the
/// active profile's mods mirror. Both fields are independent —
/// either can be set without the other. Default-empty `ctx`
/// reproduces the pre-Phase-4 install loop exactly.
///
/// `on_progress` is invoked from worker threads with each progress
/// event — keep it cheap and use atomics/channels if you need to
/// share state with the UI thread.
pub async fn install<F>(
    client: &Client,
    slug: &str,
    dest: &Path,
    concurrency: usize,
    target_version: Option<&str>,
    ctx: InstallContext,
    on_progress: F,
) -> Result<InstallReport>
where
    F: Fn(ProgressEvent) + Send + Sync + 'static,
{
    // Server pin-aware: when target_version is set, fetch THAT
    // manifest, not whatever the publisher's latest is. fetch_manifest_at
    // also asserts the returned manifest's version field matches the
    // request, so a cloud-side bug can't sneak the wrong version past us.
    let (manifest_raw, manifest) =
        client.fetch_manifest_at(slug, target_version).await?;

    fs::create_dir_all(dest)
        .await
        .with_context(|| format!("creating dest {}", dest.display()))?;
    if let Some(profile) = &ctx.profile_mods {
        fs::create_dir_all(profile)
            .await
            .with_context(|| format!("creating {}", profile.display()))?;
    }

    let total_bytes = manifest_total_bytes(&manifest);
    let file_count = manifest.files.len() as u32;

    let on_progress = Arc::new(on_progress);

    on_progress(ProgressEvent::Started {
        display_name: manifest.display_name.clone(),
        version: manifest.version.clone(),
        file_count,
        total_bytes,
    });

    let http = client.http().clone();
    let dest_arc: Arc<PathBuf> = Arc::new(dest.to_path_buf());
    let ctx_arc = Arc::new(ctx);

    // Pre-collect owned (idx, file) pairs into a Vec so the per-task
    // closures don't borrow from `manifest.files` — that borrow chain
    // wasn't `Send + 'static`-general enough when this function is
    // called from inside a Tauri command (HRTB error). Cloning the
    // file metadata up front is cheap; the file bytes are streamed
    // separately at download time.
    let work: Vec<(usize, FileEntry)> = manifest
        .files
        .iter()
        .cloned()
        .enumerate()
        .collect();

    let results: Vec<Result<()>> = stream::iter(work.into_iter().map(|(idx, file)| {
        let http = http.clone();
        let dest = dest_arc.clone();
        let url = client.file_url(&file.sha256);
        let progress = on_progress.clone();
        let ctx = ctx_arc.clone();
        async move {
            download_and_verify(&http, &url, &file, dest.as_ref(), &ctx, &*progress)
                .await
                .with_context(|| format!("file #{} ({})", idx + 1, file.path))
        }
    }))
    .buffer_unordered(concurrency)
    .collect()
    .await;

    let failures: Vec<_> = results.into_iter().filter_map(|r| r.err()).collect();
    if !failures.is_empty() {
        // Bubble up the first failure (carries the rest in context)
        // but log all of them — flaky networks often hit multiple
        // files in a row.
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
    // Mirror the sidecar into the active profile too so a future
    // switch-away captures install metadata, not just file bytes.
    if let Some(profile) = &ctx_arc.profile_mods {
        let profile_sidecar = profile.join("_packrelay-manifest.json");
        fs::write(&profile_sidecar, manifest_raw.as_bytes())
            .await
            .with_context(|| format!("writing {}", profile_sidecar.display()))?;
    }

    on_progress(ProgressEvent::Done {
        file_count,
        total_bytes,
    });

    Ok(InstallReport {
        display_name: manifest.display_name,
        version: manifest.version,
        file_count,
        total_bytes,
        dest: dest.display().to_string(),
    })
}

fn manifest_total_bytes(m: &Manifest) -> u64 {
    m.files.iter().map(|f| f.size).sum()
}

// pub(crate) so the verify::repair() helper can refetch a single
// file using the exact same streaming-hash-verified write path the
// initial install loop uses. Keeping it crate-private — external
// callers should drive installs/repairs through the high-level
// `install()` / `repair()` entry points.
//
// When `ctx.cache_root` is set, this function will:
//   - Check the cache first; on hit, link from cache → dest and
//     skip the network entirely. Emits one Bytes progress event
//     covering the whole file so the bar moves.
//   - On miss, download as usual, then promote the verified file
//     into the cache. Both dest and cache end up as hardlinks to
//     the same inode on the same volume.
//
// When `ctx.profile_mods` is set, the same file is also linked
// (from cache when possible, else copied from dest) into the
// active profile's mods/ tree so switch-away captures the install.
pub(crate) async fn download_and_verify<F>(
    http: &reqwest::Client,
    url: &str,
    file: &FileEntry,
    dest: &Path,
    ctx: &InstallContext,
    on_progress: &F,
) -> Result<()>
where
    F: Fn(ProgressEvent) + Send + Sync + ?Sized,
{
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
    let profile_target = ctx
        .profile_mods
        .as_ref()
        .map(|root| root.join(&normalized));

    // Fast path: cache already has this blob. Link it into dest +
    // profile mirror without touching the network.
    if let Some(cache) = ctx.cache_root.as_deref() {
        if blob_cache::has_blob(cache, &file.sha256).await {
            blob_cache::link_into(cache, &file.sha256, &target)
                .await
                .with_context(|| format!("linking cached blob for {}", file.path))?;
            if let Some(ptarget) = &profile_target {
                blob_cache::link_into(cache, &file.sha256, ptarget)
                    .await
                    .with_context(|| {
                        format!("mirroring cached blob to profile for {}", file.path)
                    })?;
            }
            // Emit the whole file's worth of bytes in one event so
            // the progress bar moves; the consumer doesn't need to
            // know the file came from cache.
            on_progress(ProgressEvent::Bytes { delta: file.size });
            on_progress(ProgressEvent::FileDone {
                path: file.path.clone(),
            });
            return Ok(());
        }
    }

    // Slow path: download + hash + write in one streaming pass.
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
        on_progress(ProgressEvent::Bytes {
            delta: chunk.len() as u64,
        });
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

    // Promote the just-written file into the cache (turns dest
    // into a hardlink to the canonical blob). Then mirror to the
    // profile's mods/ if set.
    if let Some(cache) = ctx.cache_root.as_deref() {
        blob_cache::promote_to_cache(cache, &file.sha256, &target)
            .await
            .with_context(|| format!("caching blob for {}", file.path))?;
        if let Some(ptarget) = &profile_target {
            blob_cache::link_into(cache, &file.sha256, ptarget)
                .await
                .with_context(|| {
                    format!("mirroring blob to profile for {}", file.path)
                })?;
        }
    } else if let Some(ptarget) = &profile_target {
        // No cache — fall back to a direct copy into profile.
        if let Some(parent) = ptarget.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(&target, ptarget)
            .await
            .with_context(|| format!("mirroring {} to profile", file.path))?;
    }

    on_progress(ProgressEvent::FileDone {
        path: file.path.clone(),
    });

    Ok(())
}
