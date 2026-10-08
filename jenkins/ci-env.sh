#!/usr/bin/env bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Sourced by every Unix shell step in the Jenkinsfile (`source jenkins/ci-env.sh`).
#
# Jenkins `sh` is a non-login shell, so it does not pick up the profile that puts rustup,
# uv or Homebrew on PATH. Put them there ourselves, then load .ci-env (written by
# jenkins/ci_setup.py: dav1d's Cargo environment plus the Meson tools on PATH), which plays
# the part of $GITHUB_ENV in the old GitHub Actions workflows.

set -o errexit
set -o nounset
set -o pipefail

export PATH="${HOME}/.cargo/bin:${HOME}/.local/bin:/opt/homebrew/bin:/usr/local/bin:${PATH}"
if [[ -f "${HOME}/.cargo/env" ]]; then
    # shellcheck source=/dev/null
    source "${HOME}/.cargo/env"
fi

export RUST_BACKTRACE=1
export SHELL=/bin/bash

# bindgen (mozangle, mozjs) needs libclang; GitHub's Ubuntu image used LLVM 14.
if [[ -z "${LIBCLANG_PATH:-}" && "$(uname -s)" == Linux ]]; then
    for dir in /usr/lib/llvm-18/lib /usr/lib/llvm-14/lib; do
        if [[ -d "${dir}" ]]; then
            export LIBCLANG_PATH="${dir}"
            break
        fi
    done
fi

if [[ -f .ci-env ]]; then
    while IFS='=' read -r key value; do
        # Skip names that aren't valid shell variables, such as the hyphenated
        # PKG_CONFIG_<target-triple> key; its underscore alias is exported instead.
        case "${key}" in
            "" | [0-9]* | *[!A-Za-z0-9_]*) continue ;;
        esac
        export "${key}=${value}"
    done < .ci-env
fi
