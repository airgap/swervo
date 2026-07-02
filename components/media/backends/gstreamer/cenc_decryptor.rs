/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A GStreamer element that decrypts Common Encryption (`cenc`) content using Clear Key keys
//! supplied by the EME `MediaKeySession` (via `servo_media_traits::clearkey`). It sinks
//! `application/x-cenc`, reads the per-sample `GstProtectionMeta` (key id / IV / subsamples) that
//! the demuxer attaches, AES-128-CTR decrypts in place, and outputs the original (cleartext) media
//! type so the decoder can be autoplugged after it.

use std::sync::LazyLock;

use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base::BaseTransform;
use gstreamer_base::subclass::BaseTransformMode;
use gstreamer_base::subclass::prelude::*;
use servo_media_traits::clearkey;

/// Clear Key protection-system UUID (org.w3.clearkey).
const CLEARKEY_SYSTEM_ID: &str = "1077efec-c0b2-4d02-ace3-3c1e52e2fb4b";

mod imp {
    use super::*;

    static CAT: LazyLock<gstreamer::DebugCategory> = LazyLock::new(|| {
        gstreamer::DebugCategory::new(
            "servocenc",
            gstreamer::DebugColorFlags::empty(),
            Some("Servo Clear Key CENC decryptor"),
        )
    });

    #[derive(Default)]
    pub struct ServoCencDecrypt;

    #[glib::object_subclass]
    impl ObjectSubclass for ServoCencDecrypt {
        const NAME: &'static str = "ServoCencDecrypt";
        type Type = super::ServoCencDecrypt;
        type ParentType = BaseTransform;
    }

    impl ObjectImpl for ServoCencDecrypt {}
    impl GstObjectImpl for ServoCencDecrypt {}

