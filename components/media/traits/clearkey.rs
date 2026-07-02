/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Clear Key (W3C EME) support shared between the DOM and the media backend.
//!
//! `MediaKeySession.update()` (script thread) publishes decoded keys into the process-global
//! store here; the GStreamer CENC decrypt element (media backend thread) looks keys up by key id
//! and decrypts samples with [`decrypt_subsamples`]. This is the seam that lets a JS-supplied
//! Clear Key license drive in-pipeline decryption without any external CDM.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use aes::Aes128;
use aes::cipher::{KeyIvInit, StreamCipher};
use ctr::Ctr128BE;

/// CENC `cenc` scheme = AES-128 in CTR mode with a big-endian 128-bit counter.
type Aes128Ctr = Ctr128BE<Aes128>;

/// keyId (16 bytes) → key (16 bytes).
static CLEAR_KEYS: LazyLock<RwLock<HashMap<Vec<u8>, Vec<u8>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Publish a Clear Key (called by `MediaKeySession.update`).
pub fn insert_key(key_id: Vec<u8>, key: Vec<u8>) {
    CLEAR_KEYS.write().unwrap().insert(key_id, key);
}

/// Look up a key by its id (called by the CENC decrypt element).
pub fn get_key(key_id: &[u8]) -> Option<Vec<u8>> {
    CLEAR_KEYS.read().unwrap().get(key_id).cloned()
}

/// Forget a session's keys (called on `MediaKeySession.close`).
pub fn remove_keys(key_ids: &[Vec<u8>]) {
    let mut store = CLEAR_KEYS.write().unwrap();
    for id in key_ids {
        store.remove(id);
    }
}

/// A CENC subsample layout entry: `clear` bytes pass through untouched, then `encrypted` bytes are
/// decrypted, with the CTR keystream continuing across the encrypted spans of the sample.
#[derive(Clone, Copy, Debug)]
pub struct Subsample {
    pub clear: usize,
    pub encrypted: usize,
}

fn iv_block(iv: &[u8]) -> [u8; 16] {
    // CENC IVs are 8 or 16 bytes; an 8-byte IV forms the high half of the 128-bit counter.
    let mut block = [0u8; 16];
    let n = iv.len().min(16);
    block[..n].copy_from_slice(&iv[..n]);
    block
}

/// Decrypt an AES-128-CTR (`cenc`) sample in place. With no subsamples the whole buffer is
/// encrypted; otherwise only the encrypted spans are, keeping one keystream across the sample
/// (per ISO/IEC 23001-7).
pub fn decrypt_subsamples(key: &[u8; 16], iv: &[u8], data: &mut [u8], subsamples: &[Subsample]) {
    let block = iv_block(iv);
    let mut cipher = Aes128Ctr::new(key.into(), (&block).into());

    if subsamples.is_empty() {
        cipher.apply_keystream(data);
        return;
    }

    let mut offset = 0usize;
    for ss in subsamples {
        offset = (offset + ss.clear).min(data.len());
        let end = (offset + ss.encrypted).min(data.len());
        if offset < end {
            cipher.apply_keystream(&mut data[offset..end]);
        }
        offset = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST SP 800-38A F.5 AES-128-CTR test vector (first block). CTR is symmetric, so decrypting
    /// the reference ciphertext must reproduce the reference plaintext — this catches key/IV/
    /// counter wiring mistakes.
    #[test]
    fn aes128_ctr_nist_vector() {
        let key: [u8; 16] = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let iv: [u8; 16] = [
            0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd,
            0xfe, 0xff,
        ];
        let plaintext: [u8; 16] = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        let mut buf: [u8; 16] = [
            0x87, 0x4d, 0x61, 0x91, 0xb6, 0x20, 0xe3, 0x26, 0x1b, 0xef, 0x68, 0x64, 0x99, 0x0d,
            0xb6, 0xce,
        ];
        decrypt_subsamples(&key, &iv, &mut buf, &[]);
        assert_eq!(buf, plaintext);
    }

    #[test]
    fn store_round_trip_and_subsamples() {
        insert_key(b"0123456789abcdef".to_vec(), b"fedcba9876543210".to_vec());
        assert_eq!(
            get_key(b"0123456789abcdef").as_deref(),
            Some(&b"fedcba9876543210"[..])
        );
        // A clear header followed by an encrypted span: the header must be untouched.
        let key = [0u8; 16];
        let iv = [0u8; 16];
        let mut data = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let before_clear = data[..4].to_vec();
        decrypt_subsamples(
            &key,
            &iv,
            &mut data,
            &[Subsample {
                clear: 4,
                encrypted: 4,
            }],
        );
        assert_eq!(&data[..4], &before_clear[..]);
    }
}
