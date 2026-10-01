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
}
