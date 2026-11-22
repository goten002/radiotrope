//! The About dialog: what it shows, and its links

use slint::ComponentHandle;

use crate::{AboutInfo, App};

/// Fill in the About dialog and handle its links
pub(crate) fn setup_about(ui: &App) {
    let info = ui.global::<AboutInfo>();
    info.set_version(env!("CARGO_PKG_VERSION").into());
    info.set_commit(env!("RADIOTROPE_GIT_HASH").into());
    info.set_commit_date(env!("RADIOTROPE_GIT_DATE").into());
    info.set_platform(platform_name().into());
    let path_text = |p: radiotrope_app::error::Result<std::path::PathBuf>| {
        p.map(|p| p.display().to_string()).unwrap_or_default()
    };
    info.set_config_dir(path_text(radiotrope_app::data::storage::config_dir()).into());
    info.set_cache_dir(path_text(radiotrope_app::data::cache::cache_dir()).into());
    info.set_repository(env!("CARGO_PKG_REPOSITORY").into());
    info.set_license(license_label(env!("CARGO_PKG_LICENSE")).into());
    info.set_authors(author_names(env!("CARGO_PKG_AUTHORS")).into());

    ui.on_open_url(|url| open_url(&url));
}

/// Human-readable OS and CPU, e.g. "Linux aarch64"
pub(crate) fn platform_name() -> String {
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        other => other,
    };
    format!("{os} {}", std::env::consts::ARCH)
}

/// Short display name for an SPDX license id, e.g. "GPLv3+" for
/// GPL-3.0-or-later; other ids are shown as they are
pub(crate) fn license_label(spdx: &str) -> &str {
    match spdx {
        "GPL-3.0-or-later" => "GPLv3+",
        "GPL-3.0-only" => "GPLv3",
        other => other,
    }
}

/// Cargo's `authors` list without the email addresses
pub(crate) fn author_names(authors: &str) -> String {
    authors
        .split(':')
        .map(|a| a.split('<').next().unwrap_or(a).trim())
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Open a web link in the default browser. Does nothing if there is no
/// browser (e.g. the embedded kiosk build).
pub(crate) fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    match std::process::Command::new(program).arg(url).spawn() {
        // Reap the launcher so it doesn't linger as a zombie
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("Failed to open {url}: {e}"),
    }
}
