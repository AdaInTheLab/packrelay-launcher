// Manifest types — mirror the schema enforced server-side by the
// cloud's parseManifest() / packUpsertSchema. Field names use
// camelCase on the wire (Zod's default), so we serde-rename here.
//
// We derive both Deserialize (to parse from the API) AND Serialize
// (to write the sidecar copy to disk). For signature-verify use cases
// later we'll preserve the original bytes alongside, since a re-
// serialization can change byte order / whitespace and break the
// cryptographic equality check.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub game: String,
    pub game_version: String,
    pub publisher: String,
    pub published_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Source provenance catalog (v2+ manifests). Empty / absent on
    /// v1 manifests; the launcher treats those as legacy-blob.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<ManifestSource>,
    pub files: Vec<FileEntry>,
    pub signature: Signature,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub path: String,
    /// Lowercase hex SHA-256 of the file's bytes.
    pub sha256: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<bool>,
    /// Pointer into the manifest's top-level `sources` array. Only
    /// set on v2 manifests; absent = legacy-blob equivalent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}

/// One entry in a v2 manifest's sources catalog. Flat-optional
/// design so deserialization never fails on a variant the launcher
/// doesn't fully understand yet ~ #154 ships Nexus rendering, #162
/// will add GitHub, #170 will add 7DTM. Until then, the unknown
/// variants still round-trip cleanly.
///
/// The discriminator is the `source` string; variant-specific
/// fields are all Option so any combination deserializes.
///
/// camelCase JSON: serde renames `mod_id` -> `modId`, `file_id` ->
/// `fileId`, etc., matching the cloud's wire format.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestSource {
    /// Slug-shaped identifier that files reference via sourceRef.
    pub id: String,
    /// Variant discriminator: "nexus" | "github" | "7dtm" | "legacy-blob".
    pub source: String,

    // ---- Nexus fields ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    // ---- GitHub fields ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_name: Option<String>,

    // ---- 7DTM fields ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    /// Always "ed25519" today; the field exists so we can rev the
    /// algorithm without breaking older launchers that hard-coded it.
    pub algo: String,
    /// "<publisherSlug>/<keyName>" — globally unique key identifier
    /// the launcher can look up via /api/v1/keys/[keyId] when we
    /// implement signature verification.
    pub public_key_id: String,
    /// Hex-encoded Ed25519 signature (64 bytes → 128 hex chars) over
    /// the canonical JSON of the manifest with its signature field
    /// removed.
    pub value: String,
}
