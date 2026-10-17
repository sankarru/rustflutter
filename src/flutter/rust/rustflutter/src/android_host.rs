// Copyright 2013 The Flutter Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Android host half: JVM entry points the Activity calls.
//!
//! Compiled only for Android (`cfg(target_os)` in `lib.rs`), so the host test
//! binary, `--cfg rustflutter_stubs` builds and the GN build never see the
//! `jni` / `ndk-context` dependencies this needs.
//!
//! ## ndk_context
//!
//! The entry point is `nativeInitNdkContext`, called from
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
//!
//! ## Platform views
//!
//! The rest drives [`crate::platform_view`]'s host half. Those calls come from
//! the engine, not the JVM, so they differ in three ways and each difference is
//! a bug if forgotten:
//!
//! **The thread.** The engine's platform thread is not a Java thread and has no
//! `JNIEnv`, so the first thing every call does is attach. A `JNIEnv` is
//! thread-local; one fetched on the engine's thread and kept would be a
//! use-after-free the moment the platform thread was joined.
//!
//! **The id.** `rf_platform_view_*` carries `i64` and the Java parameter is a
//! `long`, which is the only primitive that holds it. Passing it as an `int`
//! truncates, and the framework's ids *can* be negative -- `platform_view.rs`
//! numbers them from an offset -- so this is not hypothetical.
//!
//! **The UI thread.** A `View` may only be created, measured or removed on the
//! main looper. These run on the engine's thread, so the work is posted. See
//! [`on_ui_thread`] for why it posts rather than posts-and-forgets.

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

// -- Platform views ------------------------------------------------------------

/// Where a platform view call actually runs.
///
/// A `View` may only be created, measured, hidden or removed on the main looper,
/// and every call in this section arrives on the engine's platform thread. Each
/// therefore posts to the UI and **waits for it to finish**.
///
/// Waiting is the load-bearing part. A bounds update that returns before the
/// view has moved is a frame where the WebView is somewhere the framework no
/// longer thinks it is -- visible as a lag or a flicker during a scroll. The
/// wait costs a posted runnable that the UI was going to execute anyway, since
/// the framework sends the next bounds immediately; what it avoids is a
/// pipeline of updates racing the measure pass.
///
/// It is also why the work is described by a tag rather than a closure. A
/// closure cannot cross into Java, so passing one would mean parking it in a
/// global slot for the Java side to call back into -- a hand-off where a dropped
/// closure is a silently dropped update, and where the slot has to be
/// reclaimed. Twelve lines of arithmetic and four `match` arms on the Java side
/// cost less than getting that right.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiTask {
    /// Create the view `id` of kind `view_type`, with no params.
    Create { id: i64, view_type: i32 },
    MoveResize { id: i64, x: i32, y: i32, width: i32, height: i32 },
    SetVisible { id: i64, visible: bool },
    Destroy { id: i64 },
}

/// The `PlatformViewContainer`, looked up on the Activity.
///
/// `None` when there is none, which is a real state rather than an error: the
/// field is assigned in `onCreate`, and a platform view can be pushed before
/// then. Every caller treats it as "do nothing", because a view with nowhere to
/// go should not appear rather than end the frame.


/// Runs `task` against the Activity's platform view container, on the UI thread.
///
/// Returns whether the host acted. `false` covers every "nothing to do" case --
/// no context seeded, no container yet, no factory for the type, the post
/// refused -- and they are deliberately the same answer: the framework will
/// carry on emitting rectangles for a view that does not exist, and the frame
/// must not die because one view could not be placed.
pub fn on_ui_thread(task: UiTask) -> bool {
    let Some(vm) = java_vm() else {
        eprintln!("rustflutter: platform view task with no JavaVM");
        return false;
    };
    // Attach: the engine's platform thread has no JNIEnv of its own. Detached
    // again below whether or not the post succeeded, or the attachment leaks
    // one thread's worth of JVM state per call.
    let Ok(mut env) = (unsafe { vm.attach_current_thread() }) else {
        eprintln!("rustflutter: could not attach the engine thread to the JVM");
        return false;
    };

    let result = post_task(&mut env, task);

    let _ = unsafe { vm.detach_current_thread() };
    result
}

