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
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::manifest::Manifest;

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

// ---------- GC ----------
//
// We never delete blobs implicitly on uninstall — the deliberate
// pruning step lives here. A blob is "referenced" iff some profile's
// `_packrelay-manifest.json` sidecar lists its sha256 in `files[]`.
// Anything in the cache that no sidecar mentions is reclaimable.
//
// We DO leak some references on purpose: the live 7DTD `Mods/` dir
// is hardlinked to the active profile's manifest, so its blobs are
// already covered by the active profile's sidecar. We don't need to
// double-count by walking the live dir too.
//
// Hardlink-aware deletion: removing the cache-side hardlink to a
// blob doesn't delete the bytes if a profile's `mods/<file>` is
// also a hardlink to the same inode. The OS reclaims the bytes only
// when the last link is gone. So the byte count we report as
// "freed" by this GC is the cache-side hardlink, which on the same
// volume is the same number of bytes the user perceives as freed
// (the disk shows the file's size once for every link, summed; the
// duplication is illusory). On cross-volume installs the cache and
// the profile each hold an independent copy, and freeing the cache
// side really does return those bytes.

/// Snapshot of the cache contents alongside how much of it is
/// reclaimable. Returned by [`cache_stats`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub total_blobs: u64,
    pub total_bytes: u64,
    pub referenced_blobs: u64,
    pub unreferenced_blobs: u64,
    pub reclaimable_bytes: u64,
}

/// What a GC pass actually removed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GcResult {
    pub blobs_removed: u64,
    pub bytes_freed: u64,
}

/// Compute cache stats without modifying anything. Cheap dry-run
/// for the Settings page so the user can see "X MB reclaimable"
/// before clicking the button.
pub async fn cache_stats(
    cache_root: &Path,
    profiles_dir: &Path,
) -> Result<CacheStats> {
    let referenced = collect_referenced_hashes(profiles_dir).await?;
    let blobs = walk_blobs(cache_root).await?;

    let mut stats = CacheStats {
        total_blobs: 0,
        total_bytes: 0,
        referenced_blobs: 0,
        unreferenced_blobs: 0,
        reclaimable_bytes: 0,
    };
    for (hash, size, _path) in &blobs {
        stats.total_blobs += 1;
        stats.total_bytes += *size;
        if referenced.contains(hash) {
            stats.referenced_blobs += 1;
        } else {
            stats.unreferenced_blobs += 1;
            stats.reclaimable_bytes += *size;
        }
    }
    Ok(stats)
}

/// Delete every blob not referenced by some profile's manifest
/// sidecar. Idempotent — running it twice in a row second-time
/// returns `{ blobs_removed: 0, bytes_freed: 0 }`.
///
/// We also opportunistically remove now-empty two-char prefix
/// directories so the cache tree doesn't accumulate empty
/// directories forever.
pub async fn gc_cache(
    cache_root: &Path,
    profiles_dir: &Path,
) -> Result<GcResult> {
    let referenced = collect_referenced_hashes(profiles_dir).await?;
    let blobs = walk_blobs(cache_root).await?;

    let mut result = GcResult {
        blobs_removed: 0,
        bytes_freed: 0,
    };
    for (hash, size, path) in &blobs {
        if referenced.contains(hash) {
            continue;
        }
        // Best-effort delete: if removal fails (file in use on
        // Windows because the user is currently switching profiles,
        // antivirus has it locked, etc.) we skip and let the next
        // sweep catch it. We don't want a single stubborn blob to
        // abort the whole GC.
        if fs::remove_file(path).await.is_ok() {
            result.blobs_removed += 1;
            result.bytes_freed += *size;
        }
    }

    // Sweep empty prefix dirs. There are at most 256 of them (00..ff)
    // so this stays cheap.
    if fs::metadata(cache_root).await.is_ok() {
        let mut rd = fs::read_dir(cache_root).await?;
        while let Some(entry) = rd.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let dir = entry.path();
            let mut inner = fs::read_dir(&dir).await?;
            if inner.next_entry().await?.is_none() {
                // Empty — try to remove. Ignore errors (concurrent
                // install might have just landed a blob here).
                let _ = fs::remove_dir(&dir).await;
            }
        }
    }

    Ok(result)
}

/// Walk all profile sidecars and gather the SHA-256s of every file
/// any of them claims to own. Missing/unreadable sidecars are
/// skipped — a corrupted profile shouldn't be able to wedge the GC.
async fn collect_referenced_hashes(profiles_dir: &Path) -> Result<HashSet<String>> {
    let mut set = HashSet::new();
    if !fs::metadata(profiles_dir).await.is_ok() {
        return Ok(set);
    }

    let mut rd = fs::read_dir(profiles_dir).await?;
    while let Some(profile_entry) = rd.next_entry().await? {
        if !profile_entry.file_type().await?.is_dir() {
            continue;
        }
        let profile_root = profile_entry.path();
        // Sidecar lives in <profile>/mods/_packrelay-manifest.json,
        // which mirrors the same sidecar installed into 7DTD's
        // Mods/ dir. The mods/ layer matters: 7DTD only loads
        // anything under Mods/, so that's where install writes.
        let sidecar = profile_root.join("mods").join("_packrelay-manifest.json");
        let raw = match fs::read_to_string(&sidecar).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let manifest: Manifest = match serde_json::from_str(&raw) {
            Ok(m) => m,
            Err(_) => continue, // skip malformed
        };
        for f in manifest.files {
            // Normalize to lowercase so a sidecar that happened to
            // serialize uppercase hex doesn't slip through.
            set.insert(f.sha256.to_lowercase());
        }
    }

    Ok(set)
}

/// Enumerate every blob in the cache as `(hash, size_bytes, path)`.
/// Walks the two-char prefix fan-out structure produced by
/// [`blob_path`].
async fn walk_blobs(cache_root: &Path) -> Result<Vec<(String, u64, PathBuf)>> {
    let mut out = Vec::new();
    if !fs::metadata(cache_root).await.is_ok() {
        return Ok(out);
    }
    let mut rd = fs::read_dir(cache_root).await?;
    while let Some(prefix_entry) = rd.next_entry().await? {
        if !prefix_entry.file_type().await?.is_dir() {
            continue;
        }
        let prefix_name = prefix_entry.file_name().to_string_lossy().to_string();
        if prefix_name.len() != 2 {
            continue; // not a blob prefix dir
        }
        let mut inner = fs::read_dir(prefix_entry.path()).await?;
        while let Some(blob_entry) = inner.next_entry().await? {
            let ft = blob_entry.file_type().await?;
            if !ft.is_file() {
                continue;
            }
            let rest = blob_entry.file_name().to_string_lossy().to_string();
            let hash = format!("{prefix_name}{rest}").to_lowercase();
            let size = blob_entry
                .metadata()
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            out.push((hash, size, blob_entry.path()));
        }
    }
    Ok(out)
}
