//! Listening on the phone over WebRTC: the playing station as Opus packets
//!
//! The phone sends its offer (`POST /v1/listen/webrtc`, with its token and
//! all its ICE candidates in the SDP) and gets the player's answer back in
//! the same request, so no other signalling is needed. The player is an
//! ICE-lite peer with one host candidate: a UDP port on the address the
//! phone reached the API on. Each call has its own thread and socket.
//!
//! A call runs until the phone hangs up (`DELETE /v1/listen/webrtc/<id>`,
//! or the connection goes quiet), is unpaired, or Remote Control is turned
//! off. Next to the MP3 stream in [`super::listen`], this one plays with a
//! fraction of a second of delay: no player buffer, just WebRTC's jitter
//! buffer.

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
use str0m::format::{Codec, FormatParams};
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio_util::sync::CancellationToken;

use radiotrope::audio::recording::{ListenFormat, ListenStream, LISTEN_OPUS_FRAME};
use radiotrope_app::config::remote::{LISTEN_CHECK, WEBRTC_CONNECT_TIMEOUT};
use radiotrope_app::data::remote::random_hex;

use super::listen::{no_audio, subscribe_error};
use super::server::{error, json, no_content, read_json, Body, Shared};

/// Payload type the player offers Opus on (the usual one; the phone's
/// offer decides the real one)
const OPUS_PT: u8 = 111;

/// Longest the call's thread sleeps between looks at the audio queue
const AUDIO_TICK: Duration = Duration::from_millis(5);

/// Largest UDP packet read
const MAX_DATAGRAM: usize = 2000;

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
    let Ok(offer) = SdpOffer::from_sdp_string(&offer.sdp) else {
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

    let socket = match UdpSocket::bind(SocketAddr::new(local, 0)) {
        Ok(socket) => socket,
        Err(e) => return failed(&format!("Can't open a port for the sound: {e}")),
    };
    let Ok(local_addr) = socket.local_addr() else {
        return failed("Can't open a port for the sound");
    };
    let mut config = RtcConfig::new().set_ice_lite(true).clear_codecs();
    config.codec_config().add_config(
        Pt::new_with_value(OPUS_PT),
        None,
        Codec::Opus,
        Frequency::FORTY_EIGHT_KHZ,
        Some(2),
        FormatParams {
            min_p_time: Some(10),
            use_inband_fec: Some(true),
            // Music: ask for stereo both ways
            stereo: Some(true),
            sprop_stereo: Some(true),
            ..Default::default()
        },
    );
    let mut rtc = config.build(Instant::now());
    let Ok(candidate) = Candidate::host(local_addr, "udp") else {
        return failed("Can't open a port for the sound");
    };
    rtc.add_local_candidate(candidate);
    let answer = match rtc.sdp_api().accept_offer(offer) {
        Ok(answer) => answer,
        Err(_) => return bad_offer(),
    };

    let audio = match listen.subscribe_as(&name, ListenFormat::Opus) {
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
            run(rtc, socket, audio, wanted);
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
            sdp: answer.to_sdp_string(),
            call: id,
        },
    )
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

/// Send the audio until the call ends: drive `rtc` with the socket's
/// packets and the clock, and write each Opus packet as it comes
fn run(mut rtc: Rtc, socket: UdpSocket, audio: ListenStream, wanted: impl Fn() -> bool) {
    let Ok(local_addr) = socket.local_addr() else {
        return;
    };
    let started = Instant::now();
    let mut buf = vec![0; MAX_DATAGRAM];
    let mut mid: Option<Mid> = None;
    let mut pt: Option<Pt> = None;
    let mut connected = false;
    let mut media_time: u64 = 0;
    let mut last_check = Instant::now();

    'call: loop {
        // Drain everything the last change produced
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(event)) => match event {
                    Event::Connected => connected = true,
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        break 'call
                    }
                    Event::MediaAdded(m) if m.kind == MediaKind::Audio => mid = Some(m.mid),
                    _ => {}
                },
                Err(e) => {
                    eprintln!("Listening: {e}");
                    break 'call;
                }
            }
        };
        if !rtc.is_alive() {
            break;
        }

        let now = Instant::now();
        if now.duration_since(last_check) >= LISTEN_CHECK {
            last_check = now;
            if !wanted() || (!connected && now.duration_since(started) > WEBRTC_CONNECT_TIMEOUT) {
                break;
            }
        }

        // One packet of audio per change
        match audio.try_recv() {
            Ok(packet) => {
                if let (true, Some(mid)) = (connected, mid) {
                    if let Some(writer) = rtc.writer(mid) {
                        let pt = *pt.get_or_insert_with(|| {
                            writer
                                .payload_params()
                                .find(|p| p.spec().codec == Codec::Opus)
                                .map(|p| p.pt())
                                .unwrap_or(Pt::new_with_value(OPUS_PT))
                        });
                        let at = MediaTime::new(media_time, Frequency::FORTY_EIGHT_KHZ);
                        media_time += LISTEN_OPUS_FRAME as u64;
                        if let Err(e) = writer.write(pt, Instant::now(), at, packet) {
                            eprintln!("Listening: {e}");
                            break;
                        }
                        continue;
                    }
                }
                // Not connected yet: the packet goes (no use sending it late)
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            // Fell behind, or listening stopped
            Err(crossbeam_channel::TryRecvError::Disconnected) => break,
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
                break;
            }
        };
        if let Err(e) = rtc.handle_input(input) {
            eprintln!("Listening: {e}");
            break;
        }
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
