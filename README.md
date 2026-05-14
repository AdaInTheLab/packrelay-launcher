# PackRelay Launcher

Desktop launcher for [packrelay.cloud](https://packrelay.cloud) — a signed-manifest modpack distribution service for [7 Days to Die](https://7daystodie.com).

Browse community-published modpacks, install them into the game's `Mods/` folder with one click, and join servers running them with the connect address pre-copied to your clipboard.

---

## What it does

- **Browse** a public catalog of modpacks. Filter by tag, sort by popularity or recency, search by name.
- **Install** any pack into your 7DTD `Mods/` folder. Each file is SHA-256-verified against a signed manifest as it streams to disk — corrupt or tampered files abort the install.
- **Verify** an installed pack at any time. Mismatches surface inline; one-click **Repair** refetches only the broken files.
- **Smart update** when a new version ships. Diffs the installed manifest against the new one and refetches only changed files; unchanged files stay put. Removed files get deleted; the sidecar manifest gets refreshed.
- **Uninstall** a pack cleanly. Reads the sidecar manifest to know exactly which files to delete — never touches a file the launcher didn't install, so a shared `Mods/` folder containing other packs is safe.
- **Browse servers** running PackRelay packs. Filter by region, online status, "not full". One-click **Launch 7DTD** opens the game via Steam with the connect address on your clipboard.

## How it works

The launcher fetches a signed JSON manifest from packrelay.cloud listing every file in a pack along with its SHA-256 hash and size. It then downloads each file by hash from the cloud's content-addressed file endpoint, streaming bytes through both the hasher and the on-disk write in a single pass. If any file's actual hash doesn't match the manifest, the install aborts before writing further bytes.

The manifest itself is Ed25519-signed by the publisher; verifying that signature against the publisher's registered key happens in [`packrelay-core`](crates/packrelay-core).

No telemetry. No analytics. No background processes when the launcher window is closed.

## Architecture

- **Frontend**: React 19 + TypeScript + Vite + Tailwind 4
- **Backend**: Tauri 2 (Rust) — handles the install pipeline, filesystem ops, and the local token storage
- **Shared logic**: a `packrelay-core` crate used by both the Tauri app and a [headless CLI](crates/packrelay-cli) so the install/verify/update/uninstall paths are written once and tested once

## Download

Pre-built binaries are on the [Releases page](https://github.com/AdaInTheLab/packrelay-launcher/releases/latest). Windows and Linux builds are produced from this repo on every tagged release. macOS isn't part of the current target audience — source builds still work but we don't ship binaries.

> **Heads up — Windows SmartScreen warning.** The binary isn't yet code-signed (we're working on an OV/EV certificate). On first run you'll see Windows SmartScreen flag it as an "unrecognized app" — click **More info** → **Run anyway**. The launcher is open source and built by GitHub Actions directly from this repo; SHA-256 sums are published alongside each release.

## Build from source

Prereqs: Rust stable (≥1.77), Node 20+, npm. On Linux you'll additionally need the Tauri system dependencies — see [Tauri's prerequisites](https://tauri.app/start/prerequisites/).

```bash
git clone https://github.com/AdaInTheLab/packrelay-launcher.git
cd packrelay-launcher/app
npm install
npm run tauri dev   # dev mode: hot-reloaded frontend + recompiled backend
```

To build a release binary:

```bash
npm run tauri build
```

The Windows installer lands in `app/src-tauri/target/release/bundle/`.

To run the headless CLI instead (e.g. for CI install tests):

```bash
cargo run -p packrelay-cli -- install <slug> --dest /path/to/Mods
cargo run -p packrelay-cli -- verify --dest /path/to/Mods
```

## Repository layout

```
.
├── app/                          Tauri app
│   ├── src/                      React frontend
│   ├── src-tauri/                Rust backend (lib.rs = commands, auth.rs = token storage)
│   └── package.json
├── crates/
│   ├── packrelay-core/           Shared install/verify/update/uninstall logic
│   └── packrelay-cli/            Headless CLI wrapping packrelay-core
├── Cargo.toml                    Workspace root
└── README.md
```

## Status

Pre-1.0. The install / verify / repair / update / uninstall loop is end-to-end working and used to install real packs in production. Auth, server browse, and one-click join are also live. Active development; expect rough edges in the UI chrome.
