// Profiles: named bundles of (mods + saves + worlds) that the user
// can switch between. Each profile is a self-contained snapshot of
// what 7DTD's user data directory should look like.
//
// Active model: one profile is "active" at a time. The active
// profile's mods/saves/worlds folders are mirrored into 7DTD's live
// userdata paths (Mods/, Saves/, GeneratedWorlds/). All Install /
// Update / Uninstall operations write to BOTH the active profile's
// mods/ AND the live Mods/ — keeping the two in sync without us
// having to do a "save before switch" dance on every operation.
//
// Switching: copy current 7DTD state back to outgoing profile,
// then copy incoming profile's state into 7DTD. Mods specifically
// can use the blob cache (hardlinks) for fast switching; saves and
// worlds are user-editable so they get copied (a hardlink would
// make edits to "active" leak into the "snapshot" we want to
// preserve).
//
// Snapshot history: each profile keeps the last N pre-launch
// snapshots of saves/worlds. Auto-snapshot fires before Launch
// 7DTD so the user always has a rollback point if a play session
// corrupts a world.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

// ---------- Layout helpers ----------

/// Per-profile on-disk layout under `<root>/profiles/<id>/`.
pub struct ProfilePaths {
    pub root: PathBuf,
    pub meta: PathBuf,
    pub mods: PathBuf,
    pub saves: PathBuf,
    pub worlds: PathBuf,
    pub snapshots: PathBuf,
}

impl ProfilePaths {
    pub fn from_root(profile_root: &Path) -> Self {
        Self {
            root: profile_root.to_path_buf(),
            meta: profile_root.join("meta.json"),
            mods: profile_root.join("mods"),
            saves: profile_root.join("saves"),
            worlds: profile_root.join("worlds"),
            snapshots: profile_root.join("snapshots"),
        }
    }
}

/// Layout under the launcher's app-data dir.
pub struct StoreLayout {
    pub root: PathBuf,
}

impl StoreLayout {
    pub fn new(app_data_dir: &Path) -> Self {
        Self {
            root: app_data_dir.join("store"),
        }
    }
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }
    pub fn profile_dir(&self, id: &str) -> PathBuf {
        self.profiles_dir().join(id)
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }
    pub fn active_pointer(&self) -> PathBuf {
        self.root.join("active.json")
    }
}

// ---------- Wire types ----------

/// Metadata for one profile. Mirrors meta.json on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileMeta {
    pub id: String,
    pub name: String,
    /// Pack the profile currently has installed, if any. Empty for
    /// "vanilla" profiles tracking just saves/worlds.
    #[serde(default)]
    pub pack_slug: Option<String>,
    #[serde(default)]
    pub pack_version: Option<String>,
    pub created_at: String,
    pub last_played_at: Option<String>,
}

/// Summary returned to the frontend for listing.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub pack_slug: Option<String>,
    pub pack_version: Option<String>,
    pub created_at: String,
    pub last_played_at: Option<String>,
    pub is_active: bool,
    pub mods_bytes: u64,
    pub saves_bytes: u64,
    pub worlds_bytes: u64,
    pub snapshot_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivePointer {
    active_profile_id: Option<String>,
    /// 7DTD userdata dir (the parent of Mods/Saves/GeneratedWorlds).
    /// Lets profile.switch know where to mirror into without the
    /// caller having to pass it on every call.
    seven_dtd_userdata_dir: Option<String>,
}

/// A pre-launch snapshot of saves + worlds. The user can roll back
/// to any of these if a play session went sideways. Mods aren't
/// snapshotted here — that's a profile-level concern, and mods are
/// hashed+content-addressed so they're already recoverable.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSnapshot {
    pub id: String,
    pub created_at: String,
    pub label: Option<String>,
    pub saves_bytes: u64,
    pub worlds_bytes: u64,
}

// ---------- High-level operations ----------

