// An update where a file's bytes move to a new path and the old path
// gets new bytes. KitsuneWashitsu 0.4.1 -> 0.7.0 did exactly that: it
// renumbered its atlases, so Atlas_004's bytes became Atlas_009 and
// Atlas_004 got new art. A player who updated kitsune-casual through
// the launcher (2026-11-25) ended up with the same bytes at both
// paths, and 7DTD refused the duplicate asset bundles.
//
// The cause: with the blob cache on (the app always sets it), every
// installed file is a hard link to its cache blob. The download path
// wrote a changed file with File::create on the existing target, which
// truncates and rewrites the one file all those links share. So
// updating Atlas_004 rewrote the cached blob of its OLD sha with the
// new bytes, and Atlas_009, linked from that blob, got the new bytes
// too. Any later install or repair of the old sha linked the bad blob
// without checking it.
//
// These tests run the app's setup: cache and active-profile mirror both
// on. Manifests are signed here with the fixture's signer (the seed
// gen-signing-fixture.mts uses), so the update can be shaped freely.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use packrelay_core::blob_cache::blob_path;
use packrelay_core::canonical_json::canonicalize;
use packrelay_core::client::Client;
use packrelay_core::install::{install, InstallContext};
use packrelay_core::update::update;
use packrelay_core::verify::{repair, verify};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

mod common;
use common::{serve, TempDir};

const SLUG: &str = "fixture-moved";
const ATLAS_004: &str = "Mods/KitsuneWashitsu/Resources/Atlas_004.unity3d";
const ATLAS_009: &str = "Mods/KitsuneWashitsu/Resources/Atlas_009.unity3d";
const MODINFO: &str = "Mods/KitsuneWashitsu/ModInfo.xml";

// Same length on purpose: real atlases of one size can't be told apart
// by a size check, so nothing here may lean on one.
const ART_V1: &str = "atlas art from Washitsu 0.4.1\n";
const ART_V2: &str = "atlas art from Washitsu 0.7.0\n";
const INFO_V1: &str = "<Version value=\"0.4.1\" />\n";
const INFO_V2: &str = "<Version value=\"0.7.0\" />\n";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    key_id: String,
    public_key: String,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/signing-fixture.json")).unwrap()
}

/// The fixture's signing key, from the seed gen-signing-fixture.mts
/// uses for its signer.
fn signer(f: &Fixture) -> SigningKey {
    let key = SigningKey::from_bytes(&[0x11; 32]);
    let public = base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
    assert_eq!(public, f.public_key, "fixture signer changed");
    key
}

fn sha(body: &str) -> String {
    hex::encode(Sha256::digest(body.as_bytes()))
}

/// A signed manifest for `version` with `files` as (path, contents).
fn manifest(f: &Fixture, version: &str, files: &[(&str, &str)]) -> String {
    let mut m = json!({
        "schemaVersion": 2,
        "name": SLUG,
        "displayName": "Fixture Moved Content",
        "version": version,
        "game": "7d2d",
        "gameVersion": "V 2.4",
        "publisher": "Fixture Publisher",
        "publishedAt": "2026-11-25T12:00:00Z",
        "files": files
            .iter()
            .map(|(path, body)| json!({ "path": path, "sha256": sha(body), "size": body.len() }))
            .collect::<Vec<_>>(),
    });
    let sig = signer(f).sign(canonicalize(&m).unwrap().as_bytes());
    m["signature"] = json!({
        "algo": "ed25519",
        "publicKeyId": f.key_id,
        "value": hex::encode(sig.to_bytes()),
    });
    m.to_string()
}

/// Washitsu 0.4.1: Atlas_004 holds the old art.
fn v1(f: &Fixture) -> String {
    manifest(f, "0.6.33", &[(MODINFO, INFO_V1), (ATLAS_004, ART_V1)])
}

/// Washitsu 0.7.0: the old art moved to Atlas_009, new art at Atlas_004.
fn v2(f: &Fixture) -> String {
    manifest(
        f,
        "0.6.36",
        &[(MODINFO, INFO_V2), (ATLAS_004, ART_V2), (ATLAS_009, ART_V1)],
    )
}

/// A fake cloud serving `manifest` as the pack's latest, its key, and
/// `files` (contents, served by sha256).
async fn cloud(f: &Fixture, manifest: &str, files: &[&str]) -> Client {
    let mut routes = vec![
        (
            format!("/api/v1/packs/{SLUG}/manifest"),
            manifest.to_string(),
        ),
        (
            format!("/api/v1/keys/{}", f.key_id.replace('/', "%2F")),
            json!({
                "keyId": f.key_id,
                "publicKey": f.public_key,
                "algorithm": "ed25519",
                "createdAt": "2026-09-30T00:00:00.000Z",
                "revokedAt": null,
            })
            .to_string(),
        ),
    ];
    let bodies: BTreeMap<String, &str> = files.iter().map(|b| (sha(b), *b)).collect();
    for (sha, body) in bodies {
        routes.push((format!("/api/v1/files/{sha}"), body.to_string()));
    }
    Client::new(&serve(routes).await)
}

