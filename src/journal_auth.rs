use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub(crate) struct JournalKey(Zeroizing<[u8; 32]>);

impl JournalKey {
    pub(crate) fn load() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("JSQNL_JOURNAL_KEY_FILE") else {
            return Ok(None);
        };
        let file = crate::storage::open_regular(Path::new(&path)).context("opening journal key")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
                "journal key must be owned by the current user and inaccessible to group and others"
            );
        }
        let mut file = file;
        let mut key = Zeroizing::new([0; 32]);
        file.read_exact(&mut *key)
            .context("journal key must contain 32 raw bytes")?;
        ensure!(
            file.read(&mut [0])? == 0,
            "journal key must contain 32 raw bytes"
        );
        Ok(Some(Self(key)))
    }

    // HMAC-SHA-256 with a fixed 32-byte key, padded to SHA-256's 64-byte block size.
    fn mac(&self, parts: &[&[u8]]) -> String {
        let mut inner_pad = Zeroizing::new([0x36; 64]);
        let mut outer_pad = Zeroizing::new([0x5c; 64]);
        for (index, byte) in self.0.iter().enumerate() {
            inner_pad[index] ^= byte;
            outer_pad[index] ^= byte;
        }
        let mut inner = Sha256::new();
        inner.update(&*inner_pad);
        for part in parts {
            inner.update(part);
        }
        let mut outer = Sha256::new();
        outer.update(&*outer_pad);
        outer.update(inner.finalize());
        format!("{:x}", outer.finalize())
    }

    fn bound_mac(&self, path: &Path, payload: &[u8]) -> String {
        let encoded = path.as_os_str().as_encoded_bytes();
        self.mac(&[
            b"JSQNL journal v3\0",
            &(encoded.len() as u64).to_be_bytes(),
            encoded,
            payload,
        ])
    }

    pub(crate) fn tag(&self, path: &Path, payload: &[u8]) -> String {
        format!("hmac3:{}", self.bound_mac(path, payload))
    }

    pub(crate) fn verify(&self, path: &Path, stored: &[u8], payload: &[u8]) -> Result<u32> {
        if let Some(stored) = stored.strip_prefix(b"hmac3:") {
            ensure!(
                bool::from(stored.ct_eq(self.bound_mac(path, payload).as_bytes())),
                "journal authentication failed for this database path"
            );
            return Ok(3);
        }
        if let Some(stored) = stored.strip_prefix(b"hmac:") {
            ensure!(
                std::env::var_os("JSQNL_ALLOW_LEGACY_AUTH").is_some_and(|value| value == "1"),
                "legacy authenticated journal requires JSQNL_ALLOW_LEGACY_AUTH=1 for read-only access or FORK"
            );
            ensure!(
                bool::from(stored.ct_eq(self.mac(&[payload]).as_bytes())),
                "journal authentication failed"
            );
            return Ok(2);
        }
        anyhow::bail!("authenticated journal required; legacy frames cannot be mixed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interoperability_vector() {
        let key = JournalKey(Zeroizing::new([0x0b; 32]));
        assert_eq!(
            key.mac(&[b"Hi There"]),
            "198a607eb44bfbc69903a0f1cf2bbdc5ba0aa3f3d9ae3c1c7a3b1696a0b68cf7"
        );
    }
}
