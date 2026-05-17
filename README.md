<p align="center">
  <a href="https://packrelay.cloud">
    <img src="https://img.shields.io/badge/packrelay-cloud-7c5cff?labelColor=171717" alt="packrelay.cloud" />
  </a>
</p>

<h1 align="center">PackRelay Launcher</h1>

<p align="center">
  <strong>One-click installer + auto-updater for signed 7 Days to Die mod packs.</strong><br/>
  Browse the catalog at <a href="https://packrelay.cloud">packrelay.cloud</a>, install, join — every file SHA-256 verified against an Ed25519-signed manifest.
</p>

<p align="center">
  <a href="https://github.com/AdaInTheLab/packrelay-launcher/actions/workflows/ci.yml">
    <img src="https://img.shields.io/github/actions/workflow/status/AdaInTheLab/packrelay-launcher/ci.yml?branch=main&label=CI&labelColor=171717" alt="CI" />
  </a>
  <a href="https://github.com/AdaInTheLab/packrelay-launcher/releases/latest">
    <img src="https://img.shields.io/github/v/release/AdaInTheLab/packrelay-launcher?include_prereleases&label=release&labelColor=171717&color=7c5cff" alt="Latest release" />
  </a>
  <a href="https://codecov.io/gh/AdaInTheLab/packrelay-launcher">
    <img src="https://img.shields.io/codecov/c/github/AdaInTheLab/packrelay-launcher?label=coverage&labelColor=171717&color=06b6d4" alt="Coverage" />
  </a>
  <a href="https://github.com/AdaInTheLab/packrelay-launcher/releases">
    <img src="https://img.shields.io/github/downloads/AdaInTheLab/packrelay-launcher/total?label=downloads&labelColor=171717&color=06b6d4" alt="Total downloads" />
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/license-source--available-orange?labelColor=171717" alt="License" />
  </a>
  <br/>
  <img src="https://img.shields.io/badge/Windows-10%2F11-0078d4?labelColor=171717&logo=windows&logoColor=white" alt="Windows" />
  <img src="https://img.shields.io/badge/macOS-12%2B-000000?labelColor=171717&logo=apple&logoColor=white" alt="macOS" />
  <img src="https://img.shields.io/badge/Linux-x86__64-fcc624?labelColor=171717&logo=linux&logoColor=black" alt="Linux" />
  <br/>
  <img src="https://img.shields.io/badge/Tauri-2-24c8db?labelColor=171717&logo=tauri&logoColor=white" alt="Tauri 2" />
  <img src="https://img.shields.io/badge/React-19-61dafb?labelColor=171717&logo=react&logoColor=black" alt="React 19" />
  <img src="https://img.shields.io/badge/Rust-stable-orange?labelColor=171717&logo=rust&logoColor=white" alt="Rust" />
  <img src="https://img.shields.io/badge/7D2D-V2.0-orange?labelColor=171717" alt="7 Days to Die V2" />
</p>

<p align="center">
  <a href="https://packrelay.cloud/download"><strong>Download →</strong></a>
  &nbsp;·&nbsp;
  <a href="https://github.com/AdaInTheLab/packrelay-launcher/releases/latest"><strong>Latest release</strong></a>
  &nbsp;·&nbsp;
  <a href="#build-from-source"><strong>Build from source</strong></a>
</p>

---

## Why it exists

The 7 Days to Die mod ecosystem is great. The *install* experience for new players is not — find a modpack, download a zip, hope it's the right one, hope the files aren't tampered with, drag-drop into the right folder, hope you remembered to remove the last server's mods first, get kicked on connect because the server is on a slightly newer version anyway.

PackRelay replaces all of that with: click, install, join. Every file is content-addressed and SHA-256 verified. Pack publishers sign manifests with an Ed25519 key the launcher verifies before laying down a single byte.

## What it does

- **Browse** a public catalog of modpacks. Filter by tag, sort by popularity or recency, search by name.
- **Install** a pack into your 7DTD `Mods/` folder. Each file streams through both the SHA-256 hasher and the on-disk write in one pass — corrupt or tampered files abort the install before any further bytes are written.
- **Verify** an installed pack at any time. Mismatches surface inline; one-click **Repair** refetches only the broken files.
- **Smart update** when a new version ships. Diffs the installed manifest against the new one and refetches only changed files; unchanged files stay put. Removed files get deleted; the sidecar manifest rolls forward.
- **Uninstall** cleanly. Reads the sidecar to know exactly which files to delete — never touches a file the launcher didn't install, so a shared `Mods/` folder containing other packs stays safe.
- **Browse servers** running PackRelay packs. Filter by region, online status, "not full". One-click **Join** opens 7DTD via Steam with the connect address pre-copied to your clipboard. Kicked-from-server links open the launcher directly to the right pack-version + connect flow via `packrelay://join/<slug>`.

