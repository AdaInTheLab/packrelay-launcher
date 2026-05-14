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

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use packrelay_core::client::Client;
use packrelay_core::install::{install, InstallReport, ProgressEvent};

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

#[tauri::command]
async fn list_packs() -> Result<Vec<CatalogPack>, String> {
    let http = reqwest::Client::builder()
        .user_agent(concat!(
            "packrelay-launcher/",
            env!("CARGO_PKG_VERSION")
        ))
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![list_packs, install_pack])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
