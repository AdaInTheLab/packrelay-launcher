// Launcher auth state — pasted-Bearer-token model.
//
// Today's flow: user visits packrelay.cloud/account/tokens, mints
// a token, pastes it into the launcher. We validate it once
// against /api/v1/me, then persist it for future sessions.
//
// Storage is a plain JSON file in Tauri's app-data dir. Not the OS
// keychain — explicitly a v0 tradeoff: gets the data flow working
// without pulling in tauri-plugin-stronghold or a platform-specific
// secret store, with the understanding that anyone with read access
// to the user's profile dir also has the token. The token can be
// revoked from the website, so the blast radius is bounded.
//
// Future iterations replace this with stronghold / OS keychain
// without changing the Tauri command surface this module exposes.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tokio::fs;

/// Profile returned by GET /api/v1/me. Mirrors the cloud response
/// shape verbatim; serde renames are unnecessary because the
/// endpoint already uses camelCase on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeResponse {
    pub id: String,
    pub display_name: String,
    pub role: String,
    pub plan: String,
    pub image: Option<String>,
}

/// What the frontend listens to for "am I signed in?". Two
/// terminal states; the absence-of-token case is "out", anything
/// else is "in" with the corresponding profile.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum AuthState {
    SignedOut,
    SignedIn { token: String, user: MeResponse },
}

/// Where we persist the auth payload between sessions. Tauri's
/// app_data_dir() resolves to the standard per-user dir for the
/// host OS (e.g. %APPDATA%\packrelay-app on Windows).
fn auth_file_path(app: &AppHandle) -> Result<PathBuf> {
    let dir = app
        .path()
        .app_data_dir()
        .context("resolving app data dir")?;
    Ok(dir.join("auth.json"))
}

/// On-disk payload. Wrapping the token in a struct (vs. storing
/// just the bare string) makes future field additions (cached
/// profile, token expiry, refresh tokens) a non-breaking change.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAuth {
    token: String,
}

pub async fn load_stored_token(app: &AppHandle) -> Result<Option<String>> {
    let path = auth_file_path(app)?;
    match fs::read_to_string(&path).await {
        Ok(raw) => {
            let stored: StoredAuth = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(stored.token))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub async fn save_token(app: &AppHandle, token: &str) -> Result<()> {
    let path = auth_file_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&StoredAuth {
        token: token.to_string(),
    })?;
    fs::write(&path, body)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub async fn clear_stored_token(app: &AppHandle) -> Result<()> {
    let path = auth_file_path(app)?;
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Validate a token by calling /api/v1/me. Returns the profile on
/// success; any non-200 (incl. 401 for a bad token) bubbles up as
/// an error the frontend can show inline next to the paste field.
pub async fn validate_token(api_url: &str, token: &str) -> Result<MeResponse> {
    let http = reqwest::Client::builder()
        .user_agent(format!(
            "packrelay-launcher/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let url = format!("{api_url}/api/v1/me");
    let res = http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = res.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("That token isn't valid (or it's been revoked).");
    }
    if !status.is_success() {
        anyhow::bail!("Token validation failed: HTTP {status}");
    }
    let me: MeResponse = res
        .json()
        .await
        .with_context(|| "parsing /me response")?;
    Ok(me)
}
