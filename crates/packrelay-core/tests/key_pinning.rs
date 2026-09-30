// Key pinning end to end: real install() / update() runs against a
// local fake cloud serving manifests the cloud's own code signed (see
// fixtures/gen-signing-fixture.mts, `e2e`). fixture-e2e v1.0.0 and
// v1.1.0 are signed with the pack's usual key; `v110Rotated` is the
// same v1.1.0 signed with a different key of the same publisher.

use std::collections::BTreeMap;
use std::path::Path;

use packrelay_core::client::Client;
use packrelay_core::install::{install, InstallContext};
use packrelay_core::key_pins::{KeyChanged, KeyPinStore, KnownKey, TrustedKey};
use packrelay_core::update::update;
use serde::Deserialize;
use serde_json::json;

mod common;
use common::{serve, TempDir};

const SLUG: &str = "fixture-e2e";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    key_id: String,
    public_key: String,
    e2e: E2e,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct E2e {
    rotated_key_id: String,
    rotated_public_key: String,
    files: BTreeMap<String, String>,
    manifests: E2eManifests,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct E2eManifests {
    v100: String,
    v110: String,
    v110_rotated: String,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/signing-fixture.json")).unwrap()
}

/// A fake cloud serving `manifest` as fixture-e2e's latest, both
/// fixture keys, and the pack's files.
async fn cloud(f: &Fixture, manifest: &str) -> Client {
    let key = |key_id: &str, public_key: &str| {
        (
            format!("/api/v1/keys/{}", key_id.replace('/', "%2F")),
            json!({
                "keyId": key_id,
                "publicKey": public_key,
                "algorithm": "ed25519",
                "createdAt": "2026-09-30T00:00:00.000Z",
                "revokedAt": null,
            })
            .to_string(),
        )
    };
    let mut routes = vec![
        (format!("/api/v1/packs/{SLUG}/manifest"), manifest.to_string()),
        key(&f.key_id, &f.public_key),
        key(&f.e2e.rotated_key_id, &f.e2e.rotated_public_key),
    ];
    for (sha, body) in &f.e2e.files {
        routes.push((format!("/api/v1/files/{sha}"), body.clone()));
    }
    Client::new(&serve(routes).await)
}

fn pinned(pins: &KeyPinStore, trust_key: Option<TrustedKey>) -> InstallContext {
    InstallContext {
        key_pins: Some(pins.clone()),
        trust_key,
        ..Default::default()
    }
}

fn installed_version(dest: &Path) -> String {
    let raw = std::fs::read_to_string(dest.join("_packrelay-manifest.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&raw).unwrap()["version"]
        .as_str()
        .unwrap()
        .to_string()
}

fn key_changed(err: anyhow::Error) -> KeyChanged {
    err.downcast::<KeyChanged>()
        .unwrap_or_else(|e| panic!("expected KeyChanged, got: {e:#}"))
}

#[tokio::test]
async fn first_install_pins_and_same_key_updates_pass() {
    let f = fixture();
    let (dest, data) = (TempDir::new("dest"), TempDir::new("data"));
    let pins = KeyPinStore::in_dir(data.path());

    let client = cloud(&f, &f.e2e.manifests.v100).await;
    install(&client, SLUG, dest.path(), 2, None, pinned(&pins, None), |_| {})
        .await
        .unwrap();
    let pinned_keys = pins.pins_for(SLUG).await.unwrap();
    assert_eq!(pinned_keys.len(), 1);
    assert_eq!(pinned_keys[0].key_id, f.key_id);
    assert_eq!(pinned_keys[0].public_key, f.public_key);

    let client = cloud(&f, &f.e2e.manifests.v110).await;
    let report = update(&client, SLUG, dest.path(), 2, None, pinned(&pins, None), |_| {})
        .await
        .unwrap();
    assert_eq!(report.to_version, "1.1.0");
    assert_eq!(pins.pins_for(SLUG).await.unwrap().len(), 1);
}

#[tokio::test]
async fn rotated_key_update_is_refused_until_the_player_trusts_it() {
    let f = fixture();
    let (dest, data) = (TempDir::new("dest"), TempDir::new("data"));
    let pins = KeyPinStore::in_dir(data.path());

    let client = cloud(&f, &f.e2e.manifests.v100).await;
    install(&client, SLUG, dest.path(), 2, None, pinned(&pins, None), |_| {})
        .await
        .unwrap();

    // Signed by a key this pack has never used: refused, disk untouched.
    let client = cloud(&f, &f.e2e.manifests.v110_rotated).await;
    let err = update(&client, SLUG, dest.path(), 2, None, pinned(&pins, None), |_| {})
        .await
        .unwrap_err();
    let refusal = key_changed(err);
    assert_eq!(refusal.slug, SLUG);
    assert_eq!(
        refusal.trusted,
        vec![KnownKey {
            key_id: f.key_id.clone(),
            public_key: Some(f.public_key.clone()),
        }]
    );
    assert_eq!(refusal.offered.key_id, f.e2e.rotated_key_id);
    assert_eq!(installed_version(dest.path()), "1.0.0");
    assert!(!dest.path().join("Mods/E2E/second.txt").exists());

    // The player trusts exactly the offered key: the update goes through
    // and both keys are pinned from now on.
    let report = update(
        &client,
        SLUG,
        dest.path(),
        2,
        None,
        pinned(&pins, Some(refusal.offered.clone())),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.to_version, "1.1.0");
    assert!(dest.path().join("Mods/E2E/second.txt").exists());
    let ids: Vec<String> = pins
        .pins_for(SLUG)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.key_id)
        .collect();
    assert_eq!(ids, vec![f.key_id.clone(), f.e2e.rotated_key_id.clone()]);
}

#[tokio::test]
async fn pins_survive_uninstall_and_cover_fresh_installs() {
    let f = fixture();
    let data = TempDir::new("data");
    let pins = KeyPinStore::in_dir(data.path());

    let first = TempDir::new("dest");
    let client = cloud(&f, &f.e2e.manifests.v100).await;
    install(&client, SLUG, first.path(), 2, None, pinned(&pins, None), |_| {})
        .await
        .unwrap();
    drop(first); // the install dir is gone; the pin isn't

    let second = TempDir::new("dest");
    let client = cloud(&f, &f.e2e.manifests.v110_rotated).await;
    let err = install(&client, SLUG, second.path(), 2, None, pinned(&pins, None), |_| {})
        .await
        .unwrap_err();
    key_changed(err);
    assert!(!second.path().join("_packrelay-manifest.json").exists());
}

#[tokio::test]
async fn install_from_before_pinning_counts_its_signer_as_known() {
    let f = fixture();
    let (dest, data) = (TempDir::new("dest"), TempDir::new("data"));

    // Installed by a launcher without key pinning.
    let client = cloud(&f, &f.e2e.manifests.v100).await;
    install(&client, SLUG, dest.path(), 2, None, InstallContext::default(), |_| {})
        .await
        .unwrap();

    // First update with pinning on is signed by a different key than
    // the installed copy: that's a key change, not a first install.
    let pins = KeyPinStore::in_dir(data.path());
    let client = cloud(&f, &f.e2e.manifests.v110_rotated).await;
    let err = update(&client, SLUG, dest.path(), 2, None, pinned(&pins, None), |_| {})
        .await
        .unwrap_err();
    let refusal = key_changed(err);
    assert_eq!(
        refusal.trusted,
        vec![KnownKey {
            key_id: f.key_id.clone(),
            public_key: None,
        }]
    );
    assert!(pins.pins_for(SLUG).await.unwrap().is_empty());
}

#[tokio::test]
async fn approving_a_different_key_than_served_does_not_help() {
    let f = fixture();
    let (dest, data) = (TempDir::new("dest"), TempDir::new("data"));
    let pins = KeyPinStore::in_dir(data.path());

    let client = cloud(&f, &f.e2e.manifests.v100).await;
    install(&client, SLUG, dest.path(), 2, None, pinned(&pins, None), |_| {})
        .await
        .unwrap();

    // The player approved the rotated key id, but with other bytes
    // (as if the cloud swapped keys between the prompt and the retry).
    let wrong = TrustedKey {
        key_id: f.e2e.rotated_key_id.clone(),
        public_key: f.public_key.clone(),
    };
    let client = cloud(&f, &f.e2e.manifests.v110_rotated).await;
    let err = update(&client, SLUG, dest.path(), 2, None, pinned(&pins, Some(wrong)), |_| {})
        .await
        .unwrap_err();
    key_changed(err);
    assert_eq!(installed_version(dest.path()), "1.0.0");
}
