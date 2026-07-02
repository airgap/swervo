/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Encrypted Media Extensions `MediaKeySession` — brick 2: Clear Key session logic.
//!
//! `generateRequest` records the requested key ids (a `message` event carrying the license
//! request is a follow-up — the `MediaKeyMessageEvent`/ArrayBuffer plumbing); `update` parses a
//! Clear Key JWK license, decodes the keys, stores them, and fires `keystatuseschange`. The stored
//! keys are what the CENC decrypt element (brick 3) will consume. `keyStatuses` (maplike) is a
//! follow-up; the key store is exposed internally via `key_for`.

use std::collections::HashMap;
use std::ffi::CString;
use std::rc::Rc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::MediaKeySessionBinding::MediaKeySessionMethods;
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::Promise;
use crate::script_runtime::CanGc;

#[dom_struct]
pub(crate) struct MediaKeySession {
    eventtarget: EventTarget,
    session_id: DomRefCell<DOMString>,
    /// Key ids requested via `generateRequest` (Clear Key request contents).
    key_ids: DomRefCell<Vec<Vec<u8>>>,
    /// keyId → key material, populated by `update` from the Clear Key JWK license.
    keys: DomRefCell<HashMap<Vec<u8>, Vec<u8>>>,
}

impl MediaKeySession {
    fn new_inherited() -> MediaKeySession {
        MediaKeySession {
            eventtarget: EventTarget::new_inherited(),
            session_id: DomRefCell::new(DOMString::new()),
            key_ids: DomRefCell::new(Vec::new()),
            keys: DomRefCell::new(HashMap::new()),
        }
    }

    pub(crate) fn new(global: &GlobalScope, can_gc: CanGc) -> DomRoot<MediaKeySession> {
        reflect_dom_object(Box::new(MediaKeySession::new_inherited()), global, can_gc)
    }

    /// The key material for a given key id, if this session holds it (used by the CENC decrypt
    /// element in brick 3).
    #[allow(dead_code)]
    pub(crate) fn key_for(&self, key_id: &[u8]) -> Option<Vec<u8>> {
        self.keys.borrow().get(key_id).cloned()
    }
}