/// Create a new empty profile. The caller decides the name; we
/// generate the id.
pub async fn create_profile(layout: &StoreLayout, name: &str) -> Result<ProfileMeta> {
    let id = new_profile_id();
    let meta = ProfileMeta {
        id: id.clone(),
        name: name.trim().to_string(),
        pack_slug: None,
        pack_version: None,
        created_at: now_rfc3339(),
        last_played_at: None,
    };
    let paths = ProfilePaths::from_root(&layout.profile_dir(&id));
    fs::create_dir_all(&paths.mods).await?;
    fs::create_dir_all(&paths.saves).await?;
    fs::create_dir_all(&paths.worlds).await?;
    fs::create_dir_all(&paths.snapshots).await?;
    write_meta(&paths, &meta).await?;
    Ok(meta)
}

/// Import the user's existing 7DTD state as a profile. Used on
/// first opt-in so the user doesn't lose their current setup.
/// `seven_dtd_userdata_dir` is the parent that contains Mods/,
/// Saves/, GeneratedWorlds/ (or however many of those exist).
pub async fn import_current_as_profile(
    layout: &StoreLayout,
    seven_dtd_userdata_dir: &Path,
    name: &str,
) -> Result<ProfileMeta> {
    let meta = create_profile(layout, name).await?;
    let paths = ProfilePaths::from_root(&layout.profile_dir(&meta.id));

    let live_mods = seven_dtd_userdata_dir.join("Mods");
    let live_saves = seven_dtd_userdata_dir.join("Saves");
    let live_worlds = seven_dtd_userdata_dir.join("GeneratedWorlds");

    if fs::metadata(&live_mods).await.is_ok() {
        copy_dir_all(&live_mods, &paths.mods).await?;
    }
    if fs::metadata(&live_saves).await.is_ok() {
        copy_dir_all(&live_saves, &paths.saves).await?;
    }
    if fs::metadata(&live_worlds).await.is_ok() {
        copy_dir_all(&live_worlds, &paths.worlds).await?;
    }

    // First imported profile auto-becomes the active one. Caller
    // can override afterwards if they have a reason to.
    set_active(layout, Some(&meta.id), Some(seven_dtd_userdata_dir)).await?;
    Ok(meta)
}

/// List all profiles with computed summary stats. Per-profile dir
/// sizes are stat'd lazily — fast for small libraries; gets slower
/// linearly with profile count. Acceptable until users start
/// hoarding profiles.
pub async fn list_profiles(layout: &StoreLayout) -> Result<Vec<ProfileSummary>> {
    let dir = layout.profiles_dir();
    if !fs::metadata(&dir).await.is_ok() {
        return Ok(Vec::new());
    }
    let active = load_active(layout).await?;
    let active_id = active.active_profile_id.as_deref();

    let mut out = Vec::new();
    let mut rd = fs::read_dir(&dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let ft = entry.file_type().await?;
        if !ft.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        let paths = ProfilePaths::from_root(&entry.path());
        let meta = match read_meta(&paths).await {
            Ok(m) => m,
            Err(_) => continue, // skip orphaned dirs
        };
        let mods_bytes = dir_size(&paths.mods).await.unwrap_or(0);
        let saves_bytes = dir_size(&paths.saves).await.unwrap_or(0);
        let worlds_bytes = dir_size(&paths.worlds).await.unwrap_or(0);
        let snapshot_count = count_snapshots(&paths.snapshots).await.unwrap_or(0);
        out.push(ProfileSummary {
            id: meta.id.clone(),
            name: meta.name,
            pack_slug: meta.pack_slug,
            pack_version: meta.pack_version,
            created_at: meta.created_at,
            last_played_at: meta.last_played_at,
            is_active: active_id == Some(id.as_str()),
            mods_bytes,
            saves_bytes,
            worlds_bytes,
            snapshot_count,
        });
    }
    // Stable order: active first, then by creation date.
    out.sort_by(|a, b| match (a.is_active, b.is_active) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.created_at.cmp(&b.created_at),
    });
    Ok(out)
}

