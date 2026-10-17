// Copyright 2013 The Flutter Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package io.flutter.rustflutter;

import android.content.Context;
import android.view.View;
import android.view.ViewGroup;
import android.widget.FrameLayout;

import java.util.HashMap;
import java.util.Map;

/**
 * Where platform views live.
 *
 * <p>A platform view is a real Android {@code View} -- a WebView, a map, a video
 * player -- that the framework positions but does not draw. It is parented into
 * the window <em>beside</em> the Flutter surface, and Android composites it over
 * the top. That is hybrid composition, and it is what upstream does too; the
 * difference here is that upstream has {@code FlutterViewLayout} and this tree
 * had one {@code SurfaceView} and no container at all.
 *
 * <p><b>Why the surface has to be translucent.</b> A {@code SurfaceView} is a
 * separate hardware surface, normally composited <em>behind</em> the window, so a
 * sibling {@code View} cannot draw over it. Two flags fix that, and they are
 * worth stating together because neither is sufficient:
 *
 * <ul>
 *   <li>{@code setZOrderMediaOverlay(true)} puts the surface behind the window's
 *       own views while keeping it above other surfaces.
 *   <li>{@code PixelFormat.TRANSLUCENT} makes the surface carry an alpha
 *       channel, so the window composites it with what is behind.
 * </ul>
 *
 * <p><b>No alpha hole is punched in the Flutter output.</b> Upstream clears the
 * region where a platform view sits so that <em>translucent</em> platform views
 * blend correctly. This does not: an opaque {@code View} draws over the surface
 * regardless of what the surface contains, and a WebView is opaque. That is
 * what keeps this to a container and a registry. The cost is that a translucent
 * platform view will show whatever Flutter painted underneath it, which is a
 * real limitation rather than an oversight -- see {@link #setOpaqueOnly}.
 *
 * <p><b>Positioning is absolute on purpose.</b> The framework pushes each view's
 * rectangle in window coordinates, so {@code FrameLayout.LayoutParams} left/top
 * are set in pixels and the children are not laid out by anything but us. Using
 * {@code FrameLayout}'s own layout pass for children would mean translating the
 * framework's rectangles into constraints, and there is no second opinion
 * available: the platform view's size is not a function of its content.
 */
public class PlatformViewContainer extends FrameLayout {
  /** The Activity's own surface, kept so a rectangle can be expressed against it. */
  private final View surface;

  /** Live views by id. A map rather than a list, because id is the lookup. */
  private final Map<Long, View> views = new HashMap<>();

  /**
   * viewType -> factory.
   *
   * <p>Upstream this is the plugin registrar, and a missing entry is a plugin
   * that was never installed. Here there are no plugins, so this is the whole
   * registry: one entry per kind of view this build knows how to make.
   */
  private final Map<String, PlatformViewFactory> factories = new HashMap<>();

  public PlatformViewContainer(Context context, View surface) {
    super(context);
    this.surface = surface;
    addView(surface);
  }

  /**
   * Registers how to make a view of {@code viewType}.
   *
   * <p>The name is the whole of the protocol: the framework's
   * {@code PlatformViewCreationParams} carries a string and nothing else, so
   * this is what a {@code WebView} is called by. The id says which one; the type
   * says what kind.
   */
  public void registerFactory(String viewType, PlatformViewFactory factory) {
    factories.put(viewType, factory);
  }

  public boolean hasFactory(String viewType) {
    return factories.containsKey(viewType);
  }

  /**
   * Creates and adds the view {@code viewId} of kind {@code viewType}.
   *
   * @return whether it was made. A {@code false} here is what an unregistered
   *     type looks like, and it is deliberately not an exception: the framework
   *     will go on emitting rectangles for a view that does not exist, and the
   *     right answer is that nothing appears rather than that the frame dies.
   */
  public boolean create(long viewId, String viewType, Context context) {
    if (views.containsKey(viewId)) {
      return true;
    }
    PlatformViewFactory factory = factories.get(viewType);
    if (factory == null) {
      return false;
    }
    View view = factory.create(context);
    if (view == null) {
      return false;
    }
    LayoutParams params = new LayoutParams(0, 0);
    params.leftMargin = 0;
    params.topMargin = 0;
    view.setLayoutParams(params);
    views.put(viewId, view);
    addView(view);
    return true;
  }

