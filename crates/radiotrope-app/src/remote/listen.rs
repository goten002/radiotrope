//! Listening on the phone: the playing station as an MP3 stream
//!
//! A paired phone asks for a ticket (`POST /v1/listen`, with its token) and
//! opens the stream with it (`GET /v1/listen/stream?t=<ticket>`). The ticket
//! works once and only for a short while, so the phone's token never goes
//! in a URL, and any audio player can open the stream, even one that can't
//! send headers. The stream runs until the phone hangs up, is unpaired, or
//! Remote Control is turned off.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::Serialize;
use tokio::sync::mpsc::error::TrySendError;
use tokio_util::sync::CancellationToken;

use radiotrope::audio::recording::{
    ListenError, LISTEN_BITRATE_KBPS, LISTEN_CHANNELS, LISTEN_SAMPLE_RATE,
};
use radiotrope_app::config::remote::{LISTEN_CHECK, LISTEN_TICKET_LIFETIME};
use radiotrope_app::data::remote::random_hex;

use super::server::{error, json, Body, Events, Shared};

/// How long the sender waits for a slow phone before trying again
const SEND_RETRY: Duration = Duration::from_millis(50);

/// A phone's right to open the stream once
pub struct Ticket {
    device_id: String,
    expires: Instant,
}

/// Open tickets by their secret
pub type Tickets = HashMap<String, Ticket>;

#[derive(Serialize)]
struct Listening {
    /// Where to open the stream, on this player
    url: String,
    /// Seconds the ticket works for
    expires_in: u64,
    content_type: &'static str,
    bitrate_kbps: u32,
    sample_rate: u32,
    channels: u16,
}

/// `POST /v1/listen`: a ticket for the stream
pub(super) fn ticket(shared: &Shared, device_id: &str) -> Response<Body> {
    if shared.control.snapshot().listen.is_none() {
        return no_audio();
    }
    let Ok(secret) = random_hex(16) else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed",
            "Can't make a ticket",
        );
    };
    let now = Instant::now();
    {
        let mut tickets = shared.tickets();
        tickets.retain(|_, t| t.expires > now);
        tickets.insert(
            secret.clone(),
            Ticket {
                device_id: device_id.to_string(),
                expires: now + LISTEN_TICKET_LIFETIME,
            },
        );
    }
    json(
        StatusCode::OK,
        &Listening {
            url: format!("/v1/listen/stream?t={secret}"),
            expires_in: LISTEN_TICKET_LIFETIME.as_secs(),
            content_type: "audio/mpeg",
            bitrate_kbps: LISTEN_BITRATE_KBPS,
            sample_rate: LISTEN_SAMPLE_RATE,
            channels: LISTEN_CHANNELS,
        },
    )
}

/// `GET /v1/listen/stream?t=<ticket>`: the endless MP3 stream
pub(super) fn stream(
    shared: &Shared,
    request: &Request<Incoming>,
    cancel: CancellationToken,
) -> Response<Body> {
    let secret = request
        .uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|pair| pair.strip_prefix("t="))
                .map(str::to_string)
        })
        .unwrap_or_default();
    let ticket = shared.tickets().remove(&secret);
    let Some(ticket) = ticket.filter(|t| t.expires > Instant::now()) else {
        return error(
            StatusCode::UNAUTHORIZED,
            "bad_ticket",
            "This listening link has expired; ask for a new one",
        );
    };
    let Some(name) = shared.device_name(&ticket.device_id) else {
        return error(
            StatusCode::UNAUTHORIZED,
            "not_paired",
            "This phone isn't paired with the player",
        );
    };
    let Some(listen) = shared.control.snapshot().listen else {
        return no_audio();
    };
    let audio = match listen.subscribe(&name) {
        Ok(audio) => audio,
        Err(e) => return subscribe_error(e),
    };
    shared.changed();

    // The audio arrives on a plain thread; hand it to the connection
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let checks = shared.clone();
    let device_id = ticket.device_id;
    let spawned = std::thread::Builder::new()
        .name("listen-sender".into())
        .spawn(move || {
            // Still wanted: the phone is connected and paired, and Remote
            // Control is on
            let wanted = |tx: &tokio::sync::mpsc::Sender<Bytes>| {
                !tx.is_closed()
                    && !cancel.is_cancelled()
                    && checks.device_name(&device_id).is_some()
            };
            'stream: loop {
                match audio.recv_timeout(LISTEN_CHECK) {
                    Ok(piece) => {
                        // Waits while the phone reads slowly; if it falls
                        // far behind, the encoder drops it and this ends
                        let mut piece = Bytes::copy_from_slice(&piece);
                        loop {
                            match tx.try_send(piece) {
                                Ok(()) => break,
                                Err(TrySendError::Full(back)) => {
                                    if !wanted(&tx) {
                                        break 'stream;
                                    }
                                    piece = back;
                                    std::thread::sleep(SEND_RETRY);
                                }
                                Err(TrySendError::Closed(_)) => break 'stream,
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
                if !wanted(&tx) {
                    break;
                }
            }
            drop(audio);
            checks.changed();
        });
    if let Err(e) = spawned {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "failed",
            &format!("Can't start listening: {e}"),
        );
    }

    let mut response = Response::new(Events(rx).boxed());
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("audio/mpeg"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

/// Why a phone couldn't join the listeners, as a response
pub(super) fn subscribe_error(e: ListenError) -> Response<Body> {
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

pub(super) fn no_audio() -> Response<Body> {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "no_audio",
        "The player has no audio output to listen to",
    )
}
