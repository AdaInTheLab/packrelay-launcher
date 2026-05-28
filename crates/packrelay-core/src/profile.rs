// Profiles: named bundles of (multiple installed packs + a shared worlds
// directory) that the user can switch between, while ALSO being able to
// switch the active pack WITHIN a profile.
//
// Layout (schema_version 1, multi-pack):
//   <store>/profiles/<id>/
//     meta.json
//     worlds/                    ← shared across packs in this profile
//     packs/
//       <slug>/
//         mods/                  ← deployed to live Mods/ when this is the active pack
//         saves/                 ← per-pack character + save state
//         snapshots/             ← pre-launch safety nets for this pack
//           <snapshot-id>/
//             meta.json
//             saves/             ← captured at snapshot time
//             worlds/            ← captured at snapshot time (profile's shared worlds)
//
// The "active pack" within a profile gets its mods/ mirrored to 7DTD's
// live Mods/ and its saves/ mirrored to Saves/. The profile's shared
// worlds/ is mirrored to GeneratedWorlds/. Switching active pack swaps
// mods + saves (in that order), but leaves worlds alone.
//
// Snapshots are per-pack: each one captures the pack's saves + the
// profile's worlds at that moment, so rolling back a snapshot restores
// both. Worlds copies duplicate across snapshots; `prune_snapshots`
// keeps the count bounded (SNAPSHOT_KEEP_LAST default in callers).
//
// Migration: profiles with schema_version absent (or 0) are in the
// legacy single-pack layout (`mods/`, `saves/`, `worlds/`, `snapshots/`
// directly under the profile root). `migrate_profile_v0_to_v1` moves
// those into the new shape on next `list_profiles` call. Idempotent
// and safe: source dirs aren't deleted until the move succeeds, so an
// interrupted migration leaves the v0 layout intact for retry.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

/// On-disk schema version for profile meta.json. v0 (legacy) is
/// implied by the absence of this field; v1 is the multi-pack
/// layout described at the top of this file.
pub const PROFILE_SCHEMA_VERSION: u32 = 1;

/// Slug used inside a profile's `packs/` directory when the user
/// imports their pre-PackRelay 7DTD state -- they had mods but we
/// don't yet know what pack(s) those mods belong to. The launcher
/// can later prompt the user to rename / re-bind this pack to a
/// real catalog slug.
pub const IMPORTED_PACK_SLUG: &str = "_imported";

// ---------- Layout helpers ----------

/// Per-profile on-disk layout under `<root>/profiles/<id>/`.
pub struct ProfilePaths {
    pub root: PathBuf,
    pub meta: PathBuf,
    /// Shared across all packs in this profile.
    pub worlds: PathBuf,
    /// Parent of per-pack subdirectories (`packs/<slug>/`).
    pub packs_root: PathBuf,
}

impl ProfilePaths {
    pub fn from_root(profile_root: &Path) -> Self {
        Self {
            root: profile_root.to_path_buf(),
            meta: profile_root.join("meta.json"),
            worlds: profile_root.join("worlds"),
            packs_root: profile_root.join("packs"),
        }
    }

    /// Paths for a specific pack inside this profile. Does not check
    /// whether the pack actually exists on disk.
    pub fn pack_paths(&self, slug: &str) -> PackPaths {
        PackPaths::from_root(&self.packs_root.join(slug))
    }
}

/// On-disk layout for one pack-within-profile.
pub struct PackPaths {
    pub root: PathBuf,
    pub mods: PathBuf,
    pub saves: PathBuf,
    pub snapshots: PathBuf,
}

impl PackPaths {
    pub fn from_root(pack_root: &Path) -> Self {
        Self {
            root: pack_root.to_path_buf(),
            mods: pack_root.join("mods"),
            saves: pack_root.join("saves"),
            snapshots: pack_root.join("snapshots"),
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
    /// Where the blob-cache GC stores its `last_sweep_at` timestamp.
    /// Lives next to `active.json` so all per-launcher state is in
    /// one directory.
    pub fn cache_gc_state_path(&self) -> PathBuf {
        self.root.join("cache_gc_state.json")
    }
}

// ---------- Wire types ----------

/// One pack installed inside a profile. The "active" pack
/// (referenced by ProfileMeta.active_pack_slug) gets its mods/
/// mirrored to 7DTD's live Mods/ and its saves/ mirrored to Saves/.
/// Inactive packs sit dormant on disk; switching to one re-mirrors
/// its contents.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackInstallation {
    pub slug: String,
    pub version: String,
    pub installed_at: String,
    #[serde(default)]
    pub last_played_at: Option<String>,
}

/// Metadata for one profile. Mirrors meta.json on disk.
///
/// v1 shape (current). v0 profiles have `schema_version` absent
/// (defaults to 0) and use `pack_slug` + `pack_version` instead of
/// `packs` + `active_pack_slug`. Migration runs automatically on
/// `list_profiles` / `active_profile` calls.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileMeta {
    pub id: String,
    pub name: String,