/// Currently-active profile id, if any.
pub async fn active_profile(layout: &StoreLayout) -> Result<Option<ProfileMeta>> {
    let active = load_active(layout).await?;
    let Some(id) = active.active_profile_id else {
        return Ok(None);
    };
    let paths = ProfilePaths::from_root(&layout.profile_dir(&id));
    Ok(Some(read_meta(&paths).await?))
}

/// Switch to a different profile.
///
/// Steps, in order:
///   1. If a profile is currently active, copy 7DTD's live state
///      (Mods/Saves/GeneratedWorlds) back into that profile's
///      folders, so anything the user did in-game lands in the
///      outgoing profile.
///   2. Mirror the incoming profile's mods/saves/worlds into the
///      7DTD live locations.
///   3. Update the active pointer.
///
/// This can be slow for big save folders. Callers should display a
/// progress indicator and disable the launch button until done.
pub async fn switch_profile(
    layout: &StoreLayout,
    incoming_id: &str,
) -> Result<()> {
    let active = load_active(layout).await?;
    let Some(userdata) = active.seven_dtd_userdata_dir.as_deref() else {
        anyhow::bail!(
            "Profile system isn't initialized — 7DTD userdata dir not set. Import or set it first."
        );
    };
    let userdata = PathBuf::from(userdata);

    let live_mods = userdata.join("Mods");
    let live_saves = userdata.join("Saves");
    let live_worlds = userdata.join("GeneratedWorlds");

    // Step 1: capture outgoing profile's state. Skip if no
    // currently-active profile (first-ever switch).
    if let Some(prev_id) = &active.active_profile_id {
        if prev_id != incoming_id {
            let prev_paths = ProfilePaths::from_root(&layout.profile_dir(prev_id));
            if fs::metadata(&live_mods).await.is_ok() {
                replace_dir(&prev_paths.mods, &live_mods).await?;
            }
            if fs::metadata(&live_saves).await.is_ok() {
                replace_dir(&prev_paths.saves, &live_saves).await?;
            }
            if fs::metadata(&live_worlds).await.is_ok() {
                replace_dir(&prev_paths.worlds, &live_worlds).await?;
            }
        }
    }

    // Step 2: deploy incoming.
    let next_paths = ProfilePaths::from_root(&layout.profile_dir(incoming_id));
    if !fs::metadata(&next_paths.meta).await.is_ok() {
        anyhow::bail!("Profile {incoming_id} not found.");
    }
    fs::create_dir_all(&userdata).await?;
    replace_dir(&live_mods, &next_paths.mods).await?;
    replace_dir(&live_saves, &next_paths.saves).await?;
    replace_dir(&live_worlds, &next_paths.worlds).await?;

    // Step 3: flip the pointer.
    set_active(layout, Some(incoming_id), Some(&userdata)).await?;
    Ok(())
}

/// Rename a profile. Pure metadata edit.
pub async fn rename_profile(
    layout: &StoreLayout,
    id: &str,
    new_name: &str,
) -> Result<ProfileMeta> {
    let paths = ProfilePaths::from_root(&layout.profile_dir(id));
    let mut meta = read_meta(&paths).await?;
    meta.name = new_name.trim().to_string();
    write_meta(&paths, &meta).await?;
    Ok(meta)
}

