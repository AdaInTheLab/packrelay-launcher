// update() puts back files that the installed sidecar lists but that
// are gone from disk. Before this, a file the diff would "keep" was
// never checked, so a folder that went missing (a 233 MB texture pack
// on a player's machine, 2026-10-23) stayed missing through every
// update while the new sidecar went on claiming it. Runs against the
// same signed fixture-e2e packs as key_pinning.rs: v1.0.0 has
// Mods/E2E/hello.txt, v1.1.0 adds Mods/E2E/second.txt.

use std::collections::BTreeMap;
use std::path::Path;

use packrelay_core::client::Client;
use packrelay_core::install::{install, InstallContext};
use packrelay_core::update::update;
use serde::Deserialize;
use serde_json::json;

mod common;
use common::{serve, TempDir};

const SLUG: &str = "fixture-e2e";
const HELLO: &str = "Mods/E2E/hello.txt";
const SECOND: &str = "Mods/E2E/second.txt";

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
    files: BTreeMap<String, String>,
    manifests: E2eManifests,
}

#[derive(Deserialize)]
struct E2eManifests {
    v100: String,
    v110: String,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/signing-fixture.json")).unwrap()
}

/// A fake cloud serving `manifest` as fixture-e2e's latest, its key,
/// and the pack's files.
async fn cloud(f: &Fixture, manifest: &str) -> Client {
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
    for (sha, body) in &f.e2e.files {
        routes.push((format!("/api/v1/files/{sha}"), body.clone()));
    }
    Client::new(&serve(routes).await)
}

async fn installed(f: &Fixture, manifest: &str) -> TempDir {
    let dest = TempDir::new("restore");
    install(
        &cloud(f, manifest).await,
        SLUG,
        dest.path(),
        2,
        None,
        InstallContext::default(),
        |_| {},
    )
    .await
    .unwrap();
    dest
}

fn read(dest: &Path, path: &str) -> String {
    std::fs::read_to_string(dest.join(path)).unwrap()
}

#[tokio::test]
async fn update_puts_back_a_kept_file_missing_from_disk() {
    let f = fixture();
    let dest = installed(&f, &f.e2e.manifests.v100).await;
    let hello = read(dest.path(), HELLO);
    std::fs::remove_file(dest.path().join(HELLO)).unwrap();

    let client = cloud(&f, &f.e2e.manifests.v110).await;
    let report = update(
        &client,
        SLUG,
        dest.path(),
        2,
        None,
        InstallContext::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(report.to_version, "1.1.0");
    assert_eq!(report.files_restored, 1, "hello.txt was missing");
    assert_eq!(report.files_added, 1, "second.txt is new in v1.1.0");
    assert_eq!(report.files_kept, 0);
    assert_eq!(read(dest.path(), HELLO), hello);
    assert!(dest.path().join(SECOND).is_file());
}

#[tokio::test]
async fn same_version_update_puts_back_a_missing_folder() {
    // The player's case: the sidecar already says this version, and a
    // whole mod folder is gone. Before the fix this was a silent no-op.
    let f = fixture();
    let dest = installed(&f, &f.e2e.manifests.v110).await;
    let hello = read(dest.path(), HELLO);
    std::fs::remove_dir_all(dest.path().join("Mods/E2E")).unwrap();

    let client = cloud(&f, &f.e2e.manifests.v110).await;
    let report = update(
        &client,
        SLUG,
        dest.path(),
        2,
        None,
        InstallContext::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(report.from_version, "1.1.0");
    assert_eq!(report.to_version, "1.1.0");
    assert_eq!(report.files_restored, 2);
    assert!(report.bytes_downloaded > 0);
    assert_eq!(read(dest.path(), HELLO), hello);
    assert!(dest.path().join(SECOND).is_file());
}

#[tokio::test]
async fn same_version_update_puts_back_a_wrong_sized_file() {
    let f = fixture();
    let dest = installed(&f, &f.e2e.manifests.v110).await;
    let hello = read(dest.path(), HELLO);
    std::fs::write(dest.path().join(HELLO), "cut off").unwrap();

    let client = cloud(&f, &f.e2e.manifests.v110).await;
    let report = update(
        &client,
        SLUG,
        dest.path(),
        2,
        None,
        InstallContext::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(report.files_restored, 1);
    assert_eq!(report.files_kept, 1);
    assert_eq!(read(dest.path(), HELLO), hello);
}

#[tokio::test]
async fn same_version_update_with_everything_present_downloads_nothing() {
    let f = fixture();
    let dest = installed(&f, &f.e2e.manifests.v110).await;

    let client = cloud(&f, &f.e2e.manifests.v110).await;
    let report = update(
        &client,
        SLUG,
        dest.path(),
        2,
        None,
        InstallContext::default(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(report.files_restored, 0);
    assert_eq!(report.files_kept, 2);
    assert_eq!(report.bytes_downloaded, 0);
}
