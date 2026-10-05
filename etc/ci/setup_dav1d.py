#!/usr/bin/env python3

# Copyright 2026 The Servo Project Developers. See the COPYRIGHT
# file at the top-level directory of this distribution.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

"""Build a pinned, static dav1d for image's AVIF decoder and export its CI environment.

Requires meson, ninja, pkg-config, a C toolchain, and nasm for x86 targets.
Cross builds use the same SDK variables and Android API level as mach.
The dav1d-sys internal builder does not configure a cross toolchain.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request


VERSION = "1.5.4"
SOURCE_URL = f"https://download.videolan.org/pub/videolan/dav1d/{VERSION}/dav1d-{VERSION}.tar.xz"
# Published at SOURCE_URL + ".sha256". Update the version and checksum together.
SOURCE_SHA256 = "686616b7c69eb88d44459391ab25cac13b6647a3b288835c5784e71c1514a5c5"
ANDROID_TARGETS = {
    "aarch64-linux-android": ("aarch64", "aarch64", "aarch64-linux-android30"),
    "armv7-linux-androideabi": ("arm", "armv7", "armv7a-linux-androideabi30"),
    "i686-linux-android": ("x86", "i686", "i686-linux-android30"),
    "x86_64-linux-android": ("x86_64", "x86_64", "x86_64-linux-android30"),
}
OHOS_TARGETS = {
    "aarch64-unknown-linux-ohos": ("aarch64", "aarch64"),
    "x86_64-unknown-linux-ohos": ("x86_64", "x86_64"),
}


def run(command: list[str], env: dict[str, str] | None = None) -> None:
    print("+", subprocess.list2cmdline(command), flush=True)
    subprocess.run(command, env=env, check=True)


def require_directory(path: Path) -> Path:
    if not path.is_dir():
        raise RuntimeError(f"Required SDK directory does not exist: {path}")
    return path.resolve()


def sdk_directory(variable: str) -> Path:
    value = os.environ.get(variable)
    if not value:
        raise RuntimeError(f"Set {variable} to the target SDK directory before building dav1d")
    return require_directory(Path(value))


def meson_string(value: str) -> str:
    return "'" + value.replace("\\", "/").replace("'", "\\'") + "'"


def write_cross_file(target: str, destination: Path) -> None:
    if target in ANDROID_TARGETS:
        if sys.platform not in ("linux", "darwin"):
            raise RuntimeError("Android cross builds are supported on Linux and macOS")
        ndk = sdk_directory("ANDROID_NDK_ROOT")
        prebuilt = require_directory(ndk / "toolchains" / "llvm" / "prebuilt")
        host = platform.system().lower()
        candidates = sorted(prebuilt.glob(f"{host}-*"))
        preferred = prebuilt / f"{host}-x86_64"
        if platform.machine().lower() in ("x86_64", "amd64") and preferred.is_dir():
            toolchain = preferred
        elif len(candidates) == 1:
            toolchain = candidates[0]
        else:
            raise RuntimeError(f"Cannot select an Android LLVM toolchain in {prebuilt}")
        cpu_family, cpu, clang_target = ANDROID_TARGETS[target]
        sysroot = require_directory(toolchain / "sysroot")
        system = "android"
        flags = [f"--target={clang_target}", f"--sysroot={sysroot.as_posix()}"]
    elif target in OHOS_TARGETS:
        sdk = sdk_directory("OHOS_SDK_NATIVE")
        toolchain = require_directory(sdk / "llvm")
        sysroot = require_directory(sdk / "sysroot")
        cpu_family, cpu = OHOS_TARGETS[target]
        system = "linux"
        clang_target = target.replace("-unknown-", "-")
        flags = [f"--target={clang_target}", f"--sysroot={sysroot.as_posix()}", "-D__MUSL__"]
    else:
        raise ValueError(f"Unsupported cross target: {target}")

    def executable(name: str) -> str:
        path = toolchain / "bin" / (name + (".exe" if sys.platform == "win32" else ""))
        if not path.is_file():
            raise RuntimeError(f"Required SDK tool does not exist: {path}")
        return meson_string(path.as_posix())

    arguments = "[" + ", ".join(meson_string(flag) for flag in flags) + "]"
    destination.write_text(
        "[binaries]\n"
        f"c = {executable('clang')}\n"
        f"ar = {executable('llvm-ar')}\n"
        f"strip = {executable('llvm-strip')}\n"
        f"pkg-config = {meson_string(shutil.which('pkg-config') or 'pkg-config')}\n"
        "\n[properties]\nneeds_exe_wrapper = true\n"
        "\n[built-in options]\n"
        f"c_args = {arguments}\nc_link_args = {arguments}\n"
        "\n[host_machine]\n"
        f"system = {meson_string(system)}\ncpu_family = {meson_string(cpu_family)}\n"
        f"cpu = {meson_string(cpu)}\nendian = 'little'\n",
        encoding="utf-8",
    )


def download_source(work_dir: Path) -> Path:
    archive = work_dir / f"dav1d-{VERSION}.tar.xz"
    if not archive.is_file():
        print(f"Downloading {SOURCE_URL}", flush=True)
        request = urllib.request.Request(SOURCE_URL, headers={"User-Agent": "servo-ci-dav1d"})
        with urllib.request.urlopen(request, timeout=120) as response:
            data = response.read(16 * 1024 * 1024)
        if hashlib.sha256(data).hexdigest() != SOURCE_SHA256:
            raise RuntimeError("Downloaded dav1d archive has an unexpected SHA-256 checksum")
        archive.write_bytes(data)
    elif hashlib.sha256(archive.read_bytes()).hexdigest() != SOURCE_SHA256:
        raise RuntimeError(f"Cached dav1d archive has an unexpected SHA-256 checksum: {archive}")
    source = work_dir / f"dav1d-{VERSION}"
    if not source.is_dir():
        # A temporary extraction prevents an interrupted extraction looking like a complete source tree.
        with tempfile.TemporaryDirectory(dir=work_dir) as directory:
            with tarfile.open(archive) as tar:
                tar.extractall(directory, filter="data")
            shutil.move(str(Path(directory) / source.name), source)
    return source


def meson_setup(source: Path, build: Path, options: list[str], env: dict[str, str]) -> None:
    command = ["meson", "setup", str(build), str(source), "--buildtype=release", "--wrap-mode=nodownload"]
    if (build / "meson-private" / "coredata.dat").is_file():
        command.append("--reconfigure")
    if sys.platform == "win32":
        command.extend(["--vsenv", "-Db_vscrt=md"])
    run(command + options, env)


def verify_library(prefix: Path, work_dir: Path, target_options: list[str], native: bool) -> None:
    env = os.environ.copy()
    pkg_config_dir = (prefix / "lib" / "pkgconfig").as_posix()
    env.update(PKG_CONFIG_PATH=pkg_config_dir, PKG_CONFIG_LIBDIR=pkg_config_dir, PKG_CONFIG_SYSROOT_DIR="/")
    version = subprocess.check_output(["pkg-config", "--modversion", "dav1d"], env=env, text=True).strip()
    if version != VERSION:
        raise RuntimeError(f"pkg-config selected dav1d {version}, expected {VERSION}")
    run(["pkg-config", "--static", "--cflags", "--libs", "dav1d"], env)
    if not any((prefix / "lib" / name).is_file() for name in ("libdav1d.a", "dav1d.lib")):
        raise RuntimeError(f"No static dav1d library was installed in {prefix / 'lib'}")
    smoke_source = work_dir / "smoke-source"
    smoke_source.mkdir(exist_ok=True)
    (smoke_source / "meson.build").write_text(
        "project('dav1d-link-check', 'c')\n"
        f"dav1d = dependency('dav1d', version: '=={VERSION}', static: true, method: 'pkg-config')\n"
        "executable('dav1d-link-check', 'main.c', dependencies: dav1d)\n",
        encoding="utf-8",
    )
    (smoke_source / "main.c").write_text(
        "#include <dav1d/dav1d.h>\n#include <stdio.h>\n#include <string.h>\n"
        "int main(void) { Dav1dSettings settings; Dav1dContext *context = NULL;\n"
        "puts(dav1d_version()); dav1d_default_settings(&settings); settings.n_threads = 1;\n"
        "if (dav1d_open(&context, &settings) < 0) { return 1; }\ndav1d_close(&context);\n"
        f'return strcmp(dav1d_version(), "{VERSION}") != 0; }}\n',
        encoding="utf-8",
    )
    smoke_build = work_dir / "smoke-build"
    meson_setup(smoke_source, smoke_build, target_options, env)
    run(["meson", "compile", "-C", str(smoke_build)], env)
    if native:
        executable = smoke_build / ("dav1d-link-check.exe" if sys.platform == "win32" else "dav1d-link-check")
        run([str(executable)], env)


def cargo_environment(prefix: Path, target: str) -> dict[str, str]:
    directory = (prefix / "lib" / "pkgconfig").as_posix()
    result = {"SYSTEM_DEPS_DAV1D_LINK": "static", "SYSTEM_DEPS_DAV1D_BUILD_INTERNAL": "never"}
    if target == "native":
        current = os.environ.get("PKG_CONFIG_PATH", "")
        result["PKG_CONFIG_PATH"] = directory + (os.pathsep + current if current else "")
    else:
        # Override only dav1d probes. A global target sysroot or search-path override
        # would interfere with other libraries configured by mach's SDK environment.
        wrapper = prefix.parent / "pkg-config-dav1d"
        pkg_config = next(
            (
                os.environ[key]
                for key in (
                    f"PKG_CONFIG_{target}",
                    f"PKG_CONFIG_{target.replace('-', '_')}",
                    "TARGET_PKG_CONFIG",
                    "PKG_CONFIG",
                )
                if os.environ.get(key)
            ),
            shutil.which("pkg-config") or "pkg-config",
        )
        if Path(pkg_config).resolve() == wrapper.resolve():
            pkg_config = shutil.which("pkg-config") or "pkg-config"
        wrapper.write_text(
            "#!/usr/bin/env python3\n"
            "import os, re, subprocess, sys\n"
            "env = os.environ.copy()\n"
            "if any(re.match(r'^dav1d(?:$|[\\s<>=])', arg) for arg in sys.argv[1:]):\n"
            f"    env['PKG_CONFIG_PATH'] = {json.dumps(directory)}\n"
            f"    env['PKG_CONFIG_LIBDIR'] = {json.dumps(directory)}\n"
            "    env['PKG_CONFIG_SYSROOT_DIR'] = '/'\n"
            f"sys.exit(subprocess.call([{json.dumps(pkg_config)}, *sys.argv[1:]], env=env))\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        # Exact triples have precedence over the underscore form used by mach.
        result[f"PKG_CONFIG_{target}"] = wrapper.as_posix()
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default="native", choices=["native", *ANDROID_TARGETS, *OHOS_TARGETS])
    parser.add_argument(
        "--work-dir", type=Path, default=Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir())) / "servo-dav1d"
    )
    args = parser.parse_args()
    if args.target != "native" and sys.platform not in ("linux", "darwin"):
        raise RuntimeError("This setup script supports cross builds on Linux and macOS only")
    for executable in ("meson", "ninja", "pkg-config"):
        if not shutil.which(executable):
            raise RuntimeError(f"Install {executable} and add it to PATH before running this script")
    work_dir = args.work_dir.resolve()
    work_dir.mkdir(parents=True, exist_ok=True)
    source = download_source(work_dir)
    platform_name = f"{sys.platform}-{platform.machine()}" if args.target == "native" else args.target
    target_dir = work_dir / VERSION / platform_name
    target_dir.mkdir(parents=True, exist_ok=True)
    prefix = target_dir / "install"
    target_options = []
    if args.target != "native":
        cross_file = target_dir / "cross.ini"
        write_cross_file(args.target, cross_file)
        target_options = ["--cross-file", str(cross_file)]
    options = [
        "--prefix",
        str(prefix),
        "--libdir=lib",
        "--default-library=static",
        "-Db_staticpic=true",
        "-Denable_tools=false",
        "-Denable_tests=false",
        "-Denable_examples=false",
        "-Denable_docs=false",
    ]
    build_dir = target_dir / "build"
    meson_setup(source, build_dir, options + target_options, os.environ.copy())
    run(["meson", "compile", "-C", str(build_dir)])
    run(["meson", "install", "-C", str(build_dir)])
    verify_library(prefix, target_dir, target_options, native=args.target == "native")
    variables = cargo_environment(prefix, args.target)
    for key, value in variables.items():
        print(f"{key}={value}")
    if os.environ.get("GITHUB_ENV"):
        with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as output:
            for key, value in variables.items():
                if "\n" in value or "\r" in value:
                    raise RuntimeError(f"Invalid newline in environment variable {key}")
                output.write(f"{key}={value}\n")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.write(f"prefix={prefix.as_posix()}\n")


if __name__ == "__main__":
    main()
