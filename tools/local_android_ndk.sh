#!/bin/bash
# Copyright 2013 The Flutter Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Makes a locally installed NDK usable by `gn --android`, without asking the
# NDK to be the one the engine pins and without touching the NDK itself.
#
# ## Why this exists
#
# `build/config/android/config.gni` hardcodes where the NDK is:
#
#   android_ndk_version = "28.2.13676358"
#   android_ndk_root    = "//flutter/third_party/android_tools/sdk/ndk/$android_ndk_version"
#
# That path is inside the DEPS tree, and `DEPS:27` only populates it on macOS
# or linux-x64:
#
#   'download_android_deps': 'host_os == "mac" or
#                            (host_os == "linux" and host_cpu == "x64")'
#
# There is no arm64 variant of `flutter/android/sdk/all/` on CIPD to fetch
# instead -- `cipd ensure` answers "no such package: flutter/android/sdk/all/
# linux-arm64" -- so on an arm64 Linux host an Android engine build has no NDK
# at all, and ninja dies on the first NDK source file:
#
#   ninja: error: '.../android_tools/sdk/ndk/28.2.13676358/sources/android/
#            cpufeatures/cpu-features.c', missing and no known rule to make it
#
# Google publishes NDKs at dl.google.com under release names (r26b, r28c) and
# not under the build numbers Flutter pins, so "just download 28.2.13676358" is
# not available either. Hence this: a tree of symlinks at the path gn hardcodes,
# pointing into an NDK that is already installed.
#
# ## Why the NDK is not edited
#
# Adding a symlink inside somebody else's NDK to make one build work is the kind
# of change that outlives its reason. Everything here lives under the source
# tree instead.
#
# ## The two mismatches, and only two
#
# 1. The path. `android_ndk_root` names a build number, and the whole shim is
#    what stands at that path.
#
# 2. `android_ndk_clang_version = "19"` (config.gni:128) selects the compiler
#    runtime in
#      llvm_android_toolchain_root/lib/clang/$android_ndk_clang_version/lib/linux/<arch>
#    r29 ships 21 and 22. This is compiler-rt -- builtins and unwind tables --
#    and 19 -> the newest is the closest thing available. Getting it wrong is a
#    link error naming `libclang_rt.builtins-aarch64-android`, not a silent
#    difference.
#
# That is all. The directory gn reaches for inside the NDK is *already* correct
# on this host, which looks like a bug and is not:
#
#   build/config/android/config.gni:29
#     if (host_cpu == "x64" || host_cpu == "x86" || host_cpu == "arm64") {
#       android_host_arch = "x86_64"
#     }
#
# so `llvm_android_toolchain_root` is `prebuilt/linux-x86_64` on an arm64 host
# too. And the binaries in there are AArch64:
#
#   $ readelf -h .../prebuilt/linux-x86_64/bin/clang | grep Machine
#   Machine:                           AArch64
#
# The name is not a misnomer to be corrected; it is the name the Android build
# system insists on, for every host CPU.
#
# Usage: tools/local_android_ndk.sh [ndk-path] [engine-version]
# Prints the shim path on stdout.

set -euo pipefail

NDK="${1:-/home/flux/dev/r29}"
VERSION="${2:-28.2.13676358}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG="$HERE/../src/build/config/android/config.gni"

if [ ! -d "$NDK" ]; then
  echo "no NDK at $NDK" >&2
  exit 1
fi

# Read the two names out of config.gni rather than restating them, so that a
# change upstream is a stale error message here rather than a wrong directory.
ANDROID_HOST_OS="$(sed -n 's/^ *android_host_os *= *"\([a-z]*\)".*/\1/p' "$CONFIG" | head -1)"
ANDROID_HOST_ARCH="$(sed -n 's/^ *android_host_arch *= *"\([a-z0-9_]*\)".*/\1/p' "$CONFIG" | head -1)"
WANTED_CLANG="$(sed -n 's/^ *android_ndk_clang_version *= *"\([0-9]*\)".*/\1/p' "$CONFIG")"

