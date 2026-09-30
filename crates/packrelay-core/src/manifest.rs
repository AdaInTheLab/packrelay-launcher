// Manifest types — mirror the schema enforced server-side by the
// cloud's parseManifest() / packUpsertSchema. Field names use
// camelCase on the wire (Zod's default), so we serde-rename here.
//
// We derive both Deserialize (to parse from the API) AND Serialize
// (to write the sidecar copy to disk). Signature checks never go
// through these types: they run on the manifest's raw JSON value
// (see signature.rs), because the structs drop fields they don't
// model and re-serializing them changes the signed bytes.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::games::{game_by_id, GameLayout, SEVEN_DAYS};

/// Manifest schema versions this launcher understands. Matches the
/// cloud's discriminated union in parseManifest(). v1 and v2 are 7DTD
/// only; v3 is multi-game (PackRelayCloud docs/multi-game/DESIGN.md §4.1).
pub const SUPPORTED_SCHEMA_VERSIONS: [u64; 3] = [1, 2, 3];

/// Parse manifest JSON as served by the cloud, refusing schema
/// versions and games this launcher doesn't support: a game must be one
/// games.rs has a layout for (a pack for anything else would be written
/// into some other game's folder), and a v1/v2 manifest must be 7DTD's,
/// as the cloud guarantees. Returns the raw JSON value (what the
/// signature covers) alongside the typed view.
///
/// This does NOT check the signature — see
/// `signature::verify_manifest_signature`, which `Client::
/// fetch_manifest_at` runs on the result.
pub fn parse_manifest(raw: &str) -> Result<(Value, Manifest)> {
    let value: Value = serde_json::from_str(raw).context("parsing manifest JSON")?;
    let schema = match value.get("schemaVersion").and_then(Value::as_u64) {
        Some(v) if SUPPORTED_SCHEMA_VERSIONS.contains(&v) => v,
        Some(v) => bail!(
            "this pack uses manifest schema version {v}, which this launcher \
             version doesn't support. Update the launcher and try again."
        ),
        None => bail!("manifest has no valid schemaVersion"),
    };
    let game = match value.get("game").and_then(Value::as_str) {
        Some(id) => match game_by_id(id) {
            Some(game) => game,
            None => {
                // Untrusted text going into a user-facing message; keep it short.
                let id: String = id.chars().take(60).collect();
                bail!("this pack is for {id}, which this launcher version doesn't support")
            }
        },
        None => bail!("manifest doesn't say which game it's for"),
    };
    if schema < 3 && game.id != SEVEN_DAYS.id {
        bail!(
            "this pack claims {} on manifest schema version {schema}, which is \
             7 Days to Die only",
            game.display_name
        );
    }
    let manifest = Manifest::deserialize(&value).context("parsing manifest JSON")?;
    Ok((value, manifest))
}

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
    /// The mod loader the pack runs on (v3; BepInExPack for Valheim),
    /// installed into the game before the pack (framework.rs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<Framework>,
    pub files: Vec<FileEntry>,
    pub signature: Signature,
}

impl Manifest {
    /// The game's layout. parse_manifest refuses a game without one, so
    /// this only falls back for a Manifest built some other way.
    pub fn game_layout(&self) -> &'static GameLayout {
        game_by_id(&self.game).unwrap_or(&SEVEN_DAYS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Framework {
    /// The cloud's framework id, e.g. "bepinexpack-valheim".
    pub id: String,
    pub version: String,
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

    // ---- Thunderstore fields (v3; `version` is shared above) ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub community: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Signature {
    /// Always "ed25519" today; the field exists so we can rev the
    /// algorithm without breaking older launchers that hard-coded it.
    pub algo: String,
    /// "<publisherSlug>/<keyName>" — globally unique key identifier,
    /// resolved to key bytes via /api/v1/keys/[keyId].
    pub public_key_id: String,
    /// Hex-encoded Ed25519 signature (64 bytes → 128 hex chars) over
    /// the canonical JSON of the manifest with its signature field
    /// removed.
    pub value: String,
}