    /// Schema version of the meta.json. 0 / absent = legacy
    /// single-pack layout; 1 = multi-pack. Used to drive
    /// `migrate_profile_v0_to_v1`.
    #[serde(default)]
    pub schema_version: u32,

    /// Installed packs in this profile. Empty for a "vanilla"
    /// profile with no pack content.
    #[serde(default)]
    pub packs: Vec<PackInstallation>,

    /// Slug of the pack currently mirrored to 7DTD's live Mods/
    /// + Saves/. None for vanilla mode (no mods mounted).
    #[serde(default)]
    pub active_pack_slug: Option<String>,

    /// Legacy v0 single-pack fields. Kept on the struct so we can
    /// READ old meta.json files into this type; migration moves
    /// their content into the `packs` vec then clears them. Always
    /// None on v1+ profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    pub schema_version: u32,
    pub packs: Vec<PackSummary>,
    pub active_pack_slug: Option<String>,
    pub created_at: String,
    pub last_played_at: Option<String>,
    pub is_active: bool,
    /// Bytes used by the profile's shared worlds/ directory.
    pub worlds_bytes: u64,
}

/// Per-pack stats inside a ProfileSummary.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackSummary {
    pub slug: String,
    pub version: String,
    pub installed_at: String,
    pub last_played_at: Option<String>,
    pub mods_bytes: u64,
    pub saves_bytes: u64,
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

/// A pre-launch snapshot of a pack's saves + the profile's worlds
/// at the moment of capture. The user can roll back to any of these
/// if a play session went sideways. Mods aren't snapshotted -- they're
/// pack-managed and content-addressed in the blob cache, so they're
/// already recoverable.
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
/// generate the id. New profiles start in v1 shape with empty
/// packs[] and no active pack -- the caller adds packs via
/// `add_pack_to_profile` (typically through the install flow).
pub async fn create_profile(layout: &StoreLayout, name: &str) -> Result<ProfileMeta> {
    let id = new_profile_id();
    let meta = ProfileMeta {
        id: id.clone(),
        name: name.trim().to_string(),
        schema_version: PROFILE_SCHEMA_VERSION,
        packs: Vec::new(),
        active_pack_slug: None,
        pack_slug: None,
        pack_version: None,
        created_at: now_rfc3339(),
        last_played_at: None,
    };
    let paths = ProfilePaths::from_root(&layout.profile_dir(&id));
    fs::create_dir_all(&paths.worlds).await?;
    fs::create_dir_all(&paths.packs_root).await?;
    write_meta(&paths, &meta).await?;
    Ok(meta)
}

/// Import the user's existing 7DTD state as a profile. Used on
/// first opt-in so the user doesn't lose their current setup. The
/// imported state lands in a single pack entry with the
/// `_imported` slug; users can later rename / re-bind via the UI.
/// `seven_dtd_userdata_dir` is the parent that contains Mods/,
/// Saves/, GeneratedWorlds/ (or however many of those exist).
pub async fn import_current_as_profile(
    layout: &StoreLayout,
    seven_dtd_userdata_dir: &Path,
    name: &str,
) -> Result<ProfileMeta> {
    let mut meta = create_profile(layout, name).await?;
    let paths = ProfilePaths::from_root(&layout.profile_dir(&meta.id));

    let live_mods = seven_dtd_userdata_dir.join("Mods");
    let live_saves = seven_dtd_userdata_dir.join("Saves");
    let live_worlds = seven_dtd_userdata_dir.join("GeneratedWorlds");

    // Worlds go to profile-shared root.
    if fs::metadata(&live_worlds).await.is_ok() {
        copy_dir_all(&live_worlds, &paths.worlds).await?;
    }

    // Mods + Saves go into an `_imported` pack entry if either
    // exists. If neither exists we leave the profile pack-less
    // (vanilla mode, just worlds).
    let has_mods = fs::metadata(&live_mods).await.is_ok();
    let has_saves = fs::metadata(&live_saves).await.is_ok();
    if has_mods || has_saves {
        let pack_paths = paths.pack_paths(IMPORTED_PACK_SLUG);
        fs::create_dir_all(&pack_paths.snapshots).await?;
        if has_mods {
            copy_dir_all(&live_mods, &pack_paths.mods).await?;
        } else {
            fs::create_dir_all(&pack_paths.mods).await?;
        }
        if has_saves {
            copy_dir_all(&live_saves, &pack_paths.saves).await?;
        } else {
            fs::create_dir_all(&pack_paths.saves).await?;
        }

        meta.packs.push(PackInstallation {
            slug: IMPORTED_PACK_SLUG.to_string(),
            // Unknown version on import; sentinel string lets the UI
            // render "v?" or prompt the user to bind a real version.
            version: "0".to_string(),
            installed_at: now_rfc3339(),
            last_played_at: None,
        });
        meta.active_pack_slug = Some(IMPORTED_PACK_SLUG.to_string());
        write_meta(&paths, &meta).await?;
    }

    // First imported profile auto-becomes the active one. Caller
    // can override afterwards if they have a reason to.
    set_active(layout, Some(&meta.id), Some(seven_dtd_userdata_dir)).await?;
    Ok(meta)
}

