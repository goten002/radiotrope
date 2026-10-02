//! The command line: output formats and exit codes

mod support;

use std::io::Write;
use std::process::{Command, Output, Stdio};

use support::{status, Live, Server};

fn probe(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radiotrope-probe"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn help_and_version_exit_0() {
    let help = probe(&["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stdout(&help).contains("Exit status: 0 up, 1 degraded, 2 down"));
    assert_eq!(probe(&["--version"]).status.code(), Some(0));
}

#[test]
fn usage_errors_exit_3_not_2() {
    // 2 would read as "down" to a scheduler
    assert_eq!(probe(&[]).status.code(), Some(3));
    assert_eq!(
        probe(&["--no-such-flag", "http://x"]).status.code(),
        Some(3)
    );
    assert_eq!(probe(&["--listen", "0", "http://x"]).status.code(), Some(3));
    assert_eq!(
        probe(&["--json", "--quiet", "http://x"]).status.code(),
        Some(3)
    );
    assert_eq!(
        probe(&["--input", "/no/such/list.txt"]).status.code(),
        Some(3)
    );
}

#[test]
fn a_down_station_exits_2_with_json() {
    let output = probe(&["--json", "ftp://radio.example/live"]);
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(json["status"], "down");
    assert_eq!(json["error"]["code"], "invalid_url");
}

#[test]
fn several_stations_make_an_array_in_the_order_given() {
    let server = Server::start();
    let live = server.route("/live", Live::tone().handler());
    let gone = server.route("/gone", status(404, "Not Found"));
    let output = probe(&["--json", "-l", "1", &live, &gone, "ftp://x/"]);
    // The worst station decides
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let list = json.as_array().unwrap();
    let urls: Vec<&str> = list.iter().map(|r| r["url"].as_str().unwrap()).collect();
    assert_eq!(urls, [live.as_str(), gone.as_str(), "ftp://x/"]);
    let statuses: Vec<&str> = list.iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["up", "down", "down"]);
    assert_eq!(list[1]["error"]["http_status"], 404);
}

#[test]
fn ndjson_prints_one_object_per_line() {
    let output = probe(&["--ndjson", "ftp://a/", "ftp://b/", "ftp://c/"]);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    for line in lines {
        let json: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(json["status"], "down");
    }
}

#[test]
fn an_up_station_exits_0_with_one_line() {
    let server = Server::start();
    let url = server.route(
        "/live",
        Live {
            metaint: Some(4000),
            title: "Artist - Song",
            ..Live::tone()
        }
        .handler(),
    );
    let output = probe(&["--quiet", "-l", "2", &url]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.starts_with("UP Test FM: MP3 128 kbps, 44.1 kHz stereo, -"),
        "{text}"
    );
    assert!(text.ends_with(", \"Artist - Song\"\n"), "{text}");
}

#[test]
fn the_full_text_names_the_station() {
    let server = Server::start();
    let url = server.route("/live", Live::silence().handler());
    let output = probe(&["-l", "2", &url]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(text.starts_with("DEGRADED  Test FM\n"), "{text}");
    assert!(text.contains(&format!("  URL      {url}\n")), "{text}");
    assert!(
        text.contains("  Format   MP3 128 kbps, 44.1 kHz stereo"),
        "{text}"
    );
    assert!(
        text.contains("  Problem  No sound above -50 dBFS"),
        "{text}"
    );
    assert!(text.contains("  Now      no song info\n"), "{text}");
}

#[test]
fn a_list_skips_comments_and_blank_lines() {
    let dir = std::env::temp_dir().join(format!("radiotrope-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let list = dir.join("stations.txt");
    // As a Windows editor might save it: BOM and CRLF
    std::fs::write(
        &list,
        "\u{feff}# my stations\r\nftp://a/\r\n\r\n  ftp://b/  \r\n",
    )
    .unwrap();
    let output = probe(&["--ndjson", "-i", list.to_str().unwrap()]);
    let _ = std::fs::remove_dir_all(&dir);
    let text = stdout(&output);
    let urls: Vec<String> = text
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["url"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut sorted = urls.clone();
    sorted.sort();
    assert_eq!(sorted, ["ftp://a/", "ftp://b/"]);
}

#[test]
fn a_list_comes_from_standard_input() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_radiotrope-probe"))
        .args(["--json", "-i", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"ftp://a/\nftp://b/\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 2);
}
