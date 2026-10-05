# Copyright 2026 The Servo Project Developers. See the COPYRIGHT
# file at the top-level directory of this distribution.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import setup_dav1d as setup


class SetupDav1dTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_native_preserves_existing_pkg_config_path(self):
        with patch.dict(os.environ, {"PKG_CONFIG_PATH": "/existing"}, clear=True):
            env = setup.cargo_environment(self.root, "native")
        self.assertEqual(env["PKG_CONFIG_PATH"], (self.root / "lib/pkgconfig").as_posix() + os.pathsep + "/existing")
        self.assertEqual(env["SYSTEM_DEPS_DAV1D_LINK"], "static")
        self.assertEqual(env["SYSTEM_DEPS_DAV1D_BUILD_INTERNAL"], "never")

    def test_native_without_existing_path(self):
        with patch.dict(os.environ, {}, clear=True):
            env = setup.cargo_environment(self.root, "native")
        self.assertEqual(env["PKG_CONFIG_PATH"], (self.root / "lib/pkgconfig").as_posix())

    @unittest.skipIf(sys.platform == "win32", "Cross builds run on Linux or macOS")
    def test_cross_wrapper_only_overrides_dav1d(self):
        executable = self.root / "fake-pkg-config"
        executable.write_text(
            f"#!{sys.executable}\nimport json, os, sys\n"
            "print(json.dumps({'args': sys.argv[1:], 'path': os.getenv('PKG_CONFIG_PATH'), "
            "'libdir': os.getenv('PKG_CONFIG_LIBDIR'), 'sysroot': os.getenv('PKG_CONFIG_SYSROOT_DIR')}))\n"
        )
        executable.chmod(0o755)
        prefix = self.root / "install"
        with patch.dict(os.environ, {"PKG_CONFIG": str(executable)}, clear=True):
            env = setup.cargo_environment(prefix, "aarch64-unknown-linux-ohos")
        self.assertEqual(
            set(env),
            {
                "SYSTEM_DEPS_DAV1D_LINK",
                "SYSTEM_DEPS_DAV1D_BUILD_INTERNAL",
                "PKG_CONFIG_aarch64-unknown-linux-ohos",
            },
        )
        wrapper = env["PKG_CONFIG_aarch64-unknown-linux-ohos"]
        sdk_env = dict(
            os.environ, PKG_CONFIG_PATH="/sdk/pc", PKG_CONFIG_LIBDIR="/sdk/lib", PKG_CONFIG_SYSROOT_DIR="/sdk"
        )
        for dependency in ["dav1d", "dav1d >= 1.3.0", "gstreamer-1.0", "--version"]:
            with self.subTest(dependency=dependency):
                output = subprocess.check_output([sys.executable, wrapper, dependency], env=sdk_env, text=True)
                result = json.loads(output)
                if dependency.startswith("dav1d"):
                    self.assertEqual(result["path"], (prefix / "lib/pkgconfig").as_posix())
                    self.assertEqual(result["libdir"], (prefix / "lib/pkgconfig").as_posix())
                    self.assertEqual(result["sysroot"], "/")
                else:
                    self.assertEqual(result["path"], "/sdk/pc")
                    self.assertEqual(result["libdir"], "/sdk/lib")
                    self.assertEqual(result["sysroot"], "/sdk")
                self.assertEqual(result["args"], [dependency])

    def make_toolchain(self, base):
        (base / "bin").mkdir(parents=True)
        (base / "sysroot").mkdir()
        for tool in ["clang", "llvm-ar", "llvm-strip"]:
            (base / "bin" / tool).touch()

    @unittest.skipIf(sys.platform == "win32", "Cross builds run on Linux or macOS")
    def test_android_cross_file_matches_ndk_target_and_api(self):
        prebuilt = self.root / "toolchains/llvm/prebuilt/linux-x86_64"
        self.make_toolchain(prebuilt)
        for target, (_, _, clang_target) in setup.ANDROID_TARGETS.items():
            with (
                self.subTest(target=target),
                patch.dict(os.environ, {"ANDROID_NDK_ROOT": str(self.root)}),
                patch.object(setup.platform, "system", return_value="Linux"),
                patch.object(setup.platform, "machine", return_value="x86_64"),
            ):
                destination = self.root / "cross.ini"
                setup.write_cross_file(target, destination)
                contents = destination.read_text()
                self.assertIn(f"--target={clang_target}", contents)
                self.assertIn("system = 'android'", contents)
                self.assertIn("needs_exe_wrapper = true", contents)

    @unittest.skipIf(sys.platform == "win32", "Cross builds run on Linux or macOS")
    def test_ohos_cross_file_uses_sdk_and_clang_triple(self):
        self.make_toolchain(self.root / "llvm")
        (self.root / "sysroot").mkdir()
        with patch.dict(os.environ, {"OHOS_SDK_NATIVE": str(self.root)}):
            destination = self.root / "cross.ini"
            setup.write_cross_file("aarch64-unknown-linux-ohos", destination)
        contents = destination.read_text()
        self.assertIn("--target=aarch64-linux-ohos", contents)
        self.assertIn("-D__MUSL__", contents)
        self.assertIn(f"--sysroot={self.root.resolve().as_posix()}/sysroot", contents)

    def test_missing_sdk_fails_early(self):
        with patch.dict(os.environ, {}, clear=True), self.assertRaisesRegex(RuntimeError, "Set OHOS_SDK_NATIVE"):
            setup.write_cross_file("aarch64-unknown-linux-ohos", self.root / "cross.ini")

    def test_unknown_target_rejected(self):
        with self.assertRaisesRegex(ValueError, "Unsupported cross target"):
            setup.write_cross_file("unexpected-target", self.root / "cross.ini")

    def test_invalid_cached_archive_rejected_before_extracting(self):
        (self.root / f"dav1d-{setup.VERSION}.tar.xz").write_bytes(b"corrupt")
        with self.assertRaisesRegex(RuntimeError, "unexpected SHA-256"):
            setup.download_source(self.root)
        self.assertFalse((self.root / f"dav1d-{setup.VERSION}").exists())


if __name__ == "__main__":
    unittest.main()