/// The `JavaVM`, from the context `nativeInitNdkContext` seeded.
fn java_vm() -> Option<jni::JavaVM> {
    let context = ndk_context::android_context();
    // `vm()` is a raw pointer and an unseeded context hands back null. Reading
    // it through `JavaVM::from_raw` is what would be unsound, so it is checked
    // first.
    if context.vm().is_null() {
        return None;
    }
    // Safety: the pointer came from `ndk_context`, which was handed the VM by
    // `initialize_android_context` and has kept it since.
    // `from_raw` wants the invoke interface, and `ndk_context` documents the
    // cast itself; `vm()` is the JavaVM as an opaque pointer.
    unsafe { jni::JavaVM::from_raw(context.vm().cast()) }.ok()
}

/// Posts one task to the main looper.
///
/// The Activity has a static `dispatchPlatformViewTask(int, long, int, int, int,
/// int, boolean)` for this; doing it there rather than through a `Runnable`
/// subclass is what keeps the closure problem away. See [`UiTask`].
fn post_task(env: &mut jni::JNIEnv, task: UiTask) -> bool {
    // The class is taken from the Activity, not looked up by name.
    //
    // `find_class` on a thread that is not a Java thread resolves through the
    // *system* classloader, which cannot see the application's own classes. The
    // symptom is a ClassNotFoundException for a class that is plainly in the
    // dex, raised on the first platform view task and nothing else -- which is
    // what ANDROID-BUILD-NOTES.md records for mobile-sentinel hitting the same
    // wall. `get_object_class` asks the object's own classloader instead.
    let context = ndk_context::android_context();
    if context.context().is_null() {
        eprintln!("rustflutter: ndk_context has no Activity, so no host class");
        return false;
    }
    let activity =
        unsafe { jni::objects::JObject::from_raw(context.context() as jni::sys::jobject) };
    let Ok(class) = env.get_object_class(&activity) else {
        eprintln!("rustflutter: could not read the Activity's own class");
        return false;
    };
    // Five int slots, not four: a bounds update carries left, top, width *and*
    // height. With one slot short, every value landed one position early and the
    // height was simply never sent -- which reads as a WebView the right width
    // and a third of the right height, offset by the width. Nothing crashes and
    // nothing complains, which is why this is worth stating rather than leaving
    // to the descriptor to catch.
    //
    // `a..d` are the bounds, so the Java side reads them the same way whichever
    // task it is; `e` carries the type index for creation, where the bounds are
    // meaningless.
    let (id, tag, a, b, c, d, e, visible) = match task {
        UiTask::Create { id, view_type } => (id, 0, 0, 0, 0, 0, view_type, false),
        UiTask::MoveResize { id, x, y, width, height } => (id, 1, x, y, width, height, 0, false),
        UiTask::SetVisible { id, visible } => (id, 2, 0, 0, 0, 0, 0, visible),
        UiTask::Destroy { id } => (id, 3, 0, 0, 0, 0, 0, false),
    };
    // `(JIIIIIIZ)Z`: a `long` id, an `int` tag, five `int` arguments and a
    // `boolean`, returning a `boolean`.
    //
    // **Every type here is upper case.** JNI's descriptor alphabet is `Z B C S I
    // J F D V [ L...;` -- there is no lower-case `i`, and one is not an int, it
    // is a parse error. That failure is worth spelling out because it is
    // invisible from the Rust side: `jni` rejects the descriptor before it ever
    // reaches the JVM, the `Err` is the same shape as any other, and the symptom
    // is a host call that answers false forever with nothing in logcat. Verified
    // against jni 0.21.1: `(JIi)Z` is rejected while `(JI)Z` and `(IIIIIII)Z`
    // both parse.
    let signature = "(JIIIIIIZ)Z";
    // `call_static_method`, not `call_method`: the Java side is a static entry
    // point reached through the class, because the engine's platform thread has
    // no Activity instance to call it on. `call_method` searches for an
    // *instance* method and answers `no non-static method` -- a `NoSuchMethodError`
    // raised on the engine's thread, which on Android is a hard crash rather
    // than an error return.
    let outcome = env.call_static_method(
        jni::objects::JClass::from(class),
        "dispatchPlatformViewTask",
        signature,
        &[
            jni::objects::JValue::Long(id),
            jni::objects::JValue::Int(tag),
            jni::objects::JValue::Int(a),
            jni::objects::JValue::Int(b),
            jni::objects::JValue::Int(c),
            jni::objects::JValue::Int(d),
            jni::objects::JValue::Int(e),
            // `jni` 0.21 takes a boolean as u8, not as Rust's bool.
            jni::objects::JValue::Bool(u8::from(visible)),
        ],
    );
    match outcome {
        Ok(value) => value.z().unwrap_or(false),
        Err(error) => {
            // Not swallowed. A failed call is the whole difference between a
            // view that appears and one that does not, and there is nothing else
            // that would ever show it.
            eprintln!("rustflutter: dispatchPlatformViewTask failed: {error}");
            false
        }
    }
}

