//! MPRIS: the player on the session D-Bus, as `org.mpris.MediaPlayer2.radiotrope`
//!
//! The desktops' media widgets (GNOME, KDE, COSMIC), their media keys and
//! `playerctl` find players by this name. The service runs on a thread of
//! its own, which waits for what to show next; D-Bus calls are answered on
//! zbus's own thread and handed to the window. Dropping [`Mpris`] ends the
//! thread, and with its connection the name goes away.
//!
//! Live radio has no length and no position: `CanSeek` is false, and Seek
//! and SetPosition do nothing, as the specification asks.

use std::collections::HashMap;
use std::path::Path;

use crossbeam_channel::{unbounded, Receiver, Sender};
use zbus::blocking::connection::Builder;
use zbus::blocking::Connection;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::interface;
use zbus::zvariant::{ObjectPath, Value};

use super::{Command, Commands, Controls, Status, Track};

/// Our name on the bus; a second player (`--standalone`) adds
/// `.instance<pid>`, as the specification has it
const NAME: &str = "org.mpris.MediaPlayer2.radiotrope";
const PATH: &str = "/org/mpris/MediaPlayer2";
/// The name the widgets show for the player
const IDENTITY: &str = "Radiotrope";
/// The one "track" a live station is
const TRACK_ID: &str = "/app/radiotrope/Radiotrope/Live";
/// No station yet
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";

pub(super) struct Mpris {
    tracks: Sender<Track>,
}

impl Mpris {
    /// Register on the session bus, on a thread of its own: the window
    /// doesn't wait for the bus
    pub(super) fn start(commands: Commands) -> Result<Self, String> {
        Self::start_on(Builder::session, commands)
    }

