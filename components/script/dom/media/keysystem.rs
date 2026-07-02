/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pluggable EME key-system dispatch.
//!
//! `Navigator.requestMediaKeySystemAccess` consults [`evaluate`] instead of hard-coding a single
//! key system. Clear Key is fully implemented in-process (no external CDM). Proprietary systems
//! (Widevine, PlayReady, FairPlay) are *recognized* here so the plumbing is ready, but report
//! [`KeySystemSupport::NeedsCdm`] until a Content Decryption Module is hosted — that CDM host
//! (loading e.g. `libwidevinecdm.so` and speaking the CDM protocol) is the remaining, legally
//! gated work tracked in LYK-1364.

/// The Clear Key key system, fully supported in-process.
pub(crate) const CLEAR_KEY: &str = "org.w3.clearkey";

/// Recognized proprietary key systems, each needing an external CDM host that is not yet bundled.
pub(crate) const WIDEVINE: &str = "com.widevine.alpha";
pub(crate) const PLAYREADY: &str = "com.microsoft.playready";
pub(crate) const PLAYREADY_RECOMMENDATION: &str = "com.microsoft.playready.recommendation";
pub(crate) const FAIRPLAY: &str = "com.apple.fps";

/// How well this build can service a given key system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeySystemSupport {
    /// Fully implemented in-process (Clear Key).
    Available,
    /// A recognized DRM whose CDM host is not present in this build. The pluggable seam for a real
    /// CDM lives here — until then, access is rejected (per spec, as if unsupported).
    NeedsCdm,
    /// Unknown / unsupported key system.
    Unsupported,
}

/// Classify a key-system string. This is the single place to extend when a CDM host lands.
pub(crate) fn evaluate(key_system: &str) -> KeySystemSupport {
    match key_system {
        CLEAR_KEY => KeySystemSupport::Available,
        WIDEVINE | PLAYREADY | PLAYREADY_RECOMMENDATION | FAIRPLAY => KeySystemSupport::NeedsCdm,
        _ => KeySystemSupport::Unsupported,
    }
}
