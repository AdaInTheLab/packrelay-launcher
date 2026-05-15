// PackRelay launcher (Tauri GUI backend).
//
// Exposes two commands to the React frontend:
//   list_packs()              → fetches the public catalog
//   install_pack(slug, dest)  → runs packrelay-core's install loop,
//                               emits install://progress events as
//                               bytes land on disk
//
// Browse/install logic itself lives in packrelay-core — both the
// CLI and this GUI just decorate the same primitives differently.

mod auth;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;

use crate::auth::{
    clear_stored_token, load_stored_token, save_token, validate_token, AuthState,
};
use packrelay_core::blob_cache::{self, CacheStats, GcResult};
use packrelay_core::client::Client;
use packrelay_core::install::{install, InstallContext, InstallReport, ProgressEvent};
use packrelay_core::profile::{
    self, ProfileMeta, ProfileSnapshot, ProfileSummary, StoreLayout,
};
use packrelay_core::uninstall::{uninstall, UninstallReport};
use packrelay_core::update::{update, UpdateReport};
use packrelay_core::verify::{repair, verify, RepairReport, VerifyReport};

/// How many pre-launch snapshots we keep per profile before
/// pruning the oldest. User-tweakable in a future settings panel;
/// 5 is a reasonable default — enough to roll back the last few
/// play sessions, not so many that big saves balloon disk usage.
const SNAPSHOT_KEEP_LAST: usize = 5;

/// Resolve the launcher's app-data store layout. Centralized so
/// every profile command goes through the same root.
fn store_layout(app: &AppHandle) -> Result<StoreLayout, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolving app data dir: {e}"))?;
    Ok(StoreLayout::new(&dir))
}

/// Build the install context (cache root + active-profile mirror)
/// that install / update / repair use to populate the blob cache
/// and keep the profile's mods/ tree in sync. Always sets
/// cache_root — the cache survives any user state; only
/// profile_mods is conditional on the profile system being
/// initialized.
async fn build_install_context(app: &AppHandle) -> Result<InstallContext, String> {
    let layout = store_layout(app)?;
    let profile_mods = match profile::active_profile(&layout).await {
        Ok(Some(m)) => Some(layout.profile_dir(&m.id).join("mods")),
        Ok(None) => None,
        Err(_) => None, // profile system uninitialized → no mirror
    };
    Ok(InstallContext {
        cache_root: Some(layout.cache_dir()),
        profile_mods,
    })
}

/// Steam app id for 7 Days to Die. Encoded in the launch URI so
/// Steam handles install-validation/family-share/already-running
/// for us — we don't try to locate the binary ourselves.
const SEVEN_DAYS_STEAM_APPID: u32 = 251570;

/// The frontend always talks to packrelay.cloud unless we override
/// for local dev. Wrapped in a single constant so a future "switch
/// to staging" setting only touches one place.
const DEFAULT_API_URL: &str = "https://packrelay.cloud";

/// Mirrors GET /api/v1/packs's response shape. Tauri serializes
/// this back to the React frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPack {
    pub slug: String,
    pub name: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub latest_version: Option<String>,
    pub cover_image: Option<String>,
    pub publisher_name: String,
    pub tags: Vec<String>,
    pub file_count: i64,
    pub total_size_bytes: i64,
    pub download_count: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogResponse {
    packs: Vec<CatalogPack>,
}

/// Mirrors GET /api/v1/servers's response shape. Tauri serializes
/// this back to the React frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogServer {
    pub slug: String,
    pub name: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub region: String,
    pub connect_address: Option<String>,
    pub discord_url: Option<String>,
    pub website_url: Option<String>,
    pub tags: Vec<String>,
    pub current_players: i64,
    pub max_players: i64,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    pub online: bool,
    pub uptime_pct: f64,
    pub attached_pack: Option<AttachedPack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachedPack {
    pub slug: String,
    pub name: String,
    pub cover_image: Option<String>,
    pub latest_version: Option<String>,
    pub attached_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServersResponse {
    servers: Vec<CatalogServer>,
}

/// Payload emitted to the frontend on install progress. We track a
/// running byte total in atomics inside the command so the frontend
/// can render a single bar instead of accumulating deltas itself.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallProgressPayload {
    bytes_so_far: u64,
    total_bytes: u64,
    file_count: u32,
    /// Optional — set when the event was triggered by a file
    /// completing rather than just a byte chunk.
    last_completed_file: Option<String>,
}

/// Best-guess location for the user's 7DTD Mods/ folder. Returns
/// the OS-canonical user-data path joined with "7DaysToDie/Mods",
/// which is where the game itself reads per-user mods from. The
/// directory doesn't need to exist yet — we create it on install.
///
/// Returns None on unsupported platforms or if the relevant env var
/// is missing, in which case the frontend falls back to a hardcoded
/// guess.
#[tauri::command]
fn default_install_dest() -> Option<String> {
    let path = canonical_mods_path()?;
    Some(path.display().to_string())
}

