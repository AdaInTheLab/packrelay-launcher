// Manifest signature + compatibility checks, run against manifests
// signed and stored by PackRelayCloud's own code.
//
// fixtures/signing-fixture.json comes from fixtures/gen-signing-fixture.mts
// (run it from a PackRelayCloud checkout to regenerate). Every manifest
// in it went through the cloud's parseManifest() + verifyManifestSignature(),
// and every canonical case through its canonicalize(), so these tests
// fail if the launcher and cloud stop agreeing on the signed bytes.

use packrelay_core::canonical_json::canonicalize;
use packrelay_core::client::Client;
use packrelay_core::manifest::{parse_manifest, Signature};
use packrelay_core::signature::{verify_manifest_signature, PublisherKey};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    key_id: String,
    public_key: String,
    wrong_public_key: String,
    manifests: Manifests,
    canonical: Vec<CanonicalCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifests {
    v2: String,
    v1: String,
    other_game: String,
}

#[derive(Deserialize)]
struct CanonicalCase {
    input: Value,
    canonical: String,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/signing-fixture.json")).unwrap()
}

fn key(f: &Fixture, public_key: &str) -> PublisherKey {
    PublisherKey {
        key_id: f.key_id.clone(),
        public_key: public_key.to_string(),
        algorithm: "ed25519".to_string(),
        revoked_at: None,
    }
}

/// Everything fetch_manifest_at checks, minus the HTTP.
fn check(raw: &str, key: &PublisherKey) -> anyhow::Result<()> {
    let (value, manifest) = parse_manifest(raw)?;
    verify_manifest_signature(&value, &manifest.signature, key)
}

/// Edit the served manifest JSON, leaving its signature untouched.
fn tampered(raw: &str, edit: impl FnOnce(&mut Value)) -> String {
    let mut value: Value = serde_json::from_str(raw).unwrap();
    edit(&mut value);
    serde_json::to_string(&value).unwrap()
}

fn assert_err_contains(result: anyhow::Result<()>, needle: &str) {
    let err = format!("{:#}", result.expect_err("expected an error"));
    assert!(err.contains(needle), "error {err:?} should mention {needle:?}");
}

// ---- canonical JSON ----

#[test]
fn canonical_json_matches_the_cloud() {
    let f = fixture();
    assert!(!f.canonical.is_empty());
    for case in &f.canonical {
        assert_eq!(
            canonicalize(&case.input).unwrap(),
            case.canonical,
            "input: {}",
            case.input
        );
    }
}

// ---- valid signatures ----

#[test]
fn valid_v2_signature_passes() {
    let f = fixture();
    check(&f.manifests.v2, &key(&f, &f.public_key)).unwrap();
}

#[test]
fn valid_v1_signature_passes() {
    let f = fixture();
    check(&f.manifests.v1, &key(&f, &f.public_key)).unwrap();
}

#[test]
fn reformatted_manifest_still_verifies() {
    // The signature covers canonical JSON, not the served bytes, so
    // key order and whitespace don't matter.
    let f = fixture();
    let value: Value = serde_json::from_str(&f.manifests.v2).unwrap();
    let pretty = serde_json::to_string_pretty(&value).unwrap();
    check(&pretty, &key(&f, &f.public_key)).unwrap();
}

// ---- tampering ----

#[test]
fn tampered_file_entry_fails() {
    let f = fixture();
    let raw = tampered(&f.manifests.v2, |m| {
        m["files"][1]["sha256"] = json!("f".repeat(64));
    });
    assert_err_contains(check(&raw, &key(&f, &f.public_key)), "doesn't match publisher key");
}

#[test]
fn added_file_fails() {
    let f = fixture();
    let raw = tampered(&f.manifests.v2, |m| {
        m["files"].as_array_mut().unwrap().push(json!({
            "path": "Mods/Evil/Evil.dll",
            "sha256": "e".repeat(64),
            "size": 1,
        }));
    });
    assert_err_contains(check(&raw, &key(&f, &f.public_key)), "doesn't match publisher key");
}

#[test]
fn tampered_field_the_launcher_does_not_model_fails() {
    // scanAttestation isn't in the Manifest struct; verification must
    // still cover it, which is why it runs on the raw JSON value.
    let f = fixture();
    let raw = tampered(&f.manifests.v2, |m| {
        m["sources"][1]["scanAttestation"]["scanner"] = json!("Totally Real AV");
    });
    assert_err_contains(check(&raw, &key(&f, &f.public_key)), "doesn't match publisher key");
}

#[test]
fn stripped_signature_fails() {
    let f = fixture();
    let raw = tampered(&f.manifests.v2, |m| {
        m.as_object_mut().unwrap().remove("signature");
    });
    assert!(check(&raw, &key(&f, &f.public_key)).is_err());
}

