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
# 1. once: build the engine as a shared library, on a machine with the RAM for
#    the 5272 C++ targets. This is the android-shared-engine workflow.
gh workflow run android-shared-engine.yml -f slimpeller=false
gh run download <run-id> --name rustflutter-engine-arm64-slfalse --dir engine_arm64

# 2. compile the Rust here -- no C++ at all
cd src/flutter/rust
NDK=/home/flux/Android/ndk/r29/toolchains/llvm/prebuilt/linux-x86_64
export CC_aarch64_linux_android=$NDK/bin/aarch64-linux-android24-clang
export AR_aarch64_linux_android=$NDK/bin/llvm-ar
cargo build --release -p photo_scroller --target aarch64-linux-android

# 3. package and install
cp target/aarch64-linux-android/release/libphoto_scroller.so \
   ../../../engine_arm64/lib.stripped/
$NDK/bin/llvm-strip ../../../engine_arm64/lib.stripped/libphoto_scroller.so
ANDROID_SDK_ROOT=/home/flux/Android/Sdk JAVA_HOME=/usr/lib/jvm/java-17-openjdk \
  python3 ../host/tools/build_apks.py --out ../../../engine_arm64 \
          --permission android.permission.INTERNET
adb install -r ../../../engine_arm64/apk/photo_scroller.apk
```

Two things about step 3 that are not obvious and are easy to get wrong:

- The output directory's **name** carries the ABI. `build_apks.py` derives it
  from the directory, so a fetched engine unpacked into a directory called
  `engine` produces an APK whose libraries are packaged under `armeabi-v7a` and
  which installs on nothing. It has to end in `_arm64`.
- `--permission` is off by default and an application that fetches anything
  needs `android.permission.INTERNET`. See [No application could ever open a
  socket](#no-application-could-ever-open-a-socket).

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