#[cfg(target_os = "windows")]
fn canonical_mods_path() -> Option<PathBuf> {
    // %APPDATA% is where 7DTD reads per-user mods from on Windows.
    // Concretely: C:\Users\<name>\AppData\Roaming\7DaysToDie\Mods
    std::env::var_os("APPDATA")
        .map(|appdata| Path::new(&appdata).join("7DaysToDie").join("Mods"))
}

#[cfg(target_os = "macos")]
fn canonical_mods_path() -> Option<PathBuf> {
    // ~/Library/Application Support/7DaysToDie/Mods — same shape as
    // the game's own save directory.
    std::env::var_os("HOME").map(|home| {
        Path::new(&home)
            .join("Library")
            .join("Application Support")
            .join("7DaysToDie")
            .join("Mods")
    })
}

#[cfg(target_os = "linux")]
fn canonical_mods_path() -> Option<PathBuf> {
    // XDG_DATA_HOME wins if set; otherwise default to
    // ~/.local/share/7DaysToDie/Mods per the XDG Base Directory spec.
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        Some(Path::new(&xdg).join("7DaysToDie").join("Mods"))
    } else if let Some(home) = std::env::var_os("HOME") {
        Some(
            Path::new(&home)
                .join(".local")
                .join("share")
                .join("7DaysToDie")
                .join("Mods"),
        )
    } else {
        None
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn canonical_mods_path() -> Option<PathBuf> {
    None
}

fn user_agent_string() -> String {
    format!("packrelay-launcher/{}", env!("CARGO_PKG_VERSION"))
}

#[tauri::command]
async fn list_packs() -> Result<Vec<CatalogPack>, String> {
    let http = reqwest::Client::builder()
        .user_agent(user_agent_string())
        .build()
        .map_err(|e| e.to_string())?;

    let url = format!("{DEFAULT_API_URL}/api/v1/packs");
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("catalog fetch failed: HTTP {}", resp.status()));
    }
    let body: CatalogResponse = resp
        .json()
        .await
        .map_err(|e| format!("parsing catalog: {e}"))?;
    Ok(body.packs)
}

#[tauri::command]
async fn list_servers() -> Result<Vec<CatalogServer>, String> {
    let http = reqwest::Client::builder()
        .user_agent(user_agent_string())
        .build()
        .map_err(|e| e.to_string())?;

    let url = format!("{DEFAULT_API_URL}/api/v1/servers");
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("server catalog fetch failed: HTTP {}", resp.status()));
    }
    let body: ServersResponse = resp
        .json()
        .await
        .map_err(|e| format!("parsing server catalog: {e}"))?;
    Ok(body.servers)
}

#[tauri::command]
async fn install_pack(
    app: AppHandle,
    slug: String,
    dest: String,
) -> Result<InstallReport, String> {
    let client = Client::new(DEFAULT_API_URL);
    let dest_path = PathBuf::from(&dest);

    // Atomics so worker tasks can update a shared counter without
    // lock contention. Emitter::emit itself is cheap (queues an
    // event for the main thread to drain).
    let bytes_so_far = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));
    let file_count = Arc::new(AtomicU64::new(0));

    let bytes_clone = bytes_so_far.clone();
    let total_clone = total_bytes.clone();
    let file_count_clone = file_count.clone();
    let app_clone = app.clone();

    let ctx = build_install_context(&app).await?;
    let report = install(&client, &slug, &dest_path, 8, ctx, move |ev: ProgressEvent| {
        let payload = match ev {
            ProgressEvent::Started {
                total_bytes,
                file_count: fc,
                ..
            } => {
                total_clone.store(total_bytes, Ordering::Relaxed);
                file_count_clone.store(fc as u64, Ordering::Relaxed);
                InstallProgressPayload {
                    bytes_so_far: 0,
                    total_bytes,
                    file_count: fc,
                    last_completed_file: None,
                }
            }
            ProgressEvent::Bytes { delta } => {
                let new_total =
                    bytes_clone.fetch_add(delta, Ordering::Relaxed) + delta;
                InstallProgressPayload {
                    bytes_so_far: new_total,
                    total_bytes: total_clone.load(Ordering::Relaxed),
                    file_count: file_count_clone.load(Ordering::Relaxed) as u32,
                    last_completed_file: None,
                }
            }
            ProgressEvent::FileDone { path } => InstallProgressPayload {
                bytes_so_far: bytes_clone.load(Ordering::Relaxed),
                total_bytes: total_clone.load(Ordering::Relaxed),
                file_count: file_count_clone.load(Ordering::Relaxed) as u32,
                last_completed_file: Some(path),
            },
            ProgressEvent::Done { .. } => InstallProgressPayload {
                bytes_so_far: total_clone.load(Ordering::Relaxed),
                total_bytes: total_clone.load(Ordering::Relaxed),
                file_count: file_count_clone.load(Ordering::Relaxed) as u32,
                last_completed_file: None,
            },
        };
        // Ignore emit errors — frontend may have closed the window
        // mid-install; the install itself still completes on disk.
        let _ = app_clone.emit("install://progress", payload);
    })
    .await
    .map_err(|e| format!("{e:#}"))?;

    // Best-effort: tell the active profile what pack lives here now.
    // Failures are non-fatal — the install itself succeeded.
    if let Ok(layout) = store_layout(&app) {
        let _ = profile::bind_pack_to_active(&layout, &slug, &report.version).await;
    }

    Ok(report)
}

