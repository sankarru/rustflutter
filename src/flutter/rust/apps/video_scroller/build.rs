// Links an application against the prebuilt shared engine.
//
// This is the whole point of building the engine elsewhere: nothing here compiles
// C++, and nothing here knows what the engine needs. The engine build writes that
// down in `rustflutter_link.txt` -- read off the shared target's own link line by
// //flutter/rust:rustflutter_link_manifest rather than copied into a file that
// would rot -- and this reads it.
//
// Point RUSTFLUTTER_ENGINE_OUT at the directory holding `librustflutter_engine.so`
// and `rustflutter_link.txt` together; both arrive in the same artifact from the
// android-shared-engine workflow.
//
//   cargo build --release --target aarch64-linux-android
//
// `lib` and `framework` from the manifest are deliberately ignored. They are what
// the *archive* needs, and the manifest says so: a shared library resolved all of
// them at its own link time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn main() {
    let engine = PathBuf::from(
        std::env::var("RUSTFLUTTER_ENGINE_OUT").expect(
            "RUSTFLUTTER_ENGINE_OUT must name the directory holding \
             librustflutter_engine.so and rustflutter_link.txt. Download one from \
             the rustflutter-engine-<abi> artifact of the android-shared-engine \
             workflow, or build it with \
             `ninja -C out/<dir> flutter/rust:rustflutter_engine_shared`.",
        ),
    );

    let manifest_path = engine.join("rustflutter_link.txt");
    println!("cargo:rerun-if-env-changed=RUSTFLUTTER_ENGINE_OUT");
    println!("cargo:rerun-if-env-changed=RUSTFLUTTER_NDK_UNWIND_DIR");
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let manifest = read_manifest(&manifest_path);

    // The engine has to have been built for this target, or the link fails on
    // every rf_* symbol with a message that says nothing about the cause.
    match manifest.get("os").map(String::as_str) {
        Some("android") => {}
        Some(other) => panic!(
            "rustflutter_link.txt says os = {other}, not android. This engine was \
             built for another platform."
        ),
        None => panic!("rustflutter_link.txt has no os."),
    }

    let library = manifest.get("library").expect("no `library` in the manifest");
    let link_name = manifest
        .get("library_link_name")
        .expect("no `library_link_name` in the manifest");
    let rpath = manifest.get("library_rpath").map(String::as_str).unwrap_or("");

    let path = engine.join(library);
    if !path.is_file() {
        panic!(
            "no {library} in {}. The engine artifact is the library and \
             rustflutter_link.txt together; if only the manifest arrived, the \
             download was partial.",
            engine.display()
        );
    }

    println!("cargo:rustc-link-search=native={}", engine.display());
    println!("cargo:rustc-link-lib=dylib={link_name}");

    // Where the loader is told to look. On Android this is $ORIGIN, which
    // resolves inside the APK's lib/<abi>/, where build_apks.py packages the
    // engine beside this library -- so the two are found without a path baked in.
    if !rpath.is_empty() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
    }

    // Rust's standard library unwinds, for panics and for the backtrace it prints
    // with one. The engine is built -fno-exceptions and links -nostdlib, so
    // nothing in it pulls the unwinder in, and this application is a cdylib with
    // nothing else to supply it. Static, from the NDK's compiler runtime -- the
    // same libunwind.a //flutter/rust/rustflutter_app.gni names for a GN
    // application, and the same reason.
    if let Some(dir) = std::env::var_os("RUSTFLUTTER_NDK_UNWIND_DIR") {
        let dir = Path::new(&dir);
        if !dir.join("libunwind.a").is_file() {
            panic!(
                "RUSTFLUTTER_NDK_UNWIND_DIR={} has no libunwind.a. It wants \
                 $NDK/toolchains/llvm/prebuilt/<host>/lib/clang/<ver>/lib/linux/aarch64.",
                dir.display()
            );
        }
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=unwind");
    }
}

/// `key = value`, one per line, `#` for comments. Repeated keys collapse, which
/// is fine: the only repeated one is `lib`, and this deliberately drops those.
fn read_manifest(path: &Path) -> HashMap<String, String> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            out.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    out
}