/// The app's layout: the game's Mods folder, the blob cache, and the
/// active profile's copy of the pack, all on one volume.
struct Setup {
    root: TempDir,
    dest: PathBuf,
    cache: PathBuf,
    profile_mods: PathBuf,
}

impl Setup {
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        let dest = root.path().join("7DaysToDie");
        let cache = root.path().join("store").join("cache");
        let profile_mods = root
            .path()
            .join("store")
            .join("profiles")
            .join("p1")
            .join("packs")
            .join(SLUG)
            .join("mods");
        Self {
            root,
            dest,
            cache,
            profile_mods,
        }
    }

    fn ctx(&self) -> InstallContext {
        InstallContext {
            cache_root: Some(self.cache.clone()),
            profile_mods: Some(self.profile_mods.clone()),
            ..InstallContext::default()
        }
    }
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).unwrap()
}

async fn assert_healthy(dir: &Path) {
    let report = verify(dir).await.unwrap();
    let failed: Vec<&str> = report.failures.iter().map(|f| f.path()).collect();
    assert!(
        failed.is_empty(),
        "{} fails verify: {failed:?}",
        dir.display()
    );
}

#[tokio::test]
async fn update_keeps_bytes_that_moved_to_a_new_path() {
    let f = fixture();
    let s = Setup::new("moved-update");
    install(
        &cloud(&f, &v1(&f), &[INFO_V1, ART_V1]).await,
        SLUG,
        &s.dest,
        4,
        None,
        s.ctx(),
        |_| {},
    )
    .await
    .unwrap();

    let report = update(
        &cloud(&f, &v2(&f), &[INFO_V2, ART_V2, ART_V1]).await,
        SLUG,
        &s.dest,
        4,
        None,
        s.ctx(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.files_changed, 2, "ModInfo.xml and Atlas_004");
    assert_eq!(report.files_added, 1, "Atlas_009");

    for dir in [&s.dest, &s.profile_mods] {
        assert_eq!(read(dir, ATLAS_004), ART_V2, "{}", dir.display());
        assert_eq!(read(dir, ATLAS_009), ART_V1, "{}", dir.display());
        assert_eq!(read(dir, MODINFO), INFO_V2, "{}", dir.display());
        assert_healthy(dir).await;
    }
    // The cache still holds the old art under the old art's sha256.
    assert_eq!(
        std::fs::read_to_string(blob_path(&s.cache, &sha(ART_V1))).unwrap(),
        ART_V1
    );
}

#[tokio::test]
async fn update_leaves_the_old_version_intact_in_the_cache() {
    // The general form, without a moved file: changing a file in place
    // must not change what the cache holds for its old sha256, or the
    // next install of the old version (a server pinned to it, another
    // profile) gets the new bytes under the old name.
    let f = fixture();
    let s = Setup::new("moved-rollback");
    install(
        &cloud(&f, &v1(&f), &[INFO_V1, ART_V1]).await,
        SLUG,
        &s.dest,
        4,
        None,
        s.ctx(),
        |_| {},
    )
    .await
    .unwrap();
    update(
        &cloud(&f, &v2(&f), &[INFO_V2, ART_V2, ART_V1]).await,
        SLUG,
        &s.dest,
        4,
        None,
        s.ctx(),
        |_| {},
    )
    .await
    .unwrap();

    // Install v1 elsewhere from the cache alone: this cloud serves no
    // files, so every byte has to come from the cache.
    let other = s.root.path().join("pinned-server");
    install(
        &cloud(&f, &v1(&f), &[]).await,
        SLUG,
        &other,
        4,
        None,
        InstallContext {
            cache_root: Some(s.cache.clone()),
            ..InstallContext::default()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(read(&other, ATLAS_004), ART_V1);
    assert_eq!(read(&other, MODINFO), INFO_V1);
    assert_healthy(&other).await;
}

#[tokio::test]
async fn repair_replaces_a_cached_blob_whose_bytes_are_wrong() {
    // A player's machine after an older launcher did the bad update:
    // the cache blob named for the old art holds the new art, and so
    // does Atlas_009, which is linked to it. Repair has to fetch the
    // old art again rather than link the bad blob back in.
    let f = fixture();
    let s = Setup::new("moved-repair");
    let client = cloud(&f, &v2(&f), &[INFO_V2, ART_V2, ART_V1]).await;
    install(&client, SLUG, &s.dest, 4, None, s.ctx(), |_| {})
        .await
        .unwrap();
    let blob = blob_path(&s.cache, &sha(ART_V1));
    // Written in place, as the old download path did: every hard link
    // to this blob sees the new bytes.
    std::fs::write(&blob, ART_V2).unwrap();
    assert_eq!(
        read(&s.dest, ATLAS_009),
        ART_V2,
        "dest is linked to the blob"
    );

    let report = repair(&client, &s.dest, s.ctx()).await.unwrap();
    assert_eq!(report.files_repaired, 1);
    assert_eq!(read(&s.dest, ATLAS_009), ART_V1);
    assert_eq!(read(&s.dest, ATLAS_004), ART_V2);
    assert_healthy(&s.dest).await;
    assert_eq!(std::fs::read_to_string(&blob).unwrap(), ART_V1);
}
