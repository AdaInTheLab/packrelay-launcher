// PackRelay launcher — CLI v0.
//
// Today: install + verify subcommands that exercise the cloud's v1
// distribution endpoints (manifest fetch → per-file download →
// SHA-256 verify → place at manifest paths). Same protocol as the
// stand-in scripts/install-pack.sh, just in Rust with parallel
// downloads and proper progress bars.
//
// Future: Tauri GUI layered on top of these same modules, signature
// verify (ed25519-dalek), update/uninstall, Steam install auto-
// detect. The CLI stays useful for CI + power users even when the
// GUI lands.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod client;
mod install;
mod manifest;
mod verify;

#[derive(Parser)]
#[command(
    name = "packrelay",
    version,
    about = "Install signed 7DTD mod packs from packrelay.cloud."
)]
struct Cli {
    /// PackRelay base URL. Override for local development against a
    /// different deploy.
    #[arg(
        long,
        global = true,
        default_value = "https://packrelay.cloud",
        env = "PACKRELAY_API_URL"
    )]
    api_url: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install a pack into a directory by slug.
    Install {
        /// Pack slug (e.g. pets-of-7-days).
        slug: String,
        /// Destination directory. For 7DTD, point this at <gamedir>/Mods/.
        #[arg(long, short = 'd')]
        dest: PathBuf,
        /// How many file downloads to run in parallel.
        #[arg(long, default_value = "8")]
        concurrency: usize,
    },
    /// Re-verify an already-installed pack against its sidecar manifest.
    Verify {
        /// Directory holding the previously-installed pack
        /// (expects _packrelay-manifest.json at its root).
        #[arg(long, short = 'd')]
        dest: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = client::Client::new(&cli.api_url);

    match cli.cmd {
        Cmd::Install {
            slug,
            dest,
            concurrency,
        } => install::run(&client, &slug, &dest, concurrency).await,
        Cmd::Verify { dest } => verify::run(&dest).await,
    }
}