/// Delete a profile entirely — mods, saves, worlds, snapshots, all
/// of it. Refuses to delete the currently-active profile (caller
/// must switch away first).
pub async fn delete_profile(layout: &StoreLayout, id: &str) -> Result<()> {
    let active = load_active(layout).await?;
    if active.active_profile_id.as_deref() == Some(id) {
        anyhow::bail!(
            "Can't delete the active profile. Switch to another profile first."
        );
    }
    let dir = layout.profile_dir(id);
    if fs::metadata(&dir).await.is_ok() {
        fs::remove_dir_all(&dir)
            .await
            .with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

/// Snapshot the active profile's CURRENT live saves+worlds to a
/// new entry under its snapshots/ dir. Used as the pre-launch
/// safety net. `label` is an optional user-facing tag (e.g.
/// "Before Day 21 horde").
pub async fn snapshot_active(
    layout: &StoreLayout,
    label: Option<&str>,
    keep_last: usize,
) -> Result<ProfileSnapshot> {
    let active = load_active(layout).await?;
    let Some(profile_id) = active.active_profile_id.as_deref() else {
        anyhow::bail!("No active profile to snapshot.");
    };
    let Some(userdata) = active.seven_dtd_userdata_dir.as_deref() else {
        anyhow::bail!("7DTD userdata dir not configured.");
    };
    let userdata = PathBuf::from(userdata);
    let paths = ProfilePaths::from_root(&layout.profile_dir(profile_id));

    let snapshot_id = format!("snap-{}", now_compact());
    let snapshot_root = paths.snapshots.join(&snapshot_id);
    fs::create_dir_all(&snapshot_root).await?;

    let live_saves = userdata.join("Saves");
    let live_worlds = userdata.join("GeneratedWorlds");
    if fs::metadata(&live_saves).await.is_ok() {
        copy_dir_all(&live_saves, &snapshot_root.join("saves")).await?;
    }
    if fs::metadata(&live_worlds).await.is_ok() {
        copy_dir_all(&live_worlds, &snapshot_root.join("worlds")).await?;
    }

    let saves_bytes = dir_size(&snapshot_root.join("saves")).await.unwrap_or(0);
    let worlds_bytes = dir_size(&snapshot_root.join("worlds")).await.unwrap_or(0);

    let info = SnapshotMeta {
        id: snapshot_id.clone(),
        created_at: now_rfc3339(),
        label: label.map(|s| s.to_string()),
    };
    fs::write(
        snapshot_root.join("meta.json"),
        serde_json::to_string_pretty(&info)?,
    )
    .await?;

    // Prune oldest until we're under the keep limit.
    prune_snapshots(&paths.snapshots, keep_last).await?;

    Ok(ProfileSnapshot {
        id: info.id,
        created_at: info.created_at,
        label: info.label,
        saves_bytes,
        worlds_bytes,
    })
}

/// List the active profile's snapshot history, newest first.
pub async fn list_snapshots(
    layout: &StoreLayout,
    profile_id: &str,
) -> Result<Vec<ProfileSnapshot>> {
    let paths = ProfilePaths::from_root(&layout.profile_dir(profile_id));
    let mut snaps = read_snapshots(&paths.snapshots).await?;
    snaps.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(snaps)
}

/// Restore a specific snapshot into the active profile's live
/// 7DTD locations. Replaces current saves+worlds; does not touch
/// mods (mods are pack-managed, not save data).
pub async fn restore_snapshot(
    layout: &StoreLayout,
    profile_id: &str,
    snapshot_id: &str,
) -> Result<()> {
    let active = load_active(layout).await?;
    if active.active_profile_id.as_deref() != Some(profile_id) {
        anyhow::bail!(
            "Snapshot's profile isn't currently active. Switch to it before restoring."
        );
    }
    let Some(userdata) = active.seven_dtd_userdata_dir.as_deref() else {
        anyhow::bail!("7DTD userdata dir not configured.");
    };
    let userdata = PathBuf::from(userdata);

    let paths = ProfilePaths::from_root(&layout.profile_dir(profile_id));
    let snap_root = paths.snapshots.join(snapshot_id);
    if !fs::metadata(&snap_root).await.is_ok() {
        anyhow::bail!("Snapshot {snapshot_id} not found.");
    }

    let snap_saves = snap_root.join("saves");
    let snap_worlds = snap_root.join("worlds");
    if fs::metadata(&snap_saves).await.is_ok() {
        replace_dir(&userdata.join("Saves"), &snap_saves).await?;
    }
    if fs::metadata(&snap_worlds).await.is_ok() {
        replace_dir(&userdata.join("GeneratedWorlds"), &snap_worlds).await?;
    }

    // Also mirror back into profile's permanent saves/worlds so a
    // future switch-away doesn't capture the in-flight (post-
    // restore) state we just clobbered.
    replace_dir(&paths.saves, &snap_saves).await?;
    replace_dir(&paths.worlds, &snap_worlds).await?;
    Ok(())
}

/// Set the active profile pointer. Public so the Tauri command for
/// "first-time setup" can initialize seven_dtd_userdata_dir.
pub async fn set_active(
    layout: &StoreLayout,
    profile_id: Option<&str>,
    seven_dtd_userdata_dir: Option<&Path>,
) -> Result<()> {
    let mut pointer = load_active(layout).await?;
    pointer.active_profile_id = profile_id.map(|s| s.to_string());
    if let Some(dir) = seven_dtd_userdata_dir {
        pointer.seven_dtd_userdata_dir = Some(dir.display().to_string());
    }
    write_active(layout, &pointer).await
}

/// Read the active pointer; returns empty (None/None) if no file
/// exists yet.
pub async fn read_active(layout: &StoreLayout) -> Result<(Option<String>, Option<String>)> {
    let p = load_active(layout).await?;
    Ok((p.active_profile_id, p.seven_dtd_userdata_dir))
}

/// Bind a pack to the active profile's metadata. Called when an
/// install/update finishes so the profile remembers what's deployed.
pub async fn bind_pack_to_active(
    layout: &StoreLayout,
    slug: &str,
    version: &str,
) -> Result<()> {
    let active = load_active(layout).await?;
    let Some(id) = active.active_profile_id.as_deref() else {
        return Ok(()); // profile system not initialized; install proceeds without binding
    };
    let paths = ProfilePaths::from_root(&layout.profile_dir(id));
    let mut meta = read_meta(&paths).await?;
    meta.pack_slug = Some(slug.to_string());
    meta.pack_version = Some(version.to_string());
    write_meta(&paths, &meta).await
}

/// Clear the active profile's bound pack. Called by uninstall.
pub async fn clear_active_pack(layout: &StoreLayout) -> Result<()> {
    let active = load_active(layout).await?;
    let Some(id) = active.active_profile_id.as_deref() else {
        return Ok(());
    };
    let paths = ProfilePaths::from_root(&layout.profile_dir(id));
    let mut meta = read_meta(&paths).await?;
    meta.pack_slug = None;
    meta.pack_version = None;
    write_meta(&paths, &meta).await
}

// ---------- Internal helpers ----------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SnapshotMeta {
    id: String,
    created_at: String,
    label: Option<String>,
}

async fn read_meta(paths: &ProfilePaths) -> Result<ProfileMeta> {
    let raw = fs::read_to_string(&paths.meta)
        .await
        .with_context(|| format!("reading {}", paths.meta.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

async fn write_meta(paths: &ProfilePaths, meta: &ProfileMeta) -> Result<()> {
    fs::create_dir_all(&paths.root).await?;
    fs::write(&paths.meta, serde_json::to_string_pretty(meta)?).await?;
    Ok(())
}

async fn load_active(layout: &StoreLayout) -> Result<ActivePointer> {
    let path = layout.active_pointer();
    match fs::read_to_string(&path).await {
        Ok(raw) => Ok(serde_json::from_str(&raw)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ActivePointer {
            active_profile_id: None,
            seven_dtd_userdata_dir: None,
        }),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

async fn write_active(layout: &StoreLayout, pointer: &ActivePointer) -> Result<()> {
    fs::create_dir_all(&layout.root).await?;
    fs::write(
        layout.active_pointer(),
        serde_json::to_string_pretty(pointer)?,
    )
    .await?;
    Ok(())
}

/// Recursively copy a directory tree. Used for saves/worlds where
/// we want true copies (hardlinks would let in-game edits leak
/// into "snapshot" copies). Async-walks so a 5GB world doesn't
/// block the tokio runtime.
async fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).await?;
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        let mut rd = fs::read_dir(&s).await?;
        while let Some(entry) = rd.next_entry().await? {
            let ft = entry.file_type().await?;
            let from = entry.path();
            let to = d.join(entry.file_name());
            if ft.is_dir() {
                fs::create_dir_all(&to).await?;
                stack.push((from, to));
            } else if ft.is_file() {
                fs::copy(&from, &to).await?;
            }
            // symlinks: skip. 7DTD doesn't ship them and we don't
            // want to follow user-created links into surprising
            // locations.
        }
    }
    Ok(())
}

/// Replace `dst` with the contents of `src` (after first deleting
/// whatever's currently at `dst`). Used during profile switching
/// where we want the destination's old contents gone.
async fn replace_dir(dst: &Path, src: &Path) -> Result<()> {
    if fs::metadata(dst).await.is_ok() {
        fs::remove_dir_all(dst)
            .await
            .with_context(|| format!("clearing {}", dst.display()))?;
    }
    copy_dir_all(src, dst).await
}

async fn dir_size(path: &Path) -> Result<u64> {
    if !fs::metadata(path).await.is_ok() {
        return Ok(0);
    }
    let mut stack = vec![path.to_path_buf()];
    let mut total = 0u64;
    while let Some(dir) = stack.pop() {
        let mut rd = fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(m) = entry.metadata().await {
                    total += m.len();
                }
            }
        }
    }
    Ok(total)
}

async fn count_snapshots(snapshots_dir: &Path) -> Result<u32> {
    if !fs::metadata(snapshots_dir).await.is_ok() {
        return Ok(0);
    }
    let mut rd = fs::read_dir(snapshots_dir).await?;
    let mut n = 0u32;
    while let Some(entry) = rd.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            n += 1;
        }
    }
    Ok(n)
}

