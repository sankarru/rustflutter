// Copyright 2013 The Flutter Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package io.flutter.rustflutter;

import android.graphics.SurfaceTexture;
import android.media.MediaPlayer;
import android.view.Surface;
import java.io.IOException;
import java.lang.ref.WeakReference;

/**
 * Video playback for the rustflutter host.
 *
 * <p>There is no platform channel here on purpose. The Activity's whole native
 * boundary is JNI symbols the engine exports, so the bridge is a static Java
 * class the Rust framework calls directly: create a SurfaceTexture, register it
 * with the compositor, hand the producer end (a Surface) to MediaPlayer, and let
 * frames arrive through the SurfaceTexture's OnFrameAvailableListener.
 *
 * <p>The result is the same shape as video_player: a texture id the compositor
 * owns, a producer writing frames into it, and a Rust `Texture` layer that
 * composites it. MediaPlayer is the platform player, so there is no ExoPlayer
 * dependency to resolve and no second codec stack to configure.
 */
public final class VideoBridge {
  private VideoBridge() {}

  /** One playing (or paused) player and the surface it writes frames into. */
  private static final class Handle {
    final long textureId;
    final SurfaceTexture surfaceTexture;
    final Surface surface;
    final MediaPlayer player;

    Handle(long textureId, SurfaceTexture surfaceTexture) {
      this.textureId = textureId;
      this.surfaceTexture = surfaceTexture;
      this.surface = new Surface(surfaceTexture);
      this.player = new MediaPlayer();
    }
  }

  /**
   * The weak reference the engine's JNI facade expects.
   *
   * <p>This is the same object FlutterRenderer passes to
   * PlatformViewAndroidJNIImpl: a WeakReference whose referent is the
   * SurfaceTexture, not the SurfaceTexture itself. Handle holds the strong
   * reference, so the referent cannot be collected while the player runs.
   */
  private static WeakReference<SurfaceTexture> weak(SurfaceTexture texture) {
    return new WeakReference<>(texture);
  }

  private static final java.util.Map<Long, Handle> HANDLES = new java.util.HashMap<>();

  private static native long nativeCreateTexture(WeakReference<SurfaceTexture> surfaceTexture);

  private static native void nativeMarkFrameAvailable(long id);

  private static native void nativeDisposeTexture(long id);

  /**
   * Creates a player and registers its texture with the compositor.
   *
   * <p>{@code width}/{@code height} are the buffer size of the SurfaceTexture,
   * which is what makes the incoming frame arrive already at the size the
   * compositor wants; the producer scales into it.
   */
  public static synchronized long create(int width, int height) {
    SurfaceTexture surfaceTexture = new SurfaceTexture(/* textureName= */ 0);
    surfaceTexture.setDefaultBufferSize(width, height);
    long id = nativeCreateTexture(weak(surfaceTexture));
    if (id < 0) {
      surfaceTexture.release();
      return id;
    }
    final Handle handle = new Handle(id, surfaceTexture);
    surfaceTexture.setOnFrameAvailableListener(
        new SurfaceTexture.OnFrameAvailableListener() {
          @Override
          public void onFrameAvailable(SurfaceTexture texture) {
            nativeMarkFrameAvailable(handle.textureId);
          }
        });
    HANDLES.put(id, handle);
    return id;
  }

  /** Sets the source and prepares the player. Must be called off the main thread for network URLs. */
  public static synchronized void setSource(long id, String url) throws IOException {
    Handle handle = HANDLES.get(id);
    if (handle == null) {
      return;
    }
    handle.player.reset();
    handle.player.setDataSource(url);
    handle.player.setSurface(handle.surface);
    handle.player.setOnPreparedListener(
        new MediaPlayer.OnPreparedListener() {
          @Override
          public void onPrepared(MediaPlayer player) {
            // setSource is asynchronous (prepareAsync), so starting here is the
            // only point at which the player is in a state that accepts it.
            player.start();
          }
        });
    handle.player.prepareAsync();
  }

  public static synchronized void play(long id) {
    Handle handle = HANDLES.get(id);
    if (handle != null) {
      handle.player.start();
    }
  }

  public static synchronized void pause(long id) {
    Handle handle = HANDLES.get(id);
    if (handle != null && handle.player.isPlaying()) {
      handle.player.pause();
    }
  }

  public static synchronized void release(long id) {
    Handle handle = HANDLES.remove(id);
    if (handle == null) {
      return;
    }
    try {
      handle.player.reset();
      handle.player.release();
    } catch (IllegalStateException ignored) {
      // Already released; the Surface below still has to go.
    }
    handle.surface.release();
    handle.surfaceTexture.release();
    nativeDisposeTexture(id);
  }
}