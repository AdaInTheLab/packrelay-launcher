// The games this launcher manages packs for, and where each one's
// pack-managed files live (multi-game; PackRelayCloud's
// docs/multi-game/DESIGN.md §7). Mirrors the cloud's src/lib/games.ts:
// `id` is the manifest's `game`.
//
// Everything the profile system does is "capture what's live into the
// outgoing pack, then deploy the incoming pack", over three slots:
//   - mods:   the pack's files (a manifest's paths are relative to it)
//   - saves:  per-pack save state
//   - worlds: profile-shared worlds
// A game says where each slot lives under its *live root* (7DTD: the
// %APPDATA%\7DaysToDie userdata dir; Valheim: <install>\BepInEx), or
// that it has no such slot.

/// One entry swapped between a pack's store `mods/` dir and the live
/// mods slot, when a game swaps a list of entries rather than the whole
/// directory (see `GameLayout::mods_entries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapEntry {
    /// Relative path, the same under the store `mods/` and the live slot.
    pub rel: &'static str,
    pub is_file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameLayout {
    /// Manifest `game`, as the cloud's registry names it.
    pub id: &'static str,
    pub display_name: &'static str,
    /// For error text: "7DTD", "Valheim".
    pub short_name: &'static str,
    pub steam_appid: u32,
    /// Where the mods slot is under the live root. "" = the live root
    /// itself.
    pub mods_live: &'static str,
    /// None: the whole mods slot is the pack's (7DTD's Mods/). Some:
    /// only these entries are, and everything else in the slot is left
    /// alone (Valheim's BepInEx/ also holds the loader's own core/,
    /// cache/ and logs, which no pack owns).
    pub mods_entries: Option<&'static [SwapEntry]>,
    /// Per-pack saves, when the game keeps them where we can swap them.
    pub saves_live: Option<&'static str>,
    /// Profile-shared worlds, likewise.
    pub worlds_live: Option<&'static str>,
}

/// The sidecar copy of the installed manifest, at the root of the mods
/// slot (install.rs writes it there).
pub const MANIFEST_SIDECAR: &str = "_packrelay-manifest.json";

pub const SEVEN_DAYS: GameLayout = GameLayout {
    id: "7d2d",
    display_name: "7 Days to Die",
    short_name: "7DTD",
    steam_appid: 251570,
    mods_live: "Mods",
    mods_entries: None,
    saves_live: Some("Saves"),
    worlds_live: Some("GeneratedWorlds"),
};

/// What a Valheim pack owns under BepInEx/: the folders r2modman
/// installs packages into (the cloud lays Thunderstore zips out the same
/// way) and the manifest sidecar. BepInEx/core, cache and LogOutput are
/// the loader's, installed by the framework step, never swapped.
const VALHEIM_ENTRIES: &[SwapEntry] = &[
    SwapEntry {
        rel: "plugins",
        is_file: false,
    },
    SwapEntry {
        rel: "patchers",
        is_file: false,
    },
    SwapEntry {
        rel: "config",
        is_file: false,
    },
    SwapEntry {
        rel: "monomod",
        is_file: false,
    },
    SwapEntry {
        rel: MANIFEST_SIDECAR,
        is_file: true,
    },
];

pub const VALHEIM: GameLayout = GameLayout {
    id: "valheim",
    display_name: "Valheim",
    short_name: "Valheim",
    steam_appid: 892970,
    mods_live: "",
    mods_entries: Some(VALHEIM_ENTRIES),
    // Worlds and characters live in LocalLow/IronGate/Valheim, shared by
    // every server the player joins; a pack never owns them (DESIGN.md
    // §7). Nothing to swap.
    saves_live: None,
    worlds_live: None,
};

pub const GAMES: &[GameLayout] = &[SEVEN_DAYS, VALHEIM];

pub fn game_by_id(id: &str) -> Option<&'static GameLayout> {
    GAMES.iter().find(|g| g.id == id)
}
