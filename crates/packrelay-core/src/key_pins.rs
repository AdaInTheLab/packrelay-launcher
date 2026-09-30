// Trust-on-first-use pins for pack signing keys.
//
// A valid signature only proves that *some* key registered on
// packrelay.cloud signed a manifest: the cloud is also the key
// directory, so a compromised cloud could sign with a key of its own.
// Pinning closes that gap for packs the player already has. The first
// install of a pack remembers the key that signed it; from then on a
// version signed by any other key is refused until the player
// explicitly trusts that exact key (`TrustedKey`).
//
// A pack can legitimately gain keys: team members sign with their own
// allow-listed keys, and owners can move a pack to a new signing
// identity. So a key change is a question for the player, not a hard
// failure, and approving one ADDS it to the pack's pins rather than
// replacing the old one.
//
// Pins are keyed by pack slug and live in one JSON file in the
// launcher's data dir. They deliberately survive uninstall: otherwise
// "uninstall, reinstall" would silently reset trust. A pin file that
// can't be read fails closed instead of being treated as empty.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs;
use tokio::sync::Mutex;

use crate::signature::PublisherKey;

/// File name inside the launcher's data dir.
pub const PIN_FILE_NAME: &str = "key-pins.json";

const PIN_FILE_VERSION: u32 = 1;

/// Serializes load-modify-save across concurrent installs in this
/// process, so two packs pinning at once can't drop each other's pin.
static PIN_FILE_LOCK: Mutex<()> = Mutex::const_new(());

/// A signing key the launcher trusts for a pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinnedKey {
    pub key_id: String,
    /// Standard padded base64 of the raw 32-byte Ed25519 key.
    pub public_key: String,
    /// When the pin was made, in seconds since the Unix epoch.
    pub pinned_at: u64,
}

/// A key the player agreed to trust after a [`KeyChanged`] refusal.
/// It only unlocks an install if the served key matches it exactly,
/// id and bytes, so the cloud can't swap keys between the prompt and
/// the retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedKey {
    pub key_id: String,
    pub public_key: String,
}

impl TrustedKey {
    /// Parse the CLI's `<key-id>=<base64-key>` form.
    pub fn parse_cli(s: &str) -> Result<Self> {
        let (key_id, public_key) = s
            .split_once('=')
            .filter(|(id, key)| !id.is_empty() && !key.is_empty())
            .ok_or_else(|| anyhow!("expected <key-id>=<base64-key>, got '{s}'"))?;
        Ok(Self {
            key_id: key_id.to_string(),
            public_key: public_key.to_string(),
        })
    }
}

impl fmt::Display for TrustedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.key_id, self.public_key)
    }
}

/// A key the launcher already associates with a pack, for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownKey {
    pub key_id: String,
    /// None when all we know is the key id recorded in an installed
    /// manifest (an install made before pinning existed).
    pub public_key: Option<String>,
}

/// Refusal: the manifest is validly signed, but by a key this launcher
/// hasn't been told to trust for the pack. Returned (inside anyhow)
/// from install/update before anything touches the disk; downcast it
/// to offer the player a "trust this key" choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyChanged {
    pub slug: String,
    /// Keys already trusted for the pack.
    pub trusted: Vec<KnownKey>,
    /// The key the new manifest is signed with. Pass it back as the
    /// install's `trust_key` to accept it.
    pub offered: TrustedKey,
}

