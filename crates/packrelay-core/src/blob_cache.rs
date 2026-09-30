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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
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
pub async fn add_blob_from_file(cache_root: &Path, sha256: &str, source: &Path) -> Result<PathBuf> {
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
pub async fn add_blob_unknown_hash(cache_root: &Path, source: &Path) -> Result<(String, PathBuf)> {
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
pub async fn link_into(cache_root: &Path, sha256: &str, target: &Path) -> Result<()> {
    let src = blob_path(cache_root, sha256);
    if !fs::metadata(&src).await.is_ok() {
        anyhow::bail!("blob {sha256} not in cache at {}", src.display());
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
            fs::copy(&src, target).await.with_context(|| {
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
pub async fn promote_to_cache(cache_root: &Path, sha256: &str, landed_at: &Path) -> Result<()> {
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
            fs::copy(landed_at, &target).await.with_context(|| {
                format!("caching {} → {}", landed_at.display(), target.display())
            })?;
            Ok(())
        }
    }
}

// ---------- GC ----------
//
// We never delete blobs implicitly on uninstall — the deliberate
// pruning step lives here. A blob is "referenced" iff some
// `_packrelay-manifest.json` sidecar lists its sha256 in `files[]`.
// Anything in the cache that no sidecar mentions is reclaimable.
//
// Sidecars live in three places, and GC reads all of them:
//   - `profiles/<id>/packs/<slug>/mods/` — one per installed pack
//     (the v1 multi-pack layout, see profile.rs).
//   - `profiles/<id>/mods/` — the legacy v0 single-pack layout.
//     Migration is lazy and leaves v0 dirs in place if it's
//     interrupted, so a v0 sidecar can still be the only record.
//   - the live 7DTD `Mods/` dir (passed in by the caller). The active
//     pack's files may only exist there — e.g. a pack installed
//     straight into Mods/ before the profile mirror caught up, or an
//     install made with no profile system at all.
//
// Under-counting references is the dangerous direction: a missed
// sidecar makes its blobs look orphaned and the weekly sweep deletes
// them. So unreadable directories abort the walk rather than being
// skipped; only a missing or malformed sidecar file is skipped.
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
    /// RFC3339 timestamp of the most recent successful sweep (manual
    /// or auto). None if the launcher has never swept this cache.
    /// Surfaced in Settings so the user knows the background sweep
    /// is alive even when there's nothing to clean.
    pub last_sweep_at: Option<String>,
}

/// Persisted alongside the cache so the background sweep can gate
/// itself on a minimum interval (don't burn CPU walking the cache
/// on every app launch). Lives at `<store-root>/cache_gc_state.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheGcState {
    pub last_sweep_at: Option<String>,
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
///
/// `live_mods_dirs` are game `Mods/` dirs whose own sidecar also
/// counts as a reference (see the GC section comment above).
///
/// `state_path` is the persisted GC state (last sweep timestamp)
/// used purely to surface "Last cleaned" in the UI — pass `None`
/// if the caller doesn't care.
pub async fn cache_stats(
    cache_root: &Path,
    profiles_dir: &Path,
    live_mods_dirs: &[PathBuf],
    state_path: Option<&Path>,
) -> Result<CacheStats> {
    let referenced = collect_referenced_hashes(profiles_dir, live_mods_dirs).await?;
    let blobs = walk_blobs(cache_root).await?;

    let last_sweep_at = match state_path {
        Some(p) => read_gc_state(p).await.unwrap_or_default().last_sweep_at,
        None => None,
    };

    let mut stats = CacheStats {
        total_blobs: 0,
        total_bytes: 0,
        referenced_blobs: 0,
        unreferenced_blobs: 0,
        reclaimable_bytes: 0,
        last_sweep_at,
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

/// Delete every blob not referenced by some pack's manifest sidecar
/// (in `profiles_dir` or any of `live_mods_dirs`). Idempotent — running it twice in a row second-time
/// returns `{ blobs_removed: 0, bytes_freed: 0 }`.
///
/// We also opportunistically remove now-empty two-char prefix
/// directories so the cache tree doesn't accumulate empty
/// directories forever.
///
/// Writes `last_sweep_at = now` to `state_path` on completion (even
/// if 0 blobs were removed — the user still asked us to sweep, and
/// the background-sweep gate should respect that). Failures to
/// write the state file are non-fatal: we already did the actual
/// work, the worst case is that the next launch sweeps again.
pub async fn gc_cache(
    cache_root: &Path,
    profiles_dir: &Path,
    live_mods_dirs: &[PathBuf],
    state_path: &Path,
) -> Result<GcResult> {
    let referenced = collect_referenced_hashes(profiles_dir, live_mods_dirs).await?;
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

    // Persist last_sweep_at. Best-effort: if the write fails the
    // next launch just sweeps again, which is harmless.
    let _ = write_gc_state(
        state_path,
        &CacheGcState {
            last_sweep_at: Some(now_rfc3339()),
        },
    )
    .await;

    Ok(result)
}

/// Run [`gc_cache`] only if the cache has gone too long without
/// being swept. Returns `Ok(Some(result))` if we ran, `Ok(None)` if
/// we skipped because the last sweep is recent enough. Used by the
/// launcher startup task — "auto-clean once a week" behaviour.
///
/// `min_interval_secs` is the gap required between sweeps (e.g.
/// `7 * 24 * 60 * 60` for weekly).
pub async fn gc_if_due(
    cache_root: &Path,
    profiles_dir: &Path,
    live_mods_dirs: &[PathBuf],
    state_path: &Path,
    min_interval_secs: u64,
) -> Result<Option<GcResult>> {
    let state = read_gc_state(state_path).await.unwrap_or_default();
    let due = match state.last_sweep_at.as_deref() {
        None => true,
        Some(ts) => match parse_rfc3339_to_unix(ts) {
            Some(prev) => now_unix().saturating_sub(prev) >= min_interval_secs,
            None => true, // unparseable timestamp → treat as never-swept
        },
    };
    if !due {
        return Ok(None);
    }
    let result = gc_cache(cache_root, profiles_dir, live_mods_dirs, state_path).await?;
    Ok(Some(result))
}

async fn read_gc_state(state_path: &Path) -> Result<CacheGcState> {
    match fs::read_to_string(state_path).await {
        Ok(s) => Ok(serde_json::from_str(&s).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CacheGcState::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", state_path.display())),
    }
}

async fn write_gc_state(state_path: &Path, state: &CacheGcState) -> Result<()> {
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(state_path, serde_json::to_string_pretty(state)?).await?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_rfc3339() -> String {
    // Mirrors profile.rs::now_rfc3339 — keeping the format consistent
    // across the launcher's state files. Duplicated rather than
    // shared to avoid making profile's tiny helper module pub.
    let secs = now_unix();
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Inverse of `now_rfc3339` for the subset of RFC3339 we actually
/// emit (`YYYY-MM-DDTHH:MM:SSZ`). Returns None on anything weirder
/// so the caller falls back to "never swept".
fn parse_rfc3339_to_unix(s: &str) -> Option<u64> {
    // Bail fast on the wrong shape — we control the producer, so
    // we don't try to be lenient about timezones or sub-second
    // precision.
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    let h: u64 = s.get(11..13)?.parse().ok()?;
    let mi: u64 = s.get(14..16)?.parse().ok()?;
    let se: u64 = s.get(17..19)?.parse().ok()?;
    // Days from civil (Hinnant inverse).
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = (y_adj - era * 400) as u64;
    let m = mo as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86400 + h * 3600 + mi * 60 + se)
}

fn unix_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64 + era * 400) as u32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

const SIDECAR_NAME: &str = "_packrelay-manifest.json";

/// Walk every sidecar GC knows about and gather the SHA-256s of every
/// file any of them claims to own:
///   - `<profiles_dir>/<id>/packs/<slug>/mods/` (v1, one per pack)
///   - `<profiles_dir>/<id>/mods/` (legacy v0, pre-migration)
///   - each of `live_mods_dirs` (the game's live `Mods/`)
///
/// Missing/malformed sidecar files are skipped — a corrupted profile
/// shouldn't be able to wedge the GC. Directory read errors (other
/// than "not there") propagate instead, because silently skipping a
/// dir would make its blobs look orphaned and get them deleted.
async fn collect_referenced_hashes(
    profiles_dir: &Path,
    live_mods_dirs: &[PathBuf],
) -> Result<HashSet<String>> {
    let mut set = HashSet::new();

    for dir in live_mods_dirs {
        add_sidecar_hashes(&dir.join(SIDECAR_NAME), &mut set).await;
    }

    if fs::metadata(profiles_dir).await.is_err() {
        return Ok(set);
    }
    let mut rd = fs::read_dir(profiles_dir)
        .await
        .with_context(|| format!("reading {}", profiles_dir.display()))?;
    while let Some(profile_entry) = rd.next_entry().await? {
        if !profile_entry.file_type().await?.is_dir() {
            continue;
        }
        let profile_root = profile_entry.path();

        // Legacy v0 layout: sidecar directly in <profile>/mods/.
        add_sidecar_hashes(&profile_root.join("mods").join(SIDECAR_NAME), &mut set).await;

        // v1 layout: one sidecar per pack.
        let packs_root = profile_root.join("packs");
        let mut packs = match fs::read_dir(&packs_root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", packs_root.display()));
            }
        };
        while let Some(pack_entry) = packs.next_entry().await? {
            if !pack_entry.file_type().await?.is_dir() {
                continue;
            }
            let sidecar = pack_entry.path().join("mods").join(SIDECAR_NAME);
            add_sidecar_hashes(&sidecar, &mut set).await;
        }
    }

    Ok(set)
}

/// Add every file hash listed in one sidecar to `set`. A missing or
/// malformed sidecar adds nothing.
async fn add_sidecar_hashes(sidecar: &Path, set: &mut HashSet<String>) {
    let Ok(raw) = fs::read_to_string(sidecar).await else {
        return;
    };
    let Ok(manifest) = serde_json::from_str::<Manifest>(&raw) else {
        return;
    };
    for f in manifest.files {
        // Normalize to lowercase so a sidecar that happened to
        // serialize uppercase hex doesn't slip through.
        set.insert(f.sha256.to_lowercase());
    }
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
            let size = blob_entry.metadata().await.map(|m| m.len()).unwrap_or(0);
            out.push((hash, size, blob_entry.path()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::StoreLayout;

    const SHARED: &str = "aa11111111111111111111111111111111111111111111111111111111111111";
    const ONLY_A: &str = "bb22222222222222222222222222222222222222222222222222222222222222";
    const ORPHAN: &str = "cc33333333333333333333333333333333333333333333333333333333333333";

    fn temp_store(name: &str) -> StoreLayout {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "packrelay-gc-{name}-{}-{nanos}",
            std::process::id()
        ));
        StoreLayout::new(&root)
    }

    async fn put_blob(layout: &StoreLayout, hash: &str) {
        let p = blob_path(&layout.cache_dir(), hash);
        fs::create_dir_all(p.parent().unwrap()).await.unwrap();
        fs::write(&p, hash.as_bytes()).await.unwrap();
    }

    /// Write a sidecar listing `hashes` into `mods_dir`.
    async fn put_sidecar(mods_dir: &Path, hashes: &[&str]) {
        let files: Vec<serde_json::Value> = hashes
            .iter()
            .enumerate()
            .map(|(i, h)| serde_json::json!({ "path": format!("Mod{i}/file"), "sha256": h, "size": 64 }))
            .collect();
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "name": "pack",
            "displayName": "Pack",
            "version": "1.0.0",
            "game": "7dtd",
            "gameVersion": "1.0",
            "publisher": "test",
            "publishedAt": "2026-01-01T00:00:00Z",
            "files": files,
            "signature": { "algo": "ed25519", "publicKeyId": "test/key", "value": "00" },
        });
        fs::create_dir_all(mods_dir).await.unwrap();
        fs::write(mods_dir.join(SIDECAR_NAME), manifest.to_string())
            .await
            .unwrap();
    }

    fn pack_mods(layout: &StoreLayout, profile: &str, slug: &str) -> PathBuf {
        layout
            .profile_dir(profile)
            .join("packs")
            .join(slug)
            .join("mods")
    }

    #[tokio::test]
    async fn only_blobs_no_pack_references_are_reclaimable() {
        let layout = temp_store("two-packs");
        for h in [SHARED, ONLY_A, ORPHAN] {
            put_blob(&layout, h).await;
        }
        // Two packs in the v1 layout, sharing one blob.
        put_sidecar(&pack_mods(&layout, "p1", "a"), &[SHARED, ONLY_A]).await;
        put_sidecar(&pack_mods(&layout, "p1", "b"), &[SHARED]).await;

        let stats = cache_stats(&layout.cache_dir(), &layout.profiles_dir(), &[], None)
            .await
            .unwrap();
        assert_eq!(stats.total_blobs, 3);
        assert_eq!(stats.referenced_blobs, 2);
        assert_eq!(stats.unreferenced_blobs, 1);
        assert_eq!(stats.reclaimable_bytes, ORPHAN.len() as u64);

        let r = gc_cache(
            &layout.cache_dir(),
            &layout.profiles_dir(),
            &[],
            &layout.cache_gc_state_path(),
        )
        .await
        .unwrap();
        assert_eq!(r.blobs_removed, 1);
        assert!(has_blob(&layout.cache_dir(), SHARED).await);
        assert!(has_blob(&layout.cache_dir(), ONLY_A).await);
        assert!(!has_blob(&layout.cache_dir(), ORPHAN).await);

        let _ = fs::remove_dir_all(&layout.root).await;
    }

    #[tokio::test]
    async fn legacy_profile_and_live_mods_sidecars_still_count() {
        let layout = temp_store("legacy-live");
        for h in [SHARED, ONLY_A, ORPHAN] {
            put_blob(&layout, h).await;
        }
        // An unmigrated v0 profile: sidecar straight under <profile>/mods/.
        put_sidecar(&layout.profile_dir("old").join("mods"), &[SHARED]).await;
        // The active pack's sidecar only in the game's live Mods/.
        let live = layout.root.join("7DaysToDie").join("Mods");
        put_sidecar(&live, &[ONLY_A]).await;

        let stats = cache_stats(
            &layout.cache_dir(),
            &layout.profiles_dir(),
            std::slice::from_ref(&live),
            None,
        )
        .await
        .unwrap();
        assert_eq!(stats.referenced_blobs, 2);
        assert_eq!(stats.unreferenced_blobs, 1);

        gc_cache(
            &layout.cache_dir(),
            &layout.profiles_dir(),
            std::slice::from_ref(&live),
            &layout.cache_gc_state_path(),
        )
        .await
        .unwrap();
        assert!(has_blob(&layout.cache_dir(), SHARED).await);
        assert!(has_blob(&layout.cache_dir(), ONLY_A).await);
        assert!(!has_blob(&layout.cache_dir(), ORPHAN).await);

        let _ = fs::remove_dir_all(&layout.root).await;
    }
}
