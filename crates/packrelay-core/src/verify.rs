// Verify (and repair) subcommand — re-check an already-installed pack
// against the sidecar manifest we wrote at install time, and
// optionally refetch any files that have drifted.
//
// Useful for: detecting bit-rot, confirming a manual edit didn't
// silently corrupt a file, and as a smoke test that the launcher's
// install loop produces consistent state on disk.
//
// Two public entry points:
//   verify(dest)                → structured VerifyReport, no stdout.
//                                  Used by the Tauri GUI.
//   run(dest)                   → CLI-style: prints to stdout/stderr,
//                                  returns anyhow::Err on any failure.
//   repair(client, dest)        → verifies, then re-downloads any
//                                  failed files via packrelay-core's
//                                  install::download_and_verify.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::client::Client;
use crate::install::{download_and_verify, InstallContext};
use crate::manifest::{FileEntry, Manifest};

/// Per-file verification failure mode. Tauri serializes these out to
/// the React frontend so the UI can render different copy for
/// missing vs corrupt files without re-parsing the reason string.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum VerifyFailure {
    /// File doesn't exist on disk at all. Probably deleted manually
    /// or never finished writing.
    Missing { path: String },
    /// File exists but doesn't match the manifest (wrong size or
    /// wrong sha256). `reason` is a human-readable summary.
    Corrupt { path: String, reason: String },
}

impl VerifyFailure {
    pub fn path(&self) -> &str {
        match self {
            Self::Missing { path } | Self::Corrupt { path, .. } => path,
        }
    }
}

/// Result of a verify run. `failures` is empty when the pack is
/// healthy. Always includes the pack's display name + version so the
/// UI can label which pack the report belongs to.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReport {
    pub display_name: String,
    pub version: String,
    pub total_files: u32,
    pub failures: Vec<VerifyFailure>,
}

/// Result of a repair run — how many files were refetched. If the
/// pack was already healthy this is just `files_repaired: 0`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairReport {
    pub display_name: String,
    pub version: String,
    pub files_repaired: u32,
}

/// Verify a pack on disk against its sidecar manifest. Returns a
/// structured report; never prints. Errors only on conditions that
/// prevent verification entirely (missing sidecar, unparseable
/// manifest) — per-file failures land in `report.failures`.
pub async fn verify(dest: &Path) -> Result<VerifyReport> {
    let manifest = read_sidecar(dest).await?;

    let mut failures = Vec::<VerifyFailure>::new();
    for file in &manifest.files {
        let normalized = file.path.replace('\\', "/");
        let target = dest.join(&normalized);
        match check_file(&target, &file.sha256, file.size).await {
            Ok(()) => {}
            Err(CheckError::Missing) => failures.push(VerifyFailure::Missing {
                path: file.path.clone(),
            }),
            Err(CheckError::Corrupt(reason)) => failures.push(VerifyFailure::Corrupt {
                path: file.path.clone(),
                reason,
            }),
        }
    }

    Ok(VerifyReport {
        display_name: manifest.display_name,
        version: manifest.version,
        total_files: manifest.files.len() as u32,
        failures,
    })
}

/// CLI-flavored wrapper around `verify()`. Prints a status line per
/// file failure, returns Err on any failures. Preserved so the
/// `packrelay verify` subcommand keeps its existing output format.
pub async fn run(dest: &Path) -> Result<()> {
    let report = verify(dest).await?;

    println!(
        "[verify] {} v{} ({} files)",
        report.display_name, report.version, report.total_files
    );

    for fail in &report.failures {
        match fail {
            VerifyFailure::Missing { path } => {
                eprintln!("  MISSING {path}");
            }
            VerifyFailure::Corrupt { path, reason } => {
                eprintln!("  FAIL {path}: {reason}");
            }
        }
    }

    if report.failures.is_empty() {
        println!(
            "[verify] all {} files match the manifest.",
            report.total_files
        );
        Ok(())
    } else {
        Err(anyhow!(
            "{} file(s) failed verification.",
            report.failures.len()
        ))
    }
}

/// Verify, then re-download any files that failed. Each re-download
/// goes through the same hash-verified streaming write the install
/// loop uses, so we never end the run holding a corrupt file.
///
/// `ctx` opts into the blob cache and active-profile mirroring on
/// the same terms as install/update — repair takes any
/// re-downloaded files and tucks them into the cache, where future
/// installs of the same blob skip the network.
///
/// Returns a `RepairReport` summarizing what changed. If the pack
/// was already healthy this is a fast no-op.
pub async fn repair(
    client: &Client,
    dest: &Path,
    ctx: InstallContext,
) -> Result<RepairReport> {
    let manifest = read_sidecar(dest).await?;

    // Identify what needs refetching. We rerun verify rather than
    // taking a pre-existing VerifyReport so the caller doesn't have
    // to keep one alive between calls — and so we always act on
    // current on-disk state.
    let report = verify(dest).await?;
    let failed_paths: std::collections::HashSet<&str> =
        report.failures.iter().map(|f| f.path()).collect();

    let to_repair: Vec<&FileEntry> = manifest
        .files
        .iter()
        .filter(|f| failed_paths.contains(f.path.as_str()))
        .collect();

    if to_repair.is_empty() {
        return Ok(RepairReport {
            display_name: manifest.display_name.clone(),
            version: manifest.version.clone(),
            files_repaired: 0,
        });
    }

    // Sequential repair — repair sets are typically small (1-5
    // files), and serial avoids surprising the user with concurrent
    // overwrites if they happened to open a file mid-repair.
    let http = client.http().clone();
    for entry in &to_repair {
        let url = client.file_url(&entry.sha256);
        download_and_verify(&http, &url, entry, dest, &ctx, &|_| {})
            .await
            .with_context(|| format!("repairing {}", entry.path))?;
    }

    Ok(RepairReport {
        display_name: manifest.display_name,
        version: manifest.version,
        files_repaired: to_repair.len() as u32,
    })
}

