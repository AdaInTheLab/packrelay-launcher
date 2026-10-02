// Palworld's own mod loader, as the launcher drives it
// (PackRelayCloud docs/multi-game/PALWORLD.md §7, palworld-spike.md §5).
//
// A Palworld pack is a folder of the loader's packages:
// `<numeric id>/Info.json` plus files, installed into PackRelay's own
// folder (games.rs `live_root_in_data`). Launching with -workshopdir
// pointing there makes the game install the pack itself. Two things have
// to be true for that to work, and both are the player's install, not
// the pack's:
//
//   - Mods/PalModSettings.ini must have mods switched on and list the
//     pack's package names in ActiveModList. The player's own entries
//     stay: a plain Steam launch later prunes the pack's names and
//     brings their own Workshop mods back (spike c3).
//   - No hand-installed UE4SS proxy (Pal/Binaries/Win64/dwmapi.dll):
//     with the pack's loader-installed UE4SS it crashes the game.

use std::path::{Path, PathBuf};

/// Where the loader's settings live, under the game folder.
pub fn mod_settings_path(install: &Path) -> PathBuf {
    install.join("Mods").join("PalModSettings.ini")
}

/// The PackageNames of the packages in a pack folder: each numeric
/// sub-folder's Info.json (the loader ignores anything else, so does
/// this). Sorted, de-duplicated.
pub fn package_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return names;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let numeric = name
            .to_str()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !numeric || !entry.path().is_dir() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path().join("Info.json")) else {
            continue;
        };
        if let Some(n) = package_name_of(&raw) {
            if !names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                names.push(n);
            }
        }
    }
    names.sort();
    names
}

/// An Info.json's PackageName, when it has a usable one.
pub fn package_name_of(info_json: &str) -> Option<String> {
    let value: serde_json::Value =
        serde_json::from_str(info_json.trim_start_matches('\u{feff}')).ok()?;
    let name = value.get("PackageName")?.as_str()?.trim();
    // It becomes an ini value: refuse anything that would break the line.
    (!name.is_empty() && name.len() <= 128 && !name.contains(['\r', '\n', '=', '[', ']']))
        .then(|| name.to_string())
}

const SECTION: &str = "[PalModSettings]";

/// PalModSettings.ini with mods switched on and `packages` added to
/// ActiveModList, everything else kept as it was (the player's own
/// entries, WorkshopRootDir, any other key, other sections). Written with
/// CRLF, as the game writes it. An empty `existing` makes a fresh file.
pub fn merge_mod_settings(existing: &str, packages: &[String]) -> String {
    let mut lines: Vec<String> = existing
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }

    // Find our section, or add it.
    let start = match lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case(SECTION))
    {
        Some(i) => i,
        None => {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(SECTION.to_string());
            lines.len() - 1
        }
    };
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |p| start + 1 + p);

    let key_of = |l: &str| {
        l.split_once('=')
            .map(|(k, _)| k.trim().to_ascii_lowercase())
    };
    let mut body: Vec<String> = lines[start + 1..end].to_vec();

    // Mods on.
    match body
        .iter()
        .position(|l| key_of(l).as_deref() == Some("bglobalenablemod"))
    {
        Some(i) => body[i] = "bGlobalEnableMod=True".to_string(),
        None => body.insert(0, "bGlobalEnableMod=True".to_string()),
    }

    // The pack's packages, after the last existing ActiveModList line.
    let active: Vec<String> = body
        .iter()
        .filter(|l| key_of(l).as_deref() == Some("activemodlist"))
        .filter_map(|l| l.split_once('=').map(|(_, v)| v.trim().to_string()))
        .collect();
    let mut insert_at = body
        .iter()
        .rposition(|l| key_of(l).as_deref() == Some("activemodlist"))
        .map_or_else(
            || {
                body.iter()
                    .position(|l| key_of(l).as_deref() == Some("bglobalenablemod"))
                    .map_or(0, |i| i + 1)
            },
            |i| i + 1,
        );
    for name in packages {
        if active.iter().any(|a| a.eq_ignore_ascii_case(name)) {
            continue;
        }
        body.insert(insert_at, format!("ActiveModList={name}"));
        insert_at += 1;
    }

    let mut out: Vec<String> = Vec::with_capacity(lines.len() + packages.len() + 1);
    out.extend_from_slice(&lines[..=start]);
    out.extend(body);
    out.extend_from_slice(&lines[end..]);
    let mut text = out.join("\r\n");
    text.push_str("\r\n");
    text
}

