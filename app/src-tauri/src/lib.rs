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
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;

use crate::auth::{
    clear_stored_token, load_stored_token, save_token, validate_token, AuthState,
};
use packrelay_core::client::Client;
use packrelay_core::install::{install, InstallReport, ProgressEvent};
use packrelay_core::uninstall::{uninstall, UninstallReport};
use packrelay_core::update::{update, UpdateReport};
use packrelay_core::verify::{repair, verify, RepairReport, VerifyReport};

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

    let report = install(&client, &slug, &dest_path, 8, move |ev: ProgressEvent| {
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

    let report = update(&client, &slug, &dest_path, 8, move |ev: ProgressEvent| {
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
async fn repair_pack(dest: String) -> Result<RepairReport, String> {
    let client = Client::new(DEFAULT_API_URL);
    repair(&client, &PathBuf::from(dest))
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Remove an installed pack from disk. Reads the sidecar manifest
/// at `dest` to learn which files to delete, sweeps empty dirs the
/// pack created, and returns a structured report including any
/// per-file failures (locked, read-only, etc.) so the frontend can
/// surface them without re-querying the filesystem.
#[tauri::command]
async fn uninstall_pack(dest: String) -> Result<UninstallReport, String> {
    uninstall(&PathBuf::from(dest))
        .await
        .map_err(|e| format!("{e:#}"))
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

/// Open 7DTD via the Steam protocol. We deliberately don't pass a
/// connect address through — 7DTD's client doesn't accept one
/// cleanly from the command line, and our copy-to-clipboard flow
/// is the reliable handoff. Steam itself takes care of finding +
/// validating the install.
#[tauri::command]
async fn launch_game(app: AppHandle) -> Result<(), String> {
    let url = format!("steam://rungameid/{SEVEN_DAYS_STEAM_APPID}");
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("failed to launch 7DTD via Steam: {e}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
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
            sign_out
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
