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
use packrelay_core::verify::{
    presence_check, repair, verify, PresenceReport, RepairReport, VerifyReport,
};

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
    /// Mirrors the cloud's `favoriteCount`. The TS `Pack` type has
    /// declared this since the hearts work landed (commit 0a3852c)
    /// but the Rust side never declared the matching field — so
    /// every catalog row arrived at the frontend with
    /// `favoriteCount: undefined`, which crashed BrowseView's
    /// render path silently (no error boundary → blank subtree).
    /// `#[serde(default)]` keeps us tolerant of older API responses
    /// that didn't include the field.
    #[serde(default)]
    pub favorite_count: i64,
    /// Admin-curated editor's pick. Featured packs always sort to
    /// the top of the catalog on both /browse and here. Defaults
    /// to false so older API responses (pre-0011 migration) still
    /// parse without breaking.
    #[serde(default)]
    pub is_featured: bool,
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
    /// Same drift as CatalogPack.favorite_count — TS declared it,
    /// Rust didn't. Server browse hides the row when count is 0,
    /// and `undefined > 0` is `false`, so the visible crash was
    /// dodged; but the heart-toggle optimistic math (NaN) and the
    /// "favorites" sort comparator were both quietly broken.
    /// `#[serde(default)]` for forward-compat with older APIs.
    #[serde(default)]
    pub favorite_count: i64,
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