#[test]
fn truncated_signature_fails() {
    let f = fixture();
    let raw = tampered(&f.manifests.v2, |m| {
        let value = m["signature"]["value"].as_str().unwrap()[..126].to_string();
        m["signature"]["value"] = json!(value);
    });
    assert_err_contains(check(&raw, &key(&f, &f.public_key)), "isn't 64 bytes");
}

// ---- keys ----

#[test]
fn wrong_key_fails() {
    let f = fixture();
    assert_err_contains(
        check(&f.manifests.v2, &key(&f, &f.wrong_public_key)),
        "doesn't match publisher key",
    );
}

#[test]
fn revoked_key_fails() {
    let f = fixture();
    let mut revoked = key(&f, &f.public_key);
    revoked.revoked_at = Some("2026-09-01T00:00:00.000Z".to_string());
    assert_err_contains(check(&f.manifests.v2, &revoked), "was revoked");
}

#[test]
fn key_for_a_different_id_fails() {
    let f = fixture();
    let mut other = key(&f, &f.public_key);
    other.key_id = "someone-else/main".to_string();
    assert_err_contains(check(&f.manifests.v2, &other), "when asked for signing key");
}

// ---- compatibility ----

#[test]
fn unknown_game_fails_even_with_a_valid_signature() {
    let f = fixture();
    // The signature itself is fine...
    let value: Value = serde_json::from_str(&f.manifests.other_game).unwrap();
    let signature: Signature = serde_json::from_value(value["signature"].clone()).unwrap();
    verify_manifest_signature(&value, &signature, &key(&f, &f.public_key)).unwrap();
    // ...but it's a Valheim pack, and this launcher only knows 7DTD.
    assert_err_contains(
        check(&f.manifests.other_game, &key(&f, &f.public_key)),
        "this pack is for valheim, which this launcher version doesn't support",
    );
}

#[test]
fn unsupported_schema_version_fails() {
    let f = fixture();
    for version in [json!(0), json!(3), json!("2"), Value::Null] {
        let raw = tampered(&f.manifests.v2, |m| m["schemaVersion"] = version.clone());
        assert!(
            parse_manifest(&raw).is_err(),
            "schemaVersion {version} should be rejected"
        );
    }
    let raw = tampered(&f.manifests.v2, |m| m["schemaVersion"] = json!(3));
    assert_err_contains(parse_manifest(&raw).map(|_| ()), "schema version 3");
}

// ---- end to end through Client::fetch_manifest_at ----

/// Minimal HTTP/1.1 server answering GETs from a fixed route table
/// (anything else is a 404). Returns its base URL.
async fn serve(routes: Vec<(String, String)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut chunk = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&chunk[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&req);
                let path = req.split_whitespace().nth(1).unwrap_or_default();
                let (status, body) = match routes.iter().find(|(p, _)| p == path) {
                    Some((_, body)) => ("200 OK", body.clone()),
                    None => ("404 Not Found", r#"{"error":"not found"}"#.to_string()),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

fn key_route(f: &Fixture) -> (String, String) {
    (
        // "/" in the key id travels as %2F — the route is one segment.
        "/api/v1/keys/fixture-pub%2Fmain".to_string(),
        json!({
            "keyId": f.key_id,
            "publicKey": f.public_key,
            "algorithm": "ed25519",
            "createdAt": "2026-09-30T00:00:00.000Z",
            "revokedAt": null,
        })
        .to_string(),
    )
}

const MANIFEST_PATH: &str = "/api/v1/packs/fixture-pack/manifest";

#[tokio::test]
async fn fetch_manifest_returns_verified_manifest() {
    let f = fixture();
    let api = serve(vec![
        (MANIFEST_PATH.to_string(), f.manifests.v2.clone()),
        key_route(&f),
    ])
    .await;
    let (raw, manifest) = Client::new(&api).fetch_manifest("fixture-pack").await.unwrap();
    assert_eq!(raw, f.manifests.v2);
    assert_eq!(manifest.files.len(), 4);
}

#[tokio::test]
async fn fetch_manifest_refuses_tampered_manifest() {
    let f = fixture();
    let raw = tampered(&f.manifests.v2, |m| {
        m["files"][0]["sha256"] = json!("0".repeat(64));
    });
    let api = serve(vec![(MANIFEST_PATH.to_string(), raw), key_route(&f)]).await;
    let err = Client::new(&api)
        .fetch_manifest("fixture-pack")
        .await
        .expect_err("tampered manifest must be refused");
    let err = format!("{err:#}");
    assert!(err.contains("Refusing to install 'fixture-pack'"), "{err}");
    assert!(err.contains("doesn't match publisher key"), "{err}");
}

#[tokio::test]
async fn fetch_manifest_refuses_unregistered_key() {
    let f = fixture();
    let api = serve(vec![(MANIFEST_PATH.to_string(), f.manifests.v2.clone())]).await;
    let err = Client::new(&api)
        .fetch_manifest("fixture-pack")
        .await
        .expect_err("manifest with an unknown key must be refused");
    assert!(format!("{err:#}").contains("isn't registered"), "{err:#}");
}
