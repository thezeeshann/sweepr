mod analyze;
mod clean;
mod maintain;
mod status;
mod theme;
mod theme_cmd;
mod uninstall;
mod util;

use clap::{Parser, Subcommand};

/// Live system monitoring and deep cleaning for macOS.
#[derive(Parser)]
#[command(name = "sw", version, about)]
struct Cli {
    /// Preview only: read and report, never delete, move, write or change anything
    #[arg(short = 'n', long, global = true)]
    dry_run: bool,
    /// Color theme for this run (see `sw theme list`)
    #[arg(long, global = true, value_name = "NAME")]
    theme: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Live monitoring: CPU, memory, disk I/O, network and processes
    Status(status::Args),
    /// Deep clean: caches, logs, leftovers and orphaned app data
    Clean(clean::Args),
    /// Disk analyzer: see where space went, find large files, trash what you do not need
    Analyze(analyze::Args),
    /// Smart uninstaller: remove an app with its preferences, caches and LaunchAgents
    Uninstall(uninstall::Args),
    /// System maintenance: flush DNS, refresh QuickLook/icons, compact databases
    Maintain(maintain::Args),
    /// Terminal theme: check the active one, preview and save another
    Theme(theme_cmd::Args),
}

fn main() {
    let cli = Cli::parse();
    let result = theme::init(cli.theme.as_deref()).and_then(|choice| match cli.command {
        Command::Status(a) => status::run(a),
        Command::Clean(a) => clean::run(a, cli.dry_run),
        Command::Analyze(a) => analyze::run_cmd(a, cli.dry_run),
        Command::Uninstall(a) => uninstall::run_cmd(a, cli.dry_run),
        Command::Maintain(a) => maintain::run_cmd(a, cli.dry_run),
        Command::Theme(a) => theme_cmd::run_cmd(a, cli.dry_run, choice),
    });
    if let Err(e) = result {
        eprintln!("sw: {e:#}");
        std::process::exit(1);
    }
}