/// Duplicate an existing profile into a new one. Both the worlds
/// dir AND every pack-in-profile get cloned. Snapshots are NOT
/// copied -- per-session safety nets are tied to actual play
/// history on the source, not the clone.
pub async fn clone_profile(
    layout: &StoreLayout,
    source_id: &str,
    new_name: &str,
) -> Result<ProfileMeta> {
    let src_paths = ProfilePaths::from_root(&layout.profile_dir(source_id));
    if !fs::metadata(&src_paths.meta).await.is_ok() {
        anyhow::bail!("Source profile {source_id} not found.");
    }
    let src_meta = read_meta_migrated(layout, &src_paths).await?;

    let new_id = new_profile_id();
    let dst_paths = ProfilePaths::from_root(&layout.profile_dir(&new_id));
    fs::create_dir_all(&dst_paths.packs_root).await?;

    // Worlds (profile-shared).
    if fs::metadata(&src_paths.worlds).await.is_ok() {
        copy_dir_all(&src_paths.worlds, &dst_paths.worlds).await?;
    } else {
        fs::create_dir_all(&dst_paths.worlds).await?;
    }

    // Each pack: mods + saves (skip snapshots).
    for pack in &src_meta.packs {
        let src_pack = src_paths.pack_paths(&pack.slug);
        let dst_pack = dst_paths.pack_paths(&pack.slug);
        fs::create_dir_all(&dst_pack.snapshots).await?;
        if fs::metadata(&src_pack.mods).await.is_ok() {
            copy_dir_all(&src_pack.mods, &dst_pack.mods).await?;
        } else {
            fs::create_dir_all(&dst_pack.mods).await?;
        }
        if fs::metadata(&src_pack.saves).await.is_ok() {
            copy_dir_all(&src_pack.saves, &dst_pack.saves).await?;
        } else {
            fs::create_dir_all(&dst_pack.saves).await?;
        }
    }

    let meta = ProfileMeta {
        id: new_id,
        name: new_name.trim().to_string(),
        schema_version: PROFILE_SCHEMA_VERSION,
        packs: src_meta.packs.clone(),
        active_pack_slug: src_meta.active_pack_slug.clone(),
        pack_slug: None,
        pack_version: None,
        created_at: now_rfc3339(),
        last_played_at: None,
    };
    write_meta(&dst_paths, &meta).await?;
    Ok(meta)
}

