mod analyze;
mod clean;
mod maintain;
mod status;
mod theme;
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
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Status(a) => status::run(a),
        Command::Clean(a) => clean::run(a, cli.dry_run),
        Command::Analyze(a) => analyze::run_cmd(a, cli.dry_run),
        Command::Uninstall(a) => uninstall::run_cmd(a, cli.dry_run),
        Command::Maintain(a) => maintain::run_cmd(a, cli.dry_run),
    };
    if let Err(e) = result {
        eprintln!("sw: {e:#}");
        std::process::exit(1);
    }
}
