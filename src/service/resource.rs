use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use chacha20poly1305::{
    Key, XChaCha20Poly1305, XNonce,
    aead::{Aead, Generate, KeyInit, Payload},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Clone)]
pub struct ResourceCipher {
    key: [u8; 32],
}

impl ResourceCipher {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    pub fn from_hex(encoded: &str) -> Result<Self, String> {
        if encoded.len() != 64 {
            return Err(
                "LUX_MASTER_KEY_HEX must contain exactly 64 hexadecimal digits".to_string(),
            );
        }
        let mut key = [0u8; 32];
        for (index, byte) in key.iter_mut().enumerate() {
            let offset = index * 2;
            *byte = u8::from_str_radix(&encoded[offset..offset + 2], 16)
                .map_err(|_| "LUX_MASTER_KEY_HEX contains a non-hexadecimal digit".to_string())?;
        }
        Ok(Self::new(key))
    }

    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        match read_key_file(path) {
            Ok(cipher) => return Ok(cipher),
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(format!(
                    "could not read resource key {}: {error}",
                    path.display()
                ));
            }
            Err(_) => {}
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "could not create resource key directory {}: {error}",
                    parent.display()
                )
            })?;
        }

        let generated = Key::generate();
        let mut key = [0_u8; 32];
        key.copy_from_slice(&generated);
        let cipher = Self::new(key);

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(cipher.key_hex().as_bytes())
                    .map_err(|error| {
                        format!("could not write resource key {}: {error}", path.display())
                    })?;
                file.sync_all().map_err(|error| {
                    format!("could not sync resource key {}: {error}", path.display())
                })?;
                Ok(cipher)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_key_file(path)
                .map_err(|error| {
                    format!(
                        "could not read concurrently created resource key {}: {error}",
                        path.display()
                    )
                }),
            Err(error) => Err(format!(
                "could not create resource key {}: {error}",
                path.display()
            )),
        }
    }

    pub(crate) fn key_hex(&self) -> String {
        self.key.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    pub fn encrypt(
        &self,
        namespace: &str,
        name: &str,
        kind: &str,
        plaintext: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), String> {
        let cipher = XChaCha20Poly1305::new_from_slice(&self.key)
            .map_err(|_| "invalid resource encryption key".to_string())?;
        let nonce = XNonce::generate();
        let aad = associated_data(namespace, name, kind);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| "resource encryption failed".to_string())?;
        Ok((nonce.as_slice().to_vec(), ciphertext))
    }

    pub fn decrypt(
        &self,
        namespace: &str,
        name: &str,
        kind: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, String> {
        let cipher = XChaCha20Poly1305::new_from_slice(&self.key)
            .map_err(|_| "invalid resource encryption key".to_string())?;
        let nonce =
            XNonce::try_from(nonce).map_err(|_| "invalid resource nonce length".to_string())?;
        let aad = associated_data(namespace, name, kind);
        cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| "resource authentication failed".to_string())
    }
}

fn read_key_file(path: &Path) -> Result<ResourceCipher, io::Error> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "resource key path is not a regular file",
        ));
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "resource key must not be accessible by group or other users",
        ));
    }
    let encoded = fs::read_to_string(path)?;
    ResourceCipher::from_hex(encoded.trim())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn associated_data(namespace: &str, name: &str, kind: &str) -> Vec<u8> {
    let mut data = b"lux-resource-v1\0".to_vec();
    for value in [namespace, name, kind] {
        data.extend_from_slice(&(value.len() as u64).to_be_bytes());
        data.extend_from_slice(value.as_bytes());
    }
    data
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::ResourceCipher;

    #[test]
    fn resource_encryption_authenticates_identity_and_payload() {
        let cipher = ResourceCipher::new([7; 32]);
        let plaintext = br#"{"secret":"value"}"#;
        let (nonce, ciphertext) = cipher
            .encrypt("demo", "account", "secret/json", plaintext)
            .unwrap();
        assert_ne!(ciphertext, plaintext);
        assert_eq!(
            cipher
                .decrypt("demo", "account", "secret/json", &nonce, &ciphertext)
                .unwrap(),
            plaintext
        );
        assert!(
            cipher
                .decrypt("other", "account", "secret/json", &nonce, &ciphertext)
                .is_err()
        );
    }

    #[test]
    fn resource_key_file_is_restart_stable_and_private() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("resource-key.hex");
        let first = ResourceCipher::load_or_create(&path).unwrap();
        let second = ResourceCipher::load_or_create(&path).unwrap();
        assert_eq!(first.key_hex(), second.key_hex());
        assert_eq!(fs::read_to_string(&path).unwrap().len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
        }
    }
}