impl fmt::Display for KeyChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let trusted: Vec<&str> = self.trusted.iter().map(|k| k.key_id.as_str()).collect();
        write!(
            f,
            "the signing key for '{}' changed: this launcher trusts {} for it, but this \
             version is signed by '{}'. That happens when a pack's team adds a signer or \
             its owner changes keys, and also when someone is tampering with the pack. \
             Only continue if you trust the new key.",
            self.slug,
            trusted.join(", "),
            self.offered.key_id
        )?;
        if self.trusted.iter().any(|k| k.key_id == self.offered.key_id) {
            write!(
                f,
                " Note: the key NAME is unchanged but its bytes are different, which \
                 PackRelay keys never do on their own."
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for KeyChanged {}

/// What `check_and_pin` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinOutcome {
    /// The key was already pinned for the pack.
    AlreadyTrusted,
    /// First install of the pack: the key is now pinned.
    PinnedFirstUse,
    /// The player approved a new key; it's now pinned alongside the old.
    PinnedApproved,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PinFile {
    version: u32,
    packs: BTreeMap<String, Vec<PinnedKey>>,
}

/// The pin file. Cheap to construct; every call reads the file fresh.
#[derive(Debug, Clone)]
pub struct KeyPinStore {
    path: PathBuf,
}

impl KeyPinStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The pin file inside the launcher's data dir.
    pub fn in_dir(dir: &Path) -> Self {
        Self::new(dir.join(PIN_FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keys pinned for `slug`, oldest first.
    pub async fn pins_for(&self, slug: &str) -> Result<Vec<PinnedKey>> {
        let _guard = PIN_FILE_LOCK.lock().await;
        Ok(self.load().await?.packs.remove(slug).unwrap_or_default())
    }

    /// Check that `key` (the verified signer of a manifest for `slug`)
    /// is trusted for the pack, pinning it when this is the pack's
    /// first install or the player approved it.
    ///
    /// `installed_key_id` is the signing key id of the copy already on
    /// disk, if any. It only matters for packs with no pins yet (they
    /// were installed before pinning existed): a different key there
    /// is a key change too, not a first install.
    ///
    /// Errors with [`KeyChanged`] when the key isn't trusted and
    /// `approved` doesn't match it exactly.
    pub async fn check_and_pin(
        &self,
        slug: &str,
        key: &PublisherKey,
        installed_key_id: Option<&str>,
        approved: Option<&TrustedKey>,
    ) -> Result<PinOutcome> {
        let offered = TrustedKey {
            key_id: key.key_id.clone(),
            public_key: normalize_key(&key.public_key)
                .with_context(|| format!("signing key '{}'", key.key_id))?,
        };

        let _guard = PIN_FILE_LOCK.lock().await;
        let mut file = self.load().await?;
        let pins = file.packs.get(slug).map(Vec::as_slice).unwrap_or_default();

        if pins
            .iter()
            .any(|p| p.key_id == offered.key_id && p.public_key == offered.public_key)
        {
            return Ok(PinOutcome::AlreadyTrusted);
        }

        let approved = approved.is_some_and(|a| {
            a.key_id == offered.key_id
                && normalize_key(&a.public_key).is_ok_and(|k| k == offered.public_key)
        });
        let first_use = pins.is_empty() && installed_key_id.is_none_or(|id| id == offered.key_id);
        if !approved && !first_use {
            let trusted = if pins.is_empty() {
                installed_key_id
                    .map(|id| KnownKey {
                        key_id: id.to_string(),
                        public_key: None,
                    })
                    .into_iter()
                    .collect()
            } else {
                pins.iter()
                    .map(|p| KnownKey {
                        key_id: p.key_id.clone(),
                        public_key: Some(p.public_key.clone()),
                    })
                    .collect()
            };
            return Err(KeyChanged {
                slug: slug.to_string(),
                trusted,
                offered,
            }
            .into());
        }

        file.packs
            .entry(slug.to_string())
            .or_default()
            .push(PinnedKey {
                key_id: offered.key_id,
                public_key: offered.public_key,
                pinned_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default(),
            });
        self.save(&file).await?;
        Ok(if approved {
            PinOutcome::PinnedApproved
        } else {
            PinOutcome::PinnedFirstUse
        })
    }

    async fn load(&self) -> Result<PinFile> {
        let text = match fs::read_to_string(&self.path).await {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PinFile {
                    version: PIN_FILE_VERSION,
                    ..PinFile::default()
                })
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("reading pinned signing keys from {}", self.path.display())
                })
            }
        };
        let file: PinFile = serde_json::from_str(&text).with_context(|| {
            format!(
                "pinned signing keys in {} are unreadable. Refusing to continue rather \
                 than forget which keys you trust; fix or delete that file",
                self.path.display()
            )
        })?;
        if file.version != PIN_FILE_VERSION {
            bail!(
                "pinned signing keys in {} are version {}, which this launcher version \
                 doesn't understand. Update the launcher.",
                self.path.display(),
                file.version
            );
        }
        Ok(file)
    }

    async fn save(&self, file: &PinFile) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        // Write-then-rename so a crash mid-write can't leave a torn
        // file (which load() would then refuse).
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(file).context("serializing key pins")?;
        fs::write(&tmp, json)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .await
            .with_context(|| format!("replacing {}", self.path.display()))
    }
}

