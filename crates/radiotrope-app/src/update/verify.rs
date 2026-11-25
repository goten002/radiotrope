//! Is a download really ours? Release files are signed with Minisign
//! (Ed25519) by the release workflow, and checked here against the public
//! key compiled into the player. HTTPS alone only says the file came from
//! the server, not that we made it.

use minisign_verify::{PublicKey, Signature};

/// The Minisign public key of the releases (the second line of
/// `minisign.pub`, 56 characters starting with `RW`).
///
/// EMPTY UNTIL THE OWNER MAKES THE KEY PAIR: with no key, no build
/// installs an update itself (see [`super::resolve`]); they only say that
/// one is out. RELEASING.md, "Signing the updates", has the commands.
pub const PUBLIC_KEY: &str = "";

/// A key is compiled in
pub fn key_is_set() -> bool {
    !PUBLIC_KEY.is_empty()
}

#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The key in the program can't be read
    Key,
    /// The `.minisig` file can't be read
    Format,
    /// Not signed by our key, or changed since
    Signature,
    /// Signed by us, but as another file (an older release offered as new)
    OtherFile,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            VerifyError::Key => "the public key can't be read",
            VerifyError::Format => "the signature file can't be read",
            VerifyError::Signature => "the signature doesn't match",
            VerifyError::OtherFile => "the signature is for another file",
        })
    }
}

/// Check that `data` is the release file `file_name`, signed with
/// `public_key`: `signature` is the text of its `.minisig` file.
///
/// The signature's trusted comment (signed too) must name the file, as
/// `file:<name>`. The name holds the version, so a correctly signed older
/// release can't be passed off as a newer one.
pub fn verify(
    data: &[u8],
    signature: &str,
    public_key: &str,
    file_name: &str,
) -> Result<(), VerifyError> {
    let key = PublicKey::from_base64(public_key.trim()).map_err(|_| VerifyError::Key)?;
    let signature = Signature::decode(signature).map_err(|_| VerifyError::Format)?;
    // Pre-hashed signatures only (what minisign makes today)
    key.verify(data, &signature, false)
        .map_err(|_| VerifyError::Signature)?;
    let named = signature
        .trusted_comment()
        .split(['\t', ' '])
        .filter_map(|field| field.strip_prefix("file:"))
        .any(|name| name == file_name);
    if !named {
        return Err(VerifyError::OtherFile);
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A key pair made for the test run: no key is kept in the repository
    pub struct TestKey {
        pair: minisign::KeyPair,
    }

    impl TestKey {
        pub fn new() -> Self {
            Self {
                pair: minisign::KeyPair::generate_unencrypted_keypair().expect("a key pair"),
            }
        }

        pub fn public(&self) -> String {
            self.pair.pk.to_base64()
        }

        /// The `.minisig` text for `data`, as the release workflow signs it
        pub fn sign(&self, data: &[u8], file_name: &str) -> String {
            self.sign_with_comment(data, &format!("file:{file_name}"))
        }

        pub fn sign_with_comment(&self, data: &[u8], trusted_comment: &str) -> String {
            minisign::sign(
                Some(&self.pair.pk),
                &self.pair.sk,
                data,
                Some(trusted_comment),
                None,
            )
            .expect("a signature")
            .into_string()
        }
    }

    const NAME: &str = "Radiotrope-0.2.0-x86_64.AppImage";

    #[test]
    fn a_file_signed_with_the_key_passes() {
        let key = TestKey::new();
        let data = b"the new version";
        let signature = key.sign(data, NAME);
        assert_eq!(verify(data, &signature, &key.public(), NAME), Ok(()));
        // minisign's own comment: timestamp, file and "hashed", tab-separated
        let signature =
            key.sign_with_comment(data, &format!("timestamp:1795000000\tfile:{NAME}\thashed"));
        assert_eq!(verify(data, &signature, &key.public(), NAME), Ok(()));
    }

    #[test]
    fn a_changed_file_fails() {
        let key = TestKey::new();
        let signature = key.sign(b"the new version", NAME);
        assert_eq!(
            verify(b"the new version!", &signature, &key.public(), NAME),
            Err(VerifyError::Signature)
        );
        assert_eq!(
            verify(b"", &signature, &key.public(), NAME),
            Err(VerifyError::Signature)
        );
    }

    #[test]
    fn a_file_signed_with_another_key_fails() {
        let ours = TestKey::new();
        let theirs = TestKey::new();
        let data = b"the new version";
        let signature = theirs.sign(data, NAME);
        assert_eq!(
            verify(data, &signature, &ours.public(), NAME),
            Err(VerifyError::Signature)
        );
    }

    #[test]
    fn an_older_release_is_not_taken_for_a_newer_one() {
        let key = TestKey::new();
        let data = b"version 0.1.0, with a known hole";
        let signature = key.sign(data, "Radiotrope-0.1.0-x86_64.AppImage");
        assert_eq!(
            verify(data, &signature, &key.public(), NAME),
            Err(VerifyError::OtherFile)
        );
        // No file named at all
        let signature = key.sign_with_comment(data, "timestamp:1795000000");
        assert_eq!(
            verify(data, &signature, &key.public(), NAME),
            Err(VerifyError::OtherFile)
        );
        // A longer name that starts the same
        let signature = key.sign(data, &format!("{NAME}.zsync"));
        assert_eq!(
            verify(data, &signature, &key.public(), NAME),
            Err(VerifyError::OtherFile)
        );
    }

    #[test]
    fn a_changed_comment_fails() {
        let key = TestKey::new();
        let data = b"version 0.1.0";
        // The comment is signed: naming the new file by hand breaks it
        let signature = key
            .sign(data, "Radiotrope-0.1.0-x86_64.AppImage")
            .replace("0.1.0", "0.2.0");
        assert_eq!(
            verify(data, &signature, &key.public(), NAME),
            Err(VerifyError::Signature)
        );
    }

    #[test]
    fn what_is_not_a_signature_or_a_key_fails() {
        let key = TestKey::new();
        let data = b"the new version";
        let signature = key.sign(data, NAME);
        assert_eq!(
            verify(data, "", &key.public(), NAME),
            Err(VerifyError::Format)
        );
        assert_eq!(
            verify(data, "<html>Not Found</html>", &key.public(), NAME),
            Err(VerifyError::Format)
        );
        assert_eq!(verify(data, &signature, "", NAME), Err(VerifyError::Key));
        assert_eq!(
            verify(data, &signature, "not a key", NAME),
            Err(VerifyError::Key)
        );
    }

    #[test]
    fn the_compiled_key_is_a_key_or_empty() {
        if key_is_set() {
            assert!(
                PublicKey::from_base64(PUBLIC_KEY).is_ok(),
                "PUBLIC_KEY must be the base64 line of minisign.pub"
            );
        }
    }
}