impl MediaKeySessionMethods<crate::DomTypeHolder> for MediaKeySession {
    fn SessionId(&self) -> DOMString {
        self.session_id.borrow().clone()
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-generaterequest>
    /// Extracts the requested key ids from the init data. Firing the `message` event with the
    /// Clear Key license request is a follow-up (needs MediaKeyMessageEvent).
    fn GenerateRequest(
        &self,
        cx: &mut JSContext,
        init_data_type: DOMString,
        init_data: ArrayBufferViewOrArrayBuffer,
    ) -> Rc<Promise> {
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);

        let data = match init_data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(v) => v.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(b) => b.to_vec(),
        };
        let key_ids: Vec<Vec<u8>> = match init_data_type.to_string().as_str() {
            // "keyids": init data is JSON `{"kids":["base64url", ...]}`.
            "keyids" => serde_json::from_slice::<serde_json::Value>(&data)
                .ok()
                .and_then(|json| {
                    json.get("kids").and_then(|k| k.as_array()).map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .filter_map(|s| URL_SAFE_NO_PAD.decode(s).ok())
                            .collect()
                    })
                })
                .unwrap_or_default(),
            // "webm": init data is a single raw key id.
            "webm" => vec![data],
            // "cenc": init data is one or more `pssh` boxes; extract the key ids.
            "cenc" => parse_pssh_key_ids(&data),
            _ => Vec::new(),
        };
        *self.key_ids.borrow_mut() = key_ids;

        // TODO(brick 2b): fire a `message` MediaKeyMessageEvent carrying the Clear Key license
        // request `{"kids":[...],"type":"temporary"}` so unmodified Clear Key players work.
        promise.resolve_native_with_cx(cx, &());
        promise
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-update>
    /// Parses a Clear Key JWK license (`{"keys":[{"kty":"oct","kid":..,"k":..}]}`), stores the
    /// keys, and fires `keystatuseschange`.
    fn Update(&self, cx: &mut JSContext, response: ArrayBufferViewOrArrayBuffer) -> Rc<Promise> {
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);

        let data = match response {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(v) => v.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(b) => b.to_vec(),
        };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&data) else {
            promise
                .reject_error(cx, Error::Type(CString::new("invalid Clear Key license").unwrap()));
            return promise;
        };
        let mut stored = 0usize;
        if let Some(keys) = json.get("keys").and_then(|k| k.as_array()) {
            let mut store = self.keys.borrow_mut();
            for key in keys {
                let kid = key.get("kid").and_then(|v| v.as_str());
                let k = key.get("k").and_then(|v| v.as_str());
                if let (Some(kid), Some(k)) = (kid, k) &&
                    let (Ok(kid_bytes), Ok(k_bytes)) =
                        (URL_SAFE_NO_PAD.decode(kid), URL_SAFE_NO_PAD.decode(k))
                {
                    // Publish to the process-global Clear Key store the CENC decrypt element reads.
                    servo_media::clearkey::insert_key(kid_bytes.clone(), k_bytes.clone());
                    store.insert(kid_bytes, k_bytes);
                    stored += 1;
                }
            }
        }
        if stored == 0 {
            promise.reject_error(
                cx,
                Error::Type(CString::new("no usable keys in license").unwrap()),
            );
            return promise;
        }

        self.upcast::<EventTarget>()
            .fire_event(cx, Atom::from("keystatuseschange"));
        promise.resolve_native_with_cx(cx, &());
        promise
    }

    /// <https://w3c.github.io/encrypted-media/#dom-mediakeysession-close>
    fn Close(&self, cx: &mut JSContext) -> Rc<Promise> {
        let mut realm = CurrentRealm::assert(cx);
        let promise = Promise::new_in_realm(&mut realm);
        // Drop this session's keys from the global Clear Key store, then the local copy.
        let key_ids: Vec<Vec<u8>> = self.keys.borrow().keys().cloned().collect();
        servo_media::clearkey::remove_keys(&key_ids);
        self.keys.borrow_mut().clear();
        promise.resolve_native_with_cx(cx, &());
        promise
    }
}

/// Extract the key ids from one or more concatenated `pssh` boxes (ISO/IEC 23001-7). Handles a
/// version-1 pssh (explicit KID list) and a version-0 Clear Key pssh (data = concatenated key ids).
fn parse_pssh_key_ids(data: &[u8]) -> Vec<Vec<u8>> {
    let mut key_ids = Vec::new();
    let mut pos = 0usize;
    while pos + 32 <= data.len() {
        let box_size = u32::from_be_bytes([
            data[pos],
            data[pos + 1],
            data[pos + 2],
            data[pos + 3],
        ]) as usize;
        if &data[pos + 4..pos + 8] != b"pssh" || box_size < 32 {
            break;
        }
        let box_end = (pos + box_size).min(data.len());
        let version = data[pos + 8];
        // 4 size + 4 'pssh' + 4 version/flags + 16 system id.
        let mut off = pos + 28;
        if version >= 1 {
            if off + 4 <= box_end {
                let kid_count = u32::from_be_bytes([
                    data[off],
                    data[off + 1],
                    data[off + 2],
                    data[off + 3],
                ]) as usize;
                off += 4;
                for _ in 0..kid_count {
                    if off + 16 > box_end {
                        break;
                    }
                    key_ids.push(data[off..off + 16].to_vec());
                    off += 16;
                }
            }
        } else if off + 4 <= box_end {
            // Version 0: [u32 data_size][data]; Clear Key data is concatenated 16-byte key ids.
            let data_size = u32::from_be_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3],
            ]) as usize;
            off += 4;
            let data_end = (off + data_size).min(box_end);
            while off + 16 <= data_end {
                key_ids.push(data[off..off + 16].to_vec());
                off += 16;
            }
        }
        pos = box_end;
    }
    key_ids
}