/// The signing key id of the copy of `slug` installed in `dest`, from
/// its sidecar manifest. None when nothing (or another pack) is there.
pub async fn installed_key_id(dest: &Path, slug: &str) -> Option<String> {
    let raw = fs::read_to_string(dest.join("_packrelay-manifest.json"))
        .await
        .ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    if value.get("name")?.as_str()? != slug {
        return None;
    }
    Some(
        value
            .pointer("/signature/publicKeyId")?
            .as_str()?
            .to_string(),
    )
}

/// Re-encode as standard padded base64, so the same key always
/// compares equal however it was spelled.
fn normalize_key(b64: &str) -> Result<String> {
    let engine = base64::engine::general_purpose::STANDARD;
    let bytes = engine
        .decode(b64.trim())
        .map_err(|e| anyhow!("public key isn't valid base64: {e}"))?;
    if bytes.len() != 32 {
        bail!("public key is {} bytes, not 32", bytes.len());
    }
    Ok(engine.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "ERERERERERERERERERERERERERERERERERERERERERE=";
    const KEY_B: &str = "IiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiI=";

    fn key(key_id: &str, public_key: &str) -> PublisherKey {
        PublisherKey {
            key_id: key_id.to_string(),
            public_key: public_key.to_string(),
            algorithm: "ed25519".to_string(),
            revoked_at: None,
        }
    }

    fn trusted(key_id: &str, public_key: &str) -> TrustedKey {
        TrustedKey {
            key_id: key_id.to_string(),
            public_key: public_key.to_string(),
        }
    }

    fn store() -> (tempdir::Dir, KeyPinStore) {
        let dir = tempdir::Dir::new();
        let store = KeyPinStore::in_dir(dir.path());
        (dir, store)
    }

    fn key_changed(err: anyhow::Error) -> KeyChanged {
        err.downcast::<KeyChanged>()
            .unwrap_or_else(|e| panic!("expected KeyChanged, got: {e:#}"))
    }

    #[tokio::test]
    async fn first_install_pins_then_same_key_passes_quietly() {
        let (_dir, pins) = store();
        let a = key("pub/a", KEY_A);
        assert_eq!(
            pins.check_and_pin("pack", &a, None, None).await.unwrap(),
            PinOutcome::PinnedFirstUse
        );
        assert_eq!(
            pins.check_and_pin("pack", &a, None, None).await.unwrap(),
            PinOutcome::AlreadyTrusted
        );
        let pinned = pins.pins_for("pack").await.unwrap();
        assert_eq!(pinned.len(), 1);
        assert_eq!(
            (pinned[0].key_id.as_str(), pinned[0].public_key.as_str()),
            ("pub/a", KEY_A)
        );
    }

    #[tokio::test]
    async fn a_different_key_is_refused_until_approved() {
        let (_dir, pins) = store();
        pins.check_and_pin("pack", &key("pub/a", KEY_A), None, None)
            .await
            .unwrap();

        let b = key("pub/b", KEY_B);
        let refusal = key_changed(
            pins.check_and_pin("pack", &b, None, None)
                .await
                .unwrap_err(),
        );
        assert_eq!(refusal.offered, trusted("pub/b", KEY_B));
        assert_eq!(refusal.trusted[0].key_id, "pub/a");
        // Refusing doesn't pin anything.
        assert_eq!(pins.pins_for("pack").await.unwrap().len(), 1);

        assert_eq!(
            pins.check_and_pin("pack", &b, None, Some(&trusted("pub/b", KEY_B)))
                .await
                .unwrap(),
            PinOutcome::PinnedApproved
        );
        // Both keys are trusted now; neither prompts again.
        for k in [key("pub/a", KEY_A), b] {
            assert_eq!(
                pins.check_and_pin("pack", &k, None, None).await.unwrap(),
                PinOutcome::AlreadyTrusted
            );
        }
    }

    #[tokio::test]
    async fn approval_must_match_the_served_key_exactly() {
        let (_dir, pins) = store();
        pins.check_and_pin("pack", &key("pub/a", KEY_A), None, None)
            .await
            .unwrap();
        let b = key("pub/b", KEY_B);
        for approval in [
            trusted("pub/b", KEY_A),
            trusted("pub/other", KEY_B),
            trusted("pub/b", "junk"),
        ] {
            key_changed(
                pins.check_and_pin("pack", &b, None, Some(&approval))
                    .await
                    .unwrap_err(),
            );
        }
    }

    #[tokio::test]
    async fn same_key_name_with_new_bytes_is_a_key_change() {
        let (_dir, pins) = store();
        pins.check_and_pin("pack", &key("pub/a", KEY_A), None, None)
            .await
            .unwrap();
        let err = pins
            .check_and_pin("pack", &key("pub/a", KEY_B), None, None)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("key NAME is unchanged"), "{err}");
        key_changed(err);
    }

    #[tokio::test]
    async fn pins_are_per_pack() {
        let (_dir, pins) = store();
        pins.check_and_pin("one", &key("pub/a", KEY_A), None, None)
            .await
            .unwrap();
        assert_eq!(
            pins.check_and_pin("two", &key("pub/b", KEY_B), None, None)
                .await
                .unwrap(),
            PinOutcome::PinnedFirstUse
        );
    }

    #[tokio::test]
    async fn pre_pinning_install_signed_by_another_key_is_a_key_change() {
        let (_dir, pins) = store();
        let b = key("pub/b", KEY_B);
        let refusal = key_changed(
            pins.check_and_pin("pack", &b, Some("pub/a"), None)
                .await
                .unwrap_err(),
        );
        assert_eq!(
            refusal.trusted,
            vec![KnownKey {
                key_id: "pub/a".to_string(),
                public_key: None
            }]
        );
        // Same key id as the installed copy: just pin it.
        assert_eq!(
            pins.check_and_pin("pack", &b, Some("pub/b"), None)
                .await
                .unwrap(),
            PinOutcome::PinnedFirstUse
        );
    }

    #[tokio::test]
    async fn unreadable_pin_file_fails_closed() {
        let (_dir, pins) = store();
        std::fs::write(pins.path(), "{ not json").unwrap();
        let err = pins
            .check_and_pin("pack", &key("pub/a", KEY_A), None, None)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unreadable"), "{err:#}");
        assert!(err.downcast_ref::<KeyChanged>().is_none());
        // And the broken file is left alone for the player to look at.
        assert_eq!(std::fs::read_to_string(pins.path()).unwrap(), "{ not json");
    }

    #[test]
    fn cli_trust_key_parses_id_and_key() {
        assert_eq!(
            TrustedKey::parse_cli(&format!("pub/b={KEY_B}")).unwrap(),
            trusted("pub/b", KEY_B)
        );
        for bad in ["", "pub/b", "=abc", "pub/b="] {
            assert!(TrustedKey::parse_cli(bad).is_err(), "{bad:?}");
        }
    }

    /// Minimal self-deleting temp dir (no tempfile dependency).
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                static N: AtomicU32 = AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "packrelay-key-pins-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
