use anyhow::Result;
use clap::Parser;

use github_fs::cli::Args;

fn main() -> Result<()> {
    let args = Args::parse();
    github_fs::run(args)
}
