use clap::Parser;

/// Multi-agent orchestrator for Claude Code and herdr.
#[derive(Parser)]
#[command(version, about, arg_required_else_help = true)]
struct Cli {
    // Subcommands (`hook`, `report`, `decision`, `task`, `spawn`, ...) go here as a
    // `#[command(subcommand)]` field.
}

fn main() {
    let _cli = Cli::parse();
}
