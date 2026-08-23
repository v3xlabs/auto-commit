use std::{fs, path::PathBuf};

use clap::CommandFactory;
use clap_complete::Shell;

include!("src/cli.rs");

/// Completions and the man page are generated here rather than checked in, so
/// they cannot drift from the flags. The nix derivation installs them from
/// target/dist.
fn main() -> std::io::Result<()> {
    println!("cargo::rerun-if-changed=src/cli.rs");

    let out = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"))
        .join("target/dist");

    fs::create_dir_all(&out)?;

    let mut command = Cli::command();

    for shell in [Shell::Bash, Shell::Fish, Shell::Zsh] {
        clap_complete::generate_to(shell, &mut command, "auto-commit", &out)?;
    }

    let man = clap_mangen::Man::new(command);
    let mut page = Vec::new();
    man.render(&mut page)?;

    fs::write(out.join("auto-commit.1"), page)
}
