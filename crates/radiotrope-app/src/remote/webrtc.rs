//! Listening on the phone over WebRTC: the playing station as Opus packets
//!
//! The phone sends its offer (`POST /v1/listen/webrtc`, with its token and
//! all its ICE candidates in the SDP) and gets the player's answer back in
//! the same request, so no other signalling is needed. The player is an
//! ICE-lite peer with one host candidate: a UDP port from
//! [`WEBRTC_PORTS`] on the address the phone reached the API on, so a
//! firewall rule can name them. Each call has its own thread and socket.
//! Why a call didn't connect goes to the log (stderr, or radiotrope.log on
//! Windows).
//!
//! A call runs until the phone hangs up (`DELETE /v1/listen/webrtc/<id>`,
//! or the connection goes quiet), is unpaired, or Remote Control is turned
//! off. It plays with a fraction of a second of delay: no player buffer,
//! just WebRTC's jitter buffer. This is the only way a phone listens.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use str0m::change::SdpOffer;
use str0m::format::{Codec, PayloadParams};
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio_util::sync::CancellationToken;

use radiotrope::audio::recording::{ListenError, ListenStream, LISTEN_OPUS_FRAME};
use radiotrope_app::config::remote::{LISTEN_CHECK, WEBRTC_CONNECT_TIMEOUT, WEBRTC_PORTS};
use radiotrope_app::data::remote::random_hex;

use super::server::{error, json, no_content, read_json, Body, Shared};

/// How many packets back each redundant copy in a RED packet reaches:
/// the previous one and the one before that
const RED_DISTANCES: [u32; 2] = [1, 2];

/// Payload type the player offers Opus on (the usual one; the phone's
/// offer decides the real one)
const OPUS_PT: u8 = 111;

/// Longest the call's thread sleeps between looks at the audio queue
const AUDIO_TICK: Duration = Duration::from_millis(5);

/// Largest UDP packet read
const MAX_DATAGRAM: usize = 2000;

/// A wait this long between two packets of sound is a pause in what the
/// phone gets (packets are 20 ms each)
const PAUSE: Duration = Duration::from_millis(150);

/// How often pauses in the sound are summed up in the log
const PAUSE_REPORT: Duration = Duration::from_secs(10);

/// A call in progress
pub struct Call {
    device_id: String,
    hang_up: Arc<AtomicBool>,
}

/// Calls by their id
pub type Calls = HashMap<String, Call>;

#[derive(Deserialize)]
struct Offer {
    sdp: String,
}

#[derive(Serialize)]
struct Answer {
    /// The player's SDP answer
    sdp: String,
    /// Hang up with `DELETE /v1/listen/webrtc/<call>`
    call: String,
}

