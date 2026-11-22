fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    radiotrope_build::windows_resources(&radiotrope_build::WindowsExe {
        binary: "radiotrope-probe",
        description: "Radiotrope station checker",
    });
}