/// `version` is an optional pin: when set, the installer fetches the
/// manifest at that specific version instead of the publisher's latest.
/// Used by the deep-link join flow so a server pinned to v0.2.0 doesn't
/// accidentally get v0.4.0 laid down on the player's disk. When None,
/// behaves identically to pre-v0.1.7 (always latest).
#[tauri::command]
async fn install_pack(
    app: AppHandle,
    slug: String,
    dest: String,
    version: Option<String>,
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
    let report = install(&client, &slug, &dest_path, 8, version.as_deref(), ctx, move |ev: ProgressEvent| {
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
/// Same pin semantics as `install_pack`: route the smart-update diff
/// against a specific version instead of latest. Critical for the
/// kicked-from-server flow — server pinned to v0.2.0 + locally installed
/// v0.3.0 → `update_pack(version=Some("0.2.0"))` produces a v0.3.0→v0.2.0
/// downgrade, not v0.3.0→latest.
#[tauri::command]
async fn update_pack(
    app: AppHandle,
    slug: String,
    dest: String,
    version: Option<String>,
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
    let report = update(&client, &slug, &dest_path, 8, version.as_deref(), ctx, move |ev: ProgressEvent| {
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

/// Fast disk-presence probe for a pack we *think* we installed.
/// Parses the sidecar + stats up to 8 sample files; never hashes,
/// never reads bytes. Single-digit ms even on spinning disk for a
/// healthy install.
///
/// The screenshot bug — server detail page shows "INSTALLED + Connect"
/// for a folder that's actually empty — is this command's reason to
/// exist. ServerDetailView fires it on mount and downgrades to the
/// install flow when the verdict comes back present=false.
#[tauri::command]
async fn check_pack_present(dest: String) -> Result<PresenceReport, String> {
    presence_check(&PathBuf::from(dest))
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
///
/// `source` (#154) ~ when the manifest carries v2 provenance, the
/// launcher surfaces it as a deep link in DetailView. Resolved by
/// taking the first file's sourceRef and looking it up in the
/// manifest's sources catalog. Files in a single mod dir share a
/// source in the canonical Nexus case, so first-file is
/// representative; the rare cross-source dir loses provenance for
/// the trailing files but doesn't break the render.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackDirEntry {
    pub name: String,
    pub file_count: u32,
    pub total_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<DirEntrySource>,
}

/// Per-dir source provenance for the launcher's "What's inside"
/// render (#154). Holds enough info to draw a deep link + (future)
/// an icon, but nothing more ~ the launcher never refetches bytes
/// via these URLs (downloads are always content-addressed against
/// the blob cache).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntrySource {
    /// "nexus" | "github" | "7dtm" | "legacy-blob".
    pub kind: String,
    /// Human-readable label for the source ("Nexus", "GitHub",
    /// "7DaysToDieMods", "self-hosted"). Computed Rust-side so the
    /// React side doesn't need to know the kind->label mapping.
    pub label: String,
    /// Deep link to the upstream listing. None for legacy-blob ~
    /// nothing to link to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Small upstream icon (16-32px PNG, hosted on packrelay.cloud's
    /// CDN). None today for every variant ~ icons are a future
    /// visual-polish pass. The field exists so wiring an icon is a
    /// one-line registry change, not a schema change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
}

/// One row in the source-renderer registry (#171). Adding a new
/// source variant = adding one row here + nothing else. Each entry
/// declares:
///
///   - `kind` ~ matches `ManifestSource.source` for lookup.
///   - `display_name` ~ what the launcher renders next to "source:".
///   - `icon_url` ~ small upstream icon. None until icons ship.
///   - `url_builder` ~ takes the matched ManifestSource and returns
///     a deep link, or None if the required fields aren't present.
///
/// Why a const slice of structs instead of a trait + dyn dispatch:
/// the lookup runs once per pack-detail render (≤ a few dozen
/// mods), the variants are bounded by what the manifest schema
/// accepts (4 today, ~6 plausibly ever), and we want compile-time
/// errors when a future source kind forgets to register.
struct SourceRenderer {
    kind: &'static str,
    display_name: &'static str,
    icon_url: Option<&'static str>,
    url_builder: fn(&packrelay_core::manifest::ManifestSource) -> Option<String>,
}

/// Build the Nexus mod-page URL from a ManifestSource's `game` +
/// `mod_id`. Falls back to "7daystodie" when `game` is absent ~
/// shouldn't happen on a well-formed v2 manifest (schema requires
/// game on nexus sources), but the launcher prefers to surface a
/// best-effort link over silently dropping the row.
fn nexus_url(src: &packrelay_core::manifest::ManifestSource) -> Option<String> {
    let mod_id = src.mod_id?;
    let game = src.game.as_deref().unwrap_or("7daystodie");
    Some(format!("https://www.nexusmods.com/{game}/mods/{mod_id}"))
}

/// Build the GitHub repo URL from `owner/repo`. We could deep-link
/// to the specific release tag, but the repo root surfaces the
/// project README + license + recent activity which is what most
/// users care about. The tag is a one-line addition if it turns
/// out useful.
fn github_url(src: &packrelay_core::manifest::ManifestSource) -> Option<String> {
    let owner = src.owner.as_deref()?;
    let repo = src.repo.as_deref()?;
    Some(format!("https://github.com/{owner}/{repo}"))
}

/// 7DTM stores the captured upstream URL directly in the manifest
/// source entry (the site doesn't have stable numeric IDs we can
/// reconstruct from), so the renderer just passes it through.
fn sevendtm_url(src: &packrelay_core::manifest::ManifestSource) -> Option<String> {
    src.upstream_url.clone()
}

/// Legacy-blob has no upstream ~ it's a "self-hosted, no provenance"
/// marker. The label "self-hosted" still renders so users can tell
/// the mod is a known-quantity vs. an orphaned reference.
fn legacy_blob_url(_src: &packrelay_core::manifest::ManifestSource) -> Option<String> {
    None
}

/// The source-renderer registry. Order is rendering-stable but not
/// semantically meaningful ~ lookup goes by `kind` match.
///
/// Adding a future source kind: append one row + define one
/// `*_url` helper. resolve_source picks it up automatically and
/// the existing tests + frontend keep working.
const RENDERERS: &[SourceRenderer] = &[
    SourceRenderer {
        kind: "nexus",
        display_name: "Nexus",
        icon_url: None,
        url_builder: nexus_url,
    },
    SourceRenderer {
        kind: "github",
        display_name: "GitHub",
        icon_url: None,
        url_builder: github_url,
    },
    SourceRenderer {
        kind: "7dtm",
        display_name: "7DaysToDieMods",
        icon_url: None,
        url_builder: sevendtm_url,
    },
    SourceRenderer {
        kind: "legacy-blob",
        display_name: "self-hosted",
        icon_url: None,
        url_builder: legacy_blob_url,
    },
];

/// Resolve a file entry's `source_ref` against the manifest's
/// sources catalog and translate the matching `ManifestSource`
/// into the launcher's display shape. Returns None when:
///   - the file has no source_ref (v1 manifest or legacy-blob)
///   - the source_ref doesn't match any catalog entry (malformed
///     manifest ~ the cloud's superRefine catches this at publish
///     time, but the launcher tolerates it gracefully)
///   - the matched source variant isn't one the launcher knows
///     how to render yet (forward-compat for future source types)
///
/// Pure function, public for test access from the lib.rs test
/// module below.
pub fn resolve_source(
    sources: &[packrelay_core::manifest::ManifestSource],
    file: &packrelay_core::manifest::FileEntry,
) -> Option<DirEntrySource> {
    let source_ref = file.source_ref.as_deref()?;
    let src = sources.iter().find(|s| s.id == source_ref)?;
    let renderer = RENDERERS.iter().find(|r| r.kind == src.source)?;
    Some(DirEntrySource {
        kind: renderer.kind.to_string(),
        label: renderer.display_name.to_string(),
        url: (renderer.url_builder)(src),
        icon_url: renderer.icon_url.map(|s| s.to_string()),
    })
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

    Ok(reduce_to_dirs(&manifest))
}

/// Pure helper extracted from `fetch_pack_overview` so the
/// dir-bucketing + per-dir source resolution can be unit-tested
/// without a live API. Takes the parsed manifest and emits the
/// flat dir list with each entry's representative source.
///
/// Source resolution rule: take the FIRST file inside a dir's
/// sourceRef. In the canonical Nexus case all files in one mod
/// folder share a single sourceRef so the first is representative.
/// For a hypothetical cross-source dir the trailing files lose
/// provenance ~ acceptable as v0 since no publisher actually
/// builds such a pack, and the cloud's audit (#178-ish) will
/// flag it before publish anyway.
pub fn reduce_to_dirs(manifest: &packrelay_core::manifest::Manifest) -> Vec<PackDirEntry> {
    // Bucket file count + total bytes + first-file source by
    // first path segment. Manifest paths can use either slash
    // flavour on Windows-published packs (Zod doesn't normalize);
    // canonicalize before splitting so a backslash-in-path
    // doesn't end up as its own bogus "dir".
    let mut by_dir: std::collections::HashMap<
        String,
        (u32, u64, Option<DirEntrySource>),
    > = std::collections::HashMap::new();
    let mut root_count: u32 = 0;
    let mut root_bytes: u64 = 0;
    let mut root_source: Option<DirEntrySource> = None;

    for file in &manifest.files {
        let normalized = file.path.replace('\\', "/");
        match normalized.split_once('/') {
            Some((first, _)) => {
                let entry = by_dir
                    .entry(first.to_string())
                    .or_insert_with(|| (0, 0, resolve_source(&manifest.sources, file)));
                entry.0 += 1;
                entry.1 += file.size;
            }
            None => {
                // File sitting at the manifest root with no
                // parent dir. Group as a synthetic "(root)"
                // bucket at the end of the list — uncommon, but
                // worth showing rather than silently dropping.
                if root_count == 0 {
                    root_source = resolve_source(&manifest.sources, file);
                }
                root_count += 1;
                root_bytes += file.size;
            }
        }
    }

    let mut dirs: Vec<PackDirEntry> = by_dir
        .into_iter()
        .map(|(name, (file_count, total_bytes, source))| PackDirEntry {
            name,
            file_count,
            total_bytes,
            source,
        })
        .collect();
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    if root_count > 0 {
        dirs.push(PackDirEntry {
            name: "(root)".to_string(),
            file_count: root_count,
            total_bytes: root_bytes,
            source: root_source,
        });
    }
    dirs
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
        // Deep-link plugin — registers `packrelay://` so the
        // cloud can hand off requests via a single anchor click.
        // Two verbs today:
        //   packrelay://install/<pack-slug>     → InstallView
        //   packrelay://join/<server-slug>      → ServerDetailView
        // The on_open_url handler in setup() below forwards the
        // raw URL string up to the frontend as `deeplink://incoming`;
        // parsing + dispatch lives in App.tsx so adding a new verb
        // doesn't need a Rust rebuild.
        .plugin(tauri_plugin_deep_link::init())
        // Background blob-cache GC: weekly sweep on startup so disk
        // usage doesn't grow forever. Best-effort — failures stay
        // silent so a hiccup here never blocks the UI. Also wires
        // the deep-link handler (same setup pass).
        .setup(|app| {
            // Deep-link handler. Fires for both cold-start
            // (launcher was just opened via the URL) and
            // already-running (URL clicked while launcher is
            // open). We just forward the parsed URL up to the
            // frontend; routing happens in App.tsx.
            use tauri::Emitter;
            use tauri_plugin_deep_link::DeepLinkExt;
            let handle_for_dl = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    let _ = handle_for_dl.emit("deeplink://incoming", url.to_string());
                }
            });

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
            check_pack_present,
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

#[cfg(test)]
mod tests {
    use super::*;
    use packrelay_core::manifest::{FileEntry, Manifest, ManifestSource, Signature};

    fn nexus_source(id: &str, mod_id: u64) -> ManifestSource {
        ManifestSource {
            id: id.to_string(),
            source: "nexus".to_string(),
            game: Some("7daystodie".to_string()),
            mod_id: Some(mod_id),
            file_id: Some(100),
            version: Some("1.0".to_string()),
            owner: None,
            repo: None,
            release_tag: None,
            asset_name: None,
            mod_slug: None,
            upstream_url: None,
        }
    }

    fn github_source(id: &str, owner: &str, repo: &str) -> ManifestSource {
        ManifestSource {
            id: id.to_string(),
            source: "github".to_string(),
            game: None,
            mod_id: None,
            file_id: None,
            version: None,
            owner: Some(owner.to_string()),
            repo: Some(repo.to_string()),
            release_tag: Some("v1.0".to_string()),
            asset_name: Some("mod.zip".to_string()),
            mod_slug: None,
            upstream_url: None,
        }
    }

    fn legacy_source(id: &str) -> ManifestSource {
        ManifestSource {
            id: id.to_string(),
            source: "legacy-blob".to_string(),
            game: None,
            mod_id: None,
            file_id: None,
            version: None,
            owner: None,
            repo: None,
            release_tag: None,
            asset_name: None,
            mod_slug: None,
            upstream_url: None,
        }
    }

    fn file(path: &str, source_ref: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            sha256: "a".repeat(64),
            size: 100,
            executable: None,
            source_ref: source_ref.map(|s| s.to_string()),
        }
    }

    fn build_manifest(sources: Vec<ManifestSource>, files: Vec<FileEntry>) -> Manifest {
        Manifest {
            schema_version: 2,
            name: "test".to_string(),
            display_name: "Test".to_string(),
            version: "1.0.0".to_string(),
            game: "7d2d".to_string(),
            game_version: "1.0".to_string(),
            publisher: "tester".to_string(),
            published_at: "2026-05-19T00:00:00Z".to_string(),
            description: None,
            tags: vec![],
            sources,
            files,
            signature: Signature {
                algo: "ed25519".to_string(),
                public_key_id: "tester/key1".to_string(),
                value: "f".repeat(128),
            },
        }
    }

    // ---- resolve_source ----

    #[test]
    fn resolve_source_returns_none_when_file_has_no_source_ref() {
        let sources = vec![nexus_source("nexus-42-100", 42)];
        let f = file("Mods/Foo/ModInfo.xml", None);
        assert!(resolve_source(&sources, &f).is_none());
    }

    #[test]
    fn resolve_source_returns_none_when_source_ref_unknown() {
        let sources = vec![nexus_source("nexus-42-100", 42)];
        let f = file("Mods/Foo/ModInfo.xml", Some("does-not-exist"));
        assert!(resolve_source(&sources, &f).is_none());
    }

    #[test]
    fn resolve_source_nexus_emits_label_and_url() {
        let sources = vec![nexus_source("nexus-42-100", 42)];
        let f = file("Mods/Foo/ModInfo.xml", Some("nexus-42-100"));
        let out = resolve_source(&sources, &f).expect("Nexus source should resolve");
        assert_eq!(out.kind, "nexus");
        assert_eq!(out.label, "Nexus");
        assert_eq!(
            out.url.as_deref(),
            Some("https://www.nexusmods.com/7daystodie/mods/42")
        );
    }

    #[test]
    fn resolve_source_nexus_uses_game_field_in_url() {
        let mut src = nexus_source("nexus-7-50", 7);
        src.game = Some("stardewvalley".to_string());
        let sources = vec![src];
        let f = file("Mods/Foo/ModInfo.xml", Some("nexus-7-50"));
        let out = resolve_source(&sources, &f).unwrap();
        assert_eq!(
            out.url.as_deref(),
            Some("https://www.nexusmods.com/stardewvalley/mods/7")
        );
    }

    #[test]
    fn resolve_source_github_emits_repo_url() {
        let sources = vec![github_source("gh-foo", "ada", "foo-mod")];
        let f = file("Mods/Foo/ModInfo.xml", Some("gh-foo"));
        let out = resolve_source(&sources, &f).unwrap();
        assert_eq!(out.kind, "github");
        assert_eq!(out.label, "GitHub");
        assert_eq!(
            out.url.as_deref(),
            Some("https://github.com/ada/foo-mod")
        );
    }

    #[test]
    fn resolve_source_legacy_blob_has_no_url() {
        let sources = vec![legacy_source("legacy-old")];
        let f = file("Mods/Foo/ModInfo.xml", Some("legacy-old"));
        let out = resolve_source(&sources, &f).unwrap();
        assert_eq!(out.kind, "legacy-blob");
        assert_eq!(out.label, "self-hosted");
        assert!(out.url.is_none());
    }

    #[test]
    fn renderers_table_covers_every_known_source_kind() {
        // The cloud's manifest schema accepts exactly these four
        // source variants today (see PackRelayCloud
        // src/lib/manifest.ts ~ nexusSourceSchema,
        // githubSourceSchema, sevenDtmSourceSchema,
        // legacyBlobSourceSchema). The launcher's registry must
        // have a renderer for each, otherwise a valid v2 manifest
        // would lose provenance on its files. This test fails
        // loudly if a future cloud release adds a variant the
        // launcher hasn't onboarded yet.
        let known = ["nexus", "github", "7dtm", "legacy-blob"];
        for kind in known {
            assert!(
                RENDERERS.iter().any(|r| r.kind == kind),
                "RENDERERS missing a registration for kind={kind}. \
                 Add a row to the const RENDERERS slice in lib.rs."
            );
        }
    }

    #[test]
    fn renderers_kinds_are_unique() {
        // Two rows with the same kind would mean lookup ambiguity
        // ~ `iter().find()` returns the first match, which is
        // probably not what the author intended. Catch the typo
        // before it lands.
        let mut seen = std::collections::HashSet::new();
        for r in RENDERERS {
            assert!(
                seen.insert(r.kind),
                "Duplicate RENDERERS entry for kind={}",
                r.kind
            );
        }
    }

    #[test]
    fn resolve_source_unknown_variant_returns_none() {
        // Forward-compat: a future source type the launcher doesn't
        // know how to render should not crash deserialization OR
        // emit a half-rendered row. None lets DetailView skip.
        let src = ManifestSource {
            id: "future-1".to_string(),
            source: "neogaf".to_string(),
            game: None,
            mod_id: None,
            file_id: None,
            version: None,
            owner: None,
            repo: None,
            release_tag: None,
            asset_name: None,
            mod_slug: None,
            upstream_url: None,
        };
        let sources = vec![src];
        let f = file("Mods/Foo/ModInfo.xml", Some("future-1"));
        assert!(resolve_source(&sources, &f).is_none());
    }

    // ---- reduce_to_dirs ----

    #[test]
    fn reduce_to_dirs_groups_by_first_path_segment() {
        let manifest = build_manifest(
            vec![],
            vec![
                file("Mods/FooMod/ModInfo.xml", None),
                file("Mods/FooMod/Config.xml", None),
                file("Mods/BarMod/ModInfo.xml", None),
            ],
        );
        let dirs = reduce_to_dirs(&manifest);
        assert_eq!(dirs.len(), 1, "single top-level Mods dir expected");
        let mods = dirs.iter().find(|d| d.name == "Mods").unwrap();
        assert_eq!(mods.file_count, 3);
    }

    #[test]
    fn reduce_to_dirs_attaches_first_file_source() {
        let manifest = build_manifest(
            vec![nexus_source("nexus-42-100", 42)],
            vec![
                file("Mods/FooMod/ModInfo.xml", Some("nexus-42-100")),
                file("Mods/FooMod/Config.xml", Some("nexus-42-100")),
            ],
        );
        let dirs = reduce_to_dirs(&manifest);
        let entry = dirs.iter().find(|d| d.name == "Mods").unwrap();
        let src = entry.source.as_ref().expect("source resolved");
        assert_eq!(src.kind, "nexus");
        assert!(src.url.is_some());
    }

    #[test]
    fn reduce_to_dirs_no_source_when_first_file_has_no_source_ref() {
        let manifest = build_manifest(
            vec![nexus_source("nexus-42-100", 42)],
            // First file lacks sourceRef → entire dir gets no source,
            // because the dir's representative is the first file
            // encountered. Subsequent files don't override.
            vec![
                file("Mods/FooMod/ModInfo.xml", None),
                file("Mods/FooMod/Config.xml", Some("nexus-42-100")),
            ],
        );
        let dirs = reduce_to_dirs(&manifest);
        let entry = dirs.iter().find(|d| d.name == "Mods").unwrap();
        assert!(entry.source.is_none());
    }

    #[test]
    fn reduce_to_dirs_handles_v1_manifest_without_sources() {
        // v1 manifests have no sources[] AND files have no
        // source_ref. The launcher should still produce the dir
        // list without crashing; just no source line in the render.
        let manifest = build_manifest(
            vec![],
            vec![file("Mods/FooMod/ModInfo.xml", None)],
        );
        let dirs = reduce_to_dirs(&manifest);
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].source.is_none());
    }

    #[test]
    fn reduce_to_dirs_normalizes_windows_backslash_paths() {
        let manifest = build_manifest(
            vec![],
            vec![file("Mods\\FooMod\\ModInfo.xml", None)],
        );
        let dirs = reduce_to_dirs(&manifest);
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].name, "Mods");
    }

    #[test]
    fn reduce_to_dirs_groups_root_level_files_into_synthetic_bucket() {
        let manifest = build_manifest(
            vec![],
            vec![
                file("readme.txt", None),
                file("changelog.md", None),
                file("Mods/FooMod/ModInfo.xml", None),
            ],
        );
        let dirs = reduce_to_dirs(&manifest);
        assert_eq!(dirs.len(), 2);
        let root = dirs.iter().find(|d| d.name == "(root)").unwrap();
        assert_eq!(root.file_count, 2);
    }

    #[test]
    fn reduce_to_dirs_sorts_alphabetically_with_root_last() {
        let manifest = build_manifest(
            vec![],
            vec![
                file("Zeta/x.txt", None),
                file("Alpha/x.txt", None),
                file("Mods/x.txt", None),
                file("readme.txt", None),
            ],
        );
        let dirs = reduce_to_dirs(&manifest);
        let names: Vec<&str> = dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "Mods", "Zeta", "(root)"]);
    }

    // --- Windows code-signing regression guards ---
    //
    // These two checks lock in the fixes from the v0.1.9-v0.1.11
    // signing saga. They're plain string assertions over the config
    // files (compiled in via include_str!), so they cost nothing and
    // run inside the normal `cargo test`.

    /// Tauri's bundler only substitutes the artifact path when a
    /// signCommand argument is *exactly* `%1`. Quoting it (`"%1"`)
    /// ships the literal token to signtool, which fails with
    /// "File not found: %1" — the bug behind every failed Windows
    /// release. Keep the placeholder bare.
    #[test]
    fn windows_sign_command_uses_a_bare_placeholder() {
        let conf = include_str!("../tauri.conf.json");
        assert!(
            conf.contains("sign-windows.ps1 %1\""),
            "tauri.conf.json signCommand must pass the artifact path as a bare %1"
        );
        assert!(
            !conf.contains(r#"sign-windows.ps1 \"%1\""#),
            "tauri.conf.json signCommand must not quote %1 — Tauri won't \
             substitute a quoted placeholder"
        );
    }

    /// artifact-signing-cli shells out to `az` and `signtool`, which
    /// write progress to stderr. Tauri runs sign-windows.ps1 under
    /// Windows PowerShell 5.1 with redirected streams; with
    /// ErrorActionPreference=Stop that promotes the first stderr
    /// line to a terminating error and kills the script. The script
    /// must relax the preference before the native call.
    #[test]
    fn sign_script_relaxes_erroractionpreference_for_the_cli_call() {
        let script = include_str!("../sign-windows.ps1");
        assert!(
            script.contains("$ErrorActionPreference = \"Continue\""),
            "sign-windows.ps1 must drop ErrorActionPreference to \
             Continue around the artifact-signing-cli call"
        );
    }
}

