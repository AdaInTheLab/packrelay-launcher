// Joining a server: turning a catalog `connectAddress` into the
// command-line arguments or Steam URI that open a game straight into
// it, per game (games.rs `ConnectArgs`). Moved out of the app so every
// game's form is tested in one place.

use std::path::Path;

use crate::games::{ConnectArgs, GameLayout};

/// Split a `host[:port]` address into its parts, defaulting to the
/// game's port. Splits on the LAST colon so IPv6 literals survive,
/// though no supported game's connect args take IPv6 today.
pub fn parse_connect_address(game: &GameLayout, raw: &str) -> (String, u16) {
    match raw.rsplit_once(':') {
        Some((host, port_str)) => {
            let port = port_str.parse::<u16>().unwrap_or(game.default_port);
            (host.trim().to_string(), port)
        }
        None => (raw.trim().to_string(), game.default_port),
    }
}

/// Plausibility check: ASCII letters, digits, `.`, `-`, `:` and the
/// brackets of an IPv6 literal. Rejects spaces, quotes, command
/// separators -- anything that could escape the Steam URI or an argv
/// entry into shell-like territory.
pub fn is_safe_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

/// The client arguments that join `host:port`.
pub fn connect_args(game: &GameLayout, host: &str, port: u16) -> Vec<String> {
    match game.connect_args {
        ConnectArgs::ConnectToIpPort => {
            vec![
                format!("-connecttoip={host}"),
                format!("-connecttoport={port}"),
            ]
        }
        ConnectArgs::PlusConnect => vec!["+connect".to_string(), format!("{host}:{port}")],
        // Palworld: nothing to pass; the player pastes the address.
        ConnectArgs::None => Vec::new(),
    }
}

/// Palworld's `-workshopdir="<dir>"`: where its mod loader reads packages
/// from for this launch (palworld-spike.md §2, Q1). Quoted as the game
/// expects. None for a path that isn't absolute or would break out of
/// the quotes, which a PackRelay-owned data folder never does.
pub fn workshop_dir_arg(dir: &Path) -> Option<String> {
    let s = dir.to_str()?;
    if s.is_empty() || s.contains('"') || !dir.is_absolute() {
        return None;
    }
    Some(format!("-workshopdir=\"{s}\""))
}

/// The Steam URI that launches `game`, joining `connect_address` when
/// given and passing `workshop_dir` (Palworld's -workshopdir) when given:
/// `steam://rungameid/<appid>` bare, or `steam://run/<appid>//<args>`
/// with the args space-separated and percent-encoded. An address that
/// isn't a plausible host is dropped, as is a workshop dir that can't be
/// quoted safely.
pub fn steam_url(
    game: &GameLayout,
    connect_address: Option<&str>,
    workshop_dir: Option<&Path>,
) -> String {
    let appid = game.steam_appid;
    let mut args: Vec<String> = Vec::new();
    if let Some(raw) = connect_address.map(str::trim).filter(|s| !s.is_empty()) {
        let (host, port) = parse_connect_address(game, raw);
        if is_safe_host(&host) {
            args.extend(connect_args(game, &host, port));
        }
    }
    if let Some(arg) = workshop_dir.and_then(workshop_dir_arg) {
        args.push(arg);
    }
    if args.is_empty() {
        return format!("steam://rungameid/{appid}");
    }
    let encoded: Vec<String> = args.iter().map(|a| percent_encode_arg(a)).collect();
    format!("steam://run/{appid}//{}", encoded.join("%20"))
}

/// Conservative percent-encoder: keeps unreserved URL chars and the `=`
/// / `:` / `+` a connect arg is made of, escapes everything else. Only
/// ever called on args built from a host that passed `is_safe_host`.
fn percent_encode_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~' | b'=' | b':' | b'+')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::games::{PALWORLD, SEVEN_DAYS, VALHEIM};

    #[test]
    fn seven_days_keeps_its_connect_args_and_url() {
        assert_eq!(
            connect_args(&SEVEN_DAYS, "play.example.com", 26900),
            vec!["-connecttoip=play.example.com", "-connecttoport=26900"]
        );
        // Exactly the URL the app built before this moved here.
        assert_eq!(
            steam_url(&SEVEN_DAYS, Some("play.example.com:26901"), None),
            "steam://run/251570//-connecttoip=play.example.com%20-connecttoport=26901"
        );
        assert_eq!(
            steam_url(&SEVEN_DAYS, None, None),
            "steam://rungameid/251570"
        );
    }

    #[test]
    fn valheim_joins_with_plus_connect_on_its_own_port() {
        assert_eq!(
            connect_args(&VALHEIM, "10.0.0.5", 2456),
            vec!["+connect", "10.0.0.5:2456"]
        );
        assert_eq!(
            parse_connect_address(&VALHEIM, "10.0.0.5"),
            ("10.0.0.5".into(), 2456)
        );
        assert_eq!(
            steam_url(&VALHEIM, Some("vikings.example.com:2466"), None),
            "steam://run/892970//+connect%20vikings.example.com:2466"
        );
    }

    #[test]
    fn an_unsafe_address_launches_bare() {
        assert_eq!(
            steam_url(&VALHEIM, Some("evil host\";calc"), None),
            "steam://rungameid/892970"
        );
        assert!(!is_safe_host("a b"));
        assert!(is_safe_host("[::1]"));
    }

    #[test]
    fn palworld_takes_no_connect_args_but_its_workshop_dir() {
        assert!(connect_args(&PALWORLD, "pal.example.com", 8211).is_empty());
        assert_eq!(
            parse_connect_address(&PALWORLD, "pal.example.com"),
            ("pal.example.com".into(), 8211)
        );
        #[cfg(windows)]
        let dir = Path::new(r"C:\Users\Ada Smith\AppData\Roaming\PackRelay\palworld-workshop");
        #[cfg(not(windows))]
        let dir = Path::new("/home/ada smith/.local/share/packrelay/palworld-workshop");
        let arg = workshop_dir_arg(dir).unwrap();
        assert_eq!(arg, format!("-workshopdir=\"{}\"", dir.display()));
        // The address is shown to the player, not passed; the folder is.
        let url = steam_url(&PALWORLD, Some("pal.example.com:8211"), Some(dir));
        assert!(
            url.starts_with("steam://run/1623730//-workshopdir=%22"),
            "{url}"
        );
        assert!(url.ends_with("%22"), "{url}");
        assert!(!url.contains("pal.example.com"));
        assert_eq!(
            steam_url(&PALWORLD, Some("pal.example.com"), None),
            "steam://rungameid/1623730"
        );
    }

    #[test]
    fn a_workshop_dir_that_could_break_out_of_its_quotes_is_refused() {
        assert_eq!(workshop_dir_arg(Path::new("relative/dir")), None);
        #[cfg(windows)]
        assert_eq!(workshop_dir_arg(Path::new(r#"C:\a" -evil "b"#)), None);
        #[cfg(not(windows))]
        assert_eq!(workshop_dir_arg(Path::new(r#"/a" -evil "b"#)), None);
    }
}
