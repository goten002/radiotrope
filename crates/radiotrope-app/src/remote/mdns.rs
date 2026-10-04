//! Announcing the player to phones on the local network (mDNS/DNS-SD)
//!
//! The player shows up as `<name>._radiotrope._tcp.local.` with a TXT
//! record holding its id (`id`), the API version (`api`) and the app
//! version (`ver`). mdns-sd shares UDP port 5353 with Avahi on Linux and
//! with Windows' own mDNS responder.

use mdns_sd::{ServiceDaemon, ServiceInfo};

use radiotrope_app::config::remote::{API_VERSION, SERVICE_TYPE};

/// The announcement; dropping it withdraws it
pub struct Announcer {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Announcer {
    pub fn start(name: &str, player_id: &str, port: u16) -> Result<Self, String> {
        let daemon = ServiceDaemon::new().map_err(|e| format!("Can't announce the player: {e}"))?;
        let host = format!("{}.local.", host_label(&computer_name()));
        let api = API_VERSION.to_string();
        let properties = [
            ("id", player_id),
            ("api", api.as_str()),
            ("ver", env!("CARGO_PKG_VERSION")),
        ];
        let info = ServiceInfo::new(SERVICE_TYPE, name, &host, "", port, &properties[..])
            .map_err(|e| format!("Can't announce the player: {e}"))?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon
            .register(info)
            .map_err(|e| format!("Can't announce the player: {e}"))?;
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        // Say goodbye, so phones drop the player at once
        if let Ok(done) = self.daemon.unregister(&self.fullname) {
            let _ = done.recv_timeout(std::time::Duration::from_secs(1));
        }
        let _ = self.daemon.shutdown();
    }
}

/// This computer's name, e.g. "living-room-pc"
pub fn computer_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer outlives the call and its length is passed
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        if ok {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    #[cfg(windows)]
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    "Radiotrope".to_string()
}

/// A name usable as a DNS label: letters, digits and dashes, at most 63
fn host_label(name: &str) -> String {
    let first = name.split('.').next().unwrap_or_default();
    let label: String = first
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(63)
        .collect();
    match label.trim_matches('-') {
        "" => "radiotrope".to_string(),
        label => label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_labels_are_dns_safe() {
        assert_eq!(host_label("Living Room PC"), "Living-Room-PC");
        assert_eq!(host_label("box.example.lan"), "box");
        assert_eq!(host_label("Γιώργος"), "radiotrope");
    }

    #[test]
    fn the_computer_has_a_name() {
        assert!(!computer_name().is_empty());
    }
}