/// `POST /v1/listen/webrtc`: answer the phone's offer and start sending
pub(super) async fn offer(
    shared: &Shared,
    device_id: &str,
    local: IpAddr,
    request: Request<Incoming>,
    cancel: CancellationToken,
) -> Response<Body> {
    let offer: Offer = match read_json(request).await {
        Ok(offer) => offer,
        Err(response) => return *response,
    };
    let offered = offer.sdp;
    let Ok(offer) = SdpOffer::from_sdp_string(&offered) else {
        return bad_offer();
    };
    let Some(name) = shared.device_name(device_id) else {
        return error(
            StatusCode::UNAUTHORIZED,
            "not_paired",
            "This phone isn't paired with the player",
        );
    };
    let Some(listen) = shared.control.snapshot().listen else {
        return no_audio();
    };

    let socket = match bind(local) {
        Ok(socket) => socket,
        Err(e) => return failed(&format!("Can't open a port for the sound: {e}")),
    };
    let Ok(local_addr) = socket.local_addr() else {
        return failed("Can't open a port for the sound");
    };
    // Opus with RED (RFC 2198): each packet also carries the two before it,
    // so a phone whose Wi-Fi drops a packet here and there rebuilds the sound
    // without asking for it again or waiting. Three times the audio bytes
    // (about 390 kbit/s), nothing on a local network. A phone that doesn't
    // offer RED gets plain Opus. The decoder is stereo either way (opus/48000/2).
    let config = RtcConfig::new()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_opus(true, true)
        .set_red_distances(&RED_DISTANCES);
    let mut rtc = config.build(Instant::now());
    let Ok(candidate) = Candidate::host(local_addr, "udp") else {
        return failed("Can't open a port for the sound");
    };
    rtc.add_local_candidate(candidate);
    let answer = match rtc.sdp_api().accept_offer(offer) {
        Ok(answer) => answer,
        Err(_) => return bad_offer(),
    };

    let audio = match listen.subscribe(&name) {
        Ok(audio) => audio,
        Err(e) => return subscribe_error(e),
    };
    let Ok(id) = random_hex(8) else {
        return failed("Can't start listening");
    };
    let hang_up = Arc::new(AtomicBool::new(false));
    shared.calls().insert(
        id.clone(),
        Call {
            device_id: device_id.to_string(),
            hang_up: hang_up.clone(),
        },
    );
    shared.changed();

    let checks = shared.clone();
    let call_id = id.clone();
    let device = device_id.to_string();
    eprintln!("Listening: {name} calls; sound goes from UDP {local_addr}");
    let spawned = std::thread::Builder::new()
        .name("listen-webrtc".into())
        .spawn(move || {
            // Still wanted: not hung up, the phone is paired, and Remote
            // Control is on
            let wanted = || {
                !hang_up.load(Ordering::SeqCst)
                    && !cancel.is_cancelled()
                    && checks.device_name(&device).is_some()
            };
            run(rtc, socket, audio, wanted, &name);
            checks.calls().remove(&call_id);
            checks.changed();
        });
    if let Err(e) = spawned {
        shared.calls().remove(&id);
        return failed(&format!("Can't start listening: {e}"));
    }

    json(
        StatusCode::OK,
        &Answer {
            sdp: fill_refused(&answer.to_sdp_string(), &offered),
            call: id,
        },
    )
}

/// `answer` with each refused media line listing its offer's formats.
/// str0m refuses what it has no codec for (the video Android's WebRTC
/// offers by default) with an empty format list, and WebRTC refuses to
/// read such an answer; a refused line may list any format, so it gets
/// the offer's own
fn fill_refused(answer: &str, offer: &str) -> String {
    let mut offered = offer.lines().filter(|l| l.starts_with("m="));
    let lines = answer.split("\r\n").map(|line| {
        if !line.starts_with("m=") {
            return line.to_string();
        }
        let theirs = offered.next();
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() > 3 {
            return line.to_string();
        }
        let formats = theirs
            .map(|m| m.split_whitespace().skip(3).collect::<Vec<_>>().join(" "))
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| "0".to_string());
        format!("{} {formats}", fields.join(" "))
    });
    lines.collect::<Vec<_>>().join("\r\n")
}

/// `DELETE /v1/listen/webrtc/<call>`: the phone hangs up
pub(super) fn hang_up(shared: &Shared, device_id: &str, call: &str) -> Response<Body> {
    let calls = shared.calls();
    match calls.get(call).filter(|c| c.device_id == device_id) {
        Some(call) => {
            call.hang_up.store(true, Ordering::SeqCst);
            no_content()
        }
        None => error(StatusCode::NOT_FOUND, "no_call", "This call has ended"),
    }
}

/// A UDP socket on `local` with the first free port of [`WEBRTC_PORTS`],
/// or any port when all are taken (a firewall may then block it)
fn bind(local: IpAddr) -> std::io::Result<UdpSocket> {
    for port in WEBRTC_PORTS {
        match UdpSocket::bind(SocketAddr::new(local, port)) {
            Ok(socket) => return Ok(socket),
            Err(e) if e.kind() == ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e),
        }
    }
    eprintln!(
        "Listening: UDP ports {}-{} are all in use; using any free port",
        WEBRTC_PORTS.start(),
        WEBRTC_PORTS.end()
    );
    UdpSocket::bind(SocketAddr::new(local, 0))
}

/// How the sound goes to the phone, for the log once a call plays
fn redundancy(name: &str, opus: Option<&PayloadParams>) -> String {
    match opus.and_then(|p| p.red()) {
        Some(_) => format!(
            "{name} gets Opus with RED: each packet repeats the {} before it",
            RED_DISTANCES.len()
        ),
        None => format!("{name} gets plain Opus: the phone didn't offer RED (redundancy)"),
    }
}

