use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    generate_flags();
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
