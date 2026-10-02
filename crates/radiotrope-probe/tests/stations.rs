//! Checks against mock stations

mod support;

use std::net::TcpListener;
use std::time::{Duration, Instant};

use radiotrope_probe::report::{ErrorCode, ProblemCode};
use radiotrope_probe::{check, Options, Report, Status};
use support::{mute, page, status, Live, Server};

fn options(listen: u64, timeout: u64) -> Options {
    Options {
        listen: Duration::from_secs(listen),
        timeout: Duration::from_secs(timeout),
        ..Options::default()
    }
}

fn problems(report: &Report) -> Vec<ProblemCode> {
    report.problems.iter().map(|p| p.code).collect()
}

fn error_code(report: &Report) -> ErrorCode {
    report.error.as_ref().expect("an error").code
}

#[test]
fn a_live_mp3_station_is_up_with_its_details() {
    let server = Server::start();
    let url = server.route(
        "/live",
        Live {
            metaint: Some(4000),
            title: "Daft Punk - One More Time",
            ..Live::tone()
        }
        .handler(),
    );
    let report = check(&url, &options(3, 15));

    assert_eq!(report.status, Status::Up, "{report:#?}");
    assert!(report.live);
    assert!(
        report.error.is_none() && report.problems.is_empty(),
        "{report:#?}"
    );
    let stream = report.stream.as_ref().unwrap();
    assert_eq!(stream.kind, "direct");
    assert_eq!(stream.resolved_url, url);
    assert_eq!(stream.content_type.as_deref(), Some("audio/mpeg"));
    let station = report.station.as_ref().unwrap();
    assert_eq!(station.name.as_deref(), Some("Test FM"));
    assert_eq!(station.advertised_kbps, Some(128));

    let audio = report.audio.as_ref().unwrap();
    assert_eq!(audio.codec, "MP3");
    assert_eq!((audio.sample_rate, audio.channels), (44_100, 2));
    assert!(audio.listened_s >= 2.9, "{audio:?}");
    // The 2 s burst puts it ahead of real time
    assert!(audio.decoded_s >= audio.listened_s, "{audio:?}");
    assert_eq!(audio.stalls, 0);
    // A 0.5 sine is -9 dBFS
    let rms = audio.rms_dbfs.unwrap();
    assert!((-11.0..-7.0).contains(&rms), "{audio:?}");
    assert!(audio.peak_dbfs.unwrap() > -7.0, "{audio:?}");
    assert_eq!(audio.silent_s, 0.0);
    let kbps = audio.measured_kbps.unwrap();
    assert!(kbps > 100.0, "{audio:?}");

    let song = report.metadata.as_ref().expect("song info");
    assert_eq!(song.artist.as_deref(), Some("Daft Punk"));
    assert_eq!(song.title.as_deref(), Some("One More Time"));
    assert_eq!(song.source, "icy");

    let timing = &report.timing_ms;
    assert!(timing.resolve.unwrap() <= timing.first_audio.unwrap());
    assert!(timing.total >= 3000);
    assert!(
        report
            .summary
            .starts_with("MP3 128 kbps, 44.1 kHz stereo, -"),
        "{}",
        report.summary
    );
}

#[test]
fn a_playlist_leads_to_its_stream() {
    let server = Server::start();
    let stream = server.route("/live", Live::tone().handler());
    let list = server.route(
        "/radio.pls",
        page(
            "audio/x-scpls",
            format!("[playlist]\nNumberOfEntries=1\nFile1={stream}\n"),
        ),
    );
    let report = check(&list, &options(2, 15));
    assert_eq!(report.status, Status::Up, "{report:#?}");
    assert_eq!(report.stream.unwrap().resolved_url, stream);
}

#[test]
fn a_silent_station_is_degraded() {
    let server = Server::start();
    let url = server.route("/live", Live::silence().handler());
    let report = check(&url, &options(3, 15));
    assert_eq!(report.status, Status::Degraded, "{report:#?}");
    assert!(!report.live);
    assert_eq!(problems(&report), [ProblemCode::Silent]);
    let audio = report.audio.unwrap();
    assert!(audio.rms_dbfs.unwrap() < -50.0, "{audio:?}");
    assert!(audio.silent_s >= 2.0, "{audio:?}");
}

#[test]
fn a_station_slower_than_real_time_stalls() {
    let server = Server::start();
    let url = server.route(
        "/live",
        Live {
            burst: 0.0,
            speed: 0.4,
            ..Live::tone()
        }
        .handler(),
    );
    let report = check(&url, &options(6, 20));
    assert_eq!(report.status, Status::Degraded, "{report:#?}");
    assert_eq!(problems(&report), [ProblemCode::Stalled, ProblemCode::Slow]);
    let audio = report.audio.unwrap();
    assert!(audio.stalls >= 1, "{audio:?}");
    assert!(audio.realtime_ratio < 0.8, "{audio:?}");
}

