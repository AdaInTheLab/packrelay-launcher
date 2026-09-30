// PackRelay launcher CLI.
//
// Thin wrapper over packrelay-core's install + verify modules.
// Drives the install loop's ProgressEvents onto an indicatif bar
// for the terminal; the Tauri GUI app does the same job with
// frontend events emitted into the React UI.

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use packrelay_core::client::Client;
use packrelay_core::install::{install, InstallContext, ProgressEvent};
use packrelay_core::key_pins::{KeyChanged, KeyPinStore, TrustedKey};
use packrelay_core::verify;

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
        /// Pin each pack's signing key in this file on first install,
        /// and refuse later versions signed by a different key until
        /// you trust it with --trust-key. Off when unset.
        #[arg(long, env = "PACKRELAY_KEY_PINS")]
        key_pins: Option<PathBuf>,
        /// Trust a new signing key for this pack, as
        /// `<key-id>=<base64-key>`, exactly as a refused install prints it.
        #[arg(long, value_parser = TrustedKey::parse_cli, requires = "key_pins")]
        trust_key: Option<TrustedKey>,
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
    let client = Client::new(&cli.api_url);

    match cli.cmd {
        Cmd::Install {
            slug,
            dest,
            concurrency,
            key_pins,
            trust_key,
        } => {
            // No profiles or blob cache for headless usage; key pinning
            // only when asked for.
            let ctx = InstallContext {
                key_pins: key_pins.map(KeyPinStore::new),
                trust_key,
                ..Default::default()
            };
            run_install(&client, &slug, &dest, concurrency, ctx).await
        }
        Cmd::Verify { dest } => verify::run(&dest).await,
    }
}

/// Wire packrelay-core's progress events onto an indicatif bar. The
/// bar is created lazily on the Started event so we can pull the
/// real total_bytes out of the manifest before drawing anything.
async fn run_install(
    client: &Client,
    slug: &str,
    dest: &Path,
    concurrency: usize,
    ctx: InstallContext,
) -> Result<()> {
    println!("[install] fetching manifest for '{slug}'...");

    let bar: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));
    let bar_for_cb = bar.clone();

    // CLI defaults to latest — no --version flag yet. When/if the CLI
    // grows a pin/version arg, thread Some(&v) through here.
    let report = install(
        client,
        slug,
        dest,
        concurrency,
        None,
        ctx,
        move |ev: ProgressEvent| {
            match ev {
                ProgressEvent::Started {
                    display_name,
                    version,
                    file_count,
                    total_bytes,
                } => {
                    println!(
                        "[install] manifest: {} v{} ({} files, {:.1} MB)",
                        display_name,
                        version,
                        file_count,
                        total_bytes as f64 / (1024.0 * 1024.0),
                    );
                    let pb = ProgressBar::new(total_bytes);
                    pb.set_style(
                        ProgressStyle::with_template(
                            "{spinner:.cyan} [{elapsed_precise}] [{wide_bar:.cyan/blue}] \
                         {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12} eta {eta:>4}",
                        )
                        .unwrap()
                        .progress_chars("##-"),
                    );
                    *bar_for_cb.lock().unwrap() = Some(pb);
                }
                ProgressEvent::Bytes { delta } => {
                    if let Some(pb) = bar_for_cb.lock().unwrap().as_ref() {
                        pb.inc(delta);
                    }
                }
                ProgressEvent::FileDone { .. } => {
                    // No per-file CLI output — the byte counter is enough
                    // signal. GUI uses these for the file list view.
                }
                ProgressEvent::Done { .. } => {
                    if let Some(pb) = bar_for_cb.lock().unwrap().take() {
                        pb.finish_and_clear();
                    }
                }
            }
        },
    )
    .await
    .map_err(|e| match e.downcast_ref::<KeyChanged>() {
        Some(change) => anyhow!(
            "{change}\n\nTo trust the new key, re-run with:\n  --trust-key {}",
            change.offered
        ),
        None => e,
    })?;

    println!(
        "[install] verified {} files, {:.1} MB. Installed into {}",
        report.file_count,
        report.total_bytes as f64 / (1024.0 * 1024.0),
        report.dest
    );
    Ok(())
}
