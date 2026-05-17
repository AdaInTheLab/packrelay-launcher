// Update — the smart half of "reinstall". Diffs the installed
// sidecar manifest against the new manifest from the catalog and
// only refetches files that changed (or were added), deletes files
// that no longer exist in the new version, and skips the rest.
//
// Trust model: we trust the old sidecar's record of what we put on
// disk. If the user has manually edited a file, the diff won't
// re-download it (the manifest entries match), and the edit
// survives. If they want byte-exact integrity, verify::repair is
// the right tool — update is the fast path for the "new version
// landed" case.
//
// Removed-file deletes are best-effort: same rationale as the
// uninstall module — we'd rather complete the update than abort
// because one stale .dll is locked by 7DTD.

use anyhow::{anyhow, Context, Result};
use futures::stream::{self, StreamExt};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;

use crate::client::Client;
use crate::install::{download_and_verify, InstallContext, ProgressEvent};
use crate::manifest::{FileEntry, Manifest};

/// Summary returned by `update()`. Mirrors `InstallReport` but adds
/// the breakdown the user actually cares about — how many files we
/// touched vs. left alone.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReport {
    pub display_name: String,
    pub from_version: String,
    pub to_version: String,
    pub files_added: u32,
    pub files_changed: u32,
    pub files_removed: u32,
    pub files_kept: u32,
    pub bytes_downloaded: u64,
    pub dest: String,
}

