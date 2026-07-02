/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/encrypted-media/

enum MediaKeysRequirement {
  "required",
  "optional",
  "not-allowed"
};

enum MediaKeySessionType {
  "temporary",
  "persistent-license"
};

enum MediaKeyStatus {
  "usable",
  "expired",
  "released",
  "output-restricted",
  "output-downscaled",
  "status-pending",
  "internal-error"
};

enum MediaKeyMessageType {
  "license-request",
  "license-renewal",
  "license-release",
  "individualization-request"
};

dictionary MediaKeySystemMediaCapability {
  DOMString contentType = "";
  DOMString? encryptionScheme = null;
  DOMString robustness = "";
};

dictionary MediaKeySystemConfiguration {
  DOMString label = "";
  sequence<DOMString> initDataTypes = [];
  sequence<MediaKeySystemMediaCapability> audioCapabilities = [];
  sequence<MediaKeySystemMediaCapability> videoCapabilities = [];
  MediaKeysRequirement distinctiveIdentifier = "optional";
  MediaKeysRequirement persistentState = "optional";
  sequence<MediaKeySessionType> sessionTypes = [];
};

[Pref="dom_eme_enabled", Exposed=Window]
interface MediaKeySystemAccess {
  readonly attribute DOMString keySystem;
  // MediaKeySystemConfiguration getConfiguration();  // brick 1b (dictionary return)
  Promise<MediaKeys> createMediaKeys();
};

partial interface Navigator {
  [Pref="dom_eme_enabled"]
  Promise<MediaKeySystemAccess> requestMediaKeySystemAccess(
      DOMString keySystem,
      sequence<MediaKeySystemConfiguration> supportedConfigurations);
};

partial interface HTMLMediaElement {
  [Pref="dom_eme_enabled"] readonly attribute MediaKeys? mediaKeys;
  [Pref="dom_eme_enabled"] Promise<undefined> setMediaKeys(MediaKeys? mediaKeys);
};
