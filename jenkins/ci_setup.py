#!/usr/bin/env python3

# Copyright 2026 The Servo Project Developers. See the COPYRIGHT
# file at the top-level directory of this distribution.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

# Jenkins counterpart of .github/actions/setup-dav1d: install isolated Meson tools, build the
# pinned static dav1d via etc/ci/setup_dav1d.py, and write the resulting Cargo environment to
# an env file (KEY=VALUE per line) that later pipeline steps load. setup_dav1d.py already
# appends to $GITHUB_ENV when it is set, so we point that at our env file.
#
# Usage: python3 jenkins/ci_setup.py [--target native|<android/ohos triple>] [--env-file .ci-env]

import argparse
import os
from pathlib import Path
import subprocess
import sys
import venv

MESON = "meson==1.9.2"
NINJA = "ninja==1.13.0"
PKGCONF = "pkgconf==2.5.1.post1"  # Windows only, as in the GitHub action


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default="native")
    parser.add_argument("--env-file", type=Path, default=Path(".ci-env"))
    parser.add_argument("--work-dir", type=Path, default=Path(".ci"))
    args = parser.parse_args()

    work_dir = args.work_dir.resolve()
    env_file = args.env_file.resolve()
    tools_venv = work_dir / "dav1d-tools"
    windows = os.name == "nt"
    tools = tools_venv / ("Scripts" if windows else "bin")
    python = tools / ("python.exe" if windows else "python")

    if not python.exists():
        venv.create(tools_venv, with_pip=True)
    packages = [MESON, NINJA] + ([PKGCONF] if windows else [])
    subprocess.run([str(python), "-m", "pip", "install", "--disable-pip-version-check", "-q", *packages], check=True)

    env = os.environ.copy()
    env["PATH"] = str(tools) + os.pathsep + env.get("PATH", "")
    env["GITHUB_ENV"] = str(env_file)
    env.pop("GITHUB_OUTPUT", None)
    env.setdefault("MACOSX_DEPLOYMENT_TARGET", "13.0")

    # Each target gets a fresh env file section; drop entries from a previous run of this target.
    env_file.write_text("")
    subprocess.run(
        [
            sys.executable,
            "etc/ci/setup_dav1d.py",
            "--target",
            args.target,
            "--work-dir",
            str(work_dir / "dav1d"),
        ],
        env=env,
        check=True,
    )
    with env_file.open("a", encoding="utf-8") as output:
        output.write(f"PATH={env['PATH']}\n")


if __name__ == "__main__":
    main()
