// A game's mod loader, installed into the game before its pack
// (multi-game; PackRelayCloud docs/multi-game/DESIGN.md §7, decision 3).
//
// A v3 manifest names its framework (`bepinexpack-valheim` at some
// version) inside the signed bytes, so the publisher vouches for WHICH
// loader. The cloud serves that loader's files, re-hosted from its
// Thunderstore package, at /api/v1/frameworks/<id>/<version>; each is
// fetched by hash from /api/v1/files like a pack file and checked
// against it before it's written.
//
// Files go into the game root (winhttp.dll, doorstop_config.ini,
// BepInEx/core/...). A file already there with the right hash is left
// alone, and so is anything under BepInEx/config/ that already exists:
// that's the pack's (and the player's) config, which profile swaps own.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::client::Client;
use crate::manifest::Framework;

/// Where the installed framework is recorded, relative to the game root.
pub const FRAMEWORK_MARKER: &str = "BepInEx/.packrelay-framework.json";

/// Framework files a player's install may already have edited: written
/// only when missing.
const KEEP_IF_PRESENT: &[&str] = &["BepInEx/config/"];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameworkFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameworkBuild {
    id: String,
    version: String,
    files: Vec<FrameworkFile>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FrameworkReport {
    /// Files downloaded and written.
    pub written: Vec<String>,
    /// Files already in place (right hash, or config the player has).
    pub kept: usize,
    /// True when the marker already named this exact framework version
    /// and every file checked out, so nothing was fetched.
    pub already_installed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Marker {
    id: String,
    version: String,
}

/// Make sure `framework` is installed in `game_root`.
pub async fn ensure_framework(
    client: &Client,
    game_root: &Path,
    framework: &Framework,
) -> Result<FrameworkReport> {
    let build: FrameworkBuild = {
        let url = client.framework_url(&framework.id, &framework.version);
        let res = client
            .http()
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !res.status().is_success() {
            bail!(
                "couldn't get {} {} (HTTP {})",
                framework.id,
                framework.version,
                res.status()
            );
        }
        res.json()
            .await
            .context("parsing the framework's file list")?
    };
    if build.id != framework.id || build.version != framework.version {
        bail!(
            "asked for {} {} but the cloud answered with {} {}",
            framework.id,
            framework.version,
            build.id,
            build.version
        );
    }

    let mut report = FrameworkReport::default();
    for file in &build.files {
        let target = safe_join(game_root, &file.path)?;
        let exists = fs::metadata(&target).await.is_ok();
        if exists && KEEP_IF_PRESENT.iter().any(|p| file.path.starts_with(p)) {
            report.kept += 1;
            continue;
        }
        if exists && sha256_of(&target).await.ok().as_deref() == Some(file.sha256.as_str()) {
            report.kept += 1;
            continue;
        }
        download_verified(client, file, &target).await?;
        report.written.push(file.path.clone());
    }

    let marker = Marker {
        id: framework.id.clone(),
        version: framework.version.clone(),
    };
    let marker_path = game_root.join(FRAMEWORK_MARKER);
    let previous = fs::read_to_string(&marker_path)
        .await
        .ok()
        .and_then(|raw| serde_json::from_str::<Marker>(&raw).ok());
    report.already_installed = report.written.is_empty() && previous.as_ref() == Some(&marker);
    if let Some(parent) = marker_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(&marker_path, serde_json::to_string_pretty(&marker)?).await?;
    Ok(report)
}

/// The framework the marker says is installed, if any.
pub async fn installed_framework(game_root: &Path) -> Option<Framework> {
    let raw = fs::read_to_string(game_root.join(FRAMEWORK_MARKER))
        .await
        .ok()?;
    let m: Marker = serde_json::from_str(&raw).ok()?;
    Some(Framework {
        id: m.id,
        version: m.version,
    })
}

/// `root/rel`, refusing absolute paths and `.`/`..` segments: the file
/// list is the cloud's, and nothing it says may land outside the game.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || rel.contains(':')
        || rel
            .split(['/', '\\'])
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        bail!("unsafe path in framework: {rel}");
    }
    Ok(root.join(rel.replace('\\', "/")))
}

async fn sha256_of(path: &Path) -> Result<String> {
    let bytes = fs::read(path).await?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Download one file by hash, check it, then move it into place, so a
/// bad download never replaces a good file.
async fn download_verified(client: &Client, file: &FrameworkFile, target: &Path) -> Result<()> {
    let url = client.file_url(&file.sha256);
    let res = client
        .http()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !res.status().is_success() {
        bail!("download failed for {}: HTTP {}", file.path, res.status());
    }
    let bytes = res
        .bytes()
        .await
        .with_context(|| format!("reading {}", file.path))?;
    let got = hex::encode(Sha256::digest(&bytes));
    if got != file.sha256 || bytes.len() as u64 != file.size {
        bail!("{} didn't match its hash; nothing was written", file.path);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).await?;
    }
    let tmp = target.with_extension("packrelay-tmp");
    fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    if fs::metadata(target).await.is_ok() {
        fs::remove_file(target)
            .await
            .with_context(|| format!("replacing {}", target.display()))?;
    }
    fs::rename(&tmp, target)
        .await
        .with_context(|| format!("moving {} into place", target.display()))?;
    Ok(())
}