/// List all profiles with computed summary stats. Triggers
/// migration on any v0 profile encountered. Per-profile and
/// per-pack dir sizes are stat'd lazily; fast for small libraries.
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
        let meta = match read_meta_migrated(layout, &paths).await {
            Ok(m) => m,
            Err(_) => continue, // skip orphaned dirs / unreadable meta
        };

        let worlds_bytes = dir_size(&paths.worlds).await.unwrap_or(0);
        let mut pack_summaries = Vec::with_capacity(meta.packs.len());
        for pack in &meta.packs {
            let pp = paths.pack_paths(&pack.slug);
            let mods_bytes = dir_size(&pp.mods).await.unwrap_or(0);
            let saves_bytes = dir_size(&pp.saves).await.unwrap_or(0);
            let snapshot_count = count_snapshots(&pp.snapshots).await.unwrap_or(0);
            pack_summaries.push(PackSummary {
                slug: pack.slug.clone(),
                version: pack.version.clone(),
                installed_at: pack.installed_at.clone(),
                last_played_at: pack.last_played_at.clone(),
                mods_bytes,
                saves_bytes,
                snapshot_count,
            });
        }

        out.push(ProfileSummary {
            id: meta.id.clone(),
            name: meta.name,
            schema_version: meta.schema_version,
            packs: pack_summaries,
            active_pack_slug: meta.active_pack_slug,
            created_at: meta.created_at,
            last_played_at: meta.last_played_at,
            is_active: active_id == Some(id.as_str()),
            worlds_bytes,
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

/// Currently-active profile's full meta (migrated to v1 if needed).
pub async fn active_profile(layout: &StoreLayout) -> Result<Option<ProfileMeta>> {
    let active = load_active(layout).await?;
    let Some(id) = active.active_profile_id else {
        return Ok(None);
    };
    let paths = ProfilePaths::from_root(&layout.profile_dir(&id));
    Ok(Some(read_meta_migrated(layout, &paths).await?))
}

/// Switch to a different profile.
///
/// Steps, in order:
///   1. If a profile is currently active, copy 7DTD's live state
///      back into that profile's pack + worlds dirs:
///        - Live Mods/ -> outgoing profile's active pack's mods/
///        - Live Saves/ -> outgoing profile's active pack's saves/
///        - Live GeneratedWorlds/ -> outgoing profile's worlds/
///      So anything the user did in-game lands in the outgoing
///      profile.
///   2. Mirror the incoming profile's active pack's mods/saves +
///      the profile's shared worlds into the live locations.
///   3. Update the active pointer.
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

    // Step 1: capture outgoing profile's state.
    if let Some(prev_id) = &active.active_profile_id {
        if prev_id != incoming_id {
            let prev_paths = ProfilePaths::from_root(&layout.profile_dir(prev_id));
            let prev_meta = read_meta_migrated(layout, &prev_paths).await?;

            // Worlds (always profile-level).
            if fs::metadata(&live_worlds).await.is_ok() {
                replace_dir(&prev_paths.worlds, &live_worlds).await?;
            }
            // Mods + Saves go to the outgoing profile's ACTIVE pack.
            // If no active pack, the user was in vanilla mode and we
            // skip the per-pack capture.
            if let Some(active_slug) = &prev_meta.active_pack_slug {
                let prev_pack = prev_paths.pack_paths(active_slug);
                if fs::metadata(&live_mods).await.is_ok() {
                    replace_dir(&prev_pack.mods, &live_mods).await?;
                }
                if fs::metadata(&live_saves).await.is_ok() {
                    replace_dir(&prev_pack.saves, &live_saves).await?;
                }
            }
        }
    }

    // Step 2: deploy incoming.
    let next_paths = ProfilePaths::from_root(&layout.profile_dir(incoming_id));
    if !fs::metadata(&next_paths.meta).await.is_ok() {
        anyhow::bail!("Profile {incoming_id} not found.");
    }
    let next_meta = read_meta_migrated(layout, &next_paths).await?;
    fs::create_dir_all(&userdata).await?;

    // Worlds always mirror.
    replace_dir(&live_worlds, &next_paths.worlds).await?;

    // Mods + Saves come from incoming's active pack. If no active
    // pack (vanilla mode), CLEAR live Mods + Saves so the user
    // doesn't inherit the prior profile's leftovers.
    if let Some(next_slug) = &next_meta.active_pack_slug {
        let next_pack = next_paths.pack_paths(next_slug);
        replace_dir(&live_mods, &next_pack.mods).await?;
        replace_dir(&live_saves, &next_pack.saves).await?;
    } else {
        // Vanilla mode: empty mods + saves.
        if fs::metadata(&live_mods).await.is_ok() {
            remove_dir_all_safe(&live_mods).await?;
        }
        if fs::metadata(&live_saves).await.is_ok() {
            remove_dir_all_safe(&live_saves).await?;
        }
        fs::create_dir_all(&live_mods).await?;
        fs::create_dir_all(&live_saves).await?;
    }

    // Step 3: flip the pointer.
    set_active(layout, Some(incoming_id), Some(&userdata)).await?;
    Ok(())
}

/// Within a profile, switch which pack is mounted to 7DTD's live
/// Mods/ + Saves/. Worlds are profile-shared and untouched.
///
/// If `target_slug` is None, switches to vanilla mode (clears live
/// mods + saves). If `target_slug` isn't in the profile's packs[],
/// returns an error -- the caller (Tauri command) should install
/// the pack into the profile first.
///
/// Only valid when the target profile is currently active.
pub async fn set_active_pack(
    layout: &StoreLayout,
    profile_id: &str,
    target_slug: Option<&str>,
) -> Result<()> {
    let active = load_active(layout).await?;
    if active.active_profile_id.as_deref() != Some(profile_id) {
        anyhow::bail!(
            "set_active_pack only works on the currently active profile. \
             Switch profiles first."
        );
    }
    let Some(userdata) = active.seven_dtd_userdata_dir.as_deref() else {
        anyhow::bail!("7DTD userdata dir not configured.");
    };
    let userdata = PathBuf::from(userdata);
    let live_mods = userdata.join("Mods");
    let live_saves = userdata.join("Saves");

    let paths = ProfilePaths::from_root(&layout.profile_dir(profile_id));
    let mut meta = read_meta_migrated(layout, &paths).await?;

    // Validate the target exists in the profile (or is None for
    // vanilla). No-op if already active.
    if meta.active_pack_slug.as_deref() == target_slug {
        return Ok(());
    }
    if let Some(slug) = target_slug {
        if !meta.packs.iter().any(|p| p.slug == slug) {
            anyhow::bail!(
                "Pack '{slug}' isn't installed in this profile. Install it first."
            );
        }
    }

    // Capture current live mods + saves back into the OUTGOING
    // pack's dirs (if there is one). Same logic as switch_profile's
    // step 1, scoped to the single profile.
    if let Some(outgoing) = &meta.active_pack_slug {
        let outgoing_pack = paths.pack_paths(outgoing);
        if fs::metadata(&live_mods).await.is_ok() {
            replace_dir(&outgoing_pack.mods, &live_mods).await?;
        }
        if fs::metadata(&live_saves).await.is_ok() {
            replace_dir(&outgoing_pack.saves, &live_saves).await?;
        }
    }

    // Mount the incoming pack (or clear live for vanilla mode).
    if let Some(incoming) = target_slug {
        let incoming_pack = paths.pack_paths(incoming);
        replace_dir(&live_mods, &incoming_pack.mods).await?;
        replace_dir(&live_saves, &incoming_pack.saves).await?;
    } else {
        if fs::metadata(&live_mods).await.is_ok() {
            remove_dir_all_safe(&live_mods).await?;
        }
        if fs::metadata(&live_saves).await.is_ok() {
            remove_dir_all_safe(&live_saves).await?;
        }
        fs::create_dir_all(&live_mods).await?;
        fs::create_dir_all(&live_saves).await?;
    }

    // Update meta.json.
    meta.active_pack_slug = target_slug.map(|s| s.to_string());
    write_meta(&paths, &meta).await?;
    Ok(())
}

/// Add (or update) a pack inside the currently-active profile. Called
/// by the install / update flows so the profile remembers what's been
/// deployed. Creates the pack's mods/saves/snapshots dirs if they're
/// missing. If the pack is new and no other pack is active, this new
/// pack becomes active automatically.
pub async fn add_pack_to_profile(
    layout: &StoreLayout,
    profile_id: &str,
    slug: &str,
    version: &str,
) -> Result<()> {
    let paths = ProfilePaths::from_root(&layout.profile_dir(profile_id));
    if !fs::metadata(&paths.meta).await.is_ok() {
        anyhow::bail!("Profile {profile_id} not found.");
    }
    let mut meta = read_meta_migrated(layout, &paths).await?;

    // Make sure pack dirs exist.
    let pack_paths = paths.pack_paths(slug);
    fs::create_dir_all(&pack_paths.mods).await?;
    fs::create_dir_all(&pack_paths.saves).await?;
    fs::create_dir_all(&pack_paths.snapshots).await?;

    // Upsert the PackInstallation entry.
    let now = now_rfc3339();
    if let Some(entry) = meta.packs.iter_mut().find(|p| p.slug == slug) {
        entry.version = version.to_string();
        entry.installed_at = now.clone();
    } else {
        meta.packs.push(PackInstallation {
            slug: slug.to_string(),
            version: version.to_string(),
            installed_at: now.clone(),
            last_played_at: None,
        });
    }

    // If nothing was active, this pack becomes active. (We DON'T
    // auto-switch when adding a new pack to a profile that already
    // had an active one -- the user might be queueing up another
    // pack without wanting to swap right now.)
    if meta.active_pack_slug.is_none() {
        meta.active_pack_slug = Some(slug.to_string());
    }

    write_meta(&paths, &meta).await?;
    Ok(())
}

/// Convenience wrapper: add a pack to the *currently active* profile.
/// Returns Ok with no-op if no profile is active (the launcher will
/// have installed the pack outside the profile system in that case).
pub async fn bind_pack_to_active(
    layout: &StoreLayout,
    slug: &str,
    version: &str,
) -> Result<()> {
    let active = load_active(layout).await?;
    let Some(id) = active.active_profile_id.as_deref() else {
        return Ok(());
    };
    add_pack_to_profile(layout, id, slug, version).await
}

/// Remove a pack from a profile -- deletes its `packs/<slug>/` dir
/// (mods, saves, snapshots). If the removed pack was the active
/// one, active_pack_slug is cleared (profile drops to vanilla mode
/// or whatever the user picks next).
pub async fn remove_pack_from_profile(
    layout: &StoreLayout,
    profile_id: &str,
    slug: &str,
) -> Result<()> {
    let paths = ProfilePaths::from_root(&layout.profile_dir(profile_id));
    if !fs::metadata(&paths.meta).await.is_ok() {
        anyhow::bail!("Profile {profile_id} not found.");
    }
    let mut meta = read_meta_migrated(layout, &paths).await?;
    meta.packs.retain(|p| p.slug != slug);
    if meta.active_pack_slug.as_deref() == Some(slug) {
        meta.active_pack_slug = None;
    }
    write_meta(&paths, &meta).await?;

    let pack_dir = paths.pack_paths(slug).root;
    if fs::metadata(&pack_dir).await.is_ok() {
        remove_dir_all_safe(&pack_dir).await?;
    }
    Ok(())
}

/// Legacy helper preserved for compatibility -- callers should
/// migrate to `remove_pack_from_profile`. Removes the active
/// profile's currently-active pack, if any.
pub async fn clear_active_pack(layout: &StoreLayout) -> Result<()> {
    let active = load_active(layout).await?;
    let Some(id) = active.active_profile_id.as_deref() else {
        return Ok(());
    };
    let paths = ProfilePaths::from_root(&layout.profile_dir(id));
    let meta = read_meta_migrated(layout, &paths).await?;
    if let Some(slug) = meta.active_pack_slug.clone() {
        remove_pack_from_profile(layout, id, &slug).await?;
    }
    Ok(())
}

/// Rename a profile. Pure metadata edit.
pub async fn rename_profile(
    layout: &StoreLayout,
    id: &str,
    new_name: &str,
) -> Result<ProfileMeta> {
    let paths = ProfilePaths::from_root(&layout.profile_dir(id));
    let mut meta = read_meta_migrated(layout, &paths).await?;
    meta.name = new_name.trim().to_string();
    write_meta(&paths, &meta).await?;
    Ok(meta)
}

/// Delete a profile entirely. Refuses to delete the currently-active
/// profile (caller must switch away first).
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

/// Snapshot the active profile's active pack's live state -- its
/// current saves + the profile's shared worlds -- to a new entry
/// under that pack's snapshots/ dir.
///
/// Returns Err if no profile is active OR the active profile has
/// no active pack (vanilla mode has nothing meaningful to snapshot).
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
    let meta = read_meta_migrated(layout, &paths).await?;
    let Some(active_slug) = meta.active_pack_slug.as_deref() else {
        anyhow::bail!("Active profile has no active pack (vanilla mode); nothing to snapshot.");
    };
    let pack_paths = paths.pack_paths(active_slug);

    let snapshot_id = format!("snap-{}", now_compact());
    let snapshot_root = pack_paths.snapshots.join(&snapshot_id);
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

    // Prune oldest until we're under the keep limit (per-pack count).
    prune_snapshots(&pack_paths.snapshots, keep_last).await?;

    Ok(ProfileSnapshot {
        id: info.id,
        created_at: info.created_at,
        label: info.label,
        saves_bytes,
        worlds_bytes,
    })
}