    impl ElementImpl for ServoCencDecrypt {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static METADATA: LazyLock<gstreamer::subclass::ElementMetadata> = LazyLock::new(|| {
                gstreamer::subclass::ElementMetadata::new(
                    "Servo Clear Key CENC decryptor",
                    "Generic/Decryptor",
                    "Decrypts CENC content with Clear Key keys from an EME MediaKeySession",
                    "Servo",
                )
            });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                // Declare the Clear Key protection system so the demuxer's decryptor search
                // matches us for system 1077efec (an unadorned application/x-cenc is not treated
                // as supporting any specific system). Accept both cenc and cbcs schemes.
                let sink_caps = gstreamer::Caps::builder("application/x-cenc")
                    .field("protection-system", CLEARKEY_SYSTEM_ID)
                    .build();
                let src_caps = gstreamer::Caps::new_any();
                vec![
                    gstreamer::PadTemplate::new(
                        "sink",
                        gstreamer::PadDirection::Sink,
                        gstreamer::PadPresence::Always,
                        &sink_caps,
                    )
                    .unwrap(),
                    gstreamer::PadTemplate::new(
                        "src",
                        gstreamer::PadDirection::Src,
                        gstreamer::PadPresence::Always,
                        &src_caps,
                    )
                    .unwrap(),
                ]
            });
            TEMPLATES.as_ref()
        }
    }

    impl BaseTransformImpl for ServoCencDecrypt {
        const MODE: BaseTransformMode = BaseTransformMode::AlwaysInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        /// Answer the demuxer's `drm-preferred-decryption-system-id` context query with Clear Key,
        /// so qtdemux/matroskademux select the Clear Key pssh and attach the per-sample crypto
        /// info (without this, qtdemux "fails to attach cenc metadata" and no decryption happens).
        fn query(&self, direction: gstreamer::PadDirection, query: &mut gstreamer::QueryRef) -> bool {
            if let gstreamer::QueryViewMut::Context(q) = query.view_mut() &&
                q.context_type() == "drm-preferred-decryption-system-id"
            {
                let mut context =
                    gstreamer::Context::new("drm-preferred-decryption-system-id", false);
                context
                    .get_mut()
                    .unwrap()
                    .structure_mut()
                    .set("decryption-system-id", CLEARKEY_SYSTEM_ID);
                q.set_context(&context);
                gstreamer::debug!(CAT, "answered drm-preferred-decryption-system-id -> clearkey");
                return true;
            }
            BaseTransformImplExt::parent_query(self, direction, query)
        }

        /// Sink `application/x-cenc, original-media-type=X, …` ⇄ src `X`.
        fn transform_caps(
            &self,
            direction: gstreamer::PadDirection,
            caps: &gstreamer::Caps,
            filter: Option<&gstreamer::Caps>,
        ) -> Option<gstreamer::Caps> {
            let mut out = gstreamer::Caps::new_empty();
            {
                let out = out.get_mut().unwrap();
                for structure in caps.iter() {
                    let mut new_structure = structure.to_owned();
                    if direction == gstreamer::PadDirection::Sink {
                        // Unwrap: application/x-cenc + original-media-type=X -> X.
                        if let Ok(media_type) = structure.get::<String>("original-media-type") {
                            new_structure.set_name(&media_type);
                        }
                        for field in [
                            "protection-system",
                            "original-media-type",
                            "encryption-scheme",
                            "cipher-mode",
                        ] {
                            new_structure.remove_field(field);
                        }
                    } else {
                        // Wrap: X -> application/x-cenc + original-media-type=X.
                        let media_type = new_structure.name().to_string();
                        new_structure.set_name("application/x-cenc");
                        new_structure.set("original-media-type", media_type);
                        new_structure.set("protection-system", CLEARKEY_SYSTEM_ID);
                    }
                    out.append_structure(new_structure);
                }
            }
            if let Some(filter) = filter {
                out = out.intersect_with_mode(filter, gstreamer::CapsIntersectMode::First);
            }
            Some(out)
        }

        fn set_caps(
            &self,
            incaps: &gstreamer::Caps,
            outcaps: &gstreamer::Caps,
        ) -> Result<(), gstreamer::LoggableError> {
            gstreamer::debug!(CAT, "set_caps in={incaps} out={outcaps}");
            Ok(())
        }

        fn transform_ip(
            &self,
            buffer: &mut gstreamer::BufferRef,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            // Pull the per-sample crypto info out of the protection meta.
            let (key_id, iv, subsamples) = {
                let Some(meta) = buffer.meta::<gstreamer::meta::ProtectionMeta>() else {
                    // No protection meta: already clear, pass through.
                    return Ok(gstreamer::FlowSuccess::Ok);
                };
                let info = meta.info();
                let key_id = info
                    .get::<gstreamer::Buffer>("kid")
                    .ok()
                    .and_then(|b| b.map_readable().ok().map(|m| m.to_vec()));
                let iv = info
                    .get::<gstreamer::Buffer>("iv")
                    .ok()
                    .and_then(|b| b.map_readable().ok().map(|m| m.to_vec()));
                let subsample_count = info.get::<u32>("subsample_count").unwrap_or(0);
                let subsamples = info
                    .get::<gstreamer::Buffer>("subsamples")
                    .ok()
                    .and_then(|b| b.map_readable().ok().map(|m| m.to_vec()));
                (key_id, iv, parse_subsamples(subsamples, subsample_count))
            };

            let (Some(key_id), Some(iv)) = (key_id, iv) else {
                gstreamer::warning!(CAT, "protected buffer without kid/iv");
                return Err(gstreamer::FlowError::NotSupported);
            };
            // The key may still be in flight: an EME player sets it up asynchronously in response
            // to the `encrypted` event we fired. Wait for it (bounded), rather than erroring —
            // this is the "waiting for key" state the spec describes.
            let key = {
                let mut key = clearkey::get_key(&key_id);
                let mut waited_ms = 0u32;
                while key.is_none() && waited_ms < 5000 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    waited_ms += 20;
                    key = clearkey::get_key(&key_id);
                }
                match key {
                    Some(key) => key,
                    None => {
                        gstreamer::warning!(CAT, "no Clear Key for kid {key_id:02x?} after wait");
                        return Err(gstreamer::FlowError::NotSupported);
                    },
                }
            };
            let mut key16 = [0u8; 16];
            key16[..key.len().min(16)].copy_from_slice(&key[..key.len().min(16)]);

            // Copy the sample out, decrypt, and write it back with copy_from_slice (gst_buffer_fill).
            // NB: map_writable() can hand back a *merged copy* for a multi-memory buffer, so
            // in-place writes to it silently do not reach downstream — copy_from_slice does.
            let mut data = {
                let map = buffer.map_readable().map_err(|_| gstreamer::FlowError::Error)?;
                map.to_vec()
            };
            clearkey::decrypt_subsamples(&key16, &iv, &mut data, &subsamples);
            buffer
                .copy_from_slice(0, &data)
                .map_err(|_| gstreamer::FlowError::Error)?;

            // Drop the protection meta so downstream treats the buffer as clear.
            while let Some(mut meta) = buffer.meta_mut::<gstreamer::meta::ProtectionMeta>() {
                let _ = meta.remove();
            }
            Ok(gstreamer::FlowSuccess::Ok)
        }
    }

    /// Parse the CENC subsample table: `subsample_count` × { u16 clear, u32 encrypted } big-endian.
    fn parse_subsamples(bytes: Option<Vec<u8>>, count: u32) -> Vec<clearkey::Subsample> {
        let mut out = Vec::new();
        let Some(bytes) = bytes else {
            return out;
        };
        let mut off = 0usize;
        for _ in 0..count {
            if off + 6 > bytes.len() {
                break;
            }
            let clear = u16::from_be_bytes([bytes[off], bytes[off + 1]]) as usize;
            let encrypted = u32::from_be_bytes([
                bytes[off + 2],
                bytes[off + 3],
                bytes[off + 4],
                bytes[off + 5],
            ]) as usize;
            out.push(clearkey::Subsample { clear, encrypted });
            off += 6;
        }
        out
    }
}

glib::wrapper! {
    pub struct ServoCencDecrypt(ObjectSubclass<imp::ServoCencDecrypt>)
        @extends BaseTransform, gstreamer::Element, gstreamer::Object;
}

unsafe impl Send for ServoCencDecrypt {}
unsafe impl Sync for ServoCencDecrypt {}

/// Register the CENC decryptor with a high rank so decodebin autoplugs it for encrypted content.
pub fn register_cenc_decryptor() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servocencdecrypt",
        gstreamer::Rank::PRIMARY + 100,
        ServoCencDecrypt::static_type(),
    )
}
