// Copyright 2013 The Flutter Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Android host half: JVM entry points the Activity calls.
//!
//! Compiled only for Android (`cfg(target_os)` in `lib.rs`), so the host test
//! binary, `--cfg rustflutter_stubs` builds and the GN build never see the
//! `jni` / `ndk-context` dependencies this needs.
//!
//! The one entry point is `nativeInitNdkContext`, called from
//! `RustflutterActivity.onCreate` once per process. It seeds `ndk_context`
//! with the `JavaVM` and the `Activity`, which is what capability crates
//! (mobile-sentinel) read to reach the JVM and the Activity from threads the
//! JVM never called -- the engine's raster thread, a fetch worker. Without it
//! their first JNI call panics in `android_context()` with "android context
//! was not initialized", which is a native abort, not a catchable error.
//!
//! The `Activity` is kept as a process-lifetime `GlobalRef` on purpose: the
//! local ref the JVM passes dies when this call returns, and `ndk_context`
//! stores a bare pointer, so handing it the local ref would be a use-after-
//! free the first time a capability touches it. Leaking one global ref per
//! process is the documented cost of a process-lifetime context.

use std::ffi::c_void;

/// Called from `RustflutterActivity.onCreate`, after the application's own
/// library is loaded (the symbol lives in it, compiled in from this crate).
///
/// Quiet by contract: there is nothing useful to return and no logger this
/// early, so a failure simply leaves `ndk_context` unseeded and the first
/// capability call fails the way it always did. The Java side calls this at
/// most once per process (a static guard), because `ndk_context` asserts its
/// initializer runs exactly once and a second `onCreate` must not trip that.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_io_flutter_rustflutter_RustflutterActivity_nativeInitNdkContext(
    env: *mut jni::sys::JNIEnv,
    _class: jni::sys::jclass,
    activity: jni::sys::jobject,
) {
    if env.is_null() || activity.is_null() {
        return;
    }
    let env = match unsafe { jni::JNIEnv::from_raw(env) } {
        Ok(e) => e,
        Err(_) => return,
    };
    let vm = match env.get_java_vm() {
        Ok(v) => v,
        Err(_) => return,
    };
    let activity = unsafe { jni::objects::JObject::from_raw(activity) };
    let global = match env.new_global_ref(activity) {
        Ok(g) => g,
        Err(_) => return,
    };
    let vm_ptr = vm.get_java_vm_pointer() as *mut c_void;
    let activity_ptr = global.as_raw() as *mut c_void;
    // Process-lifetime by design (see the module docs): the ref must outlive
    // this call, and nothing ever releases the Activity before process death.
    std::mem::forget(global);
    unsafe {
        ndk_context::initialize_android_context(vm_ptr, activity_ptr);
    }
}
