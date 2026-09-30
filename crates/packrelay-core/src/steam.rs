// Finding a game's Steam install (multi-game). Steam lists its
// libraries in `<steam>/steamapps/libraryfolders.vdf`; each installed
// game has `<library>/steamapps/appmanifest_<appid>.acf`, whose
// `installdir` names its folder under steamapps/common. Reading the
// app manifest, rather than guessing the folder name, finds a game
// wherever Steam put it.

use std::path::{Path, PathBuf};

use crate::games::GameLayout;

/// Every `"path" "<value>"` in a libraryfolders.vdf. Not a full Valve
/// KeyValues parser: the path entries always sit on one line in the
/// format Steam writes.
pub fn parse_vdf_library_paths(raw: &str) -> Vec<PathBuf> {
    raw.lines()
        .filter_map(|line| quoted_value(line.trim(), &["\"path\"", "\"Path\""]))
        .map(PathBuf::from)
        .collect()
}

/// The `installdir` of an appmanifest_<appid>.acf.
pub fn parse_acf_installdir(raw: &str) -> Option<String> {
    raw.lines()
        .find_map(|line| quoted_value(line.trim(), &["\"installdir\""]))
        .filter(|dir| !dir.is_empty() && !dir.contains(['/', '\\']) && dir != "." && dir != "..")
}

/// `"key"   "value"` → value, with VDF's escaped backslashes undone.
fn quoted_value(line: &str, keys: &[&str]) -> Option<String> {
    let rest = keys.iter().find_map(|k| line.strip_prefix(k))?;
    let rest = rest.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].replace("\\\\", "\\"))
}

/// Every Steam library under the given Steam roots: each root itself,
/// plus what its libraryfolders.vdf lists, without duplicates.
pub fn steam_libraries(steam_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in steam_roots {
        let mut add = |p: PathBuf| {
            if !out.contains(&p) {
                out.push(p);
            }
        };
        add(root.clone());
        let vdf = root.join("steamapps").join("libraryfolders.vdf");
        if let Ok(raw) = std::fs::read_to_string(&vdf) {
            for lib in parse_vdf_library_paths(&raw) {
                add(lib);
            }
        }
    }
    out
}

/// The game's install folder: the library whose app manifest names it
/// (or, failing that, the one with the game's usual folder), wherever
/// the game's executable is actually there.
pub fn find_install(game: &GameLayout, libraries: &[PathBuf]) -> Option<PathBuf> {
    for lib in libraries {
        let steamapps = lib.join("steamapps");
        let acf = steamapps.join(format!("appmanifest_{}.acf", game.steam_appid));
        let dir = std::fs::read_to_string(&acf)
            .ok()
            .and_then(|raw| parse_acf_installdir(&raw))
            .unwrap_or_else(|| game.steam_install_dir.to_string());
        let install = steamapps.join("common").join(dir);
        if install.join(game.exe).is_file() {
            return Some(install);
        }
    }
    None
}

/// The game's live root inside its install (Valheim: <install>/BepInEx),
/// for a game whose pack lives there.
pub fn live_root_in(game: &GameLayout, install: &Path) -> Option<PathBuf> {
    game.live_root_in_install.map(|rel| install.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games::{SEVEN_DAYS, VALHEIM};

    #[test]
    fn reads_library_paths_and_installdir() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"G:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(
            parse_vdf_library_paths(vdf),
            vec![
                PathBuf::from("C:\\Program Files (x86)\\Steam"),
                PathBuf::from("G:\\SteamLibrary")
            ]
        );
        let acf =
            "\"AppState\"\n{\n\t\"appid\"\t\t\"892970\"\n\t\"installdir\"\t\t\"Valheim\"\n}\n";
        assert_eq!(parse_acf_installdir(acf).as_deref(), Some("Valheim"));
        assert_eq!(parse_acf_installdir("\"installdir\" \"..\""), None);
        assert_eq!(parse_acf_installdir("\"installdir\" \"a\\\\b\""), None);
    }

    fn temp(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(
            format!(
                "packrelay-steam-{label}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
            )
            .replace([':', ' '], "-"),
        );
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn finds_a_game_where_its_app_manifest_says() {
        let lib_a = temp("a");
        let lib_b = temp("b");
        // 7DTD in the first library, under its usual folder, no manifest.
        let seven = lib_a.join("steamapps/common/7 Days To Die");
        std::fs::create_dir_all(&seven).unwrap();
        std::fs::write(seven.join("7DaysToDie.exe"), "").unwrap();
        // Valheim in the second, in a renamed folder its manifest names.
        let valheim = lib_b.join("steamapps/common/Valheim Beta");
        std::fs::create_dir_all(&valheim).unwrap();
        std::fs::write(valheim.join("valheim.exe"), "").unwrap();
        std::fs::write(
            lib_b.join("steamapps/appmanifest_892970.acf"),
            "\"AppState\"\n{\n\t\"installdir\"\t\t\"Valheim Beta\"\n}\n",
        )
        .unwrap();

        let libs = vec![lib_a.clone(), lib_b.clone()];
        assert_eq!(find_install(&SEVEN_DAYS, &libs), Some(seven));
        assert_eq!(find_install(&VALHEIM, &libs), Some(valheim.clone()));
        assert_eq!(
            live_root_in(&VALHEIM, &valheim),
            Some(valheim.join("BepInEx"))
        );
        assert_eq!(live_root_in(&SEVEN_DAYS, &valheim), None);
        assert_eq!(find_install(&VALHEIM, &[lib_a]), None);
    }
}
