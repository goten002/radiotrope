use std::fmt::Write as _;
use std::path::Path;

fn main() {
    // `@platform/...` imports resolve to the UI for this build: the WiFi
    // settings only exist in the Raspberry Pi (`embedded`) build.
    let platform = if std::env::var_os("CARGO_FEATURE_EMBEDDED").is_some() {
        "ui/platform/embedded"
    } else {
        "ui/platform/desktop"
    };
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let config = slint_build::CompilerConfiguration::new()
        // Plain "fluent", not "fluent-dark": a "-dark" style fixes the
        // system colour scheme to dark when the UI is compiled, and the
        // System theme reads it (Palette.color-scheme) to follow the OS
        .with_style("fluent".into())
        .with_library_paths(
            [("platform".to_string(), manifest_dir.join(platform))]
                .into_iter()
                .collect(),
        );
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    generate_flags();
    emit_git_info();
    emit_update_mode();
    radiotrope_build::windows_resources(&radiotrope_build::WindowsExe {
        binary: "radiotrope",
        description: "Radiotrope internet radio player",
    });
}

/// Set `RADIOTROPE_UPDATES_BUILD` from `RADIOTROPE_UPDATES=off|notify|self`:
/// what this build may do about new versions at most (src/update). Packages
/// that a package manager updates set it to `off` or `notify`. Without it,
/// the desktop build may update itself and the embedded build does nothing.
fn emit_update_mode() {
    println!("cargo:rerun-if-env-changed=RADIOTROPE_UPDATES");
    let embedded_only = std::env::var_os("CARGO_FEATURE_EMBEDDED").is_some()
        && std::env::var_os("CARGO_FEATURE_DESKTOP").is_none();
    let default = if embedded_only { "off" } else { "self" };
    let mode = match std::env::var("RADIOTROPE_UPDATES") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_ascii_lowercase(),
        _ => default.to_string(),
    };
    if !["off", "notify", "self"].contains(&mode.as_str()) {
        panic!("RADIOTROPE_UPDATES must be off, notify or self, not {mode:?}");
    }
    println!("cargo:rustc-env=RADIOTROPE_UPDATES_BUILD={mode}");
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

/// Generate `$OUT_DIR/flags.rs` with the bundled country flags from `assets/flags/`.
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

    let out_path = Path::new(&std::env::var("OUT_DIR").unwrap()).join("flags.rs");
    std::fs::write(out_path, out).unwrap();
}