/// `rf_platform_view_create`'s body, for [`crate::platform_view::EngineHost`].
///
/// Encoded params are **not** passed across. There is no codec call on this
/// side of the C ABI, so the bytes would have to be decoded by the Activity to
/// be useful and there is nothing on this side that can decode them; the
/// registry answers by type name, which is all the container needs.
///
/// The consequence is worth stating rather than hiding: a plugin that wanted to
/// pass creation params cannot, because the channel that would carry them does
/// not exist in this tree. Upstream's `PlatformViewsChannel` does, and it is
/// present in `shell/platform/android` -- unused. Reinstating it is the change
/// that makes params work, and it is a separate decision from making a WebView
/// appear.
pub fn platform_view_create(view_id: i64, _view_type: &str) -> bool {
    // The name does not cross JNI: it would need a jstring with its own local
    // ref and release, and a dropped release leaks one per frame. What crosses is
    // an index into the container's own registry, which is fixed before the
    // first frame. `WEBVIEW` is that registry's first entry, registered in
    // RustflutterActivity.onCreate.
    on_ui_thread(UiTask::Create {
        id: view_id,
        view_type: WEBVIEW_TYPE_INDEX,
    })
}

/// `PlatformViewContainer`'s index for "webview".
///
/// Tied to the order `registerFactory` is called in, which is the Activity's
/// `onCreate`. Zero because it is the only registration; a second kind of view
/// makes this a lookup rather than a constant, and that is the point at which a
/// name should start crossing instead of an index.
const WEBVIEW_TYPE_INDEX: i32 = 0;

pub fn platform_view_destroy(view_id: i64) -> bool {
    on_ui_thread(UiTask::Destroy { id: view_id })
}

pub fn platform_view_set_bounds(view_id: i64, left: f32, top: f32, width: f32, height: f32) -> bool {
    // Truncation to i32 is deliberate and is what a platform view wants: the
    // container lays out in integer pixels, so a fractional rectangle would be
    // rounded by `View.layout` anyway. Rounding here means the framework and the
    // view agree on the number rather than disagreeing by a sub-pixel.
    on_ui_thread(UiTask::MoveResize {
        id: view_id,
        x: left.round() as i32,
        y: top.round() as i32,
        width: width.round() as i32,
        height: height.round() as i32,
    })
}

pub fn platform_view_set_visible(view_id: i64, visible: bool) -> bool {
    on_ui_thread(UiTask::SetVisible { id: view_id, visible })
}