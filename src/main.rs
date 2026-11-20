use anyhow::{Context, Result};
use clap::Parser;
use note_taker::{
    cli::{Cli, Command},
    config::Config,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn main() {
    if let Err(error) = entry() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn entry() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => (),
        Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e).context("Cannot read .env configuration"),
    }
    let mut arguments: Vec<_> = std::env::args_os().collect();
    if arguments.len() == 1 {
        arguments.push("--help".into());
    }
    let cli = Cli::parse_from(arguments);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        match cli.command {
            Command::Devices(args) => {
                let sources = tokio::task::spawn_blocking(move || {
                    note_taker::audio::devices(args.audio_source)
                })
                .await??;
                for source in sources {
                    println!(
                        "{}\t{}{}",
                        source.id,
                        source.name,
                        if source.is_default { " [default]" } else { "" }
                    );
                }
                Ok(())
            }
            Command::Listen(args) => {
                let config = Config::from_args(args)?;
                let stop = CancellationToken::new();
                let signal_stop = stop.clone();
                #[cfg(target_os = "macos")]
                let mut interrupt =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                        .context("Cannot install Ctrl+C handler")?;
                #[cfg(target_os = "windows")]
                let mut interrupt = tokio::signal::windows::ctrl_c()
                    .context("Cannot install Windows Ctrl+C handler")?;
                let signal = tokio::spawn(async move {
                    interrupt.recv().await;
                    tracing::info!(
                        "Stopping… waiting for final transcripts and flushing output. Please wait."
                    );
                    signal_stop.cancel();
                });
                let result = note_taker::app::listen(config, stop).await;
                signal.abort();
                result
            }
        }
    });
    // Bound process exit even if an OS filesystem operation cannot be interrupted.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}
