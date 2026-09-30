// framework.rs: installing a game's mod loader from the cloud's
// re-hosted copy (multi-game).

use packrelay_core::client::Client;
use packrelay_core::framework::{ensure_framework, installed_framework, FRAMEWORK_MARKER};
use packrelay_core::manifest::Framework;
use serde_json::json;
use sha2::{Digest, Sha256};

mod common;
use common::{serve, TempDir};

const ID: &str = "bepinexpack-valheim";
const VERSION: &str = "5.4.2333";

fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn framework() -> Framework {
    Framework {
        id: ID.into(),
        version: VERSION.into(),
    }
}

/// A cloud serving BepInExPack's file list and each file by hash.
/// `tamper` serves the named file's bytes wrong.
async fn cloud(files: &[(&str, &str)], tamper: Option<&str>) -> Client {
    let list: Vec<_> = files
        .iter()
        .map(|(path, body)| json!({ "path": path, "sha256": sha(body), "size": body.len() }))
        .collect();
    let mut routes = vec![(
        format!("/api/v1/frameworks/{ID}/{VERSION}"),
        json!({ "ok": true, "id": ID, "version": VERSION, "files": list, "sourceSha256": "x" })
            .to_string(),
    )];
    for (path, body) in files {
        let served = if tamper == Some(*path) {
            format!("{body}!")
        } else {
            body.to_string()
        };
        routes.push((format!("/api/v1/files/{}", sha(body)), served));
    }
    Client::new(&serve(routes).await)
}

const PACK: &[(&str, &str)] = &[
    ("winhttp.dll", "winhttp"),
    ("doorstop_config.ini", "[General]"),
    ("BepInEx/core/BepInEx.dll", "core"),
    ("BepInEx/config/BepInEx.cfg", "default cfg"),
];

#[tokio::test]
async fn installs_the_loader_into_the_game_root() {
    let game = TempDir::new("fw-fresh");
    let client = cloud(PACK, None).await;
    let report = ensure_framework(&client, game.path(), &framework())
        .await
        .unwrap();
    assert_eq!(report.written.len(), 4);
    assert!(!report.already_installed);
    assert_eq!(
        std::fs::read_to_string(game.path().join("winhttp.dll")).unwrap(),
        "winhttp"
    );
    assert_eq!(
        std::fs::read_to_string(game.path().join("BepInEx/core/BepInEx.dll")).unwrap(),
        "core"
    );
    assert_eq!(installed_framework(game.path()).await, Some(framework()));
}

#[tokio::test]
async fn a_second_run_fetches_nothing() {
    let game = TempDir::new("fw-again");
    let client = cloud(PACK, None).await;
    ensure_framework(&client, game.path(), &framework())
        .await
        .unwrap();
    let again = ensure_framework(&client, game.path(), &framework())
        .await
        .unwrap();
    assert!(again.written.is_empty());
    assert_eq!(again.kept, 4);
    assert!(again.already_installed);
}

#[tokio::test]
async fn keeps_config_the_player_has_but_replaces_a_stale_loader() {
    let game = TempDir::new("fw-keep");
    std::fs::create_dir_all(game.path().join("BepInEx/config")).unwrap();
    std::fs::write(
        game.path().join("BepInEx/config/BepInEx.cfg"),
        "my settings",
    )
    .unwrap();
    std::fs::create_dir_all(game.path().join("BepInEx/core")).unwrap();
    std::fs::write(game.path().join("BepInEx/core/BepInEx.dll"), "old loader").unwrap();
    let client = cloud(PACK, None).await;
    let report = ensure_framework(&client, game.path(), &framework())
        .await
        .unwrap();
    assert!(report
        .written
        .contains(&"BepInEx/core/BepInEx.dll".to_string()));
    assert!(!report
        .written
        .contains(&"BepInEx/config/BepInEx.cfg".to_string()));
    assert_eq!(
        std::fs::read_to_string(game.path().join("BepInEx/config/BepInEx.cfg")).unwrap(),
        "my settings"
    );
    assert_eq!(
        std::fs::read_to_string(game.path().join("BepInEx/core/BepInEx.dll")).unwrap(),
        "core"
    );
}

#[tokio::test]
async fn a_file_that_fails_its_hash_is_never_written() {
    let game = TempDir::new("fw-tamper");
    std::fs::write(game.path().join("winhttp.dll"), "the good old one").unwrap();
    let client = cloud(PACK, Some("winhttp.dll")).await;
    let err = ensure_framework(&client, game.path(), &framework())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("didn't match its hash"), "{err:#}");
    assert_eq!(
        std::fs::read_to_string(game.path().join("winhttp.dll")).unwrap(),
        "the good old one"
    );
    assert!(!game.path().join(FRAMEWORK_MARKER).exists());
}

#[tokio::test]
async fn refuses_a_path_outside_the_game() {
    let game = TempDir::new("fw-escape");
    let client = cloud(&[("../evil.dll", "evil")], None).await;
    let err = ensure_framework(&client, game.path(), &framework())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unsafe path"), "{err:#}");
}

#[tokio::test]
async fn an_unknown_framework_version_is_an_error() {
    let game = TempDir::new("fw-missing");
    let client = cloud(PACK, None).await;
    let other = Framework {
        id: ID.into(),
        version: "9.9.9".into(),
    };
    assert!(ensure_framework(&client, game.path(), &other)
        .await
        .is_err());
}
