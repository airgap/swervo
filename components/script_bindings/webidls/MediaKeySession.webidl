/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/encrypted-media/#mediakeysession-interface

[Pref="dom_eme_enabled", Exposed=Window]
interface MediaKeySession : EventTarget {
  readonly attribute DOMString sessionId;
  // readonly attribute unrestricted double expiration;         // brick 2
  // readonly attribute Promise<undefined> closed;              // brick 2
  // readonly attribute MediaKeyStatusMap keyStatuses;          // brick 2 (maplike)
  // attribute EventHandler onkeystatuseschange;                // brick 2
  // attribute EventHandler onmessage;                          // brick 2
  Promise<undefined> generateRequest(DOMString initDataType, BufferSource initData);
  Promise<undefined> update(BufferSource response);
  Promise<undefined> close();
  // Promise<undefined> load(DOMString sessionId);              // persistent sessions (later)
  // Promise<undefined> remove();                               // persistent sessions (later)
};
