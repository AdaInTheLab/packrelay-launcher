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

/// How a game takes "join this server" on its command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectArgs {
    /// `-connecttoip=<host> -connecttoport=<port>` (7DTD).
    ConnectToIpPort,
    /// `+connect <host>:<port>` (Valheim, and other Source-style games).
    PlusConnect,
    /// No way to join from the command line (Palworld): the game starts
    /// at its title screen and the player pastes the address into its
    /// own join box.
    None,
}

/// A game's Game Pass (Xbox app) edition, when PackRelay supports it
/// (gamepass.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XboxLayout {
    /// The package's identity name, as its MicrosoftGame.config gives it.
    pub identity: &'static str,
    /// The app to start: `<package family name>!<application id>`, opened
    /// as `shell:AppsFolder\<this>` (the Xbox app's own launch).
    pub app_user_model_id: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameLayout {
    /// Manifest `game`, as the cloud's registry names it.
    pub id: &'static str,
    pub display_name: &'static str,
    /// For error text: "7DTD", "Valheim".
    pub short_name: &'static str,
    pub steam_appid: u32,
    /// The install folder under steamapps/common when the app manifest
    /// doesn't say otherwise.
    pub steam_install_dir: &'static str,
    /// The Game Pass edition, when PackRelay can install packs into it.
    pub xbox: Option<XboxLayout>,
    /// The client executable, relative to the install folder.
    pub exe: &'static str,
    /// The client port a server uses when its address names none.
    pub default_port: u16,
    pub connect_args: ConnectArgs,
    /// Where the live root is relative to the install folder, for a game
    /// whose pack lives inside it (Valheim: "BepInEx"). None: the live
    /// root is elsewhere (7DTD's %APPDATA%/7DaysToDie, or
    /// `live_root_in_data`).
    pub live_root_in_install: Option<&'static str>,
    /// A live root inside the launcher's own data folder, for a game whose
    /// mod loader reads packs from wherever it's told (Palworld's
    /// -workshopdir; PackRelayCloud docs/multi-game/palworld-spike.md
    /// §5). The folder belongs to PackRelay, never to the player.
    pub live_root_in_data: Option<&'static str>,
    /// Launch with `-workshopdir="<live root>"`, so the game's own mod
    /// loader installs the active pack's packages (Palworld).
    pub launch_with_workshop_dir: bool,
    /// The launcher installs the manifest's `framework` into the game
    /// folder before the pack (Valheim's BepInExPack). False for a game
    /// whose framework ships inside the pack as a package of its own
    /// (Palworld's UE4SS), or that has none.
    pub installs_framework: bool,
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
    steam_install_dir: "7 Days To Die",
    xbox: None,
    exe: "7DaysToDie.exe",
    default_port: 26900,
    connect_args: ConnectArgs::ConnectToIpPort,
    live_root_in_install: None,
    live_root_in_data: None,
    launch_with_workshop_dir: false,
    installs_framework: false,
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
    steam_install_dir: "Valheim",
    xbox: None,
    exe: "valheim.exe",
    default_port: 2456,
    connect_args: ConnectArgs::PlusConnect,
    live_root_in_install: Some("BepInEx"),
    live_root_in_data: None,
    launch_with_workshop_dir: false,
    installs_framework: true,
    mods_live: "",
    mods_entries: Some(VALHEIM_ENTRIES),
    // Worlds and characters live in LocalLow/IronGate/Valheim, shared by
    // every server the player joins; a pack never owns them (DESIGN.md
    // §7). Nothing to swap.
    saves_live: None,
    worlds_live: None,
};

/// Palworld (preview in the cloud). A pack is a folder of packages for
/// Palworld's own mod loader (`<numeric id>/Info.json` plus files). It
/// lives in PackRelay's data folder, and the game is launched with
/// -workshopdir pointing at it, so the loader installs it, swaps it out
/// when another pack's folder is passed, and uninstalls it on a plain
/// launch (palworld-spike.md §2, §5). The whole folder is the pack's.
/// UE4SS rides in the pack as package 9000000000 when a mod needs it,
/// so there's no framework step. Worlds live in the game's own save
/// folders and are never swapped. The Game Pass edition works the same
/// way (gamepass.rs).
pub const PALWORLD: GameLayout = GameLayout {
    id: "palworld",
    display_name: "Palworld",
    short_name: "Palworld",
    steam_appid: 1623730,
    steam_install_dir: "Palworld",
    // The Game Pass build has the same mod loader as Steam's, and reads
    // -workshopdir from its UECommandLine.txt (PALWORLD.md §7).
    xbox: Some(XboxLayout {
        identity: "PocketpairInc.Palworld",
        app_user_model_id: "PocketpairInc.Palworld_ad4psfrxyesvt!AppPalShipping",
    }),
    exe: "Palworld.exe",
    default_port: 8211,
    connect_args: ConnectArgs::None,
    live_root_in_install: None,
    live_root_in_data: Some("palworld-workshop"),
    launch_with_workshop_dir: true,
    installs_framework: false,
    mods_live: "",
    mods_entries: None,
    saves_live: None,
    worlds_live: None,
};

pub const GAMES: &[GameLayout] = &[SEVEN_DAYS, VALHEIM, PALWORLD];

pub fn game_by_id(id: &str) -> Option<&'static GameLayout> {
    GAMES.iter().find(|g| g.id == id)
}
