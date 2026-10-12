mod analyze;
mod clean;
mod maintain;
mod status;
mod uninstall;
mod util;

use clap::{Parser, Subcommand};

/// Live system monitoring and deep cleaning for macOS.
#[derive(Parser)]
#[command(name = "sweepr", version, about)]
struct Cli {
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
        Command::Clean(a) => clean::run(a),
        Command::Analyze(a) => analyze::run_cmd(a),
        Command::Uninstall(a) => uninstall::run_cmd(a),
        Command::Maintain(a) => maintain::run_cmd(a),
    };
    if let Err(e) = result {
        eprintln!("sweepr: {e:#}");
        std::process::exit(1);
    }
}
