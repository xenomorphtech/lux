#[allow(unused_imports)]
use crate::prelude::*;
use sha2::{Digest, Sha256};

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

pub fn hash_str(input: &str) -> String {
    hash_bytes(input.as_bytes())
}

pub fn hash_tagged(tag: &str, parts: &[&str]) -> String {
    let mut material = String::new();
    material.push_str(&format!("{tag}:{}", tag.len()));
    for part in parts {
        material.push('|');
        material.push_str(&part.len().to_string());
        material.push(':');
        material.push_str(part);
    }
    hash_str(&material)
}