/// List a specific pack's snapshot history, newest first.
pub async fn list_snapshots(
    layout: &StoreLayout,
    profile_id: &str,
    pack_slug: &str,
) -> Result<Vec<ProfileSnapshot>> {
    let paths = ProfilePaths::from_root(&layout.profile_dir(profile_id));
    let pack_paths = paths.pack_paths(pack_slug);
    let mut snaps = read_snapshots(&pack_paths.snapshots).await?;
    snaps.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(snaps)
}

/// Restore a specific snapshot into the active profile's live 7DTD
/// locations: snapshot saves -> live Saves AND -> the pack's saves
/// (so a future switch-away doesn't capture in-flight state);
/// snapshot worlds -> live GeneratedWorlds AND -> the profile's
/// shared worlds (same reasoning).
pub async fn restore_snapshot(
    layout: &StoreLayout,
    profile_id: &str,
    pack_slug: &str,
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
    let pack_paths = paths.pack_paths(pack_slug);
    let snap_root = pack_paths.snapshots.join(snapshot_id);
    if !fs::metadata(&snap_root).await.is_ok() {
        anyhow::bail!("Snapshot {snapshot_id} not found.");
    }

    // Make sure pack_slug is the active one -- restoring a snapshot
    // for an inactive pack would clobber live state with the wrong
    // pack's data.
    let meta = read_meta_migrated(layout, &paths).await?;
    if meta.active_pack_slug.as_deref() != Some(pack_slug) {
        anyhow::bail!(
            "Can only restore snapshots for the active pack. Switch to '{pack_slug}' first."
        );
    }

    let snap_saves = snap_root.join("saves");
    let snap_worlds = snap_root.join("worlds");
    if fs::metadata(&snap_saves).await.is_ok() {
        replace_dir(&userdata.join("Saves"), &snap_saves).await?;
        replace_dir(&pack_paths.saves, &snap_saves).await?;
    }
    if fs::metadata(&snap_worlds).await.is_ok() {
        replace_dir(&userdata.join("GeneratedWorlds"), &snap_worlds).await?;
        replace_dir(&paths.worlds, &snap_worlds).await?;
    }
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

// ---------- Migration v0 -> v1 ----------

/// Migrate one profile from the legacy single-pack layout to the
/// multi-pack v1 layout. Idempotent + safe to retry: source dirs
/// aren't deleted until move succeeds, so an interrupted migration
/// leaves the v0 layout intact.
///
/// Concretely, for a v0 profile with `pack_slug = Some(s)`:
///   profiles/P/mods/       ->  profiles/P/packs/s/mods/
///   profiles/P/saves/      ->  profiles/P/packs/s/saves/
///   profiles/P/snapshots/  ->  profiles/P/packs/s/snapshots/
///   profiles/P/worlds/     ->  (unchanged; profile-shared in v1)
///   meta.json              ->  rewritten with schema_version=1,
///                              packs=[{slug, version}], active_pack_slug=Some(s)
///
/// For a v0 profile with pack_slug=None (vanilla):
///   profiles/P/mods/       ->  deleted (vanilla has no mods)
///   profiles/P/saves/      ->  retained at profile root? No -- in
///                              v1, saves are per-pack. We move them
///                              under packs/_imported/ so the user
///                              doesn't lose them (rare edge case).
///   profiles/P/snapshots/  ->  same -- moved to packs/_imported/
///   meta.json              ->  rewritten with schema_version=1,
///                              packs=[_imported v0] or [] if no
///                              saves/snapshots existed.
async fn migrate_profile_v0_to_v1(paths: &ProfilePaths) -> Result<()> {
    // Read the existing meta. We use the raw `read_meta` here to
    // get the file content untouched by migration logic.
    let mut meta = read_meta(paths).await?;
    if meta.schema_version >= PROFILE_SCHEMA_VERSION {
        return Ok(()); // already migrated; no-op
    }

    // Detect v0 layout: legacy `mods/`, `saves/`, `snapshots/`
    // directly under the profile root.
    let legacy_mods = paths.root.join("mods");
    let legacy_saves = paths.root.join("saves");
    let legacy_snapshots = paths.root.join("snapshots");

    // Decide the destination pack slug for the migrated content.
    let dest_slug = meta
        .pack_slug
        .clone()
        .unwrap_or_else(|| IMPORTED_PACK_SLUG.to_string());

    // Are there contents worth migrating? If neither mods nor saves
    // exist on disk, the v0 profile was empty/vanilla -- just rewrite
    // meta in v1 shape with no packs.
    let has_mods = fs::metadata(&legacy_mods).await.is_ok();
    let has_saves = fs::metadata(&legacy_saves).await.is_ok();
    let has_snapshots = fs::metadata(&legacy_snapshots).await.is_ok();

    if has_mods || has_saves || has_snapshots {
        let pack_paths = paths.pack_paths(&dest_slug);
        fs::create_dir_all(&pack_paths.root).await?;

        // Move legacy_mods/ -> packs/<slug>/mods/. We use rename
        // when possible (atomic, instant); fall back to copy+delete
        // if rename fails (different drive, perms, etc.).
        if has_mods {
            move_or_copy_dir(&legacy_mods, &pack_paths.mods).await?;
        } else {
            fs::create_dir_all(&pack_paths.mods).await?;
        }
        if has_saves {
            move_or_copy_dir(&legacy_saves, &pack_paths.saves).await?;
        } else {
            fs::create_dir_all(&pack_paths.saves).await?;
        }
        if has_snapshots {
            move_or_copy_dir(&legacy_snapshots, &pack_paths.snapshots).await?;
        } else {
            fs::create_dir_all(&pack_paths.snapshots).await?;
        }

        // Add the PackInstallation entry. Version comes from v0's
        // pack_version if present; sentinel "0" for the imported case.
        let version = meta
            .pack_version
            .clone()
            .unwrap_or_else(|| "0".to_string());
        meta.packs.push(PackInstallation {
            slug: dest_slug.clone(),
            version,
            installed_at: meta.created_at.clone(),
            last_played_at: meta.last_played_at.clone(),
        });
        meta.active_pack_slug = Some(dest_slug);
    }

    // Clear legacy fields + bump schema_version.
    meta.pack_slug = None;
    meta.pack_version = None;
    meta.schema_version = PROFILE_SCHEMA_VERSION;

    // Make sure worlds + packs_root exist (idempotent).
    fs::create_dir_all(&paths.worlds).await?;
    fs::create_dir_all(&paths.packs_root).await?;

    // Persist the migrated meta LAST. If anything above failed, we
    // haven't rewritten meta yet, so v0 callers still see v0 shape
    // and a retry can pick up from a partial state.
    write_meta(paths, &meta).await?;
    Ok(())
}

/// Read meta + auto-migrate if needed. Most callers use this rather
/// than `read_meta` directly.
async fn read_meta_migrated(
    _layout: &StoreLayout,
    paths: &ProfilePaths,
) -> Result<ProfileMeta> {
    let meta = read_meta(paths).await?;
    if meta.schema_version >= PROFILE_SCHEMA_VERSION {
        return Ok(meta);
    }
    migrate_profile_v0_to_v1(paths).await?;
    read_meta(paths).await
}

/// `rename` if possible, else `copy_dir_all` then `remove`. Used for
/// migrations where we want atomicity if the filesystem allows it
/// but tolerate cross-volume / permission edge cases.
async fn move_or_copy_dir(src: &Path, dst: &Path) -> Result<()> {
    // Try rename first.
    if fs::rename(src, dst).await.is_ok() {
        return Ok(());
    }
    // Fall back to copy + remove.
    copy_dir_all(src, dst).await?;
    remove_dir_all_safe(src).await?;
    Ok(())
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
/// we want true copies (hardlinks would let in-game edits leak into
/// "snapshot" copies). Async-walks so a 5GB world doesn't block the
/// tokio runtime.
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
        remove_dir_all_safe(dst)
            .await
            .with_context(|| format!("clearing {}", dst.display()))?;
    }
    copy_dir_all(src, dst).await
}

