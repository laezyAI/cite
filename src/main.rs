mod cli;
mod core;
mod tui;

use clap::Parser;
use cli::Cli;
use colored::Colorize;
use std::io::Write;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

struct FormattedTimestamp;

impl FormatTime for FormattedTimestamp {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%H:%M:%S"))
    }
}

fn build_filter(verbose: bool, quiet: bool) -> EnvFilter {
    if verbose {
        EnvFilter::new("cite=trace")
    } else if quiet {
        EnvFilter::new("cite=error")
    } else {
        EnvFilter::new("cite=info")
    }
}

#[tokio::main]
async fn main() {
    let mut cli = Cli::parse();
    let filter = build_filter(cli.verbose, cli.quiet);

    let Some(cmd) = cli.command.take() else {
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_level(true)
            .with_timer(FormattedTimestamp)
            .with_ansi(false)
            .with_writer(move || LogWriter {
                tx: log_tx.clone(),
                buf: String::new(),
            })
            .init();
        info!("cite v{}", env!("CARGO_PKG_VERSION"));
        let root = PathBuf::from(&cli.path);
        if let Err(e) = tui::run_tui(log_rx, root).await {
            eprintln!("{} {}", "error:".red().bold(), e);
            std::process::exit(1);
        }
        return;
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true)
        .with_timer(FormattedTimestamp)
        .with_writer(std::io::stderr)
        .init();
    info!("cite v{}", env!("CARGO_PKG_VERSION"));
    if let Err(e) = cmd.execute(&cli).await {
        eprintln!("{} {}", "error:".red().bold(), e);
        std::process::exit(1);
    }
}

struct LogWriter {
    tx: mpsc::UnboundedSender<String>,
    buf: String,
}

impl Write for LogWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.push_str(&String::from_utf8_lossy(data));
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..pos].trim_end().to_string();
            self.buf.drain(..=pos);
            let _ = self.tx.send(line);
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.trim_end().is_empty() {
            let line = std::mem::take(&mut self.buf).trim_end().to_string();
            let _ = self.tx.send(line);
        }
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