async fn read_sidecar(dest: &Path) -> Result<Manifest> {
    let sidecar = dest.join("_packrelay-manifest.json");
    let raw = fs::read_to_string(&sidecar)
        .await
        .with_context(|| format!("reading {}", sidecar.display()))?;
    serde_json::from_str(&raw).with_context(|| "parsing sidecar manifest")
}

/// Cheap disk-presence probe. Does the install actually still exist
/// on disk? Decoupled from `verify()` because the GUI wants this on
/// every ServerDetailView mount (and on Library row render later) —
/// hashing all 332 files of a 5GB pack is far too expensive for that.
///
/// Three-state result:
///  - `present: true` and `missing_samples == 0` — sidecar parses,
///    every sampled file exists. Safe to show "INSTALLED + Connect."
///  - `present: false`, `sampled_files == 0` — sidecar itself is gone.
///    The install was wiped (manual delete, 7DTD reinstall, drive
///    remap). Treat as not-installed.
///  - `present: false`, `missing_samples > 0` — sidecar there but
///    sample files are missing. Partially wiped install; UI should
///    route to re-install rather than letting the user Connect into
///    a broken Mods/ folder.
///
/// Sampling: up to 8 files spread evenly across `manifest.files`.
/// Pure tokio metadata() calls — no bytes read, no hashes computed.
/// A fully-intact install completes the probe in single-digit
/// milliseconds even on a hard-drive system.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceReport {
    /// Overall verdict: true iff the sidecar parses AND every sampled
    /// file is on disk. UI keys off this single bool for the
    /// installed-or-not decision.
    pub present: bool,
    /// Pack name from the sidecar, when we got that far. None means
    /// we couldn't even open the sidecar.
    pub display_name: Option<String>,
    /// Version from the sidecar, when we got that far. None means
    /// we couldn't even open the sidecar.
    pub version: Option<String>,
    /// What the manifest *says* should be on disk.
    pub total_files: u32,
    /// How many files we actually stat'd (cap at 8).
    pub sampled_files: u32,
    /// Of the sampled files, how many came back missing. >0 with
    /// `sampled_files > 0` means "sidecar lied"; the install is
    /// partially wiped.
    pub missing_samples: u32,
}

const PRESENCE_SAMPLE_CAP: usize = 8;

pub async fn presence_check(dest: &Path) -> Result<PresenceReport> {
    let manifest = match read_sidecar(dest).await {
        Ok(m) => m,
        Err(_) => {
            // Sidecar missing or unparseable. The install isn't really
            // there from PackRelay's point of view — even if 7DTD has
            // some random Mods/ folders, we can't claim ownership.
            return Ok(PresenceReport {
                present: false,
                display_name: None,
                version: None,
                total_files: 0,
                sampled_files: 0,
                missing_samples: 0,
            });
        }
    };

    let n = manifest.files.len();
    let sample_count = n.min(PRESENCE_SAMPLE_CAP);
    let mut missing = 0u32;
    // Even spread across the manifest: 0, n/8, 2n/8, ... so we catch
    // "first dir wiped" and "last dir wiped" failure modes with the
    // same probe. Skips when there are zero files (a degenerate but
    // technically-valid pack).
    if sample_count > 0 {
        for i in 0..sample_count {
            let idx = (i * n) / sample_count;
            let f = &manifest.files[idx];
            // Manifest paths use forward slashes; PathBuf.join handles
            // mixed separators on Windows, but normalize anyway so the
            // path that ends up in the error log matches what's in the
            // sidecar verbatim.
            let target = dest.join(f.path.replace('\\', "/"));
            if fs::metadata(&target).await.is_err() {
                missing += 1;
            }
        }
    }

    Ok(PresenceReport {
        present: missing == 0,
        display_name: Some(manifest.display_name),
        version: Some(manifest.version),
        total_files: n as u32,
        sampled_files: sample_count as u32,
        missing_samples: missing,
    })
}

/// Internal: distinguishes "file isn't there" from "file is there
/// but wrong." We can't surface that distinction from a single
/// anyhow::Error without parsing the message, so we use a typed
/// error and let the caller map it onto VerifyFailure.
enum CheckError {
    Missing,
    Corrupt(String),
}

async fn check_file(path: &Path, expected_sha: &str, expected_size: u64) -> Result<(), CheckError> {
    let metadata = match fs::metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(CheckError::Missing),
        Err(e) => {
            return Err(CheckError::Corrupt(format!(
                "stat failed: {e}"
            )))
        }
    };
    if metadata.len() != expected_size {
        return Err(CheckError::Corrupt(format!(
            "size mismatch (got {}, expected {expected_size})",
            metadata.len()
        )));
    }

    let mut file = fs::File::open(path)
        .await
        .map_err(|e| CheckError::Corrupt(format!("open failed: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| CheckError::Corrupt(format!("read failed: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let digest = hex::encode(hasher.finalize());
    if digest != expected_sha {
        return Err(CheckError::Corrupt(format!(
            "sha256 mismatch (got {digest}, expected {expected_sha})"
        )));
    }
    Ok(())
}
