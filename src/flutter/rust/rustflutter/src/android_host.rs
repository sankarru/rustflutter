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

/// The ICU data that ships beside the native libraries, as a C string, or `None`
/// off Android.
///
/// Java knows where the APK's native libraries were extracted to and the engine
/// has no way to find that on its own: `fml::paths::GetExecutablePath()` is
/// `{false, ""}` on Android, so the usual "look next to the executable" fallback
/// cannot resolve. `icudtl.dat` is staged next to the libraries, so joining that
/// directory with the name the engine hardcodes gives a real path.
///
/// Leaked on purpose. It is asked for once during startup and read by the
/// engine for the life of the process, so handing out a borrowed pointer would
/// only invite a use-after-free.
#[cfg(target_os = "android")]
pub fn icu_data_path() -> Option<std::ffi::CString> {
    use jni::objects::JObject;

    let ctx = ndk_context::android_context();
    if ctx.vm().is_null() || ctx.context().is_null() {
        return None;
    }
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    // The Activity that seeded ndk_context, and so the ApplicationInfo that
    // says where the APK's native libraries were extracted.
    let activity = unsafe { JObject::from_raw(ctx.context() as jni::sys::jobject) };
    let info = env
        .call_method(
            &activity,
            "getApplicationInfo",
            "()Landroid/content/pm/ApplicationInfo;",
            &[],
        )
        .ok()?
        .l()
        .ok()?;
    let dir = env
        .get_field(&JObject::from(info), "nativeLibraryDir", "Ljava/lang/String;")
        .ok()?
        .l()
        .ok()?;
    let dir: String = env
        .get_string(&JObject::from(dir).into())
        .ok()?
        .into();
    std::ffi::CString::new(format!("{dir}/icudtl.dat")).ok()
}

#[cfg(not(target_os = "android"))]
pub fn icu_data_path() -> Option<std::ffi::CString> {
    None
}
