//! Entry point for the `k` AI chat CLI. In the release bundle
//! `Contents/MacOS/k` is a symlink to `kaku-gui`, which calls [`k_main`] when
//! it is started under that name; the `k` bin target calls it for dev builds.

use super::{run, CliArgs};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "k",
    about = "AI chat from any terminal",
    long_about = "Slash commands (interactive mode): /new  /resume [id]  /clear  /status  /memory  /exit\n\nThe CLI intentionally supports a smaller command set than the Cmd+L overlay."
)]
struct Cli {
    /// One-shot query (omit for interactive mode)
    prompt: Vec<String>,
    /// Force a new conversation
    #[arg(long, short)]
    new: bool,
    /// List recent conversations or resume by ID
    #[arg(long, short = 'r', value_name = "ID", num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,
}

pub fn k_main() {
    let cli = Cli::parse();
    let args = CliArgs {
        prompt: if cli.prompt.is_empty() {
            None
        } else {
            Some(cli.prompt.join(" "))
        },
        new: cli.new,
        resume: cli
            .resume
            .map(|id| if id.is_empty() { None } else { Some(id) }),
    };
    if let Err(e) = run(args) {
        eprintln!("k: {}", e);
        std::process::exit(1);
    }
}