/// A hand-installed UE4SS proxy DLL in the game's binaries, which crashes
/// Palworld alongside a loader-installed UE4SS (spike §3). Steam installs
/// (Win64); Game Pass would be WinGDK.
pub fn manual_ue4ss_proxy(install: &Path) -> Option<PathBuf> {
    ["Win64", "WinGDK"]
        .iter()
        .map(|b| {
            install
                .join("Pal")
                .join("Binaries")
                .join(b)
                .join("dwmapi.dll")
        })
        .find(|p| p.is_file())
}

/// Does this pack folder ship UE4SS (the loader's `UE4SS` package)? Then
/// a manual proxy DLL would crash the game.
pub fn pack_ships_ue4ss(root: &Path) -> bool {
    root.join("9000000000").join("Info.json").is_file()
}

/// The folder inside the game's install that the active pack is mirrored
/// into for the loader (`mirror_pack`).
pub const PACK_MIRROR_DIR: &str = "PackRelayWorkshop";

/// Mirror the pack folder `root` into the game's install folder (Steam's
/// game folder, or the Xbox app copy's Content folder), and return the
/// copy's path for -workshopdir.
///
/// Palworld's loader installs nothing from a -workshopdir inside the
/// user's AppData (Roaming or Local), where the launcher keeps its packs,
/// on Steam and Game Pass alike. The same files elsewhere (another drive,
/// C:\, the user profile, a folder that's merely named AppData) install
/// fine. Found in the join tests, 2026-10-02. The game's own folder always
/// works. The copy is skipped when the mirror already holds the same pack
/// (the same manifest sidecar), and otherwise built beside the old one
/// and swapped in, so a half-made copy is never what the game reads.
pub fn mirror_pack(install: &Path, root: &Path) -> anyhow::Result<PathBuf> {
    use crate::games::MANIFEST_SIDECAR;
    let mirror = install.join(PACK_MIRROR_DIR);
    let sidecar = std::fs::read(root.join(MANIFEST_SIDECAR)).ok();
    if sidecar.is_some() && std::fs::read(mirror.join(MANIFEST_SIDECAR)).ok() == sidecar {
        return Ok(mirror);
    }
    let staging = install.join(format!("{PACK_MIRROR_DIR}.staging"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|e| anyhow::anyhow!("clearing {}: {e}", staging.display()))?;
    }
    copy_tree(root, &staging, true)?;
    // The sidecar last: a mirror only counts as complete once it's there.
    if sidecar.is_some() {
        std::fs::copy(root.join(MANIFEST_SIDECAR), staging.join(MANIFEST_SIDECAR))
            .map_err(|e| anyhow::anyhow!("copying the pack's manifest: {e}"))?;
    }
    if mirror.exists() {
        std::fs::remove_dir_all(&mirror)
            .map_err(|e| anyhow::anyhow!("clearing {}: {e}", mirror.display()))?;
    }
    std::fs::rename(&staging, &mirror)
        .map_err(|e| anyhow::anyhow!("moving the pack into {}: {e}", mirror.display()))?;
    Ok(mirror)
}

