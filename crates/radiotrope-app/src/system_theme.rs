//! The Linux desktop's "no preference" light/dark answer (desktop builds)
//!
//! Slint follows the freedesktop Settings portal's `color-scheme` (1 dark,
//! 2 light) by itself, but reads 0, "no preference", the same as no portal
//! at all. GNOME answers 0 whenever Dark Style is off: GNOME Settings only
//! offers "default" and "prefer-dark", and GNOME apps are light then. So the
//! app asks the portal itself, and treats 0 as the desktop's light theme.

/// Whether the desktop's settings portal answers "no preference" for its
/// colour scheme. False when there is no portal or it says dark or light,
/// and always false off Linux (Windows always reports dark or light).
pub fn no_preference() -> bool {
    imp::color_scheme() == Some(0)
}

#[cfg(target_os = "linux")]
mod imp {
    use radiotrope_app::config::ui::PORTAL_ANSWER;
    use zbus::blocking::{connection, Proxy};
    use zbus::zvariant::OwnedValue;

    /// The portal's `org.freedesktop.appearance color-scheme`, as Slint
    /// reads it: `ReadOne`, or `Read` on portals older than 1.15 (Ubuntu
    /// 22.04), whose extra variant `downcast_ref` unwraps.
    pub fn color_scheme() -> Option<u32> {
        let conn = connection::Builder::session()
            .ok()?
            .method_timeout(PORTAL_ANSWER)
            .build()
            .ok()?;
        let portal = Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
        )
        .ok()?;
        let args = ("org.freedesktop.appearance", "color-scheme");
        let value: OwnedValue = portal
            .call("ReadOne", &args)
            .or_else(|_| portal.call("Read", &args))
            .ok()?;
        value.downcast_ref::<u32>().ok()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn color_scheme() -> Option<u32> {
        None
    }
}
