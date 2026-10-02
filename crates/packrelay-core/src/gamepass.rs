// Finding a game's Game Pass (Xbox app) install (PackRelayCloud
// docs/multi-game/PALWORLD.md §7). The Xbox app marks each drive it
// installs games on with a `.GamingRoot` file at the drive's root, which
// names the library folders on that drive ("XboxGames"). Each game sits
// in `<library>/<title>/Content`, next to a MicrosoftGame.config whose
// <Identity Name="…"> is the game's package name. Reading that, rather
// than guessing the title folder, finds a game wherever the player put
// it, under whatever folder name.
//
// Only installs in such a library are ever found: older Game Pass
// installs under Program Files\WindowsApps aren't writable, so PackRelay
// couldn't install a pack into them anyway.

use std::path::{Path, PathBuf};

/// The library folders a `.GamingRoot` names, relative to its drive's
/// root. The file is `RGBX`, a little-endian u32 count, then that many
/// NUL-terminated UTF-16LE paths. Anything malformed gives what was read
/// before it.
pub fn parse_gaming_root(raw: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if raw.len() < 8 || &raw[..4] != b"RGBX" {
        return out;
    }
    let count = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let units: Vec<u16> = raw[8..]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut rest = units.as_slice();
    while out.len() < count {
        let Some(end) = rest.iter().position(|&u| u == 0) else {
            break;
        };
        let Ok(path) = String::from_utf16(&rest[..end]) else {
            break;
        };
        rest = &rest[end + 1..];
        // A folder on this drive, never somewhere else.
        let path = path.trim_start_matches(['\\', '/']).to_string();
        if !path.is_empty() && !path.contains(':') && !path.split(['\\', '/']).any(|p| p == "..") {
            out.push(path);
        }
    }
    out
}

/// Every Game Pass library on the given drive roots (`C:\`, `G:\`, …):
/// the folders each drive's `.GamingRoot` names, where they exist.
pub fn gaming_libraries(drive_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in drive_roots {
        let Ok(raw) = std::fs::read(root.join(".GamingRoot")) else {
            continue;
        };
        for rel in parse_gaming_root(&raw) {
            let lib = root.join(rel);
            if lib.is_dir() && !out.contains(&lib) {
                out.push(lib);
            }
        }
    }
    out
}

/// A MicrosoftGame.config's `<Identity Name="…">`. Not an XML parser:
/// the attribute is always written plainly on the Identity element.
pub fn identity_name(config: &str) -> Option<String> {
    let at = config.find("<Identity")?;
    let element = &config[at..];
    let element = &element[..element.find('>')?];
    let rest = &element[element.find(" Name=\"")? + " Name=\"".len()..];
    Some(rest[..rest.find('"')?].to_string()).filter(|n| !n.is_empty())
}

/// The first `<Executable Name="…">` in a MicrosoftGame.config, relative
/// to the Content folder (`Pal\Binaries\WinGDK\…-Shipping.exe`).
pub fn executable(config: &str) -> Option<String> {
    let at = config.find("<Executable ")?;
    let element = &config[at..];
    let element = &element[..element.find('>')?];
    let rest = &element[element.find(" Name=\"")? + " Name=\"".len()..];
    let exe = &rest[..rest.find('"')?];
    (!exe.is_empty() && !exe.contains(':') && !exe.split(['\\', '/']).any(|p| p == ".."))
        .then(|| exe.to_string())
}

/// The Content folder of the game whose package is `identity`, in any of
/// `libraries`.
pub fn find_install(identity: &str, libraries: &[PathBuf]) -> Option<PathBuf> {
    for lib in libraries {
        let Ok(titles) = std::fs::read_dir(lib) else {
            continue;
        };
        for title in titles.flatten() {
            let content = title.path().join("Content");
            if is_install_of(&content, identity) {
                return Some(content);
            }
        }
    }
    None
}