/// Copy the tree at `from` to `to`, leaving out the root's manifest
/// sidecar when `skip_sidecar`.
fn copy_tree(from: &Path, to: &Path, skip_sidecar: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(to).map_err(|e| anyhow::anyhow!("creating {}: {e}", to.display()))?;
    let entries =
        std::fs::read_dir(from).map_err(|e| anyhow::anyhow!("reading {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if skip_sidecar && name == crate::games::MANIFEST_SIDECAR {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst, false)?;
        } else {
            std::fs::copy(&src, &dst)
                .map_err(|e| anyhow::anyhow!("copying {}: {e}", src.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "pr-palworld-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    #[test]
    fn reads_package_names_from_numeric_folders_only() {
        let root = tmp();
        for (dir, info) in [
            (
                "9000000000",
                r#"{"PackageName":"UE4SSExperimentalPW","InstallRule":[]}"#,
            ),
            ("9123456789", "\u{feff}{\"PackageName\":\"BetterCamp\"}"),
            ("9123456790", r#"{"PackageName":"bettercamp"}"#),
            ("notes", r#"{"PackageName":"Ignored"}"#),
            ("9000000002", r#"{"PackageName":"Bad=Name"}"#),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("Info.json"), info).unwrap();
        }
        std::fs::write(root.join("_packrelay-manifest.json"), "{}").unwrap();
        assert_eq!(
            package_names(&root),
            vec!["BetterCamp", "UE4SSExperimentalPW"]
        );
        assert!(pack_ships_ue4ss(&root));
        assert!(package_names(&root.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn switches_mods_on_and_adds_packages_keeping_the_players_entries() {
        let existing = "[PalModSettings]\r\nbGlobalEnableMod=False\r\nWorkshopRootDir=G:\\SteamLibrary\\steamapps\\workshop\\content\\1623730\r\nActiveModList=CampPalSize\r\nConfigVersion=1.0\r\n\r\n";
        let merged = merge_mod_settings(
            existing,
            &[
                "UE4SSExperimentalPW".into(),
                "campPALsize".into(),
                "BetterCamp".into(),
            ],
        );
        assert_eq!(
            merged,
            "[PalModSettings]\r\nbGlobalEnableMod=True\r\nWorkshopRootDir=G:\\SteamLibrary\\steamapps\\workshop\\content\\1623730\r\nActiveModList=CampPalSize\r\nActiveModList=UE4SSExperimentalPW\r\nActiveModList=BetterCamp\r\nConfigVersion=1.0\r\n"
        );
        // Running it again changes nothing.
        assert_eq!(merge_mod_settings(&merged, &["BetterCamp".into()]), merged);
    }

    #[test]
    fn writes_a_fresh_file_and_leaves_other_sections_alone() {
        assert_eq!(
            merge_mod_settings("", &["A".into()]),
            "[PalModSettings]\r\nbGlobalEnableMod=True\r\nActiveModList=A\r\n"
        );
        let other = "[Other]\nkey=1\n";
        assert_eq!(
            merge_mod_settings(other, &["A".into()]),
            "[Other]\r\nkey=1\r\n\r\n[PalModSettings]\r\nbGlobalEnableMod=True\r\nActiveModList=A\r\n"
        );
        let before_other =
            "[PalModSettings]\nbGlobalEnableMod=False\n[Other]\nActiveModList=NotOurs\n";
        assert_eq!(
            merge_mod_settings(before_other, &["A".into()]),
            "[PalModSettings]\r\nbGlobalEnableMod=True\r\nActiveModList=A\r\n[Other]\r\nActiveModList=NotOurs\r\n"
        );
    }

    #[test]
    fn finds_a_manual_ue4ss_proxy() {
        let install = tmp();
        assert_eq!(manual_ue4ss_proxy(&install), None);
        let bin = install.join("Pal").join("Binaries").join("Win64");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("dwmapi.dll"), b"x").unwrap();
        assert_eq!(manual_ue4ss_proxy(&install), Some(bin.join("dwmapi.dll")));
        assert_eq!(
            mod_settings_path(&install),
            install.join("Mods").join("PalModSettings.ini")
        );
        let _ = std::fs::remove_dir_all(install);
    }

    #[test]
    fn mirrors_the_pack_into_the_install() {
        use crate::games::MANIFEST_SIDECAR;
        let content = tmp();
        let root = tmp();
        std::fs::create_dir_all(root.join("9000000000/Mods")).unwrap();
        std::fs::write(root.join("9000000000/Info.json"), "{}").unwrap();
        std::fs::write(root.join("9000000000/Mods/mods.txt"), "a").unwrap();
        std::fs::write(root.join(MANIFEST_SIDECAR), "v1").unwrap();

        let mirror = mirror_pack(&content, &root).unwrap();
        assert_eq!(mirror, content.join(PACK_MIRROR_DIR));
        assert_eq!(
            std::fs::read_to_string(mirror.join("9000000000/Mods/mods.txt")).unwrap(),
            "a"
        );
        assert_eq!(
            std::fs::read_to_string(mirror.join(MANIFEST_SIDECAR)).unwrap(),
            "v1"
        );
        assert!(!content.join(format!("{PACK_MIRROR_DIR}.staging")).exists());

        // The same pack: left as it is.
        std::fs::write(mirror.join("marker"), "kept").unwrap();
        mirror_pack(&content, &root).unwrap();
        assert!(mirror.join("marker").exists());

        // Another pack: replaced whole, nothing of the old one left.
        std::fs::remove_dir_all(root.join("9000000000")).unwrap();
        std::fs::create_dir_all(root.join("9900000003")).unwrap();
        std::fs::write(root.join("9900000003/Info.json"), "{}").unwrap();
        std::fs::write(root.join(MANIFEST_SIDECAR), "v2").unwrap();
        mirror_pack(&content, &root).unwrap();
        assert!(!mirror.join("marker").exists());
        assert!(!mirror.join("9000000000").exists());
        assert!(mirror.join("9900000003/Info.json").exists());
        assert_eq!(
            std::fs::read_to_string(mirror.join(MANIFEST_SIDECAR)).unwrap(),
            "v2"
        );

        let _ = std::fs::remove_dir_all(&content);
        let _ = std::fs::remove_dir_all(&root);
    }
}
