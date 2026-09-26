use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    generate_flags();
    emit_git_info();
}

/// Set `RADIOTROPE_GIT_HASH` and `RADIOTROPE_GIT_DATE` for the About dialog.
/// Both are empty when building outside a git checkout (e.g. Yocto tarballs).
fn emit_git_info() {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let hash = git(&["rev-parse", "--short=9", "HEAD"]);
    let date = git(&["log", "-1", "--format=%cs"]);
    println!("cargo:rustc-env=RADIOTROPE_GIT_HASH={hash}");
    println!("cargo:rustc-env=RADIOTROPE_GIT_DATE={date}");

    // Rebuild when HEAD moves (checkout, commit). Only watch files that
    // exist: cargo reruns the script every build for a missing path.
    let git_dir = git(&["rev-parse", "--absolute-git-dir"]);
    if !git_dir.is_empty() {
        let git_dir = Path::new(&git_dir);
        let head_ref = git(&["symbolic-ref", "-q", "HEAD"]);
        for path in [
            git_dir.join("HEAD"),
            git_dir.join(&head_ref),
            git_dir.join("packed-refs"),
        ] {
            if path.is_file() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

/// Generate `$OUT_DIR/flags.rs` with the bundled country flags and the
/// country name table from `assets/flags/`.
fn generate_flags() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/flags");
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut codes: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension()? != "png" {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_string())
        })
        .collect();
    codes.sort();

    let mut out =
        String::from("/// Flag PNGs keyed by lowercase ISO 3166-1 alpha-2 code, sorted by code\n");
    out.push_str("static FLAGS: &[(&str, &[u8])] = &[\n");
    for code in &codes {
        let path = dir.join(format!("{code}.png"));
        writeln!(
            out,
            "    ({code:?}, include_bytes!({:?})),",
            path.display().to_string()
        )
        .unwrap();
    }
    out.push_str("];\n\n");

    let names = std::fs::read_to_string(dir.join("names.tsv")).unwrap();
    let mut pairs: Vec<(&str, &str)> = names
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split_once('\t'))
        .collect();
    pairs.sort();

    out.push_str("/// Lowercase country name to ISO 3166-1 alpha-2 code, sorted by name\n");
    out.push_str("static COUNTRY_NAMES: &[(&str, &str)] = &[\n");
    for (name, code) in pairs {
        writeln!(out, "    ({name:?}, {code:?}),").unwrap();
    }
    out.push_str("];\n");

    let out_path = Path::new(&std::env::var("OUT_DIR").unwrap()).join("flags.rs");
    std::fs::write(out_path, out).unwrap();
}