/// Smart update: diff the installed sidecar against the catalog
/// manifest and only refetch files that changed. Emits the same
/// install://progress events as install_pack — the frontend's bar
/// renders identically, just with a smaller total_bytes since
/// kept-unchanged files aren't counted.
#[tauri::command]
async fn update_pack(
    app: AppHandle,
    slug: String,
    dest: String,
) -> Result<UpdateReport, String> {
    let client = Client::new(DEFAULT_API_URL);
    let dest_path = PathBuf::from(&dest);

    // Same atomic-counter shape as install_pack so the frontend's
    // progress listener doesn't have to differentiate between
    // install and update event streams.
    let bytes_so_far = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));
    let file_count = Arc::new(AtomicU64::new(0));

    let bytes_clone = bytes_so_far.clone();
    let total_clone = total_bytes.clone();
    let file_count_clone = file_count.clone();
    let app_clone = app.clone();

    let ctx = build_install_context(&app).await?;
    let report = update(&client, &slug, &dest_path, 8, ctx, move |ev: ProgressEvent| {
        let payload = match ev {
            ProgressEvent::Started {
                total_bytes,
                file_count: fc,
                ..
            } => {
                total_clone.store(total_bytes, Ordering::Relaxed);
                file_count_clone.store(fc as u64, Ordering::Relaxed);
                InstallProgressPayload {
                    bytes_so_far: 0,
                    total_bytes,
                    file_count: fc,
                    last_completed_file: None,
                }
            }
            ProgressEvent::Bytes { delta } => {
                let new_total =
                    bytes_clone.fetch_add(delta, Ordering::Relaxed) + delta;
                InstallProgressPayload {
                    bytes_so_far: new_total,
                    total_bytes: total_clone.load(Ordering::Relaxed),
                    file_count: file_count_clone.load(Ordering::Relaxed) as u32,
                    last_completed_file: None,
                }
            }
            ProgressEvent::FileDone { path } => InstallProgressPayload {
                bytes_so_far: bytes_clone.load(Ordering::Relaxed),
                total_bytes: total_clone.load(Ordering::Relaxed),
                file_count: file_count_clone.load(Ordering::Relaxed) as u32,
                last_completed_file: Some(path),
            },
            ProgressEvent::Done { .. } => InstallProgressPayload {
                bytes_so_far: total_clone.load(Ordering::Relaxed),
                total_bytes: total_clone.load(Ordering::Relaxed),
                file_count: file_count_clone.load(Ordering::Relaxed) as u32,
                last_completed_file: None,
            },
        };
        let _ = app_clone.emit("install://progress", payload);
    })
    .await
    .map_err(|e| format!("{e:#}"))?;

    // Update the profile's bound version to the new one.
    if let Ok(layout) = store_layout(&app) {
        let _ = profile::bind_pack_to_active(&layout, &slug, &report.to_version).await;
    }

    Ok(report)
}