## How it works

The launcher fetches a signed JSON manifest from packrelay.cloud listing every file in a pack along with its SHA-256 hash and size. It then downloads each file by hash from the cloud's content-addressed file endpoint, streaming bytes through both the hasher and the on-disk write in a single pass. If any file's actual hash doesn't match the manifest, the install aborts before writing further bytes.

The manifest itself is Ed25519-signed by the publisher. The signature is verified against the publisher's registered public key on every install — the launcher refuses to lay down any byte from an unsigned, wrong-key, or tampered manifest. See [`packrelay-core`](crates/packrelay-core) for the verify path.

**No telemetry. No analytics. No background processes when the launcher window is closed.**

## Architecture

| Layer | Stack | What it does |
|---|---|---|
| **Frontend** | React 19 + TypeScript + Vite + Tailwind 4 | UI, browse/install/verify/repair/update flows |
| **Backend** | Tauri 2 (Rust) | Filesystem ops, token storage, deep-link routing, native install pipeline |
| **Shared** | `packrelay-core` (Rust crate) | Manifest fetch + Ed25519 verify + install loop. Reused by the [headless CLI](crates/packrelay-cli) for CI install tests. |

## Download

Pre-built binaries for **Windows / macOS / Linux** are on the [Releases page](https://github.com/AdaInTheLab/packrelay-launcher/releases/latest) — produced by GitHub Actions on every tagged release directly from this repo's source.

Once installed, the launcher checks for new versions on startup and offers a non-blocking toast to install. Updates are downloaded from GitHub Releases and verified against an Ed25519 signing key baked into the launcher binary — see [`docs/UPDATER.md`](docs/UPDATER.md) for the keypair generation + CI secrets walkthrough.

macOS builds are codesigned + notarized with the project's Developer ID Application certificate ([`docs/NOTARIZATION.md`](docs/NOTARIZATION.md)) — the `.dmg` opens cleanly on first run, no right-click → Open dance.

> **Windows SmartScreen heads-up.** The Windows binary isn't yet code-signed (working on an OV/EV cert). On first run Windows SmartScreen flags it as an "unrecognized app" — click **More info** → **Run anyway**. Every binary on the Releases page is built from this exact source by GitHub Actions; you can verify the SHA-256 sums published alongside each release.

## Build from source

Prereqs: Rust stable (≥ 1.77), Node 20+, npm. On Linux you'll additionally need the Tauri system dependencies — see [Tauri's prerequisites](https://tauri.app/start/prerequisites/).

```bash
git clone https://github.com/AdaInTheLab/packrelay-launcher.git
cd packrelay-launcher/app
npm install
npm run tauri dev    # hot-reloaded frontend + recompiled backend
```

To build a release binary:

```bash
npm run tauri build
```

The Windows installer lands in `app/src-tauri/target/release/bundle/`.

### Headless CLI

The same install/verify/update/uninstall logic the launcher uses is exposed as a CLI for CI tests + scripting:

```bash
cargo run -p packrelay-cli -- install <slug> --dest /path/to/Mods
cargo run -p packrelay-cli -- verify   --dest /path/to/Mods
cargo run -p packrelay-cli -- update   <slug> --dest /path/to/Mods
```

## Repository layout

```
.
├── app/                            Tauri app
│   ├── src/                        React frontend (App.tsx is most of it)
│   ├── src-tauri/                  Rust backend (lib.rs = commands, auth.rs = token storage)
│   └── package.json
├── crates/
│   ├── packrelay-core/             Shared install/verify/update/uninstall logic
│   └── packrelay-cli/              Headless CLI wrapping packrelay-core
├── docs/
│   ├── UPDATER.md                  Auto-update keypair + CI secrets walkthrough
│   └── NOTARIZATION.md             macOS codesigning + notarization setup
├── .github/workflows/
│   ├── ci.yml                      cargo check + cargo test + tsc + vite build (PR feedback)
│   └── release.yml                 cross-platform binary build matrix on tag push
├── Cargo.toml                      Workspace root
└── README.md
```

## Status

**Pre-1.0.** The install / verify / repair / update / uninstall loop is end-to-end working and used to install real packs in production. Auth, server browse, deep-link join (`packrelay://join/<slug>`), and one-click connect are all live. Active development; expect rough edges in the UI chrome.

## License

Source-available, **not** open source. See [`LICENSE`](LICENSE) — short version: you can read it and build it locally to verify the binaries aren't malware, but the source isn't free for redistribution, forking, or use in another project. Pre-built binaries from the official Releases page are the sanctioned way to actually run the launcher.

---

<p align="center">
  <sub>Built by <a href="https://github.com/AdaInTheLab">AdaInTheLab</a> for the 7DTD modding community.</sub>
</p>
