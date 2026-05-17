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
    ///
    /// Equivalent to `fetch_manifest_at(slug, None)`. Kept as a
    /// thin wrapper for the call sites that genuinely want "whatever
    /// the publisher says is latest right now" (e.g. the browse view's
    /// detail prefetch, the smart-update no-op probe).
    pub async fn fetch_manifest(&self, slug: &str) -> Result<(String, Manifest)> {
        self.fetch_manifest_at(slug, None).await
    }

    /// Fetch a *specific* signed manifest version when `version` is
    /// set, falling back to the publisher's current latest when it's
    /// `None`. The version path covers the case where a server pins
    /// a particular pack version (`servers.attachedVersion` on the
    /// cloud) — without this, the launcher would silently install
    /// latest and the player would get kicked again for a version
    /// mismatch on first connect.
    ///
    /// 404 maps to a friendlier error than "HTTP 404" because the
    /// two common causes have very different fixes (server-pin
    /// pointing at a deleted version vs. publisher hasn't shipped
    /// a public version yet).
    pub async fn fetch_manifest_at(
        &self,
        slug: &str,
        version: Option<&str>,
    ) -> Result<(String, Manifest)> {
        let url = match version {
            Some(v) => format!("{}/api/v1/packs/{}/manifest/{}", self.api_url, slug, v),
            None => format!("{}/api/v1/packs/{}/manifest", self.api_url, slug),
        };
        let res = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            match version {
                Some(v) => anyhow::bail!(
                    "Version v{v} of '{slug}' isn't available. The server may be \
                     pinned to a withdrawn or unpublished version — ask the server \
                     admin to refresh the pin, or open the pack page to see the \
                     versions currently published."
                ),
                None => anyhow::bail!(
                    "Pack '{slug}' not found, not public, or its latest version is \
                     awaiting moderation."
                ),
            }
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
        // Thicc check: if the caller asked for a specific version,
        // the manifest we got back had better be that version. A
        // mismatch here would be a cloud-side bug (wrong row served)
        // but the player-facing failure mode is the same as a silent
        // latest-fallback, so we'd rather error loudly here than
        // download 5GB of the wrong pack.
        if let Some(want) = version {
            if manifest.version != want {
                anyhow::bail!(
                    "Cloud returned manifest v{} when v{want} was requested for \
                     '{slug}' — refusing to install a mismatched version.",
                    manifest.version
                );
            }
        }
        Ok((raw, manifest))
    }
}
