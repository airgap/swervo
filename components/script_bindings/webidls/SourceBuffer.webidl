/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/media-source/#sourcebuffer

enum AppendMode {
  "segments",
  "sequence"
};

[Pref="dom_mediasource_enabled", Exposed=Window]
interface SourceBuffer : EventTarget {
  [Throws] attribute AppendMode mode;
  readonly attribute boolean updating;
  [Throws] readonly attribute TimeRanges buffered;
  [Throws] attribute unrestricted double timestampOffset;
  [Throws] attribute unrestricted double appendWindowStart;
  [Throws] attribute unrestricted double appendWindowEnd;
  //   attribute EventHandler onupdatestart;
  //   attribute EventHandler onupdate;
  //   attribute EventHandler onupdateend;
  //   attribute EventHandler onerror;
  //   attribute EventHandler onabort;
  // Phase 2 (append pipeline):
  // [Throws] undefined appendBuffer(BufferSource data);
  // [Throws] undefined abort();
  // [Throws] undefined remove(double start, unrestricted double end);
};