fn is_install_of(content: &Path, identity: &str) -> bool {
    read_config(content)
        .and_then(|raw| identity_name(&raw))
        .is_some_and(|n| n.eq_ignore_ascii_case(identity))
}

fn read_config(content: &Path) -> Option<String> {
    std::fs::read_to_string(content.join("MicrosoftGame.config"))
        .ok()
        .map(|raw| raw.trim_start_matches('\u{feff}').to_string())
}

/// The game's process name (`Palworld-WinGDK-Shipping.exe`), from the
/// install's MicrosoftGame.config.
pub fn process_name(content: &Path) -> Option<String> {
    let exe = executable(&read_config(content)?)?;
    exe.rsplit(['\\', '/']).next().map(str::to_string)
}

/// The Xbox app starts an Unreal game through its gamelaunchhelper with
/// no arguments; the game reads them from this file in its Content
/// folder instead (`../../../Pal/Pal.uproject` plus whatever follows).
pub const COMMAND_LINE_FILE: &str = "UECommandLine.txt";

/// `existing` (a UECommandLine.txt) with any -workshopdir taken out, and
/// `-workshopdir="<dir>"` added when `workshop_dir` is given. The rest of
/// the line is kept as it was. None when the folder can't be quoted
/// safely, which a PackRelay-owned data folder never is.
pub fn command_line_with_workshop_dir(
    existing: &str,
    workshop_dir: Option<&Path>,
) -> Option<String> {
    let mut line = without_workshop_dir(existing.trim_start_matches('\u{feff}'));
    if let Some(dir) = workshop_dir {
        let arg = crate::launch::workshop_dir_arg(dir)?;
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&arg);
    }
    Some(line)
}

/// The line with every `-workshopdir=…` token removed, quoted or not.
fn without_workshop_dir(line: &str) -> String {
    const KEY: &str = "-workshopdir=";
    let mut kept: Vec<&str> = Vec::new();
    let mut rest = line.trim();
    while !rest.is_empty() {
        let is_ours = rest
            .get(..KEY.len())
            .is_some_and(|k| k.eq_ignore_ascii_case(KEY));
        let end = if is_ours {
            let value = &rest[KEY.len()..];
            KEY.len()
                + match value.strip_prefix('"') {
                    Some(quoted) => quoted.find('"').map_or(value.len(), |i| i + 2),
                    None => value.find(char::is_whitespace).unwrap_or(value.len()),
                }
        } else {
            token_end(rest)
        };
        if !is_ours {
            kept.push(&rest[..end]);
        }
        rest = rest[end..].trim_start();
    }
    kept.join(" ")
}

/// Where the token at the start of `s` ends: at whitespace outside
/// double quotes.
fn token_end(s: &str) -> usize {
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => return i,
            _ => {}
        }
    }
    s.len()
}

/// Write the install's UECommandLine.txt so its next start loads
/// `workshop_dir` (or, None, loads no PackRelay pack). Returns what the
/// file held before, for putting back once the game has read it.
pub fn set_workshop_dir(content: &Path, workshop_dir: Option<&Path>) -> anyhow::Result<String> {
    let path = content.join(COMMAND_LINE_FILE);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let line = command_line_with_workshop_dir(&existing, workshop_dir)
        .ok_or_else(|| anyhow::anyhow!("the pack folder can't be passed to the game safely"))?;
    if line != existing {
        std::fs::write(&path, &line)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    }
    Ok(existing)
}

/// Take PackRelay's -workshopdir back out of the install's
/// UECommandLine.txt, so starting the game from the Xbox app loads no
/// PackRelay pack (the loader then uninstalls it, as a plain Steam
/// launch does).
pub fn clear_workshop_dir(content: &Path) -> anyhow::Result<()> {
    let path = content.join(COMMAND_LINE_FILE);
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let line = without_workshop_dir(existing.trim_start_matches('\u{feff}'));
    if line != existing {
        std::fs::write(&path, &line)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    }
    Ok(())
}