/// Re-check an installed pack's files against its sidecar manifest.
/// Cheap enough to run on demand from a history-row button — pure
/// disk IO + SHA-256 hashing, no network. The frontend renders the
/// returned VerifyReport (healthy vs. N files missing/corrupt).
#[tauri::command]
async fn verify_pack(dest: String) -> Result<VerifyReport, String> {
    verify(&PathBuf::from(dest))
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Re-download any files in an installed pack that failed verify.
/// Reuses the install loop's hash-verified streaming write path
/// (see install::download_and_verify), so a half-applied repair
/// can't leave the dest worse than it started.
#[tauri::command]
async fn repair_pack(app: AppHandle, dest: String) -> Result<RepairReport, String> {
    let client = Client::new(DEFAULT_API_URL);
    let ctx = build_install_context(&app).await?;
    repair(&client, &PathBuf::from(dest), ctx)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Remove an installed pack from disk. Reads the sidecar manifest
/// at `dest` to learn which files to delete, sweeps empty dirs the
/// pack created, and returns a structured report including any
/// per-file failures (locked, read-only, etc.) so the frontend can
/// surface them without re-querying the filesystem.
#[tauri::command]
async fn uninstall_pack(app: AppHandle, dest: String) -> Result<UninstallReport, String> {
    // Resolve the active profile's mods dir (if any) so the
    // uninstall also clears the profile's mirror — otherwise
    // switching back to this profile later would re-install the
    // pack we just removed.
    let layout = store_layout(&app)?;
    let profile_mods = match profile::active_profile(&layout).await {
        Ok(Some(m)) => Some(layout.profile_dir(&m.id).join("mods")),
        _ => None,
    };
    let report = uninstall(&PathBuf::from(dest), profile_mods.as_deref())
        .await
        .map_err(|e| format!("{e:#}"))?;

    // Clear the profile's bound pack so it doesn't claim a pack
    // that's no longer there.
    let _ = profile::clear_active_pack(&layout).await;
    Ok(report)
}

/// Resolve current auth state on startup (and any time the frontend
/// wants to refresh). Reads the stored token from app-data, asks
/// the cloud who it belongs to, and returns SignedOut when either
/// step fails. A failed validation auto-clears the stored token so
/// a revoked token doesn't stick around forever.
#[tauri::command]
async fn get_auth_state(app: AppHandle) -> Result<AuthState, String> {
    let token = match load_stored_token(&app).await {
        Ok(Some(t)) => t,
        Ok(None) => return Ok(AuthState::SignedOut),
        Err(e) => return Err(format!("{e:#}")),
    };
    match validate_token(DEFAULT_API_URL, &token).await {
        Ok(user) => Ok(AuthState::SignedIn { token, user }),
        Err(_) => {
            // Token failed validation — most likely revoked from the
            // website. Drop it so the user sees a clean sign-in state
            // instead of repeated 401s.
            let _ = clear_stored_token(&app).await;
            Ok(AuthState::SignedOut)
        }
    }
}

/// Validate a pasted token, persist it on success, return the user
/// profile so the frontend can render "Welcome back, X" immediately.
/// On bad token the saved state is left untouched — the user might
/// have mis-pasted and we don't want to evict their previous session.
#[tauri::command]
async fn sign_in(app: AppHandle, token: String) -> Result<AuthState, String> {
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err("Paste a token to sign in.".to_string());
    }
    let user = validate_token(DEFAULT_API_URL, &trimmed)
        .await
        .map_err(|e| format!("{e:#}"))?;
    save_token(&app, &trimmed)
        .await
        .map_err(|e| format!("saving token: {e:#}"))?;
    Ok(AuthState::SignedIn {
        token: trimmed,
        user,
    })
}

/// Clear the persisted token. We don't bother revoking it on the
/// cloud here — that's the website's job via /account/tokens. This
/// is a local "forget me on this machine."
#[tauri::command]
async fn sign_out(app: AppHandle) -> Result<AuthState, String> {
    clear_stored_token(&app)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(AuthState::SignedOut)
}

// ---------- Favorites ----------

/// Wire shape the launcher consumes from /api/v1/me/favorites.
/// Two slug sets the React side merges client-side with the
/// catalog list to render filled vs hollow hearts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyFavorites {
    pub packs: Vec<String>,
    pub servers: Vec<String>,
}

/// Wire shape returned by the toggle endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteToggleResult {
    pub favorited: bool,
    pub count: i64,
}

/// Read the persisted auth token from disk. The favorites
/// endpoints all require it — un-signed-in users see the page
/// without filled hearts and toggling is disabled in the UI, so
/// the command surface fails fast with a clear error rather than
/// hitting the cloud and returning 401.
async fn require_auth_token(app: &AppHandle) -> Result<String, String> {
    match load_stored_token(app).await {
        Ok(Some(token)) => Ok(token),
        Ok(None) => Err("Sign in to favorite packs and servers.".to_string()),
        Err(e) => Err(format!("reading saved token: {e:#}")),
    }
}

/// Fetch the signed-in user's full favorites set in one round-trip.
/// Launcher calls this on boot + after every successful toggle so
/// the UI's heart state stays in sync.
#[tauri::command]
async fn fetch_my_favorites(app: AppHandle) -> Result<MyFavorites, String> {
    let token = require_auth_token(&app).await?;
    let http = reqwest::Client::builder()
        .user_agent(user_agent_string())
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{DEFAULT_API_URL}/api/v1/me/favorites");
    let res = http
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Your token was rejected — sign in again.".to_string());
    }
    if !res.status().is_success() {
        return Err(format!(
            "fetching favorites failed: HTTP {}",
            res.status()
        ));
    }
    res.json::<MyFavorites>()
        .await
        .map_err(|e| format!("parsing favorites: {e}"))
}

#[tauri::command]
async fn toggle_pack_favorite(
    app: AppHandle,
    slug: String,
) -> Result<FavoriteToggleResult, String> {
    let token = require_auth_token(&app).await?;
    toggle_favorite_impl(&token, &format!("/api/v1/packs/{slug}/favorite")).await
}

#[tauri::command]
async fn toggle_server_favorite(
    app: AppHandle,
    slug: String,
) -> Result<FavoriteToggleResult, String> {
    let token = require_auth_token(&app).await?;
    toggle_favorite_impl(&token, &format!("/api/v1/servers/{slug}/favorite")).await
}

async fn toggle_favorite_impl(
    token: &str,
    path: &str,
) -> Result<FavoriteToggleResult, String> {
    let http = reqwest::Client::builder()
        .user_agent(user_agent_string())
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{DEFAULT_API_URL}{path}");
    let res = http
        .post(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Your token was rejected — sign in again.".to_string());
    }
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("That pack or server no longer exists.".to_string());
    }
    if !res.status().is_success() {
        return Err(format!("favorite toggle failed: HTTP {}", res.status()));
    }
    res.json::<FavoriteToggleResult>()
        .await
        .map_err(|e| format!("parsing toggle response: {e}"))
}