  /** Moves and resizes, in window pixels. Sent every frame the rectangle changes. */
  public void setBounds(long viewId, int left, int top, int width, int height) {
    View view = views.get(viewId);
    if (view == null) {
      return;
    }
    LayoutParams params = (LayoutParams) view.getLayoutParams();
    // Written directly rather than through requestLayout(): these are absolute
    // window coordinates handed down by the framework, not a layout pass the
    // container is going to recompute differently.
    if (params.leftMargin != left || params.topMargin != top
        || params.width != width || params.height != height) {
      params.leftMargin = left;
      params.topMargin = top;
      params.width = width;
      params.height = height;
      view.setLayoutParams(params);
    }
  }

  /**
   * Hides without destroying.
   *
   * <p>A view scrolled out of a viewport is still alive and still holds its
   * state; destroying it would reload the page. This is the difference between
   * {@code View.GONE} and removing the child, and it is the reason the framework
   * sends visibility separately from bounds.
   */
  public void setVisible(long viewId, boolean visible) {
    View view = views.get(viewId);
    if (view != null) {
      view.setVisibility(visible ? View.VISIBLE : View.GONE);
    }
  }

  /** Destroys the view and removes it from the window. */
  public void destroy(long viewId) {
    View view = views.remove(viewId);
    if (view != null) {
      removeView(view);
      // Stop it before dropping the reference: a WebView holds a JavaScript VM
      // and keeps running until it is told not to.
      if (view instanceof android.webkit.WebView) {
        ((android.webkit.WebView) view).destroy();
      }
    }
  }

  /** Live ids, for a test that asserts a view outlived a scroll. */
  public int liveCount() {
    return views.size();
  }

  /**
   * Whether only opaque platform views are supported.
   *
   * <p>True, because nothing punches a hole in the Flutter surface. An opaque
   * view covers it; a translucent one would blend with whatever Flutter painted
   * underneath rather than with what is behind the window. Recorded here rather
   * than left as a surprise because it is the one behaviour that differs from
   * upstream, and upstream does support the translucent case.
   */
  public static boolean setOpaqueOnly() {
    return true;
  }

  /** The Flutter surface, for a child that has to measure against it. */
  public View surface() {
    return surface;
  }

  @Override
  protected void onLayout(boolean changed, int left, int top, int right, int bottom) {
    // The surface fills the window, always.
    surface.layout(0, 0, right - left, bottom - top);
    // Children carry absolute rectangles from the framework and are not laid out
    // here; calling measure/layout on them would overwrite those.
    for (int i = 0; i < getChildCount(); i++) {
      View child = getChildAt(i);
      if (child == surface) {
        continue;
      }
      LayoutParams params = (LayoutParams) child.getLayoutParams();
      child.measure(
          View.MeasureSpec.makeMeasureSpec(params.width, View.MeasureSpec.EXACTLY),
          View.MeasureSpec.makeMeasureSpec(params.height, View.MeasureSpec.EXACTLY));
      child.layout(params.leftMargin, params.topMargin,
          params.leftMargin + child.getMeasuredWidth(),
          params.topMargin + child.getMeasuredHeight());
    }
  }

  @Override
  protected void onMeasure(int widthMeasureSpec, int heightMeasureSpec) {
    int width = MeasureSpec.getSize(widthMeasureSpec);
    int height = MeasureSpec.getSize(heightMeasureSpec);
    surface.measure(
        MeasureSpec.makeMeasureSpec(width, MeasureSpec.EXACTLY),
        MeasureSpec.makeMeasureSpec(height, MeasureSpec.EXACTLY));
    setMeasuredDimension(width, height);
  }

  /** Makes one kind of platform view. */
  public interface PlatformViewFactory {
    View create(Context context);
  }

  /** The WebView, registered under the type name the framework asks for. */
  public static class WebViewFactory implements PlatformViewFactory {
    @Override
    public View create(Context context) {
      android.webkit.WebView view = new android.webkit.WebView(context);
      // The WebView must know its size before it loads or it lays out once with
      // zero and the page never reflows.
      view.getSettings().setJavaScriptEnabled(true);
      view.getSettings().setDomStorageEnabled(true);
      view.setLayoutParams(new LayoutParams(0, 0));
      return view;
    }
  }
}