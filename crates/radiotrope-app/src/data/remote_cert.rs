//! The certificate the player serves the Remote API with
//!
//! Made by the player for itself on first use (self-signed, so no domain or
//! certificate authority is needed) and kept with its private key in
//! `remote-cert.pem` next to `remote.json`, readable by the user only.
//! Phones don't check it against any authority: they keep its SHA-256
//! fingerprint when they pair (from the QR code, or from the first
//! connection when pairing with a code) and refuse any other certificate
//! after that. A damaged file is replaced by a new certificate, and phones
//! then pair again.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::remote::CERT_FILE;
use crate::data::{agent_token, storage};

/// The certificate and its private key, as DER
#[derive(Clone)]
pub struct PlayerCert {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl PlayerCert {
    /// The saved certificate, or a new one saved first
    pub fn load_or_create() -> io::Result<Self> {
        Self::load_or_create_at(&path()?)
    }

    pub fn load_or_create_at(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => match Self::from_pem(&text) {
                Some(cert) => return Ok(cert),
                None => eprintln!(
                    "Remote Control: the certificate in {} is damaged; making a new one, so phones pair again",
                    path.display()
                ),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let (cert, pem) = Self::generate()?;
        storage::write_private(path, &pem)?;
        Ok(cert)
    }

    /// A new certificate, and the PEM text to keep it in
    pub fn generate() -> io::Result<(Self, String)> {
        let made = rcgen::generate_simple_self_signed(vec!["radiotrope.local".to_string()])
            .map_err(io::Error::other)?;
        let pem = format!("{}{}", made.signing_key.serialize_pem(), made.cert.pem());
        let cert = Self {
            cert_der: made.cert.der().to_vec(),
            key_der: made.signing_key.serialize_der(),
        };
        Ok((cert, pem))
    }

    /// The certificate's SHA-256 fingerprint as lowercase hex, as phones
    /// keep it
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert_der)
    }

    fn from_pem(text: &str) -> Option<Self> {
        let mut cert_der = None;
        let mut key_der = None;
        for block in pem_blocks(text) {
            match block.0 {
                "CERTIFICATE" => cert_der = Some(block.1),
                "PRIVATE KEY" => key_der = Some(block.1),
                _ => {}
            }
        }
        Some(Self {
            cert_der: cert_der?,
            key_der: key_der?,
        })
    }
}

/// SHA-256 of `der` as lowercase hex
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Each `-----BEGIN <label>-----` block: its label and decoded bytes
fn pem_blocks(text: &str) -> Vec<(&str, Vec<u8>)> {
    let mut blocks = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|l| l.strip_suffix("-----"))
        else {
            continue;
        };
        let end = format!("-----END {label}-----");
        let body: String = lines.by_ref().take_while(|l| *l != end).collect();
        if let Some(bytes) = base64_decode(&body) {
            blocks.push((label, bytes));
        }
    }
    blocks
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let text = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= value(c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        match chunk.len() {
            4 => out.extend_from_slice(&bytes[1..4]),
            3 => out.extend_from_slice(&bytes[1..3]),
            2 => out.push(bytes[1]),
            _ => return None,
        }
    }
    Some(out)
}

fn path() -> io::Result<PathBuf> {
    let dir = agent_token::private_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(CERT_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("radiotrope-cert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn the_certificate_is_made_once_and_kept() {
        let path = temp_file("kept.pem");
        let first = PlayerCert::load_or_create_at(&path).unwrap();
        let again = PlayerCert::load_or_create_at(&path).unwrap();
        assert_eq!(first.cert_der, again.cert_der);
        assert_eq!(first.key_der, again.key_der);
        assert_eq!(first.fingerprint(), again.fingerprint());
        assert_eq!(first.fingerprint().len(), 64);
    }

    #[test]
    fn a_damaged_file_gets_a_new_certificate() {
        let path = temp_file("damaged.pem");
        std::fs::write(&path, "-----BEGIN CERTIFICATE-----\n@@@\n").unwrap();
        let cert = PlayerCert::load_or_create_at(&path).unwrap();
        let again = PlayerCert::load_or_create_at(&path).unwrap();
        assert_eq!(cert.fingerprint(), again.fingerprint());
    }

    #[test]
    fn base64_round_trips() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert_eq!(base64_decode("TQ==").unwrap(), b"M");
        assert!(base64_decode("T").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_file("mode.pem");
        PlayerCert::load_or_create_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