/// One entry in a pack's "what's inside" overview. Each top-level
/// directory in the manifest's file list is one mod, by 7DTD's
/// pack-on-disk convention (Mods/<ModName>/ModInfo.xml).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackDirEntry {
    pub name: String,
    pub file_count: u32,
    pub total_bytes: u64,
}

/// Fetch a pack's manifest and reduce it to its top-level dirs.
/// Used by DetailView to render "what's inside" without shipping
/// the entire file list to the React side — for big packs the
/// manifest can be thousands of entries; we'd waste bytes
/// serializing it just to throw 99% away.
#[tauri::command]
async fn fetch_pack_overview(slug: String) -> Result<Vec<PackDirEntry>, String> {
    let client = Client::new(DEFAULT_API_URL);
    let (_raw, manifest) = client
        .fetch_manifest(&slug)
        .await
        .map_err(|e| format!("{e:#}"))?;

    // Bucket file count + total bytes by first path segment.
    // Manifest paths can use either slash flavour on Windows-
    // published packs (Zod doesn't normalize); canonicalize before
    // splitting so a backslash-in-path doesn't end up as its own
    // bogus "dir".
    let mut by_dir: std::collections::HashMap<String, (u32, u64)> =
        std::collections::HashMap::new();
    let mut root_count: u32 = 0;
    let mut root_bytes: u64 = 0;
    for file in &manifest.files {
        let normalized = file.path.replace('\\', "/");
        match normalized.split_once('/') {
            Some((first, _)) => {
                let entry = by_dir.entry(first.to_string()).or_insert((0, 0));
                entry.0 += 1;
                entry.1 += file.size;
            }
            None => {
                // File sitting at the manifest root with no
                // parent dir. Group as a synthetic "(root)"
                // bucket at the end of the list — uncommon, but
                // worth showing rather than silently dropping.
                root_count += 1;
                root_bytes += file.size;
            }
        }
    }
    let mut dirs: Vec<PackDirEntry> = by_dir
        .into_iter()
        .map(|(name, (file_count, total_bytes))| PackDirEntry {
            name,
            file_count,
            total_bytes,
        })
        .collect();
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    if root_count > 0 {
        dirs.push(PackDirEntry {
            name: "(root)".to_string(),
            file_count: root_count,
            total_bytes: root_bytes,
        });
    }
    Ok(dirs)
}

// ---------- Profile commands ----------
//
// Profiles are named bundles of (mods + saves + worlds). The core
// logic lives in packrelay_core::profile; these commands are thin
// wrappers that resolve the store layout from Tauri's app-data
// dir and translate anyhow errors into frontend-friendly strings.

/// State returned by `profile_initial_state` — drives the
/// ProfilesView's first-run flow. SignedOut-equivalent: no
/// profiles + no userdata dir set → show onboarding.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum ProfileInitialState {
    /// Profile system has never been used. Frontend offers
    /// "Import current setup" onboarding.
    Uninitialized {
        /// Best guess at the 7DTD userdata dir, derived from the
        /// canonical Mods/ path. The frontend pre-fills the
        /// onboarding form with this.
        suggested_userdata_dir: Option<String>,
    },
    /// At least one profile exists; system is set up.
    Initialized {
        active_profile_id: Option<String>,
        userdata_dir: Option<String>,
    },
}

#[tauri::command]
async fn profile_initial_state(app: AppHandle) -> Result<ProfileInitialState, String> {
    let layout = store_layout(&app)?;
    let (active_id, userdata) = profile::read_active(&layout)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let profiles = profile::list_profiles(&layout)
        .await
        .map_err(|e| format!("{e:#}"))?;
    if profiles.is_empty() && active_id.is_none() {
        // Suggest the userdata dir: it's the parent of the
        // canonical Mods/ path. The frontend already calls
        // default_install_dest() to get Mods/ — we duplicate the
        // derivation here so the suggestion's always available
        // even if the frontend hasn't queried yet.
        let suggested = canonical_mods_path()
            .and_then(|p| p.parent().map(|x| x.display().to_string()));
        return Ok(ProfileInitialState::Uninitialized {
            suggested_userdata_dir: suggested,
        });
    }
    Ok(ProfileInitialState::Initialized {
        active_profile_id: active_id,
        userdata_dir: userdata,
    })
}

