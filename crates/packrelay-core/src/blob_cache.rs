// Content-addressed blob cache.
//
// Every file we install is identified by its SHA-256 — we already
// hash-verify every byte during install, so reusing that hash as the
// cache key is free. The cache sits in the OS app-data dir
// (resolved by the caller; this module is path-agnostic), and any
// number of profile destination folders can refer back to the same
// blob via hard links.
//
// Hard linking is the win: switching a 5GB profile becomes
// metadata-only filesystem ops instead of a 5GB copy. On the same
// volume on Windows/macOS/Linux, hardlinks have effectively zero
// cost and zero extra disk space. Cross-volume we fall back to a
// regular copy and accept the duplication.
//
// We never delete blobs implicitly. Even after a pack is
// uninstalled, the blobs stick around until a deliberate sweep —
// reasoning: switching back to an old version should never have to
// re-download. A future GC pass can prune blobs that no profile
// (or sidecar) references.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncReadExt;

/// Resolve the on-disk location of a blob in the given cache root.
/// Two-level fan-out (`ab/cdef...`) so directories don't accumulate
/// hundreds of thousands of entries — keeps filesystem traversals
/// fast on Windows.
pub fn blob_path(cache_root: &Path, sha256: &str) -> PathBuf {
    let (prefix, rest) = sha256.split_at(2.min(sha256.len()));
    cache_root.join(prefix).join(rest)
}

/// Check whether a blob is already in the cache. Cheap — only stats
/// the file. Used by install to skip the network fetch when an
/// older profile already has the bytes we need.
pub async fn has_blob(cache_root: &Path, sha256: &str) -> bool {
    fs::metadata(blob_path(cache_root, sha256)).await.is_ok()
}

/// Add a file to the cache by copying it in. Used when we have file
/// bytes that don't go through the streaming install path (e.g.
/// importing the user's existing 7DTD state).
///
/// Returns the blob's path in the cache. Idempotent: if the blob is
/// already present we leave it alone.
pub async fn add_blob_from_file(
    cache_root: &Path,
    sha256: &str,
    source: &Path,
) -> Result<PathBuf> {
    let target = blob_path(cache_root, sha256);
    if fs::metadata(&target).await.is_ok() {
        return Ok(target);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::copy(source, &target)
        .await
        .with_context(|| format!("caching {} → {}", source.display(), target.display()))?;
    Ok(target)
}

/// Hash a file and add it to the cache in one shot. Returns the
/// computed SHA-256 alongside the cached path. Use when the caller
/// doesn't know the hash up front (e.g. discovering pre-existing
/// files during "import current state as profile").
pub async fn add_blob_unknown_hash(
    cache_root: &Path,
    source: &Path,
) -> Result<(String, PathBuf)> {
    let mut file = fs::File::open(source)
        .await
        .with_context(|| format!("opening {}", source.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let sha = hex::encode(hasher.finalize());
    let path = add_blob_from_file(cache_root, &sha, source).await?;
    Ok((sha, path))
}

/// Materialize a cached blob into a target path. Prefers hard link
/// (zero-cost, zero-space); falls back to copy when the cache and
/// target are on different volumes (Windows: different drive
/// letters; Linux/macOS: different mount points).
///
/// The target's parent dirs are created if missing. If the target
/// already exists, it's replaced — install/repair/update flows
/// expect overwrite semantics.
pub async fn link_into(
    cache_root: &Path,
    sha256: &str,
    target: &Path,
) -> Result<()> {
    let src = blob_path(cache_root, sha256);
    if !fs::metadata(&src).await.is_ok() {
        anyhow::bail!(
            "blob {sha256} not in cache at {}",
            src.display()
        );
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Remove any prior file at target — both hard_link and copy
    // fail if the destination exists.
    match fs::remove_file(target).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("clearing {}", target.display()));
        }
    }
    // Try hardlink first.
    match fs::hard_link(&src, target).await {
        Ok(()) => Ok(()),
        Err(_) => {
            // Cross-volume, FAT32 (no hard links), or some other
            // restriction. Plain copy is correct; we just lose
            // dedup for this file.
            fs::copy(&src, target)
                .await
                .with_context(|| {
                    format!("fallback-copy {} → {}", src.display(), target.display())
                })?;
            Ok(())
        }
    }
}

/// Take a streaming write that already produced a verified file at
/// `landed_at`, and promote it into the cache by hard-linking the
/// blob into place. Used by the install loop after it finishes
/// writing+verifying a download — turns the in-place file into a
/// cache-backed one without re-reading the bytes.
///
/// If the cache already has this blob (e.g. another profile
/// previously installed the same file), we delete the just-written
/// duplicate at `landed_at` and re-link from the canonical cache
/// copy, ensuring the dest is always the hardlink (not the
/// standalone copy).
pub async fn promote_to_cache(
    cache_root: &Path,
    sha256: &str,
    landed_at: &Path,
) -> Result<()> {
    let target = blob_path(cache_root, sha256);
    if fs::metadata(&target).await.is_ok() {
        // Already cached. Replace landed_at with a hardlink to the
        // canonical blob so future profile snapshots see a linked
        // file, not an independent copy.
        let _ = fs::remove_file(landed_at).await;
        link_into(cache_root, sha256, landed_at).await?;
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Try to hardlink the landed file INTO the cache. This is
    // zero-cost on the same volume — both paths just point at the
    // same inode afterwards.
    match fs::hard_link(landed_at, &target).await {
        Ok(()) => Ok(()),
        Err(_) => {
            // Cross-volume: copy into cache. landed_at stays as its
            // own standalone file. Less efficient but correct.
            fs::copy(landed_at, &target)
                .await
                .with_context(|| {
                    format!("caching {} → {}", landed_at.display(), target.display())
                })?;
            Ok(())
        }
    }
}
