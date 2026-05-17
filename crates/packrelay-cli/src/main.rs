// PackRelay launcher CLI.
//
// Thin wrapper over packrelay-core's install + verify modules.
// Drives the install loop's ProgressEvents onto an indicatif bar
// for the terminal; the Tauri GUI app does the same job with
// frontend events emitted into the React UI.

use anyhow::Result;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use packrelay_core::client::Client;
use packrelay_core::install::{install, ProgressEvent};
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
        } => run_install(&client, &slug, &dest, concurrency).await,
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
) -> Result<()> {
    println!("[install] fetching manifest for '{slug}'...");

    let bar: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));
    let bar_for_cb = bar.clone();

    // CLI doesn't manage profiles or a blob cache — pass a default
    // (empty) context so install behaves exactly as it did before
    // the Phase 4 changes for headless usage.
    let ctx = packrelay_core::install::InstallContext::default();
    // CLI defaults to latest — no --version flag yet. When/if the CLI
    // grows a pin/version arg, thread Some(&v) through here.
    let report = install(client, slug, dest, concurrency, None, ctx, move |ev: ProgressEvent| {
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
    })
    .await?;

    println!(
        "[install] verified {} files, {:.1} MB. Installed into {}",
        report.file_count,
        report.total_bytes as f64 / (1024.0 * 1024.0),
        report.dest
    );
    Ok(())
}