/// The folder inside the install's Content folder that the active pack is
/// mirrored into for the game's loader (`mirror_pack`).
pub const PACK_MIRROR_DIR: &str = "PackRelayWorkshop";

/// Mirror the pack folder `root` into the install, and return the copy's
/// path for -workshopdir.
///
/// A Game Pass game runs as a packaged app, and Windows gives packaged
/// apps their own view of AppData: the game can't see the launcher's
/// pack folder there, and installs nothing from it (found in the Game
/// Pass join test, 2026-10-02). Its own Content folder it can always
/// read. The copy is skipped when the mirror already holds the same pack
/// (the same manifest sidecar), and otherwise built beside the old one
/// and swapped in, so a half-made copy is never what the game reads.
pub fn mirror_pack(content: &Path, root: &Path) -> anyhow::Result<PathBuf> {
    use crate::games::MANIFEST_SIDECAR;
    let mirror = content.join(PACK_MIRROR_DIR);
    let sidecar = std::fs::read(root.join(MANIFEST_SIDECAR)).ok();
    if sidecar.is_some() && std::fs::read(mirror.join(MANIFEST_SIDECAR)).ok() == sidecar {
        return Ok(mirror);
    }
    let staging = content.join(format!("{PACK_MIRROR_DIR}.staging"));
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

/// The URI that starts the game through the Xbox app.
pub fn launch_uri(app_user_model_id: &str) -> String {
    format!("shell:AppsFolder\\{app_user_model_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gaming_root(paths: &[&str]) -> Vec<u8> {
        let mut raw = b"RGBX".to_vec();
        raw.extend((paths.len() as u32).to_le_bytes());
        for p in paths {
            for u in p.encode_utf16().chain([0]) {
                raw.extend(u.to_le_bytes());
            }
        }
        raw
    }

    #[test]
    fn reads_a_gaming_root() {
        assert_eq!(
            parse_gaming_root(&gaming_root(&["XboxGames"])),
            vec!["XboxGames"]
        );
        assert_eq!(
            parse_gaming_root(&gaming_root(&["\\XboxGames", "Games\\Xbox"])),
            vec!["XboxGames", "Games\\Xbox"]
        );
        // Off this drive, or out of it: skipped.
        assert_eq!(
            parse_gaming_root(&gaming_root(&["D:\\Elsewhere", "..\\Up", "Ok"])),
            vec!["Ok"]
        );
        assert!(parse_gaming_root(b"RGBY\x01\0\0\0").is_empty());
        assert!(parse_gaming_root(b"RGBX").is_empty());
        // Truncated: what was read before it.
        let mut cut = gaming_root(&["One", "Two"]);
        cut.truncate(cut.len() - 4);
        assert_eq!(parse_gaming_root(&cut), vec!["One"]);
    }

    const CONFIG: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Game configVersion="0">
  <Identity Name="Example.Game" Publisher="CN=Example" Version="1.2.3.0" />
  <ExecutableList>
    <Executable Name="Game\Binaries\WinGDK\Game-WinGDK-Shipping.exe" Id="App" TargetDeviceFamily="PC" />
  </ExecutableList>
</Game>"#;

    #[test]
    fn reads_a_microsoft_game_config() {
        assert_eq!(identity_name(CONFIG).as_deref(), Some("Example.Game"));
        assert_eq!(
            executable(CONFIG).as_deref(),
            Some("Game\\Binaries\\WinGDK\\Game-WinGDK-Shipping.exe")
        );
        assert_eq!(identity_name("<Game />"), None);
        assert_eq!(executable(r#"<Executable Name="..\evil.exe" />"#), None);
    }

    #[test]
    fn adds_and_takes_out_the_workshop_dir() {
        let base = "../../../Pal/Pal.uproject";
        // Absolute on whichever OS runs the tests (CI is Linux).
        let dir = std::env::temp_dir()
            .join("PackRelay")
            .join("palworld-workshop");
        let with = command_line_with_workshop_dir(base, Some(&dir)).unwrap();
        assert_eq!(
            with,
            format!(
                "../../../Pal/Pal.uproject -workshopdir=\"{}\"",
                dir.display()
            )
        );
        // Again: replaced, not added twice.
        assert_eq!(
            command_line_with_workshop_dir(&with, Some(&dir)).unwrap(),
            with
        );
        // Off: back to what it was.
        assert_eq!(command_line_with_workshop_dir(&with, None).unwrap(), base);
        // Other args stay, quoted ones included; an unquoted -workshopdir
        // goes too.
        assert_eq!(
            command_line_with_workshop_dir(
                "\u{feff}../../../Pal/Pal.uproject -log -WorkshopDir=D:\\Old \"-x=a b\"\r\n",
                None
            )
            .unwrap(),
            "../../../Pal/Pal.uproject -log \"-x=a b\""
        );
        // A folder that would break out of the quotes: refused.
        assert_eq!(
            command_line_with_workshop_dir(base, Some(Path::new("C:\\a\"b"))),
            None
        );
        assert_eq!(
            command_line_with_workshop_dir(base, Some(Path::new("relative"))),
            None
        );
    }

    #[test]
    fn sets_and_clears_the_file() {
        let content = temp("cmdline");
        let file = content.join(COMMAND_LINE_FILE);
        std::fs::write(&file, "../../../Pal/Pal.uproject").unwrap();
        let dir = content.join("workshop");

        let before = set_workshop_dir(&content, Some(&dir)).unwrap();
        assert_eq!(before, "../../../Pal/Pal.uproject");
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .contains("-workshopdir=\""));

        clear_workshop_dir(&content).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "../../../Pal/Pal.uproject"
        );
        // No file: nothing to clear.
        clear_workshop_dir(&content.join("none")).unwrap();
        let _ = std::fs::remove_dir_all(&content);
    }

    #[test]
    fn mirrors_the_pack_into_the_install() {
        use crate::games::MANIFEST_SIDECAR;
        let content = temp("mirror-content");
        let root = temp("mirror-root");
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

    #[test]
    fn launches_through_the_apps_folder() {
        assert_eq!(
            launch_uri("PocketpairInc.Palworld_ad4psfrxyesvt!AppPalShipping"),
            "shell:AppsFolder\\PocketpairInc.Palworld_ad4psfrxyesvt!AppPalShipping"
        );
    }

    fn temp(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(
            format!(
                "packrelay-gamepass-{label}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
            )
            .replace([':', ' '], "-"),
        );
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn finds_a_game_by_its_package_name_in_any_library() {
        let drive_a = temp("a");
        let drive_b = temp("b");
        std::fs::write(drive_a.join(".GamingRoot"), gaming_root(&["XboxGames"])).unwrap();
        std::fs::write(
            drive_b.join(".GamingRoot"),
            gaming_root(&["XboxGames", "Missing"]),
        )
        .unwrap();
        let other = drive_a.join("XboxGames/Other Game/Content");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("MicrosoftGame.config"),
            CONFIG.replace("Example.Game", "Other"),
        )
        .unwrap();
        // Under a renamed title folder, with a BOM.
        let game = drive_b.join("XboxGames/Renamed/Content");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(
            game.join("MicrosoftGame.config"),
            format!("\u{feff}{CONFIG}"),
        )
        .unwrap();

        let libs = gaming_libraries(&[drive_a.clone(), drive_b.clone(), temp("none")]);
        assert_eq!(
            libs,
            vec![drive_a.join("XboxGames"), drive_b.join("XboxGames")]
        );
        assert_eq!(find_install("example.game", &libs), Some(game.clone()));
        assert_eq!(find_install("Nope", &libs), None);
        assert_eq!(
            process_name(&game).as_deref(),
            Some("Game-WinGDK-Shipping.exe")
        );

        let _ = std::fs::remove_dir_all(&drive_a);
        let _ = std::fs::remove_dir_all(&drive_b);
    }
}
