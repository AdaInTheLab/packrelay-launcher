// Joining a server: turning a catalog `connectAddress` into the
// command-line arguments or Steam URI that open a game straight into
// it, per game (games.rs `ConnectArgs`). Moved out of the app so every
// game's form is tested in one place.

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
    }
}

/// The Steam URI that launches `game`, joining `connect_address` when
/// given: `steam://rungameid/<appid>` bare, or
/// `steam://run/<appid>//<args>` with the game's connect args,
/// space-separated and percent-encoded. An address that isn't a
/// plausible host falls back to the bare launch.
pub fn steam_url(game: &GameLayout, connect_address: Option<&str>) -> String {
    let appid = game.steam_appid;
    let bare = format!("steam://rungameid/{appid}");
    let Some(raw) = connect_address.map(str::trim).filter(|s| !s.is_empty()) else {
        return bare;
    };
    let (host, port) = parse_connect_address(game, raw);
    if !is_safe_host(&host) {
        return bare;
    }
    let args: Vec<String> = connect_args(game, &host, port)
        .iter()
        .map(|a| percent_encode_arg(a))
        .collect();
    format!("steam://run/{appid}//{}", args.join("%20"))
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
    use crate::games::{SEVEN_DAYS, VALHEIM};

    #[test]
    fn seven_days_keeps_its_connect_args_and_url() {
        assert_eq!(
            connect_args(&SEVEN_DAYS, "play.example.com", 26900),
            vec!["-connecttoip=play.example.com", "-connecttoport=26900"]
        );
        // Exactly the URL the app built before this moved here.
        assert_eq!(
            steam_url(&SEVEN_DAYS, Some("play.example.com:26901")),
            "steam://run/251570//-connecttoip=play.example.com%20-connecttoport=26901"
        );
        assert_eq!(steam_url(&SEVEN_DAYS, None), "steam://rungameid/251570");
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
            steam_url(&VALHEIM, Some("vikings.example.com:2466")),
            "steam://run/892970//+connect%20vikings.example.com:2466"
        );
    }

    #[test]
    fn an_unsafe_address_launches_bare() {
        assert_eq!(
            steam_url(&VALHEIM, Some("evil host\";calc")),
            "steam://rungameid/892970"
        );
        assert!(!is_safe_host("a b"));
        assert!(is_safe_host("[::1]"));
    }
}