# config.gni computes these inside a host_os/host_cpu branch, so the bare
# assignments are only present on some branches; fall back to the one line the
# build actually reaches for.
[ -n "$ANDROID_HOST_OS" ] || ANDROID_HOST_OS="linux"
[ -n "$ANDROID_HOST_ARCH" ] || ANDROID_HOST_ARCH="x86_64"

PREBUILT="$NDK/toolchains/llvm/prebuilt/${ANDROID_HOST_OS}-${ANDROID_HOST_ARCH}"
if [ ! -d "$PREBUILT" ]; then
  echo "config.gni wants $PREBUILT and it does not exist" >&2
  exit 1
fi

SHIM="$HERE/../src/flutter/third_party/android_tools/sdk/ndk/$VERSION"
echo "NDK        : $NDK"
echo "prebuilt   : $PREBUILT  (config.gni: android_host_os=$ANDROID_HOST_OS android_host_arch=$ANDROID_HOST_ARCH)"
if ! "$PREBUILT/bin/clang" --version >/dev/null 2>&1; then
  echo "that clang will not run on this host" >&2
  exit 1
fi
echo "clang      : $("$PREBUILT/bin/clang" --version | head -1)"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHIM="$HERE/../src/flutter/third_party/android_tools/sdk/ndk/$VERSION"
echo "shimming into $SHIM"
rm -rf "$SHIM"
mkdir -p "$SHIM"

# Everything at the NDK root, with toolchains/ rebuilt below. The NDK root has
# no `lib`: the compiler runtime lives under the host toolchain, which is also
# the path config.gni:140 walks, so that is the `lib` that has to be rebuilt.
for entry in "$NDK"/*; do
  case "$(basename "$entry")" in
    toolchains) continue ;;
  esac
  ln -sfn "$entry" "$SHIM/$(basename "$entry")"
done

mkdir -p "$SHIM/toolchains/llvm/prebuilt"
for entry in "$NDK"/toolchains/*; do
  ln -sfn "$entry" "$SHIM/toolchains/$(basename "$entry")"
done
HOST_PREBUILT="$SHIM/toolchains/llvm/prebuilt/${ANDROID_HOST_OS}-${ANDROID_HOST_ARCH}"
mkdir -p "$HOST_PREBUILT"
for entry in "$PREBUILT"/*; do
  case "$(basename "$entry")" in
    lib) continue ;;
  esac
  ln -sfn "$entry" "$HOST_PREBUILT/$(basename "$entry")"
done

mkdir -p "$HOST_PREBUILT/lib"
for entry in "$PREBUILT"/lib/*; do
  ln -sfn "$entry" "$HOST_PREBUILT/lib/$(basename "$entry")"
done
if [ -n "$WANTED_CLANG" ] && [ ! -e "$HOST_PREBUILT/lib/clang/$WANTED_CLANG" ]; then
  NEWEST="$(ls -1 "$PREBUILT"/lib/clang | sort -V | tail -1)"
  echo "clang runtime: lib/clang/$WANTED_CLANG -> lib/clang/$NEWEST (what the NDK has)"
  ln -sfn "$PREBUILT/lib/clang/$NEWEST" "$HOST_PREBUILT/lib/clang/$WANTED_CLANG"
fi

# Everything config.gni resolves that the build will actually read. The list is
# deliberately short: each entry is a path whose absence turns into a ninja
# error several steps later that does not name itself.
# `android_ndk_include_dir` (config.gni:60) is absent on purpose -- it is
# assigned once and read nowhere in the build graph, and no NDK has the
# top-level `usr/` it points at anyway.
echo "--- what gn will look for, resolved ---"
for p in \
  "sources/android/cpufeatures/cpu-features.c" \
  "toolchains/llvm/prebuilt/${ANDROID_HOST_OS}-${ANDROID_HOST_ARCH}/bin" \
  "toolchains/llvm/prebuilt/${ANDROID_HOST_OS}-${ANDROID_HOST_ARCH}/sysroot/usr/lib" \
  "toolchains/llvm/prebuilt/${ANDROID_HOST_OS}-${ANDROID_HOST_ARCH}/lib/clang/${WANTED_CLANG}/lib/linux/aarch64"
do
  if [ -e "$SHIM/$p" ]; then
    echo "  ok      $p"
  else
    echo "  MISSING $p"
    exit 1
  fi
done

echo "$SHIM"