/// Smart update: diff the installed manifest against the new one,
/// download only what changed, delete obsolete files, and rewrite
/// the sidecar so the next update has a fresh baseline.
///
/// Errors only when verification of a downloaded file fails or the
/// new manifest can't be fetched; deleting an obsolete file that's
/// locked is non-fatal (the file stays on disk, but the update is
/// still considered to have succeeded — the sidecar now says it
/// shouldn't be there, so a follow-up uninstall will retry).
pub async fn update<F>(
    client: &Client,
    slug: &str,
    dest: &Path,
    concurrency: usize,
    target_version: Option<&str>,
    ctx: InstallContext,
    on_progress: F,
) -> Result<UpdateReport>
where
    F: Fn(ProgressEvent) + Send + Sync + 'static,
{
    let sidecar_path = dest.join("_packrelay-manifest.json");
    let old_raw = fs::read_to_string(&sidecar_path)
        .await
        .with_context(|| format!("reading {}", sidecar_path.display()))?;
    let old_manifest: Manifest =
        serde_json::from_str(&old_raw).with_context(|| "parsing installed sidecar")?;

    // Pin-aware: target_version=Some routes to the pinned manifest
    // endpoint so a server pinned to v0.2.0 doesn't silently get
    // updated to latest v0.4.0 — this is the v0.1.6 bug screenshot
    // upgrade-to-pinned shows in the wild.
    let (new_raw, new_manifest) = client.fetch_manifest_at(slug, target_version).await?;

    // No-op early exit when the catalog matches what's installed.
    // Saves us from emitting a Started/Done pair for zero work.
    if old_manifest.version == new_manifest.version {
        on_progress(ProgressEvent::Started {
            display_name: new_manifest.display_name.clone(),
            version: new_manifest.version.clone(),
            file_count: 0,
            total_bytes: 0,
        });
        on_progress(ProgressEvent::Done {
            file_count: 0,
            total_bytes: 0,
        });
        return Ok(UpdateReport {
            display_name: new_manifest.display_name,
            from_version: old_manifest.version.clone(),
            to_version: new_manifest.version,
            files_added: 0,
            files_changed: 0,
            files_removed: 0,
            files_kept: old_manifest.files.len() as u32,
            bytes_downloaded: 0,
            dest: dest.display().to_string(),
        });
    }

    // Index the old manifest by normalized path so the diff loop
    // is O(n) instead of O(n²). Paths in manifests can use either
    // separator on Windows-published packs; canonicalize before
    // comparing so a slug change in the packer doesn't masquerade
    // as a churn.
    let old_by_path: HashMap<String, &FileEntry> = old_manifest
        .files
        .iter()
        .map(|f| (normalize_path(&f.path), f))
        .collect();
    let new_paths: HashSet<String> = new_manifest
        .files
        .iter()
        .map(|f| normalize_path(&f.path))
        .collect();

    let mut to_download: Vec<FileEntry> = Vec::new();
    let mut files_added: u32 = 0;
    let mut files_changed: u32 = 0;
    let mut files_kept: u32 = 0;
    for file in &new_manifest.files {
        let key = normalize_path(&file.path);
        match old_by_path.get(&key) {
            Some(old) if old.sha256 == file.sha256 => files_kept += 1,
            Some(_) => {
                to_download.push(file.clone());
                files_changed += 1;
            }
            None => {
                to_download.push(file.clone());
                files_added += 1;
            }
        }
    }

    let to_delete: Vec<&FileEntry> = old_manifest
        .files
        .iter()
        .filter(|f| !new_paths.contains(&normalize_path(&f.path)))
        .collect();

    let bytes_to_download: u64 = to_download.iter().map(|f| f.size).sum();

    on_progress(ProgressEvent::Started {
        display_name: new_manifest.display_name.clone(),
        version: new_manifest.version.clone(),
        file_count: to_download.len() as u32,
        total_bytes: bytes_to_download,
    });

    // Parallel downloads, same pattern as install::install — we
    // re-use download_and_verify so byte-streaming + sha256 +
    // sidecar-correct write semantics are identical. The same
    // InstallContext flows through too, so update populates the
    // blob cache + mirrors into the active profile on the same
    // terms as a fresh install.
    if let Some(profile) = &ctx.profile_mods {
        fs::create_dir_all(profile)
            .await
            .with_context(|| format!("creating {}", profile.display()))?;
    }
    let on_progress = Arc::new(on_progress);
    let http = client.http().clone();
    let dest_arc: Arc<PathBuf> = Arc::new(dest.to_path_buf());
    let ctx_arc = Arc::new(ctx);

    let work: Vec<(usize, FileEntry)> = to_download.into_iter().enumerate().collect();
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
        for err in &failures {
            eprintln!("[update]  - {err:#}");
        }
        return Err(anyhow!(
            "{} of {} updated file(s) failed verification.",
            failures.len(),
            files_added + files_changed
        ));
    }

    // Best-effort obsolete-file cleanup. Locked files (7DTD running)
    // are intentionally not fatal here — the new sidecar will mark
    // them as no-longer-tracked, so a follow-up uninstall sees them
    // as "out of scope" and the user can clean up later.
    let mut files_removed: u32 = 0;
    let mut touched_dirs: HashSet<PathBuf> = HashSet::new();
    for old in &to_delete {
        let normalized = old.path.replace('\\', "/");
        let target = dest.join(&normalized);
        if let Some(parent) = target.parent() {
            let mut cursor = parent.to_path_buf();
            while cursor.starts_with(dest) && cursor != dest {
                touched_dirs.insert(cursor.clone());
                match cursor.parent() {
                    Some(p) => cursor = p.to_path_buf(),
                    None => break,
                }
            }
        }
        match fs::remove_file(&target).await {
            Ok(()) => files_removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => files_removed += 1,
            Err(_) => { /* leave it; sidecar will treat it as out-of-scope */ }
        }
        // Mirror the delete into the active profile so a future
        // switch-away doesn't capture an obsolete file. Best-effort.
        if let Some(profile) = &ctx_arc.profile_mods {
            let p_target = profile.join(&normalized);
            let _ = fs::remove_file(&p_target).await;
        }
    }
    let mut dirs: Vec<PathBuf> = touched_dirs.into_iter().collect();
    dirs.sort_by(|a, b| b.components().count().cmp(&a.components().count()));
    for dir in dirs {
        let _ = fs::remove_dir(&dir).await;
    }

    // Write the new sidecar (preserving the server's exact bytes —
    // see the note in install::install for why re-serializing would
    // break a future signature check).
    fs::write(&sidecar_path, new_raw.as_bytes())
        .await
        .with_context(|| format!("writing {}", sidecar_path.display()))?;
    if let Some(profile) = &ctx_arc.profile_mods {
        let p_sidecar = profile.join("_packrelay-manifest.json");
        fs::write(&p_sidecar, new_raw.as_bytes())
            .await
            .with_context(|| format!("writing {}", p_sidecar.display()))?;
    }

    on_progress(ProgressEvent::Done {
        file_count: files_added + files_changed,
        total_bytes: bytes_to_download,
    });

    Ok(UpdateReport {
        display_name: new_manifest.display_name,
        from_version: old_manifest.version,
        to_version: new_manifest.version,
        files_added,
        files_changed,
        files_removed,
        files_kept,
        bytes_downloaded: bytes_to_download,
        dest: dest.display().to_string(),
    })
}

fn normalize_path(p: &str) -> String {
    p.replace('\\', "/")
}
