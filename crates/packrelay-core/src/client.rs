// HTTP client wrapper for the cloud's v1 endpoints.
//
// One reqwest::Client reused across the launcher's lifetime so we
// keep the connection pool warm — parallel downloads from the file
// endpoint reuse the same TLS sessions.

use anyhow::{Context, Result};
use reqwest::Client as HttpClient;

use crate::manifest::{parse_manifest, Manifest};
use crate::signature::{is_valid_key_id, verify_manifest_signature, PublisherKey};

pub struct Client {
    http: HttpClient,
    api_url: String,
}

/// A manifest whose signature has been checked, with the key that
/// signed it.
#[derive(Debug, Clone)]
pub struct VerifiedManifest {
    /// The exact bytes the cloud served (what the sidecar stores).
    pub raw: String,
    pub manifest: Manifest,
    pub key: PublisherKey,
}

impl Client {
    pub fn new(api_url: &str) -> Self {
        let http = HttpClient::builder()
            .user_agent(format!("packrelay-launcher/{}", env!("CARGO_PKG_VERSION")))
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

    /// A game's mod loader, as the cloud re-hosts it (framework.rs).
    pub fn framework_url(&self, id: &str, version: &str) -> String {
        format!("{}/api/v1/frameworks/{}/{}", self.api_url, id, version)
    }

    /// Fetch the latest signed manifest for a pack slug. Returns both
    /// the typed manifest and the raw JSON bytes — the raw bytes are
    /// what we save to disk for the sidecar, so the signature can be
    /// re-checked later against the exact bytes the publisher signed.
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
    ///
    /// Fails closed: the manifest is only returned once its schema
    /// version and game are ones this launcher supports, its Ed25519
    /// signature verifies against the publisher's key, and its signed
    /// `name` is the slug that was asked for. Nothing gets installed
    /// from a manifest that fails any of those.
    pub async fn fetch_manifest_at(
        &self,
        slug: &str,
        version: Option<&str>,
    ) -> Result<(String, Manifest)> {
        let verified = self.fetch_verified_manifest_at(slug, version).await?;
        Ok((verified.raw, verified.manifest))
    }

    /// `fetch_manifest_at`, plus the publisher key the signature was
    /// verified against. Install and update need the key to check it
    /// against the pack's pinned keys (see key_pins.rs).
    pub async fn fetch_verified_manifest_at(
        &self,
        slug: &str,
        version: Option<&str>,
    ) -> Result<VerifiedManifest> {
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
        let raw = res.text().await.with_context(|| "reading manifest body")?;
        let refusing = || format!("Refusing to install '{slug}'");
        let (value, manifest) = parse_manifest(&raw).with_context(refusing)?;
        let key = self
            .fetch_publisher_key(&manifest.signature.public_key_id)
            .await
            .with_context(refusing)?;
        verify_manifest_signature(&value, &manifest.signature, &key).with_context(refusing)?;
        // The signature says who signed this manifest, not which pack
        // it's for: without this, the cloud could serve another pack's
        // perfectly valid signed manifest under this slug. The cloud
        // enforces name == slug at publish time, so they only differ
        // if something is wrong, or the pack's slug was renamed after
        // this version was signed.
        if manifest.name != slug {
            anyhow::bail!(
                "Refusing to install '{slug}': the cloud served the signed manifest \
                 for '{}' instead. If the pack was renamed, its publisher needs to \
                 publish a new version under the new name.",
                manifest.name
            );
        }
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
        Ok(VerifiedManifest { raw, manifest, key })
    }

    /// Resolve a manifest's `signature.publicKeyId` to the publisher's
    /// verifying key via the cloud's public key directory.
    pub async fn fetch_publisher_key(&self, key_id: &str) -> Result<PublisherKey> {
        if !is_valid_key_id(key_id) {
            anyhow::bail!("manifest names an invalid signing key id '{key_id}'");
        }
        // The key route is a single [keyId] segment, so the "/" inside
        // "<publisher>/<key-name>" has to travel as %2F. is_valid_key_id
        // guarantees nothing else needs encoding.
        let url = format!(
            "{}/api/v1/keys/{}",
            self.api_url,
            key_id.replace('/', "%2F")
        );
        let res = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "signing key '{key_id}' isn't registered with PackRelay, so the \
                 manifest's signature can't be checked"
            );
        }
        if !res.status().is_success() {
            anyhow::bail!(
                "couldn't fetch signing key '{key_id}': HTTP {}",
                res.status()
            );
        }
        res.json::<PublisherKey>()
            .await
            .with_context(|| format!("parsing signing key '{key_id}'"))
    }
}