/// Recursive delete that never traverses into reparse points
/// (Windows junctions, OneDrive cloud placeholders, etc.). On
/// Windows, `std::fs::remove_dir_all` *should* skip reparse points
/// but has historically blown up with ERROR_REPARSE_POINT_ENCOUNTERED
/// (os error 4395) on specific reparse tag types -- most commonly
/// OneDrive's `IO_REPARSE_TAG_CLOUD` for files-on-demand. Walking
/// manually with `symlink_metadata` (which never follows links)
/// avoids the entire class of failure.
///
/// Reparse-point entries themselves get unlinked via `remove_file`,
/// which on Windows removes the link itself without touching the
/// target. If the user has set up a junction inside their Saves/
/// pointing at another drive, we're going to remove that junction
/// here -- which is the right behavior for "switching profiles
/// clears this slot," not a data-loss bug.
async fn remove_dir_all_safe(path: &Path) -> Result<()> {
    let meta = match fs::symlink_metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("stat {}", path.display()));
        }
    };
    if meta.file_type().is_symlink() {
        // The top of our tree is itself a reparse point. Just
        // unlink it -- no recursion.
        return fs::remove_file(path)
            .await
            .with_context(|| format!("unlinking reparse {}", path.display()));
    }
    if meta.is_file() {
        return fs::remove_file(path)
            .await
            .with_context(|| format!("removing {}", path.display()));
    }

    // Directory walk: pop deepest-first so children are gone before
    // we hit their parent.
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];
    let mut to_remove: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = match fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading {}", dir.display()));
            }
        };
        while let Some(entry) = rd
            .next_entry()
            .await
            .with_context(|| format!("iter {}", dir.display()))?
        {
            // file_type() from DirEntry on Windows doesn't follow
            // reparse points -- that's the property we need.
            let ft = entry
                .file_type()
                .await
                .with_context(|| format!("file_type {}", entry.path().display()))?;
            let p = entry.path();
            if ft.is_symlink() {
                // Reparse point of any flavor -- unlink without
                // recursing.
                fs::remove_file(&p)
                    .await
                    .with_context(|| format!("unlinking reparse {}", p.display()))?;
            } else if ft.is_dir() {
                stack.push(p.clone());
                to_remove.push(p);
            } else {
                fs::remove_file(&p)
                    .await
                    .with_context(|| format!("removing {}", p.display()))?;
            }
        }
    }
    // Deepest-first so we don't try to delete a non-empty parent.
    to_remove.sort_by(|a, b| b.components().count().cmp(&a.components().count()));
    for d in to_remove {
        match fs::remove_dir(&d).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("removing dir {}", d.display()));
            }
        }
    }
    Ok(())
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

/// Tiny Gregorian conversion -- avoids pulling in `time` or `chrono`
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
