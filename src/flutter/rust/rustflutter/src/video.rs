// Copyright 2013 The Flutter Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Video playback, Android only.
//!
//! The player is the platform's own `MediaPlayer`, reached through
//! [`VideoBridge`](../../host/android/io/flutter/rustflutter/VideoBridge.java):
//! the Activity's native boundary is JNI, not a platform channel, so this calls
//! the Java static methods directly through `ndk_context` and takes back the
//! texture id the compositor registered.
//!
//! A `VideoPlayer` is not `Send`: it owns a Java object graph and an engine
//! texture id, both of which belong to the thread that created them.

use std::sync::OnceLock;

use jni::objects::JValue;
use jni::sys::{jint, jlong, JNIEnv};

const BRIDGE: &str = "io/flutter/rustflutter/VideoBridge";

/// A video player and the compositor texture it renders into.
pub struct VideoPlayer {
    texture_id: jlong,
    released: bool,
}

impl VideoPlayer {
    /// Creates a player and registers a `SurfaceTexture` with the compositor.
    ///
    /// The buffer size is the size frames arrive at, so the producer scales
    /// into it and the compositor can draw it without a crop or a UV transform.
    pub fn create(width: i32, height: i32) -> Result<VideoPlayer, jni::errors::Error> {
        let mut env = attach()?;
        let class = env.find_class(BRIDGE)?;
        let (w, h) = (jint::from(width), jint::from(height));
        let id = env
            .call_static_method(&class, "create", "(II)J", &[JValue::Int(w), JValue::Int(h)])?
            .j()?;
        if id < 0 {
            return Err(jni::errors::Error::NullPtr("nativeCreateTexture refused"));
        }
        Ok(VideoPlayer {
            texture_id: id,
            released: false,
        })
    }

    /// The compositor texture id, for a `Texture` layer.
    pub fn texture_id(&self) -> i64 {
        self.texture_id
    }

    /// Sets and prepares the source. Network URLs are prepared asynchronously
    /// by `MediaPlayer`; this call only hands over the data source and surface.
    pub fn set_source(&mut self, url: &str) -> Result<(), jni::errors::Error> {
        let mut env = attach()?;
        let class = env.find_class(BRIDGE)?;
        let jurl = env.new_string(url)?;
        env.call_static_method(
            &class,
            "setSource",
            "(JLjava/lang/String;)V",
            &[JValue::Long(self.texture_id), JValue::Object(&jurl)],
        )?;
        Ok(())
    }

    pub fn play(&self) -> Result<(), jni::errors::Error> {
        let mut env = attach()?;
        let class = env.find_class(BRIDGE)?;
        env.call_static_method(
            &class,
            "play",
            "(J)V",
            &[JValue::Long(self.texture_id)],
        )?;
        Ok(())
    }

    pub fn pause(&self) -> Result<(), jni::errors::Error> {
        let mut env = attach()?;
        let class = env.find_class(BRIDGE)?;
        env.call_static_method(
            &class,
            "pause",
            "(J)V",
            &[JValue::Long(self.texture_id)],
        )?;
        Ok(())
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Ok(mut env) = attach() {
            if let Ok(class) = env.find_class(BRIDGE) {
                let _ = env.call_static_method(
                    &class,
                    "release",
                    "(J)V",
                    &[JValue::Long(self.texture_id)],
                );
            }
        }
    }
}

static JAVA_VM: OnceLock<jni::JavaVM> = OnceLock::new();

fn attach() -> Result<jni::JNIEnv<'static>, jni::errors::Error> {
    let ctx = ndk_context::android_context();
    if ctx.vm().is_null() {
        return Err(jni::errors::Error::NullPtr("ndk_context has no vm"));
    }
    let vm = JAVA_VM.get_or_init(|| {
        unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
            .expect("ndk_context carried an invalid JavaVM pointer")
    });
    vm.attach_current_thread_permanently()
}