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
    pub files: Vec<FileEntry>,
    pub signature: Signature,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileEntry {
    pub path: String,
    /// Lowercase hex SHA-256 of the file's bytes.
    pub sha256: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<bool>,
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
