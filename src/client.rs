// HTTP client wrapper for the cloud's v1 endpoints.
//
// One reqwest::Client reused across the launcher's lifetime so we
// keep the connection pool warm — parallel downloads from the file
// endpoint reuse the same TLS sessions.

use anyhow::{Context, Result};
use reqwest::Client as HttpClient;

use crate::manifest::Manifest;

pub struct Client {
    http: HttpClient,
    api_url: String,
}

impl Client {
    pub fn new(api_url: &str) -> Self {
        let http = HttpClient::builder()
            .user_agent(format!(
                "packrelay-launcher/{}",
                env!("CARGO_PKG_VERSION")
            ))
            // Aggressive enough that a stalled CDN edge won't lock the
            // whole install up; gentle enough to ride out brief blips.
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client build");
        Self {
            http,
            api_url: api_url.trim_end_matches('/').to_string(),
        }
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    pub fn file_url(&self, sha256: &str) -> String {
        format!("{}/api/v1/files/{}", self.api_url, sha256)
    }

    /// Fetch the latest signed manifest for a pack slug. Returns both
    /// the typed manifest and the raw JSON bytes — the raw bytes are
    /// what we save to disk for the sidecar, so a future signature
    /// check can re-verify against the exact bytes the server signed.
    pub async fn fetch_manifest(&self, slug: &str) -> Result<(String, Manifest)> {
        let url = format!("{}/api/v1/packs/{}/manifest", self.api_url, slug);
        let res = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "Pack '{slug}' not found, not public, or its latest version is awaiting moderation."
            );
        }
        if !res.status().is_success() {
            anyhow::bail!("Manifest fetch failed: HTTP {}", res.status());
        }
        let raw = res
            .text()
            .await
            .with_context(|| "reading manifest body")?;
        let manifest: Manifest = serde_json::from_str(&raw)
            .with_context(|| "parsing manifest JSON")?;
        Ok((raw, manifest))
    }
}