/// What happened on a call's socket, for the log
#[derive(Default)]
struct Trace {
    /// Packets that came in, and the first one's sender
    received: u64,
    from: Option<SocketAddr>,
    /// Packets sent back
    sent: u64,
    /// The last ICE state reached
    ice: Option<IceConnectionState>,
}

/// Pauses in the sound sent on a connected call, for the log: the player
/// should send a packet every 20 ms, so a long wait, or a packet the
/// computer refused to send, means the phone's sound stutters because of
/// this side, not the network
#[derive(Default)]
struct Pauses {
    last_packet: Option<Instant>,
    packets: u64,
    count: u64,
    longest: Duration,
    /// Packets the socket refused, and the last reason
    unsent: u64,
    send_error: Option<String>,
}

impl Pauses {
    fn sent(&mut self, now: Instant) {
        if let Some(last) = self.last_packet {
            let wait = now.duration_since(last);
            if wait >= PAUSE {
                self.count += 1;
                self.longest = self.longest.max(wait);
            }
        }
        self.last_packet = Some(now);
        self.packets += 1;
    }

    fn unsent(&mut self, error: &std::io::Error) {
        self.unsent += 1;
        self.send_error = Some(error.to_string());
    }

    /// The log line for the last report period, if the sound paused or
    /// packets weren't sent; starts a new period
    fn report(&mut self, name: &str) -> Option<String> {
        let mut parts = Vec::new();
        if self.count > 0 {
            parts.push(format!(
                "sound paused {} times in the last {} s (longest {} ms); \
                 {} packets went (about {} expected)",
                self.count,
                PAUSE_REPORT.as_secs(),
                self.longest.as_millis(),
                self.packets,
                PAUSE_REPORT.as_millis() / 20
            ));
        }
        if self.unsent > 0 {
            parts.push(format!(
                "{} packets of sound couldn't be sent in the last {} s ({})",
                self.unsent,
                PAUSE_REPORT.as_secs(),
                self.send_error.as_deref().unwrap_or("?")
            ));
        }
        let line = (!parts.is_empty()).then(|| format!("{name}: {}", parts.join("; ")));
        *self = Self {
            last_packet: self.last_packet,
            ..Self::default()
        };
        line
    }
}

/// Why a call that never connected didn't, for the log
fn not_connected(name: &str, local_addr: SocketAddr, trace: &Trace, secs: u64) -> String {
    if trace.received == 0 {
        format!(
            "{name}'s call ended after {secs} s without connecting: nothing from the phone \
             reached UDP {local_addr}. A firewall on this computer (or Wi-Fi client \
             isolation) is blocking it; allow UDP ports {}-{} for Radiotrope.",
            WEBRTC_PORTS.start(),
            WEBRTC_PORTS.end()
        )
    } else {
        let from = trace
            .from
            .map_or_else(|| "?".to_string(), |a| a.to_string());
        let ice = trace
            .ice
            .map_or_else(|| "never started".to_string(), |s| format!("{s:?}"));
        format!(
            "{name}'s call ended after {secs} s without connecting: {} packets came to UDP \
             {local_addr} from {from}, {} went back; ICE {ice}",
            trace.received, trace.sent
        )
    }
}

