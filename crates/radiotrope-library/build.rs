use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let path = Path::new("assets/country-names.tsv");
    println!("cargo:rerun-if-changed={}", path.display());
    let names = std::fs::read_to_string(path).unwrap();
    let mut pairs: Vec<(&str, &str)> = names
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split_once('\t'))
        .collect();
    pairs.sort();

    let mut out = String::new();
    out.push_str("/// Lowercase country name to ISO 3166-1 alpha-2 code, sorted by name\n");
    out.push_str("static COUNTRY_NAMES: &[(&str, &str)] = &[\n");
    for (name, code) in pairs {
        writeln!(out, "    ({name:?}, {code:?}),").unwrap();
    }
    out.push_str("];\n");

    let out_path = Path::new(&std::env::var("OUT_DIR").unwrap()).join("country_names.rs");
    std::fs::write(out_path, out).unwrap();
}
