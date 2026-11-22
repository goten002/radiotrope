//! Build-script helpers shared by the three Radiotrope programs
//! (`radiotrope`, `radiotrope-cli`, `radiotrope-probe`). Used from their
//! `build.rs` only; nothing here ends up in a program.

use std::path::Path;

/// What Explorer shows for one program under Properties > Details
pub struct WindowsExe<'a> {
    /// The binary's name without `.exe`, as in `[[bin]] name`
    pub binary: &'a str,
    /// The "File description" line, also what Task Manager lists
    pub description: &'a str,
}

/// The publisher name, as WinGet and the installer show it
pub const COMPANY: &str = "Radiotrope";
/// The product all three programs belong to
pub const PRODUCT: &str = "Radiotrope";
/// The copyright line in the version details
pub const COPYRIGHT: &str = "\u{a9} 2026 George Alexiou. GPL-3.0-or-later";

/// Give a Windows exe the Radiotrope icon and its version details (product,
/// company, description, copyright, version from the crate's `Cargo.toml`).
/// Does nothing unless the target is Windows. Uses the Windows SDK's `rc.exe`
/// for MSVC builds, `windres` for GNU builds; a missing resource compiler
/// costs the details, not the build.
pub fn windows_resources(exe: &WindowsExe) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icons/radiotrope.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let file_name = format!("{}.exe", exe.binary);
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon.to_string_lossy())
        .set("ProductName", PRODUCT)
        .set("CompanyName", COMPANY)
        .set("FileDescription", exe.description)
        .set("LegalCopyright", COPYRIGHT)
        .set("InternalName", exe.binary)
        .set("OriginalFilename", &file_name);
    if let Err(e) = res.compile_for(exe.binary) {
        println!("cargo:warning=no Windows icon or version details for {file_name}: {e}");
    }
}
