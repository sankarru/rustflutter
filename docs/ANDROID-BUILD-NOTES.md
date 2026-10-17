# Android build and packaging notes

Every item here is something that actually stopped a build, install or run while
getting this tree to produce an installable `arm64-v8a` APK. Each one is the
literal symptom, what caused it, and the fix.

Two things this is not. It is not a claim that the tree is now sound — see
[Still open](#still-open). And where a mistake was mine rather than the tree's,
it says so, because those are the ones worth reading twice.

## The recipe that works

Nothing below is needed for this. It is the shortest path through the notes.

```sh
# 1. once: the engine, as a shared library. Either fetch it from CI --
#    the android-shared-engine workflow, on a machine with the RAM for the
#    5272 C++ targets ...
gh workflow run android-shared-engine.yml -f slimpeller=false
gh run download <run-id> --name rustflutter-engine-arm64-slfalse --dir engine_arm64

#    ... or build it here, which is the only option once you are editing C++.
#    See "Building the engine here" below; it needs a fetched DEPS tree.
cd src
gn gen out/android_release_arm64
ninja -C out/android_release_arm64 flutter/rust:rustflutter_engine

# 2. compile the Rust here -- no C++ at all
cd src/flutter/rust
NDK=/home/flux/dev/r29/toolchains/llvm/prebuilt/linux-x86_64
export CC_aarch64_linux_android=$NDK/bin/aarch64-linux-android24-clang
export AR_aarch64_linux_android=$NDK/bin/llvm-ar
cargo build --release -p photo_scroller --target aarch64-linux-android

# 3. package and install
cp target/aarch64-linux-android/release/libphoto_scroller.so \
   ../../../engine_arm64/lib.stripped/
$NDK/bin/llvm-strip ../../../engine_arm64/lib.stripped/libphoto_scroller.so
ANDROID_SDK_ROOT=/home/flux/dev/android-sdk \
JAVA_HOME=/home/flux/opt/jdk-25.0.4.1+1-temurin \
  python3 ../host/tools/build_apks.py --out ../../../engine_arm64 \
          --permission android.permission.INTERNET
adb install -r ../../../engine_arm64/apk/photo_scroller.apk
```

The SDK, NDK and JDK paths above are this machine's. `JAVA_HOME` pointing at a
25.x JDK is fine: `make_apk.py` invokes `javac` with `--release 11`, so what
reaches `d8` is Java 11 class files.

For everything that is *not* an engine change, `acs --rust` is the shorter
path and does steps 2 and 3 in one -- see
[acs packages a pointer](#acs-packages-a-pointer-and-the-pointer-is-not-the-library).

Two things about step 3 that are not obvious and are easy to get wrong:

- The output directory's **name** carries the ABI. `build_apks.py` derives it
  from the directory, so a fetched engine unpacked into a directory called
  `engine` produces an APK whose libraries are packaged under `armeabi-v7a` and
  which installs on nothing. It has to end in `_arm64`.
- `--permission` is off by default and an application that fetches anything
  needs `android.permission.INTERNET`. See [No application could ever open a
  socket](#no-application-could-ever-open-a-socket).

## Building the engine here

Editing C++ means building the engine here, and the engine is 5272 targets.
This box has 7 GB of RAM and a full swap file, so a cold `ninja` is not an
option -- it is not slow, it thrashes. What makes it viable is that the DEPS
tree and a warm ccache can be fetched from an ARM runner and the local build
then hits cache on essentially every edge.

### The pieces

`tools/local_android_ndk.sh` is an NDK shim. `build/config/android/config.gni`
hardcodes an NDK path and `gn` resolves four locations from it; the shim
satisfies all four so nothing in the build graph has to change.

`.github/workflows/local-deps-bundle.yml` runs on `ubuntu-24.04-arm` and packs
a fetched DEPS tree plus a warm android ccache into one tarball. Extract it
over `src/` (about 6.3 GB from a 1.2 GB archive) and the local build is
incremental from then on.

**The runner has to be ARM.** ccache keys on the compiler, and `DEPS` ships a
separate clang per host CPU, so an x86_64 cache scores zero hits on arm64. The
same reason Google's NDKs cannot be used to seed it: they put x86_64 binaries
in `prebuilt/linux-x86_64` and they will not execute on an ARM host.

### ccache needs all four of these

```sh
export CCACHE_DIR=$PWD/.ccache
export CCACHE_BASEDIR=$PWD/src
export CCACHE_COMPILERCHECK=content
export CCACHE_IGNOREOPTIONS='-fdebug-prefix-map=.*'
```

None of them is sufficient. `CCACHE_IGNOREOPTIONS` strips `-fdebug-prefix-map`
from the command-line hash and `CCACHE_BASEDIR` normalises the absolute paths
that still reach the manifest key -- each fixes half the problem, and dropping
either one costs most of the hit rate. (What `CCACHE_COMPILERCHECK=content`
solves, on the CI side, is a different failure: see
[ccache scored 0 hits on a restored cache](#ccache-scored-0-hits-on-a-restored-cache).)
Measured against objects CI actually built (not a random sample of local `.o`
files, which is how a first measurement looks like failure and is not):

| set | hits |
|---|---|
| `CCACHE_IGNOREOPTIONS` only | 3 / 80 |
| `CCACHE_BASEDIR` only | 0 / 25 -- breaks every android edge |
| both | 80 / 80, twice running |

### `--ccache` never reaches the host toolchain

`gn args` has `use_ccache`, and it does not work for host objects.
`build/toolchain/gcc_toolchain.gni` assigns `compiler_prefix = ""` in the
template's own scope before `cc = compiler_prefix + invoker.cc`, which throws
away the `"ccache "` that `linux/BUILD.gn` set. Two fixes inside the GN files
were tried and both produced a byte-identical `toolchain.ninja`, so both were
reverted:

- stopping the reset, which does not fire because the template reads it from
  its own scope;
- guarding with `defined(compiler_prefix)`, which inside a template body tests
  the *template*'s scope, not the caller's, so it is never true.

`gn desc` reports that `//build/toolchain/linux:clang_arm64` matches no
targets, which confirms the root `toolchain.ninja` is not generated from that
file either. Do not spend more time on the GN route.

What works is wrapping the host clang itself, in the workflow and locally:

```sh
for t in clang clang++; do
  test -x "$BT/$t.real" || mv "$BT/$t" "$BT/$t.real"
  printf '#!/bin/sh\nexec ccache %s "$@"\n' "$BT/$t.real" > "$BT/$t"
  chmod +x "$BT/$t"
done
```

That took CI's cacheable calls from 2283 to 4090 and the artifact from 86 MB to
250 MB. Renamed rather than deleted, so a `gclient sync` restores the originals.

### Local prerequisites, each of which was a separate failure

- `tools/local_android_ndk.sh` (the NDK shim above).
- `python3 build/linux/sysroot_scripts/install-sysroot.py --arch=arm64`.
- `apt-get install ccache` -- it is not present by default.
- **Use `/usr/bin/ninja`, not depot_tools' shim.** The shim dies on
  `ModuleNotFoundError: pipes` under Python 3.13.
- `export PATH="$PWD/flutter/third_party/depot_tools:$PATH"` before `gn` of any
  kind. Without it `gn desc` and `gn refs` fail with `Returned 127`, which
  reads exactly like a missing sysroot and sends you looking in the wrong place.

### Never let a failed step take the bundle

googlesource 503s constantly, so both `gclient sync` and the depot_tools clone
need retries. The archive is packed immediately after the sync, both uploads
are `if: always()`, and eight sentinels refuse to ship a partial tree -- a
387 MB partial that looked like a valid 1.2 GB one had already cost a run.

## Environment

**The NDK's `linux-x86_64` directory holds an aarch64 clang.** On an aarch64
host, `.../prebuilt/linux-x86_64/bin/clang --version` reports
`Target: aarch64-unknown-linux-gnu`. Do not "correct" the path; it is a
misnamed directory, not a wrong-architecture toolchain.

**`cc-rs` wants the generic tool name.** Anything pulling in a C dependency
(`ring`, via `ureq`'s TLS) looks for `aarch64-linux-android-clang`, which the
NDK does not ship — it ships versioned wrappers like `aarch64-linux-android24-clang`.
Fix with `CC_aarch64_linux_android` and `AR_aarch64_linux_android`, which is
cleaner than symlinking into a directory on `PATH`.

**API level 24, not 21.** The NDK offers wrappers from 21 up. `21` links
happily and then fails at install time on the device, because
`flutter/shell/platform/android/AndroidManifest.xml` declares
`android:minSdkVersion="24"`. The engine's own manifest is the authority here,
not the NDK's lowest wrapper.

## gn and ninja

### gn could not load the third_party wrappers

```
ERROR at //flutter/skia/BUILD.gn:516:12: Can't load input file.
      deps = [ "//flutter/third_party/libjpeg-turbo:libjpeg" ]
```

`DEPS` fetches most of `third_party` at its own root, so the checkout supplies
its own `BUILD.gn`. Four are the exception: their sources land under
`<name>/src`, so the `BUILD.gn` at `<name>/` is a wrapper that **upstream
commits into the repo** rather than fetches. `gen_deps.py` dropped them along
with everything else it regenerated.

Added, verbatim from `flutter/flutter` at `cf97bfbcb9f`:

- `src/flutter/third_party/boringssl/BUILD.gn`
- `src/flutter/third_party/libjpeg-turbo/BUILD.gn`
- `src/flutter/third_party/cpu_features/BUILD.gn`

Only these three are reachable from an Android build. `re2` is used solely by
`flutter/tools/licenses_cpp`, `spring_animation` only by the iOS shell, and
`tonic` is commented out at `shell/platform/embedder/BUILD.gn:436` — so a web or
iOS build would still fail, and that is not fixed.

### .gitignore hid the wrappers

`src/flutter/third_party/` was excluded wholesale, which meant the wrappers
above could not be committed. Narrowing it is not a one-line change, because
**git cannot re-include a file whose parent directory is excluded**. The rule
has to match files rather than the directory, and re-include the directories
so git descends far enough:

```gitignore
src/flutter/third_party/**/*
!src/flutter/third_party/**/
!src/flutter/third_party/**/BUILD.gn
```

### `DEPS` was fine; the search was wrong

Worth recording because it cost a build. `boringssl` *is* in `DEPS`, keyed on
`src/flutter/third_party/boringssl/src`. A search for
`'src/flutter/third_party/boringssl'` with a closing quote finds nothing,
because the key has a `/src` suffix — so the entry looks absent, gets added
again, and `gclient` then refuses the file:

```
ValueError: duplicate key in dictionary: src/flutter/third_party/boringssl/src
```

A static analysis pass made the same mistake and reported 16 missing deps when
the real number was one. Check `/src`-suffixed keys before concluding a
dependency is missing.

### depot_tools' ninja and gn need bootstrapping

```
python3_bin_reldir.txt not found. need to initialize depot_tools by
running gclient, update_depot_tools or ensure_bootstrap.
```

`ninja` on `PATH` is depot_tools' wrapper, not a real ninja, and it refuses to
run unbootstrapped. `DEPOT_TOOLS_UPDATE=0` is what suppresses the bootstrap,
so remove it or run `update_depot_tools` explicitly. There is no engine build
before this, so it fails at step one.

### The Rust standard library for the target

```
error[E0463]: can't find crate for `std`
  = note: the `aarch64-linux-android` target may not be installed
  [2/5272] RUST obj/flutter/rust/rustflutter/librustflutter.rlib
```

GN calls `rustc` directly with `--target=aarch64-linux-android`. Nothing in the
checkout declares that target, and Cargo is not involved, so nothing downloads
its std. `rustup target add aarch64-linux-android`, or `targets:` on
`dtolnay/rust-toolchain`.

### `--slimpeller` moves the output directory

`tools/gn:65-66` appends `slimpeller` to the target directory name, so switching
the flag moves `out/android_release_arm64` to
`out/android_release_arm64_slimpeller` and ninja fails on a directory that was
never created. `--target-dir=android_release_arm64` pins the one name both
configurations write to, which is also what keeps two artifacts comparable.

## Packaging

### No application could ever open a socket

```
No android.jar at .../platforms/android-36.1/android.jar
```

is the packaging failure you see first, but underneath it:

```
$ python3 -c "... 'android.permission.INTERNET' in raw ..."
android.permission.INTERNET: ABSENT
```

`make_apk.py` emitted a manifest with **no `<uses-permission>` element at all**
and no way to ask for one. Android refuses every socket an application has not
requested, before any of its code runs, so no example packaged by this tree
could ever fetch anything — the failure is silent, and shows up as a list of
placeholders forever.

Both `make_apk.py` and `build_apks.py` take `--permission NAME` now, repeatable,
off by default: least privilege is the better default for an application with
no network in it, which is most of them.

### Verifying a permission needs `aapt2`, not grep

`AndroidManifest.xml` inside an APK is **compiled binary XML**, and its string
pool is UTF-16, so a byte search for a permission name is a false negative even
when it is present. This reads correctly:

```sh
aapt2 dump xmltree app.apk --file AndroidManifest.xml | grep -i permission
```

### The platform and build-tools defaults disagree with the engine

`make_apk.py` defaulted to `--platform android-36.1` and `--build-tools 36.0.0`.
`build/config/android/config.gni` asks the engine for `android_sdk_version = 36`
and build tools `36.1.0`, and the SDK `DEPS` fetches over CIPD has
`platforms/android-36`. The APK has to agree with the engine that produced the
library it packages, so both are passable now and default by detecting what the
SDK actually has. Comparing those version numbers as strings picks `9.0.0` over
`36.1.0`.

### acs packages a pointer, and the pointer is not the library

`acs --rust` does not stage from `src/flutter/rust/target`. Its
`rustapp:cargo` task runs cargo with `--target-dir .ca-cache/rust-target`
and then writes a **pointer file**, `app/build/debug/rustapp-work/cargo.so` —
99 bytes of text holding the path to the real `.so`. `rustapp:stageLibs`
follows the pointer and copies whatever it names into the APK.

So an edit to Rust that is rebuilt correctly and then not seen on the device
has two likely causes, and both look like the tool being broken. First, if
that pointer has been hand-written — it is tempting, because a stale pointer
produces `cargo stamp names .../libphoto_scroller.so, which is not there`,
and pointing it somewhere that does exist silences that error — then acs
packages that path forever and never rebuilds. Second, the task is keyed on
`.ca-cache/fingerprints-app-debug.json` and acs writes that file only on a
fully successful run, so a failed run leaves no fingerprint and the next run
redoes everything.

What works every time is to build into acs's own target directory and let acs
package:

```
cd src/flutter/rust
cargo build -p photo_scroller --target aarch64-linux-android \
    --target-dir /home/flux/pro/rustflutter/.ca-cache/rust-target
cd /home/flux/pro/rustflutter
export RUSTFLUTTER_ENGINE_OUT=/home/flux/pro/rustflutter/engine_arm64
acs . --rust --rust-crate src/flutter/rust/apps/photo_scroller --engine-root .
```

sccache is configured, so the hand-driven run costs about 50 seconds even on a
cold profile. acs prints nothing for a task it skips, so the absence of
`[EXEC] rustapp:cargo` is the tell that it decided nothing changed.

### A thin debug profile acs can actually reach

Iterating the Rust half against a device wants something between `dev` and
`release`: `dev` optimises nothing, which is wrong for an app whose frames
have to keep up, and `release` spends minutes per build. `Cargo.toml` sets

```toml
[profile.dev.package."*"]
opt-level = 1
debug = 1
```

Scoped to `package."*"` on purpose. That key covers dependencies but never the
root package, so the two commands land where each wants to be: building
`photo_scroller` compiles the framework at `-O1`, while
`cargo test -p rustflutter` still compiles that same very large crate at `0`
and its 6995 tests stay fast. Scoping it to `dev` itself would make the test
suite pay `-O1` to build the crate it is about to exercise.

It has to live in `dev` rather than a separately named profile because **acs
has no `--profile` flag** — it passes `--release` or nothing at all. A profile
named `thin` is unreachable by the packaging tool, which would go on staging
whatever `dev` last produced.

## The ABI comes from the directory name

`build_apks.py:abi_for()` reads the ABI off the output directory — `_arm64` →
`arm64-v8a` — and falls through to `armeabi-v7a` for a name it does not
recognise. An engine fetched into `engine/` yields an APK that installs on
nothing, with no error at any stage. Name the directory `engine_arm64`.

## Cargo, Cranelift and sccache

### Cranelift miscompiles this crate's panic handling

Three tests leak a panic past the `catch_unwind` at
`rustflutter/src/framework.rs:2565`:

```
a_component_that_always_panics_stays_bounded
a_panicking_build_is_replaced_and_the_frame_finishes
a_retried_build_recovers_the_subtree
```

All three pass under LLVM and all three fail under `-Zcodegen-backend=cranelift`.
Recovering from a panicking `build` is a guarantee the framework makes, so
Cranelift is an opt-in `[profile.cl]` rather than the `dev` default.

Worth knowing: it compiles clean and the other 6992 tests pass, so a workflow
that only cross-compiles to Android — which never runs the suite — will not
surface it.

### `-Zcodegen-backend` is a gate, not a value

```
error: flag -Zcodegen-backend does not take a value, found: `cranelift`
```

Cargo's flag is boolean and only unlocks the unstable feature; the backend is
selected by `[profile.X] codegen-backend = "cranelift"` in a manifest, or by
`-Zcodegen-backend=cranelift` reaching rustc through `RUSTFLAGS`.

### `jobs` is not the knob that matters here

The workspace is one very large crate, so there is nothing for `-j` to overlap.
Intra-crate parallelism comes from `codegen-units` and from rustc's own thread
pools:

| | cold build |
|---|---|
| `jobs = 8`, default threads | 54.57s |
| `-Zthreads=8` | 13.40s |

### sccache needs non-incremental

It rejects incremental compilation outright, so every profile sets
`incremental = false` and `[env] CARGO_INCREMENTAL = "0"` pins it in case the two
disagree.

## CI

### ccache scored 0 hits on a restored cache

The cache restored (37 MB) and every one of 4560 compilations missed. ccache
keys on the compiler binary's size and **mtime** by default, and `DEPS` downloads
a fresh clang into `src/flutter/buildtools` on every runner, so each run looks
like a different compiler. `CCACHE_COMPILERCHECK=content` fixed it — 2280/6840
hits, ninja 33.5 min → 16.5 min. Chromium's own CI sets this for the same
reason.

A restored ccache does not make a build incremental, only its compilation cheap:
ninja still re-stats every input and re-does every **link**, which is why it
halves the build rather than making it instant. Caching `src/out` would be the
real prize and exceeds the 10 GB ceiling `actions/cache` puts on one entry.

### `wrap.<pkg>` cannot set an environment variable here

`RfAppHost::post_task` is the one host callback safe to call from any thread,
and `RUSTFLUTTER_FRAME_STATS=1` is the diagnostic worth having — but injecting
it on Android goes through the `wrap.<package>` system property, and **property
names cap at 31 characters** while `io.flutter.rustflutter.counter` is 35:

```
Failed to set property 'wrap.io.flutter.rustflutter.counter'
```

Workaround: package under a shorter id (`make_apk.py` takes `--package`), which
makes `wrap.rf.j` fit.

## Things that are not bugs

**Android already renders with Impeller.** `--slimpeller` is not the renderer
switch; its help text says it reduces binary size "by assuming only the Impeller
rendering engine is supported". The switch is `Settings::enable_impeller`, which
defaults to `true` on Android (`common/settings.h:234`), and `RunOptions.impeller`
defaults to `true` (`rustflutter/src/app.rs:1383`). Chasing a jank complaint by
turning on Impeller was a wrong turn and cost a build.

**`--slimpeller` does not compile here anyway**, from two errors that both come
from Skia Ganesh being compiled out from under fork code:

```
rustflutter_ffi.cc:661: error: no member named 'DlSkCanvasAdapter'
rustflutter_host_android.cc:1930: error: cannot assign to static data member
                                'enable_impeller' with const-qualified type
```

`RasterizeToSurface` flattens the LayerTree to a DisplayList and replays it into
a **CPU** `SkSurface` through `DlSkCanvasAdapter` — that is how
`rf_layer_tree_write_png` rasterises, and the class is Ganesh-only. The second
is that `enable_impeller` becomes `static constexpr const bool` under SLIMPELLER
while the host assigns to it from the framework's runtime switch.

**The README's "Known limitations" are stale.** Both the flat-layer-tree and
whole-tree-rebuild items describe a state of the code that has moved on:
`flush_layout` lays out only dirty relayout boundaries, `flush_paint`
re-records only dirty repaint boundaries, and clip/opacity/transform each push
a real layer through `in_layer`. `PORTING_STATUS.md` is maintained; that section
appears not to be.

## Mistakes of mine, since they are the reusable ones

**A regex that stopped at the first `/` misreported four dependencies as
missing.** `"(//flutter/third_party/([A-Za-z0-9_.+-]+))"` excludes any label
carrying a `:target`, which is how `libjpeg-turbo:libjpeg` — the label GN
actually complained about — went unlisted.

**A loop variable shadowed the outer one.** `for name in args.permission:`
inside `for name in names:` made every APK print itself as
`packaging android.permission.INTERNET`. Both loops now have distinct names, and
the comment says why.

**Compiled binary XML cannot be grepped.** See
[Verifying a permission needs `aapt2`](#verifying-a-permission-needs-aapt2-not-grep) —
I "verified" the permission was missing after adding it.

**`HashMap<String, String>::get` returns `&String`.** `Some("android")` and
`unwrap_or("")` do not typecheck against it; `.map(String::as_str)` does.

**A `Fn` closure cannot move its captures.** `leaf` takes `Fn() -> R`, so
handlers built *inside* it cannot be moved out to the drag callbacks, and a
`ListView` cannot be rebuilt from one captured by move. The handlers are built
outside and cloned in; the list is rebuilt inside from a snapshot of cloneable
data.

**Cargo reads `src/main.rs` as a second, binary target.** The package is a
library; the binary target wanted a `fn main`. `autobins = false` in
`[package]`.

**`Theme::dark()` does not paint the window.** `WidgetApplication::background()`
is a separate method. Overriding one and not the other gives a white
background with light-grey captions on it.

**A sign error, one sign.** The drag advanced the offset by `-delta.dy`; the
fling stored the same quantity and subtracted it, so a flick scrolled back the
way it came. See `Io::step_fling`.

## Capabilities: the mobile-sentinel hybrid

rustflutter stays the host. `mobile-sentinel` (currently the `permissions`
feature only) contributes capability code *underneath* it: its Rust links
into the app's `.so`, its Kotlin compiles into the APK's dex, its init
provider merges into the manifest. There is deliberately no sentinel
`Application`, no sentinel `Activity`, no Gradle.

**The capability call dies without `ndk_context`.** Sentinel reaches JNI
through its own `JNI_OnLoad` VM cache, but resolving its helper classes from
a worker thread falls back to the Activity's classloader, which needs
`ndk_context` -- and nothing initialises that in a custom host. The symptom
is `android context was not initialized` panicking the draw thread: a native
abort, in the tombstone, not logcat. The fix is host-side, in two halves that
must both exist: `RustflutterActivity.onCreate` calls
`nativeInitNdkContext(this)` (once per process; `ndk_context` asserts
exactly-once), and the framework's `android_host` module implements it by
seeding `ndk_context` with the VM and a leaked `GlobalRef` to the Activity.
The JVM finds the symbol by dynamic lookup in the already-loaded app
library, so no engine change is needed.

**The provider name must be qualified.** Sentinel's core manifest declares
`android:name=".SentinelCoreInitializer"`, resolved against the *library*
namespace under Gradle. Spliced raw into the app manifest it resolves
against the app package instead, and the provider install dies with
`ClassNotFoundException` before `Application.onCreate`. Qualify it to
`com.mobilesentinel.*` at splice time.

**A permission nobody declared shows no dialog.** `requestPermissions` for a
permission missing from the manifest is an instant silent denial. The app's
own `uses-permission` entries are the declaration; `status()` before that
fix read `Denied` forever and no tap could change it. Check with `aapt2 dump xmltree`
(see [Verifying a permission needs `aapt2`](#verifying-a-permission-needs-aapt2-not-grep)), not by tapping.

**The request control must not scroll away.** The permission card started as
the first list item; any scroll put it off screen and taps hit photographs.
It is pinned above the scroll region now. The grant arrives asynchronously,
so a tap starts a bounded watch (per-frame rebuilds, same mechanism as a
fling) that flips the line when the status changes.

The other packager that knows all of this is acs `--rust`, which derives the
permissions from the sources, compiles the sentinel Kotlin itself, and
writes the manifest -- `INTERNET` included, as a named `const` next to the
code that needs it, so the requirement lives with its reason.

## Platform views: a WebView in the middle of a scroll

`photo_scroller` now hosts a live Android `WebView` between the photographs.
It is real hybrid composition: a genuine `View` parented into the window
beside the `SurfaceView`, drawn by Android over the Flutter surface, moving
with the list and keeping its page.

The surprising part is how little of the engine that needed. The tree is a
de-Dart-ified upstream engine and it has no `PlatformViewsDelegate` anywhere
and no platform-view branch in the `Compositor` -- that is what Phase A was
written around and what looked like the whole job. It is not, because
**hybrid composition of an opaque view needs no help from the compositor at
all.** Upstream pushes a `PlatformViewLayer` so the compositor knows to clear
that rectangle of its own output and draw the platform view over the top --
that is the *texture* case, where the View's pixels arrive as an image the
compositor must place. Here the View draws over the surface on its own, so
the framework's whole job is to tell the host *where* the view goes. The
layer push was removed, and with it the reason to touch the `Compositor`.

### Six failures that each looked like something else

None of these announce themselves. Each one produced a working-looking build
that did the wrong thing.

**JNI's descriptor alphabet is all capitals.** There is no lower-case `i`;
`int` is `I`. `(JIi)Z` is a parse error, not an int, and `jni` rejects it
before the JVM ever sees it. The `Err` has the same shape as any other and
the symptom is a host call that answers `false` forever with nothing in
logcat. Checked directly against jni 0.21.1: `(JIi)Z` is rejected, `(JI)Z`
and `(IIIIIII)Z` both parse. A scratch crate that parses the descriptor on the
host costs a minute and saves an afternoon.

**The class has to come from the Activity.** `find_class` on the engine's
platform thread resolves through the *system* classloader, which cannot see
the application's own classes. The symptom is a `ClassNotFoundException`
for a class that is plainly in the dex, raised on the first platform view
task and nothing else -- the same wall
[mobile-sentinel hit](#capabilities-the-mobile-sentinel-hybrid), for the same
reason. `env.get_object_class(activity)` asks the object's own classloader
instead.

**`call_static_method`, not `call_method`.** The dispatch entry point is
static. `call_method` searches for an *instance* method and raises
`NoSuchMethodError`, which on the engine's thread is a hard crash rather than
an error return.

**A View may only be built on the main looper.** `new WebView()` on the
engine's thread builds a `Handler`, whose constructor dereferences
`Looper.mQueue` on the *current* thread -- null on a thread that has never
looped -- so it dies with a `NullPointerException` before any of our code
runs. `on_ui_thread` in Rust used to claim it posted and waited; it only ever
called the static method on the calling thread. The post now happens in Java,
because a `Runnable` carrying a closure cannot be built from Rust without
parking that closure in a global for Java to call back into.

**Logical pixels in, physical pixels out.** Upstream's
`PlatformViewsController.toPhysicalPixels` is
`logicalPixels * displayDensity`, and it has to happen host-side for the same
reason it does there: a rectangle handed straight to `setLayoutParams` lands
at the same numeric offset on every device and is therefore correct only on
the one whose density is 1. On a 2.625x screen a 387-wide view would be 147
logical pixels, a little over a third of the card it is meant to fill.

**Five int slots, not four.** A bounds update carries left, top, width *and*
height. With one slot short, every value landed one position early and the
height was never sent at all -- which reads as a view of the right width and
a third of the right height. The signature is `(JIIIIIIZ)Z`, with `a..d` the
bounds so the Java side reads them identically for every task, and `e`
carrying the type index where the bounds are meaningless.

### The view was not being clipped. It was being rebuilt.

`RenderPlatformView` had `impl Drop { controller.destroy(id) }`, on the
reasoning that a render object going away should take its view with it.
Render objects here are **per-frame**: `Component::build` runs again on every
rebuild, and a drag is a rebuild sixty times a second. One swipe produced
`CREATE=30 DESTROY=30` -- the WebView was destroyed and recreated thirty
times in a single gesture, reloading its page each time, and never finished
rendering. It was not misplaced, not clipped, and not a compositing problem;
it was being rebuilt while you looked at it.

The lifetime belongs to the `PlatformViewController`, which the application
holds, and the render object is a handle to it. `Drop` now lives on the
controller and only fires when `Rc::strong_count(&inner) == 1`, which is
when the *application* drops its own handle. That is also what the long
standing claim wanted -- a view scrolled out of a viewport is still alive and
still holding its page -- and it is what upstream does: there the view is
owned by a `PlatformViewLayer` and released when the layer is disposed, not
when a render box is collected.

### And the "rendering limits" were a stale dirty region

The other disappearing act looked like a compositing bug and was not. Moving
the view with `view.layout(l, t, r, b)` directly is cheaper than
`setLayoutParams` and is wrong: outside a layout traversal Android does not
recompute which regions the view covers, so the strip it has just vacated is
never invalidated, and the view's content is clipped against a rectangle that
no longer matches where it is. What that looks like is a hard-edged band of
the WebView going blank as it scrolls, with stale pixels trailing it -- the
contents hiding behind the rendering limits. `setLayoutParams` is a
`requestLayout`, and the traversal it schedules is what invalidates both the
old and the new rectangle.

The per-frame cost of that traversal is kept off the WebView by
`PlatformViewContainer.onLayout`, which measures a child only when its size is
not already right. Bounds and visibility are also *posted* rather than
posted-and-waited, because waiting puts a full UI round trip into every frame
of a scroll and leaves the render thread paced by the UI thread. Creation still
waits: it happens once per view, so the round trip is free, and the caller
needs the answer, since a framework told "created" about a view that was never
made will go on positioning a view that is not there.

### What platform views do not do yet
- Creation params are not implemented. The registry answers by type name and
  `WebViewFactory` hardcodes a `data:` URL. Upstream's WebView plugin takes
  its URL from `PlatformViewCreationParams`. Making params work means
  reinstating the `PlatformViewsChannel` that already sits unused in
  `shell/platform/android`.
- Only opaque platform views. Nothing punches an alpha hole in the Flutter
  output, so a translucent view would blend with whatever Flutter painted
  underneath it rather than with what is behind the window.
  `PlatformViewContainer.setOpaqueOnly()` records this rather than leaving it
  as a surprise.

## Writing an application: two things that cost an afternoon

`apps/flower` is the newest one, and both of these cost real time on it. Neither
is in a compiler message that points at the cause.

**Every application needs its own `build.rs`, and without one it dies in
`onCreate`.** The engine is linked by `build.rs`, per application, reading
`rustflutter_link.txt` out of the engine directory — copy
`apps/photo_scroller/build.rs`. Leave it out and the application compiles with
no warning, packages, installs, and then:

```
dlopen failed: cannot locate symbol "rf_path_cubic_to" referenced by "libflower.so"
```

The engine exports that symbol; `nm -D` on it says so. What is missing is
`DT_NEEDED`: without the build script `libflower.so` is linked with no record
that it needs `librustflutter_engine.so`, so the loader has nothing to search and
reports the first `rf_*` it cannot resolve — which is why the message names a
function the application never called, and why it moved when unrelated code
changed. `readelf -d <app>.so | grep NEEDED` settles it in one command; the
engine has to be on that list. `photo_scroller.so` loads and does not, which is
what makes this so confusing: it is the one application that never noticed.

**`advance` is handed a timestamp, not a duration.** `FrameContext` documents
`frame_time_micros` as "the time the frame is targeted at, in microseconds since
epoch", and the name reads like a duration. It is not: adding it to a running
clock advances that clock by however long ago the epoch was. The first draft of
the flower read `bloom 2246` about two minutes after launch, which looks like a
fast animation rather than a wrong one. Keep the previous stamp and difference
it, clamped so a stalled frame jumps rather than plays:

```rust
let now = frame_time_micros as f32 / 1_000_000.0;
let delta = io.last.replace(now).map(|t| (now - t).min(0.1)).unwrap_or(0.0);
```

Two more, both layout, both silent:

**A render object that answers a layout with `constraints.biggest()` collapses
to nothing in a `Column`.** The main axis there is unbounded, and the tree
clamps an unbounded `biggest()` to the minimum — zero. The scene drew off the
bottom of the screen with nothing on it and no error. `Expanded` hands it the
space that is actually left over.

**`Canvas::saved()` is not reentrant.** It takes `&mut Canvas` and a closure that
also borrows it, so nesting one inside another is a borrow error rather than a
nested save/restore. Use explicit `save()` / `restore()` pairs, which is what
every transform in the flower uses.

And one that is not silent at all but reads like something else:
**`RenderPath::quadratic_to` fails to link against this engine**, so
`dlopen` reports it as `cannot locate symbol "rf_path_quadratic_to"` — in the
flower, named for a call the application had already removed, because
`borders.rs` uses it too. `quadratic_as_cubic` in `apps/flower` is the
conversion, and it is exact rather than approximate.

## Still open
- `re2` and `spring_animation` are still referenced by the build graph with no
  `DEPS` entry. Unreachable from an Android build; a web or iOS build fails.
- Frame cost is unmeasured. `RUSTFLUTTER_FRAME_STATS=1` reports per-phase
  medians and is the right tool, but the property-name limit above blocks it.
- `photo_scroller` fetches all of its photographs at once rather than on
  demand, and keeps every decoded handle. A list of any length wants viewport
  demand, which needs child heights before layout.
- Nothing is pushed to `linzj/rustflutter`; all commits are on `sankarru`'s
  fork. It has **pull-only** access to the original.