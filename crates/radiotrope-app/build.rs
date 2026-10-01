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
        .with_style("fluent-dark".into())
        .with_library_paths(
            [("platform".to_string(), manifest_dir.join(platform))]
                .into_iter()
                .collect(),
        );
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    generate_flags();
    emit_git_info();
    embed_windows_resources();
}

/// Give the Windows exe its icon and version details (name, version,
/// description, copyright), as shown by Explorer and Task Manager. Uses the
/// Windows SDK's `rc.exe` for MSVC builds, `windres` for GNU builds.
fn embed_windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icons/radiotrope.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon.to_string_lossy())
        .set("ProductName", "Radiotrope")
        .set("FileDescription", "Radiotrope internet radio player")
        .set("LegalCopyright", "George Alexiou, GPL-3.0-or-later")
        .set("OriginalFilename", "radiotrope.exe");
    if let Err(e) = res.compile() {
        // A missing resource compiler costs the icon, not the build
        println!("cargo:warning=no Windows icon or version details: {e}");
    }
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