/// Send the audio until the call ends: drive `rtc` with the socket's
/// packets and the clock, and write each Opus packet as it comes
fn run(
    mut rtc: Rtc,
    socket: UdpSocket,
    audio: ListenStream,
    wanted: impl Fn() -> bool,
    name: &str,
) {
    let Ok(local_addr) = socket.local_addr() else {
        return;
    };
    // What got through, both ways: nothing in means something blocks UDP
    let mut trace = Trace::default();
    let started = Instant::now();
    let mut buf = vec![0; MAX_DATAGRAM];
    let mut mid: Option<Mid> = None;
    let mut pt: Option<Pt> = None;
    let mut connected = false;
    let mut media_time: u64 = 0;
    let mut last_check = Instant::now();
    let mut pauses = Pauses::default();
    let mut last_report = Instant::now();
    // Why a connected call ended, and when the phone was last heard, for
    // the log
    let mut why = "it stopped";
    let mut heard = Instant::now();

    'call: loop {
        // Drain everything the last change produced
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => match socket.send_to(&t.contents, t.destination) {
                    Ok(_) => trace.sent += 1,
                    Err(e) if trace.sent == 0 => {
                        eprintln!("Listening: can't send to {}: {e}", t.destination)
                    }
                    Err(e) => pauses.unsent(&e),
                },
                Ok(Output::Event(event)) => match event {
                    Event::Connected => {
                        connected = true;
                        eprintln!("Listening: {name}'s call connected");
                    }
                    Event::IceConnectionStateChange(state) => {
                        if !connected {
                            eprintln!("Listening: {name}'s call: ICE {state:?}");
                        }
                        trace.ice = Some(state);
                        if state == IceConnectionState::Disconnected {
                            why = "nothing came from the phone for 15 s";
                            break 'call;
                        }
                    }
                    Event::MediaAdded(m) if m.kind == MediaKind::Audio => mid = Some(m.mid),
                    _ => {}
                },
                Err(e) => {
                    eprintln!("Listening: {e}");
                    why = "an error (above)";
                    break 'call;
                }
            }
        };
        if !rtc.is_alive() {
            why = "the phone closed it";
            break;
        }

        let now = Instant::now();
        if now.duration_since(last_check) >= LISTEN_CHECK {
            last_check = now;
            if !wanted() {
                why = "the phone hung up, or Remote Control went off";
                break;
            }
            if !connected && now.duration_since(started) > WEBRTC_CONNECT_TIMEOUT {
                break;
            }
            if connected && now.duration_since(last_report) >= PAUSE_REPORT {
                last_report = now;
                if let Some(line) = pauses.report(name) {
                    eprintln!("Listening: {line}");
                }
            }
        }

        // One packet of audio per change
        match audio.try_recv() {
            Ok(packet) => {
                if let (true, Some(mid)) = (connected, mid) {
                    if let Some(writer) = rtc.writer(mid) {
                        let pt = *pt.get_or_insert_with(|| {
                            let opus = writer
                                .payload_params()
                                .find(|p| p.spec().codec == Codec::Opus);
                            eprintln!("Listening: {}", redundancy(name, opus));
                            opus.map(|p| p.pt()).unwrap_or(Pt::new_with_value(OPUS_PT))
                        });
                        let at = MediaTime::new(media_time, Frequency::FORTY_EIGHT_KHZ);
                        media_time += LISTEN_OPUS_FRAME as u64;
                        let now = Instant::now();
                        if let Err(e) = writer.write(pt, now, at, packet) {
                            eprintln!("Listening: {e}");
                            why = "an error (above)";
                            break;
                        }
                        pauses.sent(now);
                        continue;
                    }
                }
                // Not connected yet: the packet goes (no use sending it late)
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            // Fell behind, or listening stopped
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                why = "the player's sound stopped coming to it";
                break;
            }
        }

        let wait = timeout
            .saturating_duration_since(Instant::now())
            .clamp(Duration::from_millis(1), AUDIO_TICK);
        if socket.set_read_timeout(Some(wait)).is_err() {
            break;
        }
        buf.resize(MAX_DATAGRAM, 0);
        let input = match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                trace.received += 1;
                heard = Instant::now();
                trace.from.get_or_insert(source);
                buf.truncate(n);
                let Ok(contents) = buf.as_slice().try_into() else {
                    continue;
                };
                Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local_addr,
                        contents,
                    },
                )
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Input::Timeout(Instant::now())
            }
            // Windows reports a phone's closed port on the next read
            Err(e) if e.kind() == ErrorKind::ConnectionReset => Input::Timeout(Instant::now()),
            Err(e) => {
                eprintln!("Listening: {e}");
                why = "an error (above)";
                break;
            }
        };
        if let Err(e) = rtc.handle_input(input) {
            eprintln!("Listening: {e}");
            why = "an error (above)";
            break;
        }
    }
    if connected {
        eprintln!(
            "Listening: {name}'s call ended after {} s: {why} (last packet from the phone {} s \
             before)",
            started.elapsed().as_secs(),
            heard.elapsed().as_secs()
        );
    } else {
        let secs = started.elapsed().as_secs();
        eprintln!(
            "Listening: {}",
            not_connected(name, local_addr, &trace, secs)
        );
    }
    rtc.disconnect();
    // Say goodbye if anything is still queued
    while let Ok(Output::Transmit(t)) = rtc.poll_output() {
        let _ = socket.send_to(&t.contents, t.destination);
    }
}

