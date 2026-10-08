#!/usr/bin/env bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Verify the Rust bindings link the static dav1d built by jenkins/ci_setup.py
# (the "Verify Rust bindings link the static decoder" step of the old dav1d.yml).

set -o errexit
set -o nounset
set -o pipefail

dir="$(mktemp -d)"
trap 'rm -rf "${dir}"' EXIT
cd "${dir}"
cargo init --quiet --name dav1d-smoke --bin
printf '\n[dependencies.dav1d-sys]\nversion = "=0.8.3"\n' >> Cargo.toml
cat > src/main.rs <<'RS'
fn main() {
    unsafe {
        let mut settings = std::mem::MaybeUninit::uninit();
        dav1d_sys::dav1d_default_settings(settings.as_mut_ptr());
        let mut context = std::ptr::null_mut();
        assert_eq!(dav1d_sys::dav1d_open(&mut context, settings.as_ptr()), 0);
        dav1d_sys::dav1d_close(&mut context);
    }
}
RS
cargo update --quiet -p system-deps --precise 7.0.8
cargo run --locked