#[tauri::command]
async fn profile_list(app: AppHandle) -> Result<Vec<ProfileSummary>, String> {
    let layout = store_layout(&app)?;
    profile::list_profiles(&layout)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn profile_active(app: AppHandle) -> Result<Option<ProfileMeta>, String> {
    let layout = store_layout(&app)?;
    profile::active_profile(&layout)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn profile_create(app: AppHandle, name: String) -> Result<ProfileMeta, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Name is required.".to_string());
    }
    let layout = store_layout(&app)?;
    profile::create_profile(&layout, &trimmed)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// First-run onboarding: snapshot the user's existing 7DTD Mods/
/// Saves/ GeneratedWorlds/ into a brand-new profile, mark it
/// active, and remember the userdata dir so subsequent switches
/// know where to mirror to/from.
#[tauri::command]
async fn profile_import_current(
    app: AppHandle,
    userdata_dir: String,
    name: String,
) -> Result<ProfileMeta, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Name is required.".to_string());
    }
    let dir = PathBuf::from(userdata_dir);
    if !dir.exists() {
        return Err(format!(
            "7DTD userdata dir doesn't exist: {}",
            dir.display()
        ));
    }
    let layout = store_layout(&app)?;
    profile::import_current_as_profile(&layout, &dir, &trimmed)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn profile_switch(app: AppHandle, id: String) -> Result<(), String> {
    let layout = store_layout(&app)?;
    profile::switch_profile(&layout, &id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn profile_rename(
    app: AppHandle,
    id: String,
    name: String,
) -> Result<ProfileMeta, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Name is required.".to_string());
    }
    let layout = store_layout(&app)?;
    profile::rename_profile(&layout, &id, &trimmed)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn profile_delete(app: AppHandle, id: String) -> Result<(), String> {
    let layout = store_layout(&app)?;
    profile::delete_profile(&layout, &id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Dry-run snapshot of what's in the blob cache + how much of it
/// could be reclaimed. Backs the "Cache" section of the Settings
/// page — the user sees the number BEFORE clicking the destructive
/// Clean button.
#[tauri::command]
async fn cache_stats(app: AppHandle) -> Result<CacheStats, String> {
    let layout = store_layout(&app)?;
    blob_cache::cache_stats(
        &layout.cache_dir(),
        &layout.profiles_dir(),
        Some(&layout.cache_gc_state_path()),
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

/// Delete every blob in the cache that no profile sidecar still
/// references. Reports how many blobs went away + how many bytes
/// the user actually got back.
#[tauri::command]
async fn cache_gc(app: AppHandle) -> Result<GcResult, String> {
    let layout = store_layout(&app)?;
    blob_cache::gc_cache(
        &layout.cache_dir(),
        &layout.profiles_dir(),
        &layout.cache_gc_state_path(),
    )
    .await
    .map_err(|e| format!("{e:#}"))
}

/// How long the launcher waits between automatic cache sweeps.
/// Weekly is the right cadence for a desktop app — long enough
/// that the disk walk + delete is unnoticeable amortized, short
/// enough that uninstalls don't linger forever.
const BACKGROUND_GC_INTERVAL_SECS: u64 = 7 * 24 * 60 * 60;

/// Delay before kicking off the startup sweep. Lets the UI bring
/// itself up + the user start clicking before we burn CPU walking
/// the cache. 60s feels invisible in practice.
const BACKGROUND_GC_STARTUP_DELAY_SECS: u64 = 60;

#[tauri::command]
async fn profile_clone(
    app: AppHandle,
    id: String,
    name: String,
) -> Result<ProfileMeta, String> {
    let trimmed = name.trim().to_string();
    if trimmed.is_empty() {
        return Err("Name is required.".to_string());
    }
    let layout = store_layout(&app)?;
    profile::clone_profile(&layout, &id, &trimmed)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn profile_snapshot_active(
    app: AppHandle,
    label: Option<String>,
) -> Result<ProfileSnapshot, String> {
    let layout = store_layout(&app)?;
    profile::snapshot_active(&layout, label.as_deref(), SNAPSHOT_KEEP_LAST)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn profile_list_snapshots(
    app: AppHandle,
    profile_id: String,
) -> Result<Vec<ProfileSnapshot>, String> {
    let layout = store_layout(&app)?;
    profile::list_snapshots(&layout, &profile_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn profile_restore_snapshot(
    app: AppHandle,
    profile_id: String,
    snapshot_id: String,
) -> Result<(), String> {
    let layout = store_layout(&app)?;
    profile::restore_snapshot(&layout, &profile_id, &snapshot_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Default 7DTD client port. Used when a server's `connectAddress`
/// is just an IP / hostname with no port — every documented 7DTD
/// server install binds 26900 by default.
const SEVEN_DAYS_DEFAULT_PORT: u16 = 26900;

/// Open 7DTD via the Steam protocol. If `connect_address` is
/// provided (server-browse one-click-join flow), we ride Steam's
/// `steam://run/<appid>//<args>` form to pass
/// `-connecttoip=<ip> -connecttoport=<port>` through to the
/// client — the long-documented community pattern for "launch
/// the game and jump straight into a server."
///
/// Steam occasionally strips args (varies by Steam client
/// version). When that happens the user still lands on the main
/// menu — the clipboard fallback (the connect address was already
/// copied client-side before the launch fired) lets them paste
/// into Join Game manually.
///
/// Before opening Steam we auto-snapshot the active profile's
/// saves+worlds so the user has a rollback point if the play
/// session corrupts a world. Snapshot failures are logged but
/// non-fatal — we'd rather let the user play with no snapshot
/// than block the launch on a transient filesystem error.
#[tauri::command]
async fn launch_game(
    app: AppHandle,
    connect_address: Option<String>,
) -> Result<(), String> {
    // Best-effort pre-launch snapshot. Only runs when the profile
    // system is initialized — first-time users without profiles
    // get the existing launch behavior unchanged.
    let layout = store_layout(&app)?;
    match profile::snapshot_active(&layout, Some("pre-launch"), SNAPSHOT_KEEP_LAST).await {
        Ok(_) => {}
        Err(e) => {
            // Common shapes: no active profile (expected pre-onboarding),
            // userdata dir not configured. Don't block launch on these.
            eprintln!("[launch] pre-launch snapshot skipped: {e:#}");
        }
    }

    // Preferred path: direct exe spawn with the connect args
    // baked into the argv. Bypasses Steam's URI-argument stripping
    // entirely. Only attempted when we actually have a connect
    // address — bare-launch goes through Steam so the user lands
    // on the main menu with Steam's launch flow intact.
    if let Some(addr) = connect_address.as_deref() {
        match try_spawn_seven_days(addr) {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("[launch] direct spawn unavailable ({e}); falling back to Steam URI");
            }
        }
    }

    let url = build_launch_url(connect_address.as_deref());
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("failed to launch 7DTD via Steam: {e}"))
}

/// Try to spawn 7DaysToDie.exe directly with `-connecttoip` /
/// `-connecttoport`. Direct spawn dodges Steam URI's habit of
/// silently stripping args — we hand the game its arg vector and
/// the OS, not Steam's URI parser, decides what reaches it.
///
/// Returns Ok(()) if the spawn succeeded (the game is now its own
/// process; we don't wait on it). Returns Err with a message if
/// we couldn't find the exe or the spawn itself failed — caller
/// then falls back to the Steam URI path.
///
/// Steam still needs to be running for the client's session
/// validation; in the common case Steam was the thing that
/// installed 7DTD so it's already up. If not the user sees
/// 7DTD's own "Steam is required" prompt rather than us
/// silently failing.
fn try_spawn_seven_days(connect_address: &str) -> Result<(), String> {
    let Some(exe) = find_seven_days_exe() else {
        return Err("client install not located".to_string());
    };
    let (host, port) = parse_connect_address(connect_address);
    if host.is_empty() || !is_safe_host(&host) {
        return Err("connect address didn't pass safety check".to_string());
    }

    // cwd matters — 7DTD looks for sibling _Data dir at startup.
    let cwd = exe.parent().ok_or_else(|| "exe has no parent dir".to_string())?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.current_dir(cwd)
        .arg(format!("-connecttoip={host}"))
        .arg(format!("-connecttoport={port}"));

    // Detach the child so closing the launcher doesn't take 7DTD
    // down with it. On Windows DETACHED_PROCESS gives the child
    // no console; CREATE_NEW_PROCESS_GROUP keeps Ctrl-C in the
    // parent from propagating.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn().map(|_| ()).map_err(|e| format!("spawn: {e}"))
}

/// Locate 7DaysToDie.exe by walking every Steam library on the
/// machine. Steam stores library paths in
/// `<steam>/steamapps/libraryfolders.vdf`; we parse it loosely
/// (find every `"path" "<value>"` entry) and probe the canonical
/// `steamapps/common/7 Days To Die/7DaysToDie.exe` subpath in
/// each. Returns the first hit.
///
/// We don't cache the result — call sites fire on user click
/// (rare), and an install can move between calls (e.g. Steam
/// rebalances libraries). The whole probe is a few stats; cheap.
fn find_seven_days_exe() -> Option<PathBuf> {
    let mut steam_roots: Vec<PathBuf> = Vec::new();
    if let Some(pf86) = std::env::var_os("ProgramFiles(x86)") {
        steam_roots.push(PathBuf::from(pf86).join("Steam"));
    }
    if let Some(pf) = std::env::var_os("ProgramFiles") {
        steam_roots.push(PathBuf::from(pf).join("Steam"));
    }

    let mut libraries: Vec<PathBuf> = Vec::new();
    for root in &steam_roots {
        // Default Steam install also IS a library — include it
        // even if libraryfolders.vdf doesn't list it.
        libraries.push(root.clone());
        let vdf = root.join("steamapps").join("libraryfolders.vdf");
        if let Ok(raw) = std::fs::read_to_string(&vdf) {
            libraries.extend(parse_vdf_library_paths(&raw));
        }
    }

    // Dedup with a HashSet so a path that appears in multiple
    // Steam VDFs isn't probed twice.
    let mut seen = std::collections::HashSet::new();
    for lib in libraries {
        if !seen.insert(lib.clone()) {
            continue;
        }
        let exe = lib
            .join("steamapps")
            .join("common")
            .join("7 Days To Die")
            .join("7DaysToDie.exe");
        if exe.exists() {
            return Some(exe);
        }
    }
    None
}

/// Pull every `"path" "<value>"` quoted-string pair out of a
/// libraryfolders.vdf. We don't try to fully parse the Valve
/// KeyValues format — only the path entries matter, and they
/// always sit on a single line in the format Steam writes.
fn parse_vdf_library_paths(raw: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        // Each library entry has `"path"   "<value>"` on its own
        // line — be tolerant about the variable whitespace between
        // the key and value, and accept the rare uppercase key
        // that older Steam versions wrote.
        let Some(rest) = line
            .strip_prefix("\"path\"")
            .or_else(|| line.strip_prefix("\"Path\""))
        else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        // VDF escapes backslashes — "C:\\Program Files\\Steam".
        // Unescape so the resulting PathBuf round-trips back to
        // a usable Windows path.
        let unescaped = rest[..end].replace("\\\\", "\\");
        out.push(PathBuf::from(unescaped));
    }
    out
}

/// Build the Steam URI we open to launch 7DTD. Without a connect
/// address we use the bare `rungameid` form (no args). With one,
/// we parse `<host>[:<port>]` and pass `-connecttoip` /
/// `-connecttoport` via Steam's `steam://run/<appid>//<args>`
/// form. Args are space-separated inside the URI; Steam decodes
/// percent-encoding before handing them to the game.
fn build_launch_url(connect_address: Option<&str>) -> String {
    let bare = format!("steam://rungameid/{SEVEN_DAYS_STEAM_APPID}");
    let Some(raw) = connect_address.map(str::trim).filter(|s| !s.is_empty()) else {
        return bare;
    };
    let (host, port) = parse_connect_address(raw);
    // Refuse to forward anything that isn't a plausible host —
    // belt-and-suspenders against a malformed catalog entry
    // smuggling shell-like content into the Steam URI.
    if host.is_empty() || !is_safe_host(&host) {
        return bare;
    }
    let host_enc = percent_encode_arg(&host);
    // `steam://run/<appid>//<args>` — args go after the second
    // slash. The leading "// " keeps the args block syntactically
    // visible to Steam as a single field.
    format!(
        "steam://run/{SEVEN_DAYS_STEAM_APPID}//-connecttoip={host_enc}%20-connecttoport={port}"
    )
}

/// Split a `host[:port]` address into its parts. Defaults to
/// 26900 when the port is missing. We split on the LAST colon so
/// IPv6 literals (which contain multiple colons) survive — though
/// 7DTD's connect args don't actually support IPv6 today, so
/// IPv6-only addresses will fail at the game side and the user
/// falls back to clipboard paste.
fn parse_connect_address(raw: &str) -> (String, u16) {
    match raw.rsplit_once(':') {
        Some((host, port_str)) => {
            let port = port_str.parse::<u16>().unwrap_or(SEVEN_DAYS_DEFAULT_PORT);
            (host.trim().to_string(), port)
        }
        None => (raw.trim().to_string(), SEVEN_DAYS_DEFAULT_PORT),
    }
}

/// Plausibility check: ASCII letters, digits, `.`, `-`, `:` (for
/// IPv6 in brackets if we ever support it). Rejects spaces,
/// quotes, command separators — anything that could escape the
/// Steam URI into shell-like territory.
fn is_safe_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')
        })
}

/// Conservative percent-encoder: keeps unreserved URL chars,
/// escapes everything else. We only call this on a host that's
/// already passed `is_safe_host`, so the encoded output is
/// effectively a no-op except for the rare case of square
/// brackets in IPv6 literals.
fn percent_encode_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // Updater plugin — checks `endpoints` from tauri.conf.json
        // on demand (we drive the check from JS at launch). Signature
        // verification happens locally against the `pubkey` baked into
        // tauri.conf.json, so a compromised endpoint can't push a
        // malicious binary as long as the private key stays safe in
        // CI secrets.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // Background blob-cache GC: weekly sweep on startup so disk
        // usage doesn't grow forever. Best-effort — failures stay
        // silent so a hiccup here never blocks the UI.
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(
                    BACKGROUND_GC_STARTUP_DELAY_SECS,
                ))
                .await;
                let Ok(layout) = store_layout(&handle) else {
                    return;
                };
                match blob_cache::gc_if_due(
                    &layout.cache_dir(),
                    &layout.profiles_dir(),
                    &layout.cache_gc_state_path(),
                    BACKGROUND_GC_INTERVAL_SECS,
                )
                .await
                {
                    Ok(Some(r)) => eprintln!(
                        "[cache-gc] auto-swept: removed {} blob(s), freed {} bytes",
                        r.blobs_removed, r.bytes_freed
                    ),
                    Ok(None) => {} // not yet due — quiet
                    Err(e) => eprintln!("[cache-gc] auto-sweep failed: {e:#}"),
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_packs,
            list_servers,
            install_pack,
            default_install_dest,
            launch_game,
            verify_pack,
            repair_pack,
            uninstall_pack,
            update_pack,
            get_auth_state,
            sign_in,
            sign_out,
            profile_initial_state,
            profile_list,
            profile_active,
            profile_create,
            profile_import_current,
            profile_switch,
            profile_rename,
            profile_delete,
            profile_clone,
            cache_stats,
            cache_gc,
            profile_snapshot_active,
            profile_list_snapshots,
            profile_restore_snapshot,
            fetch_pack_overview,
            fetch_my_favorites,
            toggle_pack_favorite,
            toggle_server_favorite
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