    /// Register on the bus `connect` reaches
    fn start_on(
        connect: impl FnOnce() -> zbus::Result<Builder<'static>> + Send + 'static,
        commands: Commands,
    ) -> Result<Self, String> {
        let (tracks, shown) = unbounded();
        std::thread::Builder::new()
            .name("mpris".into())
            .spawn(move || {
                if let Err(e) = connect().and_then(|builder| serve(builder, commands, &shown)) {
                    eprintln!("Media keys unavailable: {e}");
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Mpris { tracks })
    }
}

impl Controls for Mpris {
    fn show(&mut self, track: &Track) {
        let _ = self.tracks.send(track.clone());
    }
}

/// Serve the player until [`Mpris`] is dropped
fn serve(builder: Builder<'_>, commands: Commands, shown: &Receiver<Track>) -> zbus::Result<()> {
    let connection = builder
        .serve_at(
            PATH,
            Root {
                commands: commands.clone(),
            },
        )?
        .serve_at(
            PATH,
            Player {
                track: Track::default(),
                commands,
            },
        )?
        .build()?;
    request_name(&connection)?;
    let player = connection.object_server().interface::<_, Player>(PATH)?;
    // Blocks until there is something to show, or the sender is gone
    for mut track in shown.iter() {
        // Only the latest counts
        while let Ok(newer) = shown.try_recv() {
            track = newer;
        }
        let mut iface = player.get_mut();
        let old = std::mem::replace(&mut iface.track, track);
        let new = &iface.track;
        let emitter = player.signal_emitter();
        zbus::block_on(async {
            if metadata(&old) != metadata(new) {
                iface.metadata_changed(emitter).await?;
            }
            if old.status != new.status {
                iface.playback_status_changed(emitter).await?;
            }
            if old.can_play != new.can_play {
                iface.can_play_changed(emitter).await?;
                iface.can_pause_changed(emitter).await?;
            }
            zbus::Result::Ok(())
        })?;
    }
    Ok(())
}

/// Our name, or one of our own when another player has it
fn request_name(connection: &Connection) -> zbus::Result<()> {
    let flags = RequestNameFlags::DoNotQueue.into();
    match connection.request_name_with_flags(NAME, flags) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Ok(()),
        Ok(_) | Err(zbus::Error::NameTaken) => {
            let name = format!("{NAME}.instance{}", std::process::id());
            connection.request_name_with_flags(name.as_str(), flags)?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// `org.mpris.MediaPlayer2`
struct Root {
    commands: Commands,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {
        (self.commands)(Command::Raise);
    }

    fn quit(&self) {
        (self.commands)(Command::Quit);
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        IDENTITY
    }

    /// The launcher's file, without `.desktop`: the widgets take the
    /// player's icon from it
    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        radiotrope_app::config::APP_ID
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `org.mpris.MediaPlayer2.Player`
struct Player {
    track: Track,
    commands: Commands,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        (self.commands)(Command::Next);
    }

    fn previous(&self) {
        (self.commands)(Command::Previous);
    }

    fn pause(&self) {
        (self.commands)(Command::Pause);
    }

    fn play_pause(&self) {
        (self.commands)(Command::PlayPause);
    }

    fn stop(&self) {
        (self.commands)(Command::Stop);
    }

    fn play(&self) {
        (self.commands)(Command::Play);
    }

    /// Live radio: nothing to seek in
    fn seek(&self, _offset: i64) {}

    /// Live radio: nothing to seek in
    fn set_position(&self, _track_id: ObjectPath<'_>, _position: i64) {}

    fn open_uri(&self, _uri: &str) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "Radiotrope opens no addresses this way".into(),
        ))
    }

    #[zbus(property)]
    fn playback_status(&self) -> &str {
        match self.track.status {
            Status::Playing => "Playing",
            Status::Paused => "Paused",
            Status::Stopped => "Stopped",
        }
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<&'static str, Value<'static>> {
        metadata(&self.track)
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn position(&self) -> i64 {
        0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.track.can_play
    }

    /// Pause stops: a live station can't wait where it was
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.track.can_play
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

/// The MPRIS metadata of `track`: no length, which live radio hasn't
fn metadata(track: &Track) -> HashMap<&'static str, Value<'static>> {
    let mut map = HashMap::new();
    let id = if track.title.is_empty() {
        NO_TRACK
    } else {
        TRACK_ID
    };
    map.insert(
        "mpris:trackid",
        Value::new(ObjectPath::from_static_str_unchecked(id)),
    );
    if !track.title.is_empty() {
        map.insert("xesam:title", Value::new(track.title.clone()));
    }
    if let Some(artist) = &track.artist {
        map.insert("xesam:artist", Value::new(vec![artist.clone()]));
    }
    if let Some(album) = &track.album {
        map.insert("xesam:album", Value::new(album.clone()));
    }
    if let Some(art) = track.art.as_deref().and_then(file_url) {
        map.insert("mpris:artUrl", Value::new(art));
    }
    map
}

/// `file://` and the path, with what a URL can't hold escaped
fn file_url(path: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    if !path.is_absolute() {
        return None;
    }
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Child, Command as Process, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use zbus::blocking::fdo::DBusProxy;
    use zbus::blocking::proxy::Builder as ProxyBuilder;
    use zbus::blocking::Proxy;
    use zbus::proxy::CacheProperties;
    use zbus::zvariant::OwnedValue;

    use super::*;

    #[test]
    fn the_logo_is_a_file_url_with_odd_characters_escaped() {
        assert_eq!(
            file_url(Path::new("/home/me/.cache/radiotrope/logos/a b#ü.png")).as_deref(),
            Some("file:///home/me/.cache/radiotrope/logos/a%20b%23%C3%BC.png")
        );
        assert_eq!(file_url(Path::new("logos/a.png")), None);
    }

    #[test]
    fn metadata_has_no_length_and_names_the_song_and_station() {
        let track = Track {
            title: "So What".into(),
            artist: Some("Miles Davis".into()),
            album: Some("Jazz FM".into()),
            art: Some(PathBuf::from("/cache/logo.png")),
            status: Status::Playing,
            can_play: true,
        };
        let map = metadata(&track);
        assert_eq!(map["xesam:title"], Value::new("So What"));
        assert_eq!(
            map["xesam:artist"],
            Value::new(vec!["Miles Davis".to_string()])
        );
        assert_eq!(map["xesam:album"], Value::new("Jazz FM"));
        assert_eq!(map["mpris:artUrl"], Value::new("file:///cache/logo.png"));
        assert!(!map.contains_key("mpris:length"));

        let nothing = metadata(&Track::default());
        assert_eq!(nothing.len(), 1);
        assert_eq!(
            nothing["mpris:trackid"],
            Value::new(ObjectPath::from_static_str_unchecked(NO_TRACK))
        );
    }

    /// A D-Bus daemon of the test's own, so the test doesn't take the
    /// player's name on the user's session bus
    struct PrivateBus {
        daemon: Child,
        address: String,
    }

    impl PrivateBus {
        fn start() -> Option<Self> {
            let mut daemon = Process::new("dbus-daemon")
                .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let mut address = String::new();
            let stdout = daemon.stdout.take()?;
            BufReader::new(stdout).read_line(&mut address).ok()?;
            let address = address.trim().to_string();
            if address.is_empty() {
                let _ = daemon.kill();
                return None;
            }
            Some(PrivateBus { daemon, address })
        }

        fn connect(&self) -> Connection {
            Builder::address(self.address.as_str())
                .and_then(Builder::build)
                .expect("connect to the test's bus")
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
        }
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn proxy<'a>(connection: &'a Connection, interface: &'a str) -> Proxy<'a> {
        ProxyBuilder::<Proxy>::new(connection)
            .destination(NAME)
            .and_then(|b| b.path(PATH))
            .and_then(|b| b.interface(interface))
            .map(|b| b.cache_properties(CacheProperties::No))
            .and_then(|b| b.build())
            .expect("proxy")
    }

    /// The player on a bus, as a desktop's media widget or `playerctl`
    /// sees it. Skipped where there is no `dbus-daemon`, unless
    /// `RADIOTROPE_REQUIRE_DBUS` is set (CI).
    #[test]
    fn a_media_widget_sees_the_player_and_controls_it() {
        let Some(bus) = PrivateBus::start() else {
            assert!(
                std::env::var_os("RADIOTROPE_REQUIRE_DBUS").is_none(),
                "RADIOTROPE_REQUIRE_DBUS is set but dbus-daemon can't start"
            );
            eprintln!("skipped: no dbus-daemon to start a test bus with");
            return;
        };
        let (sent, received) = unbounded();
        let commands: Commands = Arc::new(move |command| {
            let _ = sent.send(command);
        });
        let address = bus.address.clone();
        let mut mpris =
            Mpris::start_on(move || Builder::address(address.as_str()), commands).unwrap();

        let client = bus.connect();
        let dbus = DBusProxy::new(&client).unwrap();
        wait_for("the player's name", || {
            dbus.name_has_owner(NAME.try_into().unwrap())
                .unwrap_or(false)
        });

        let root = proxy(&client, "org.mpris.MediaPlayer2");
        let player = proxy(&client, "org.mpris.MediaPlayer2.Player");
        assert_eq!(
            root.get_property::<String>("Identity").unwrap(),
            "Radiotrope"
        );
        assert_eq!(
            root.get_property::<String>("DesktopEntry").unwrap(),
            radiotrope_app::config::APP_ID
        );
        assert_eq!(
            player.get_property::<String>("PlaybackStatus").unwrap(),
            "Stopped"
        );
        assert!(!player.get_property::<bool>("CanSeek").unwrap());
        assert!(!player.get_property::<bool>("CanPlay").unwrap());

        mpris.show(&Track {
            title: "So What".into(),
            artist: Some("Miles Davis".into()),
            album: Some("Jazz FM".into()),
            art: None,
            status: Status::Playing,
            can_play: true,
        });
        wait_for("Playing", || {
            player.get_property::<String>("PlaybackStatus").unwrap() == "Playing"
        });
        let metadata: HashMap<String, OwnedValue> = player.get_property("Metadata").unwrap();
        assert_eq!(
            String::try_from(metadata["xesam:title"].try_clone().unwrap()).unwrap(),
            "So What"
        );
        assert_eq!(
            String::try_from(metadata["xesam:album"].try_clone().unwrap()).unwrap(),
            "Jazz FM"
        );
        assert!(player.get_property::<bool>("CanPlay").unwrap());

        let call = |proxy: &Proxy, method: &str| {
            proxy
                .call_method(method, &())
                .unwrap_or_else(|e| panic!("{method}: {e}"));
        };
        let next = || received.recv_timeout(Duration::from_secs(5)).unwrap();
        call(&player, "PlayPause");
        assert_eq!(next(), Command::PlayPause);
        call(&player, "Next");
        assert_eq!(next(), Command::Next);
        call(&player, "Previous");
        assert_eq!(next(), Command::Previous);
        call(&player, "Stop");
        assert_eq!(next(), Command::Stop);
        call(&root, "Raise");
        assert_eq!(next(), Command::Raise);
        // Seeking does nothing
        player.call_method("Seek", &(10_000_000i64)).unwrap();
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());

        // Turned off: the name goes away
        drop(mpris);
        wait_for("the name to go", || {
            !dbus
                .name_has_owner(NAME.try_into().unwrap())
                .unwrap_or(true)
        });
    }
}
