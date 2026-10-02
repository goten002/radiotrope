//! Mock radio stations on 127.0.0.1 for the tests: no real station can be
//! reached from CI, and these behave on cue (slow, silent, gone…)

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, Quality};

pub type Handler = Arc<dyn Fn(&mut TcpStream) + Send + Sync>;

/// An HTTP server answering each path with its handler
pub struct Server {
    port: u16,
    routes: Arc<Mutex<HashMap<String, Handler>>>,
}

impl Server {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes: Arc<Mutex<HashMap<String, Handler>>> = Arc::default();
        let shared = routes.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let routes = shared.clone();
                thread::spawn(move || {
                    let Some(path) = read_request(&stream) else {
                        return;
                    };
                    let handler = routes.lock().unwrap().get(&path).cloned();
                    match handler {
                        Some(handler) => handler(&mut stream),
                        None => status(404, "Not Found")(&mut stream),
                    }
                });
            }
        });
        Self { port, routes }
    }

    pub fn route(&self, path: &str, handler: Handler) -> String {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_string(), handler);
        self.url(path)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

/// The request's path, once its headers are read
fn read_request(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line == "\r\n" {
            break;
        }
    }
    first.split_whitespace().nth(1).map(str::to_string)
}

pub fn status(code: u16, reason: &'static str) -> Handler {
    Arc::new(move |stream| {
        let _ = write!(
            stream,
            "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    })
}

pub fn page(content_type: &'static str, body: String) -> Handler {
    Arc::new(move |stream| {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
    })
}

/// Headers, then nothing at all
pub fn mute() -> Handler {
    Arc::new(|stream| {
        let _ = write!(
            stream,
            "HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\nicy-name: Mute FM\r\n\r\n"
        );
        thread::sleep(Duration::from_secs(60));
    })
}

/// A live MP3 station
#[derive(Clone)]
pub struct Live {
    pub audio: Arc<Vec<u8>>,
    pub kbps: u32,
    pub name: Option<&'static str>,
    /// ICY song info every this many audio bytes
    pub metaint: Option<usize>,
    pub title: &'static str,
    /// Seconds of audio sent at once before keeping time
    pub burst: f64,
    /// Times real time
    pub speed: f64,
    /// Stop sending (keeping the connection open) after this many seconds
    pub stop_after: Option<f64>,
}

impl Live {
    pub fn tone() -> Self {
        Self {
            audio: tone_mp3(),
            kbps: 128,
            name: Some("Test FM"),
            metaint: None,
            title: "",
            burst: 2.0,
            speed: 1.0,
            stop_after: None,
        }
    }

    pub fn silence() -> Self {
        Self {
            audio: silent_mp3(),
            ..Self::tone()
        }
    }

    pub fn handler(self) -> Handler {
        Arc::new(move |stream| self.serve(stream))
    }

    fn serve(&self, stream: &mut TcpStream) {
        let mut head = "HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\n".to_string();
        head.push_str(&format!("icy-br: {}\r\n", self.kbps));
        if let Some(name) = self.name {
            head.push_str(&format!("icy-name: {name}\r\n"));
        }
        if let Some(metaint) = self.metaint {
            head.push_str(&format!("icy-metaint: {metaint}\r\n"));
        }
        head.push_str("\r\n");
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }

        let bytes_per_sec = self.kbps as f64 * 125.0;
        let mut out = IcyWriter {
            metaint: self.metaint,
            title: self.title,
            until_meta: self.metaint.unwrap_or(usize::MAX),
        };
        let mut pos = 0;
        let mut send = |n: usize, stream: &mut TcpStream| -> bool {
            let mut chunk = Vec::with_capacity(n);
            for _ in 0..n {
                chunk.push(self.audio[pos]);
                pos = (pos + 1) % self.audio.len();
            }
            stream.write_all(&out.wrap(&chunk)).is_ok()
        };
        if !send((bytes_per_sec * self.burst) as usize, stream) {
            return;
        }
        let start = Instant::now();
        let tick = Duration::from_millis(50);
        let per_tick = (bytes_per_sec * self.speed * tick.as_secs_f64()) as usize;
        loop {
            if self
                .stop_after
                .is_some_and(|after| start.elapsed().as_secs_f64() >= after)
            {
                thread::sleep(Duration::from_secs(60));
                return;
            }
            thread::sleep(tick);
            if !send(per_tick, stream) {
                return;
            }
        }
    }
}

/// Puts an ICY song info block after every `metaint` bytes of audio
struct IcyWriter {
    metaint: Option<usize>,
    title: &'static str,
    until_meta: usize,
}

impl IcyWriter {
    fn wrap(&mut self, mut audio: &[u8]) -> Vec<u8> {
        let Some(metaint) = self.metaint else {
            return audio.to_vec();
        };
        let mut out = Vec::with_capacity(audio.len() + 64);
        while !audio.is_empty() {
            let n = audio.len().min(self.until_meta);
            out.extend_from_slice(&audio[..n]);
            audio = &audio[n..];
            self.until_meta -= n;
            if self.until_meta == 0 {
                let text = format!("StreamTitle='{}';", self.title);
                let blocks = text.len().div_ceil(16);
                out.push(blocks as u8);
                out.extend_from_slice(text.as_bytes());
                out.resize(out.len() + blocks * 16 - text.len(), 0);
                self.until_meta = metaint;
            }
        }
        out
    }
}

/// Three seconds of a 440 Hz tone as 128 kbps MP3, made once
pub fn tone_mp3() -> Arc<Vec<u8>> {
    static TONE: OnceLock<Arc<Vec<u8>>> = OnceLock::new();
    TONE.get_or_init(|| {
        Arc::new(encode(|i| {
            0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin()
        }))
    })
    .clone()
}

/// Three seconds of silence as 128 kbps MP3
pub fn silent_mp3() -> Arc<Vec<u8>> {
    static SILENCE: OnceLock<Arc<Vec<u8>>> = OnceLock::new();
    SILENCE.get_or_init(|| Arc::new(encode(|_| 0.0))).clone()
}

fn encode(sample: impl Fn(usize) -> f32) -> Vec<u8> {
    let mut builder = Builder::new().unwrap();
    builder.set_sample_rate(44_100).unwrap();
    builder.set_num_channels(2).unwrap();
    builder.set_brate(Bitrate::Kbps128).unwrap();
    builder.set_quality(Quality::Good).unwrap();
    let mut encoder = builder.build().unwrap();
    let pcm: Vec<f32> = (0..44_100 * 3).flat_map(|i| [sample(i); 2]).collect();
    // LAME writes into the spare capacity, and takes none as no limit
    let mut out = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.len()) + 7200);
    encoder
        .encode_to_vec(InterleavedPcm(&pcm), &mut out)
        .unwrap();
    encoder.flush_to_vec::<FlushNoGap>(&mut out).unwrap();
    out
}