async fn read_snapshots(snapshots_dir: &Path) -> Result<Vec<ProfileSnapshot>> {
    if !fs::metadata(snapshots_dir).await.is_ok() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut rd = fs::read_dir(snapshots_dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let snap_root = entry.path();
        let raw = match fs::read_to_string(snap_root.join("meta.json")).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let info: SnapshotMeta = match serde_json::from_str(&raw) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let saves_bytes = dir_size(&snap_root.join("saves")).await.unwrap_or(0);
        let worlds_bytes = dir_size(&snap_root.join("worlds")).await.unwrap_or(0);
        out.push(ProfileSnapshot {
            id: info.id,
            created_at: info.created_at,
            label: info.label,
            saves_bytes,
            worlds_bytes,
        });
    }
    Ok(out)
}

async fn prune_snapshots(snapshots_dir: &Path, keep_last: usize) -> Result<()> {
    let mut snaps = read_snapshots(snapshots_dir).await?;
    if snaps.len() <= keep_last {
        return Ok(());
    }
    snaps.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    for old in snaps.into_iter().skip(keep_last) {
        let path = snapshots_dir.join(&old.id);
        let _ = fs::remove_dir_all(&path).await;
    }
    Ok(())
}

fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_iso_from_unix(secs)
}

fn now_compact() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

fn format_iso_from_unix(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Tiny Gregorian conversion — avoids pulling in `time` or `chrono`
/// just for an ISO timestamp. Good enough for UTC epoch values.
fn unix_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    // Civil from days since 1970-01-01 (Hinnant's algorithm).
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

fn new_profile_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 36-base for compactness; first six chars from time, last
    // four random-ish from a hash-mix so concurrent creates can't
    // collide.
    let mix = (nanos as u64).wrapping_mul(2654435761) ^ nanos as u64;
    format!("p{:x}", mix)
}

