// Uninstall — the inverse of install. Reads the sidecar manifest
// we wrote at install time, deletes every file the manifest
// declares, removes the sidecar itself, and then sweeps empty
// directories under dest so the launcher doesn't leave behind a
// skeleton of folders.
//
// We intentionally only delete files the manifest knows about. The
// dest is typically a shared Mods/ folder containing other packs
// or hand-installed mods — wiping dest itself or anything we
// didn't write would be a footgun.
//
// Per-file delete failures are non-fatal and collected into
// `UninstallReport.files_failed`. A pack with one read-only file
// shouldn't block removal of the other 99.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::manifest::Manifest;

/// Summary returned by `uninstall()`. `files_failed` is empty on a
/// clean run; entries here mean the file is still on disk and the
/// user may need to close 7DTD / remove a read-only flag and retry.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallReport {
    pub display_name: String,
    pub version: String,
    pub files_removed: u32,
    pub files_failed: Vec<UninstallFailure>,
    pub sidecar_removed: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UninstallFailure {
    pub path: String,
    pub reason: String,
}

/// Uninstall a pack from `dest`. Returns Err only if we can't load
/// the sidecar manifest in the first place (we don't know what to
/// delete). Per-file failures land in `report.files_failed` so the
/// caller can surface them without aborting mid-sweep.
pub async fn uninstall(dest: &Path) -> Result<UninstallReport> {
    let sidecar = dest.join("_packrelay-manifest.json");
    let raw = fs::read_to_string(&sidecar)
        .await
        .with_context(|| format!("reading {}", sidecar.display()))?;
    let manifest: Manifest =
        serde_json::from_str(&raw).with_context(|| "parsing sidecar manifest")?;

    let display_name = manifest.display_name.clone();
    let version = manifest.version.clone();
    let total = manifest.files.len();

    let mut files_removed: u32 = 0;
    let mut files_failed = Vec::<UninstallFailure>::new();

    // Track every directory we touch so we can sweep empty ones
    // after deleting files. We pop the leaves first, so deepest-
    // path-first ordering gives a single bottom-up pass.
    let mut touched_dirs: HashSet<PathBuf> = HashSet::new();

    for file in &manifest.files {
        let normalized = file.path.replace('\\', "/");
        let target = dest.join(&normalized);

        if let Some(parent) = target.parent() {
            // Only record directories *inside* dest. We never want
            // to consider dest itself for pruning.
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone — treat as a successful removal so
                // the count matches user expectations ("the pack is
                // no longer on disk").
                files_removed += 1;
            }
            Err(e) => files_failed.push(UninstallFailure {
                path: file.path.clone(),
                reason: format!("{e}"),
            }),
        }
    }

    // Remove the sidecar last. If file deletes mostly succeeded we
    // still want the manifest gone so the launcher doesn't think
    // there's a pack here. If they all failed (read-only volume,
    // say), leaving the sidecar lets the user retry uninstall.
    let sidecar_removed = if files_failed.len() == total {
        false
    } else {
        match fs::remove_file(&sidecar).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    };

    // Prune empty directories the pack created. Sort deepest-first
    // so we never try to delete a parent before its children.
    let mut dirs: Vec<PathBuf> = touched_dirs.into_iter().collect();
    dirs.sort_by(|a, b| b.components().count().cmp(&a.components().count()));
    for dir in dirs {
        // remove_dir only succeeds when the dir is empty — that's
        // exactly the behavior we want. Anything else (NotFound,
        // NotEmpty, permission) is silently ignored: we never
        // delete a non-empty directory another pack might own.
        let _ = fs::remove_dir(&dir).await;
    }

    Ok(UninstallReport {
        display_name,
        version,
        files_removed,
        files_failed,
        sidecar_removed,
    })
}