#[test]
fn a_slow_station_is_degraded_before_it_stalls() {
    let server = Server::start();
    let url = server.route(
        "/live",
        Live {
            burst: 0.0,
            speed: 0.4,
            ..Live::tone()
        }
        .handler(),
    );
    let report = check(&url, &options(2, 20));
    assert_eq!(report.status, Status::Degraded, "{report:#?}");
    assert!(
        problems(&report).contains(&ProblemCode::Slow),
        "{report:#?}"
    );
}

#[test]
fn a_station_that_stops_sending_stalls() {
    let server = Server::start();
    let url = server.route(
        "/live",
        Live {
            stop_after: Some(0.5),
            ..Live::tone()
        }
        .handler(),
    );
    let report = check(&url, &options(6, 20));
    assert_eq!(report.status, Status::Degraded, "{report:#?}");
    assert!(
        problems(&report).contains(&ProblemCode::Stalled),
        "{report:#?}"
    );
}

#[test]
fn a_missing_stream_is_down_with_its_http_status() {
    let server = Server::start();
    let url = server.route("/gone", status(404, "Not Found"));
    let report = check(&url, &options(2, 15));
    assert_eq!(report.status, Status::Down, "{report:#?}");
    let error = report.error.as_ref().unwrap();
    assert_eq!(error.code, ErrorCode::HttpStatus);
    assert_eq!(error.http_status, Some(404));
    assert!(error.message.contains("404"), "{error:?}");
    assert!(report.audio.is_none() && report.stream.is_none());
    assert_eq!(report.summary, error.message);
}

#[test]
fn a_web_page_is_down() {
    let server = Server::start();
    let url = server.route(
        "/",
        page(
            "text/html; charset=utf-8",
            "<html><body>Listen live!</body></html>".to_string(),
        ),
    );
    let report = check(&url, &options(2, 15));
    assert_eq!(report.status, Status::Down, "{report:#?}");
    assert_eq!(error_code(&report), ErrorCode::WebPage, "{report:#?}");
}

#[test]
fn an_empty_playlist_is_down() {
    let server = Server::start();
    let url = server.route(
        "/radio.pls",
        page(
            "audio/x-scpls",
            "[playlist]\nNumberOfEntries=0\n".to_string(),
        ),
    );
    let report = check(&url, &options(2, 15));
    assert_eq!(report.status, Status::Down, "{report:#?}");
    assert_eq!(error_code(&report), ErrorCode::EmptyPlaylist, "{report:#?}");
}

#[test]
fn a_closed_port_is_down() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let started = Instant::now();
    let report = check(&format!("http://127.0.0.1:{port}/live"), &options(2, 6));
    assert_eq!(report.status, Status::Down, "{report:#?}");
    assert!(
        matches!(error_code(&report), ErrorCode::Connect | ErrorCode::Timeout),
        "{report:#?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn a_station_that_never_sends_audio_is_down_in_time() {
    let server = Server::start();
    let url = server.route("/live", mute());
    let started = Instant::now();
    let report = check(&url, &options(2, 4));
    let took = started.elapsed();
    assert_eq!(report.status, Status::Down, "{report:#?}");
    assert!(
        matches!(error_code(&report), ErrorCode::Timeout | ErrorCode::NoAudio),
        "{report:#?}"
    );
    assert!(took < Duration::from_millis(5500), "{took:?}");
}

#[test]
fn the_json_has_every_section() {
    let server = Server::start();
    let url = server.route("/live", Live::tone().handler());
    let report = check(&url, &options(1, 15));
    let json = serde_json::to_value(&report).unwrap();
    for key in [
        "schema",
        "url",
        "checked_at",
        "status",
        "live",
        "summary",
        "timing_ms",
        "stream",
        "station",
        "audio",
        "metadata",
        "problems",
        "error",
    ] {
        assert!(json.get(key).is_some(), "{key} missing: {json}");
    }
    assert_eq!(json["schema"], 1);
    assert_eq!(json["status"], "up");
    assert_eq!(json["stream"]["type"], "direct");
    assert_eq!(json["audio"]["codec"], "MP3");
    assert!(json["error"].is_null());
    let at = json["checked_at"].as_str().unwrap();
    assert!(at.len() == 20 && at.ends_with('Z'), "{at}");
}