fn bad_offer() -> Response<Body> {
    error(
        StatusCode::BAD_REQUEST,
        "bad_offer",
        "The phone's WebRTC offer can't be used",
    )
}

fn failed(message: &str) -> Response<Body> {
    error(StatusCode::SERVICE_UNAVAILABLE, "failed", message)
}

/// Why a phone couldn't join the listeners, as a response
fn subscribe_error(e: ListenError) -> Response<Body> {
    match e {
        ListenError::TooMany => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too_many_listeners",
            &ListenError::TooMany.to_string(),
        ),
        e => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "failed",
            &format!("Can't start listening: {e}"),
        ),
    }
}

fn no_audio() -> Response<Body> {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "no_audio",
        "The player has no audio output to listen to",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_says_whether_the_phone_got_through() {
        let addr: SocketAddr = "192.168.1.20:8767".parse().unwrap();
        let blocked = not_connected("Pixel", addr, &Trace::default(), 8);
        assert!(blocked.contains("after 8 s"), "{blocked}");
        assert!(blocked.contains("nothing from the phone reached UDP 192.168.1.20:8767"));
        assert!(blocked.contains("allow UDP ports 8767-8770"));
        let trace = Trace {
            received: 12,
            from: Some("192.168.1.50:40000".parse().unwrap()),
            sent: 3,
            ice: Some(IceConnectionState::Checking),
        };
        let half = not_connected("Pixel", addr, &trace, 8);
        assert!(
            half.contains("12 packets came to UDP 192.168.1.20:8767 from 192.168.1.50:40000"),
            "{half}"
        );
        assert!(half.contains("3 went back; ICE Checking"), "{half}");
    }

    #[test]
    fn pauses_in_the_sound_are_summed_up() {
        let mut pauses = Pauses::default();
        let start = Instant::now();
        for i in 0..10 {
            pauses.sent(start + Duration::from_millis(20 * i));
        }
        // Steady packets: nothing to say
        assert_eq!(pauses.report("Pixel"), None);
        pauses.sent(start + Duration::from_millis(180));
        pauses.sent(start + Duration::from_millis(700));
        pauses.sent(start + Duration::from_millis(720));
        let line = pauses.report("Pixel").unwrap();
        assert!(
            line.contains("Pixel: sound paused 1 times in the last 10 s (longest 520 ms)"),
            "{line}"
        );
        assert!(
            line.contains("3 packets went (about 500 expected)"),
            "{line}"
        );
        // A new period starts from the last packet
        pauses.sent(start + Duration::from_millis(740));
        assert_eq!(pauses.report("Pixel"), None);
        // Packets the computer refused to send are counted too
        let refused = std::io::Error::other("No buffer space available");
        pauses.unsent(&refused);
        pauses.unsent(&refused);
        assert_eq!(
            pauses.report("Pixel").unwrap(),
            "Pixel: 2 packets of sound couldn't be sent in the last 10 s \
             (No buffer space available)"
        );
    }

    #[test]
    fn a_refused_video_line_lists_the_offer_s_formats() {
        let offer = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111 63\r\na=mid:0\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\na=mid:1\r\n";
        let answer = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n\
                      m=video 0 UDP/TLS/RTP/SAVPF \r\na=mid:1\r\n";
        assert_eq!(
            fill_refused(answer, offer),
            "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n\
             m=video 0 UDP/TLS/RTP/SAVPF 96 97\r\na=mid:1\r\n"
        );
        // An answer with nothing refused stays as it is
        let plain = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n";
        assert_eq!(fill_refused(plain, offer), plain);
    }

    #[test]
    fn calls_take_the_firewall_friendly_ports_first() {
        let socket = bind(IpAddr::from([127, 0, 0, 1])).unwrap();
        let port = socket.local_addr().unwrap().port();
        // Other tests may hold some of them; never a port below the range
        assert!(port >= *WEBRTC_PORTS.start(), "{port}");
    }
}
