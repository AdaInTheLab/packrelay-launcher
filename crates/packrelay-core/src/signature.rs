// Manifest signature verification — the launcher's half of the
// cloud's verifyManifestSignature() in PackRelayCloud's
// src/lib/manifest.ts.
//
// Contract: Ed25519 over the canonical-JSON bytes (see
// canonical_json.rs) of the manifest with its `signature` field
// removed. `signature.publicKeyId` is "<publisher>/<key-name>" and
// resolves to a key via GET /api/v1/keys/{keyId} (Client::
// fetch_publisher_key). `signature.value` is 128 hex chars.
//
// We verify against the manifest's parsed JSON *value*, not the typed
// `Manifest` struct: the struct drops fields it doesn't model (source
// sha256, scanAttestation, discoveredVia, ...), and re-serializing it
// would sign-check different bytes than the publisher signed.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use ed25519_dalek::{Signature as EdSignature, VerifyingKey};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::canonical_json::canonicalize;
use crate::manifest::Signature;

/// A publisher's verifying key, as served by `GET /api/v1/keys/{keyId}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherKey {
    pub key_id: String,
    /// Base64 of the raw 32-byte Ed25519 verifying key.
    pub public_key: String,
    pub algorithm: String,
    /// Set once the publisher revokes the key. The cloud leaves
    /// already-published manifests in the catalog and says launchers
    /// should refuse to verify against revoked keys, so we do.
    #[serde(default)]
    pub revoked_at: Option<String>,
}

/// Same shape the cloud enforces on `signature.publicKeyId`:
/// `^[a-z0-9-]+/[a-z0-9-]+$`. Checked before the id goes into a URL.
pub fn is_valid_key_id(key_id: &str) -> bool {
    let part_ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    matches!(key_id.split_once('/'), Some((publisher, name)) if part_ok(publisher) && part_ok(name))
}

/// Check `signature` over `manifest` (the parsed JSON of the manifest
/// as served, signature field included) against `key`. Any failure,
/// including an unusable key or signature, is an error: callers must
/// not install from a manifest this rejects.
pub fn verify_manifest_signature(
    manifest: &Value,
    signature: &Signature,
    key: &PublisherKey,
) -> Result<()> {
    if signature.algo != "ed25519" {
        bail!(
            "manifest is signed with '{}', which this launcher version can't verify",
            signature.algo
        );
    }
    if key.key_id != signature.public_key_id {
        bail!(
            "the cloud returned key '{}' when asked for signing key '{}'",
            key.key_id,
            signature.public_key_id
        );
    }
    if key.algorithm != "ed25519" {
        bail!(
            "signing key '{}' is a '{}' key, not ed25519",
            key.key_id,
            key.algorithm
        );
    }
    if let Some(revoked_at) = &key.revoked_at {
        bail!(
            "signing key '{}' was revoked by its publisher ({revoked_at}); \
             manifests signed with it are no longer trusted",
            key.key_id
        );
    }

    let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(&key.public_key)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("signing key '{}' isn't a 32-byte Ed25519 key", key.key_id))?;
    let verifying_key = VerifyingKey::from_bytes(&key_bytes)
        .with_context(|| format!("signing key '{}' isn't a valid Ed25519 key", key.key_id))?;

    let sig_bytes: [u8; 64] = hex::decode(&signature.value)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("manifest signature isn't 64 bytes of hex"))?;
    let sig = EdSignature::from_bytes(&sig_bytes);

    let fields = manifest
        .as_object()
        .ok_or_else(|| anyhow!("manifest isn't a JSON object"))?;
    let unsigned: Map<String, Value> = fields
        .iter()
        .filter(|(k, _)| k.as_str() != "signature")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let message = canonicalize(&Value::Object(unsigned))?;

    verifying_key
        .verify_strict(message.as_bytes(), &sig)
        .map_err(|_| {
            anyhow!(
                "manifest signature doesn't match publisher key '{}' — it was \
                 changed after signing, or signed with a different key",
                key.key_id
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_id_shape_matches_the_cloud() {
        assert!(is_valid_key_id("good-times/main-2026"));
        for bad in [
            "",
            "no-slash",
            "/name",
            "pub/",
            "Pub/name",
            "pub/na/me",
            "pub/na%2Fme",
            "pub/../x",
            "pub/name ",
        ] {
            assert!(!is_valid_key_id(bad), "{bad:?} should be rejected");
        }
    }
}
