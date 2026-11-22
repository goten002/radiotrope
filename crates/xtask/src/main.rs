//! Project tasks, run with `cargo xtask <task>`.
//!
//! `cargo xtask man-and-completions [OUT_DIR]` writes the man pages and the
//! bash, zsh and fish completions of the three programs, by default into
//! `target/generated/`:
//!
//! ```text
//! man/radiotrope.1  man/radiotrope-cli.1  man/radiotrope-probe.1
//! completions/bash/<program>  completions/zsh/_<program>  completions/fish/<program>.fish
//! ```
//!
//! The programs' command lines are the same source files the programs are
//! built from (`src/args.rs` in each crate, which use clap and std only), so
//! the pages can't drift from `--help`, and the programs carry no code for
//! this. The version comes from the workspace, which every crate shares.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::CommandFactory;
use clap_complete::Shell;

// The `ui-snapshots` feature is the app's, not ours: its hidden arguments
// compile out here, as they do in a release build.
#[allow(dead_code, unexpected_cfgs)]
#[path = "../../radiotrope-app/src/args.rs"]
mod app_args;
#[allow(dead_code)]
#[path = "../../radiotrope-cli/src/args.rs"]
mod cli_args;
#[allow(dead_code)]
#[path = "../../radiotrope-probe/src/args.rs"]
mod probe_args;

const USAGE: &str = "usage: cargo xtask man-and-completions [OUT_DIR]";

fn main() {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("man-and-completions") => {
            let out = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(default_out_dir);
            man_and_completions(&out).map(|()| println!("wrote {}", out.display()))
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}

/// `target/generated` next to the workspace's `Cargo.toml`
fn default_out_dir() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2);
    workspace
        .expect("crates/xtask sits two levels down")
        .join("target/generated")
}

/// The three programs' command lines, as clap describes them
fn commands() -> [clap::Command; 3] {
    [
        app_args::Args::command(),
        cli_args::Cli::command(),
        probe_args::Args::command(),
    ]
}

fn man_and_completions(out: &Path) -> io::Result<()> {
    let man_dir = out.join("man");
    fs::create_dir_all(&man_dir)?;
    for mut cmd in commands() {
        let name = cmd.get_name().to_string();
        cmd.build();
        let mut page = Vec::new();
        clap_mangen::Man::new(cmd.clone()).render(&mut page)?;
        fs::write(man_dir.join(format!("{name}.1")), page)?;

        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let dir = out.join("completions").join(shell.to_string());
            fs::create_dir_all(&dir)?;
            let file = match shell {
                Shell::Zsh => format!("_{name}"),
                Shell::Fish => format!("{name}.fish"),
                _ => name.clone(),
            };
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut cmd, &name, &mut script);
            fs::write(dir.join(file), script)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_are_valid() {
        for cmd in commands() {
            cmd.debug_assert();
        }
    }

    #[test]
    fn names_and_version_match_the_programs() {
        let names: Vec<_> = commands()
            .iter()
            .map(|c| c.get_name().to_string())
            .collect();
        assert_eq!(names, ["radiotrope", "radiotrope-cli", "radiotrope-probe"]);
        for cmd in commands() {
            assert_eq!(cmd.get_version(), Some(env!("CARGO_PKG_VERSION")));
        }
    }

    #[test]
    fn writes_every_file() {
        let out = std::env::temp_dir().join(format!("radiotrope-xtask-{}", std::process::id()));
        man_and_completions(&out).unwrap();
        for name in ["radiotrope", "radiotrope-cli", "radiotrope-probe"] {
            for file in [
                format!("man/{name}.1"),
                format!("completions/bash/{name}"),
                format!("completions/zsh/_{name}"),
                format!("completions/fish/{name}.fish"),
            ] {
                let path = out.join(&file);
                assert!(
                    fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false),
                    "{file}"
                );
            }
        }
        fs::remove_dir_all(&out).unwrap();
    }
}
