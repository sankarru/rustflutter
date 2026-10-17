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
  private static final String TAG = "PlatformViewContainer";

  // The task tags. Named rather than left as numbers at the switch, because the
  // caller that has to know whether a task is worth waiting on -- the Rust side
  // does not wait for a move -- names TAG_CREATE from the other file.
  /** Create the view and answer whether it was made. */
  private static final int TAG_CREATE = 0;
  /** Move and resize. */
  private static final int TAG_MOVE_RESIZE = 1;
  /** Show or hide without destroying. */
  private static final int TAG_SET_VISIBLE = 2;
  /** Destroy and remove. */
  private static final int TAG_DESTROY = 3;

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
    // The index a task carries is the position here. Registration happens in
    // onCreate before any frame, so the list is fixed before the first lookup and
    // an index stays valid for the life of the process.
    if (!typeNames.contains(viewType)) {
      typeNames.add(viewType);
    }
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
    if (viewType == null) {
      return false;
    }
    PlatformViewFactory factory = factories.get(viewType);
    if (factory == null) {
      android.util.Log.w(TAG, "no factory for view type \"" + viewType + "\", id " + viewId);
      return false;
    }
    View view = factory.create(context);
    if (view == null) {
      android.util.Log.w(TAG, "factory for \"" + viewType + "\" returned null, id " + viewId);
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

  /**
   * Moves and resizes. Sent every frame the rectangle changes.
   *
   * <p><b>Logical in, physical out.</b> The framework's rectangles are in logical
   * pixels -- the same units layout and paint work in -- and Android lays out in
   * physical ones. Upstream makes exactly this conversion in
   * {@code PlatformViewsController.toPhysicalPixels}, and it has to happen here
   * for the same reason: a rectangle handed straight to {@code setLayoutParams}
   * lands at the same numeric offset on every device and is therefore correct
   * only on the one whose density is 1. On a 2.625x screen a 387-wide view
   * would be 147 logical pixels wide, a little over a third of the card it is
   * meant to fill.
   */
  public void setBounds(long viewId, int left, int top, int width, int height) {
    View view = views.get(viewId);
    if (view == null) {
      // Not a warning in normal use: the framework emits rectangles for a view
      // it believes exists, and one that could not be created leaves the two
      // out of step. Worth saying once, which is what a `Log.w` in a per-frame
      // path amounts to.
      android.util.Log.w(TAG, "bounds for id " + viewId + ", which is not here");
      return;
    }
    final int physicalLeft = toPhysicalPixels(left);
    final int physicalTop = toPhysicalPixels(top);
    final int physicalWidth = toPhysicalPixels(width);
    final int physicalHeight = toPhysicalPixels(height);
    final LayoutParams params = (LayoutParams) view.getLayoutParams();

    final boolean sizeChanged = params.width != physicalWidth || params.height != physicalHeight;
    final boolean moved = params.leftMargin != physicalLeft || params.topMargin != physicalTop;
    if (!sizeChanged && !moved) {
      return;
    }
    // The params object is the one the view already holds, so writing to it is
    // enough to make onLayout agree; whether it is *pushed* through
    // setLayoutParams depends on what changed.
    params.width = physicalWidth;
    params.height = physicalHeight;
    params.leftMargin = physicalLeft;
    params.topMargin = physicalTop;

    if (sizeChanged) {
      // Stated rather than inherited: FrameLayout's default gravity is
      // TOP|START, and the position here is absolute.
      params.gravity = android.view.Gravity.LEFT | android.view.Gravity.TOP;
    }
    // **Always through setLayoutParams, for a move as well as a resize.**
    //
    // Calling `view.layout(...)` directly looks cheaper and is wrong. Outside a
    // layout traversal Android does not recompute which regions the view covers,
    // so the strip it has just vacated is never invalidated: it keeps whatever
    // was last drawn there, and the view's own content is clipped against a
    // stale rectangle. That is the "contents hide behind the rendering limits"
    // -- a hard-edged band of the WebView that goes blank while it scrolls, and
    // a band of stale pixels trailing it.
    //
    // `setLayoutParams` is a `requestLayout`, and the traversal it schedules is
    // what invalidates both the old and the new rectangle. The cost is the one
    // traversal per frame of a scroll, which is what every Android view pays for
    // moving, and the WebView is not re-measured by it -- onLayout only measures
    // a child whose size is not already right.
    view.setLayoutParams(params);
  }

  /**
   * Logical pixels to physical, as upstream's {@code PlatformViewsController} does it.
   *
   * <p>{@code displayMetrics.density} rather than a stored ratio, so it follows a
   * configuration change instead of going stale for the life of the process.
   */
  private int toPhysicalPixels(double logicalPixels) {
    return (int) Math.round(logicalPixels * getResources().getDisplayMetrics().density);
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
      // Measured only when the size is not already right. `setLayoutParams`
      // forces a layout, and a forced layout makes `View.measure` run again --
      // which for a WebView means dropping and rebuilding its content, the same
      // flicker as re-measuring per frame, here just less often.
      if (child.getMeasuredWidth() != params.width || child.getMeasuredHeight() != params.height) {
        child.measure(
            View.MeasureSpec.makeMeasureSpec(params.width, View.MeasureSpec.EXACTLY),
            View.MeasureSpec.makeMeasureSpec(params.height, View.MeasureSpec.EXACTLY));
      }
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

  /**
   * Runs one framework task on the UI thread and waits.
   *
   * <p>Static, and reaching a {@code private} container method through it,
   * because the caller is the engine's platform thread: it holds no Activity to
   * hand us and no {@code JNIEnv} that outlives the call. The Activity finds its
   * own container through {@link #sInstance}.
   *
   * <p>The task is a tag plus seven scalars rather than a callback. A
   * {@code Runnable} carrying a closure cannot be built from Rust without parking
   * that closure in a global for Java to call back into, and a parked closure
   * that gets dropped is a bounds update that silently never happens -- which
   * during a scroll looks like the WebView lagging behind the list. Twelve lines
   * of arithmetic here is cheaper than getting that hand-off right.
   *
   * <p><b>Waiting is load-bearing.</b> A bounds update that returns before the
   * view has moved leaves the view somewhere the framework no longer thinks it
   * is, for a frame. The wait costs a post the UI was going to run anyway,
   * because the framework sends the next bounds immediately.
   *
   * @return whether the container acted. False covers "no container yet", "no
   *     factory for that type" and "the view is gone": all three mean the same
   *     thing to the caller, and none of them should end a frame.
   */
  static boolean dispatchPlatformViewTask(
      long id, int tag, int a, int b, int c, int d, int e, boolean visible) {
    final PlatformViewContainer container = sInstance;
    if (container == null) {
      android.util.Log.w(TAG, "platform view task " + tag + " for id " + id + " with no container");
      return false;
    }
    // **This method has to move the work to the main looper, and the post has to
    // happen here rather than in Rust.** Two facts force it:
    //
    // The caller is the engine's platform thread, which is not a Java thread. A
    // `View` may only be built, measured, moved or removed on the main looper,
    // and the WebView in particular dies on the spot otherwise: constructing one
    // builds a `Handler`, whose constructor dereferences `Looper.mQueue` on the
    // *current* thread -- null on a thread that has never looped -- so it dies
    // with a NullPointerException before any of our code runs. That is the
    // symptom, and it is a hard crash on the engine's thread, not an error.
    //
    // And it cannot be done from Rust: `Handler`/`Looper` are only reachable
    // through a `jobject`, and posting a `Runnable` built in Rust would mean
    // parking a closure in a global for Java to call back into -- a hand-off
    // where a dropped closure is a bounds update that silently never happens.
    // Twelve lines of scalars and a `match` is cheaper than getting that right.
    //
    // Already on the UI thread is handled by running the task inline. Posting and
    // then waiting would deadlock against ourselves, and "who calls this" is not a
    // question worth answering wrongly with a hang.
    if (android.os.Looper.myLooper() == android.os.Looper.getMainLooper()) {
      return dispatchNow(container, id, tag, a, b, c, d, e, visible);
    }
    final UiTask task = new UiTask(container, id, tag, a, b, c, d, e, visible);
    // `View.post` rather than a bare Handler: it resolves to the looper this view
    // is attached to, which is the main one, and it degrades sanely if the view
    // is not attached yet rather than posting to nothing.
    if (!container.post(task)) {
      android.util.Log.w(TAG, "post refused for platform view task " + tag + ", id " + id);
      return false;
    }
    // **Bounds and visibility are posted, not posted-and-waited.** An earlier
    // version blocked the caller until the UI thread had run the task, on the
    // argument that a bounds update returning early leaves the view behind the
    // framework's idea of it for a frame. That is wrong about both the cost and
    // the fix. The caller is the engine's render thread, so waiting puts a full
    // UI round trip in the middle of *every* frame of a scroll: the thread
    // producing the frame stops producing until the thread drawing it gets round
    // to it. A fling is a hundred frames a second, and the two threads end up
    // pacing each other -- which is precisely when the view can least afford to
    // be a frame behind.
    //
    // Not waiting is safe because these tasks are ordered and idempotent: they
    // arrive in the order the framework sent them and a rectangle simply
    // overwrites the last, so a backlog costs a few discarded moves and the view
    // lands on the most recent rectangle one UI frame later. That is the same
    // latency every other Android view has.
    //
    // Creation is the exception and still waits. It happens once per view, so the
    // round trip costs nothing, and the caller needs the answer: it is what
    // decides whether the view is recorded as existing, and a framework told
    // "created" about a view that was never made will position a view that is
    // not there.
    return tag == TAG_CREATE ? task.awaitResult() : true;
  }

  /** One task, on its way to the main looper. */
  private static final class UiTask implements Runnable {
    private final PlatformViewContainer container;
    private final long id;
    private final int tag;
    private final int a;
    private final int b;
    private final int c;
    private final int d;
    private final int e;
    private final boolean visible;
    // Present only for the one task that waits. See dispatchPlatformViewTask.
    private final java.util.concurrent.CountDownLatch done;
    private volatile boolean result;

    UiTask(
        PlatformViewContainer container,
        long id,
        int tag,
        int a,
        int b,
        int c,
        int d,
        int e,
        boolean visible) {
      this.container = container;
      this.id = id;
      this.tag = tag;
      this.a = a;
      this.b = b;
      this.c = c;
      this.d = d;
      this.e = e;
      this.visible = visible;
      this.done =
          tag == TAG_CREATE ? new java.util.concurrent.CountDownLatch(1) : null;
    }

    @Override
    public void run() {
      try {
        result = dispatchNow(container, id, tag, a, b, c, d, e, visible);
      } finally {
        // Counted down in a `finally`: a task that throws would otherwise leave
        // the engine's thread blocked on a latch nobody opens, which is a hang
        // rather than a crash and far harder to notice.
        if (done != null) {
          done.countDown();
        }
      }
    }

    boolean awaitResult() {
      try {
        done.await();
      } catch (InterruptedException interrupted) {
        // Restore the flag: swallowing it leaves a thread that believes it was
        // interrupted when it was not, which is its own bug downstream.
        Thread.currentThread().interrupt();
        return false;
      }
      return result;
    }
  }

  /** The work itself, on the main looper. */
  private static boolean dispatchNow(
      PlatformViewContainer container,
      long id,
      int tag,
      int a,
      int b,
      int c,
      int d,
      int e,
      boolean visible) {
    switch (tag) {
      case TAG_CREATE:
        // `e` is the view type as a registry index, and `a..d` are the bounds --
        // which is why a creation task spends its other four slots on nothing.
        // A String would need a jstring crossing this boundary with its own local
        // ref and release, and the set of names is fixed at registration, so an
        // index is stable.
        return container.create(id, container.typeName(e), container.getContext());
      case TAG_MOVE_RESIZE:
        container.setBounds(id, a, b, c, d);
        return true;
      case TAG_SET_VISIBLE:
        container.setVisible(id, visible);
        return true;
      case TAG_DESTROY:
        container.destroy(id);
        return true;
      default:
        return false;
    }
  }

  /** The live container, or null before {@code onCreate} has made one. */
  private static volatile PlatformViewContainer sInstance;

  @Override
  protected void onAttachedToWindow() {
    sInstance = this;
    super.onAttachedToWindow();
  }

  @Override
  protected void onDetachedFromWindow() {
    if (sInstance == this) {
      sInstance = null;
    }
    super.onDetachedFromWindow();
  }

  /** Registered type names, by the index a task carries. */
  private final java.util.ArrayList<String> typeNames = new java.util.ArrayList<>();

  /**
   * The name for a type index, or null when there is no such type.
   *
   * <p>An index rather than the string because a {@code jstring} crossing this
   * boundary needs a local ref and a matching release, and a dropped release is a
   * leaked one per frame.
   */
  String typeName(int index) {
    return index >= 0 && index < typeNames.size() ? typeNames.get(index) : null;
  }

  /** Makes one kind of platform view. */
  public interface PlatformViewFactory {
    View create(Context context);
  }

  /** The WebView, registered under the type name the framework asks for. */
  public static class WebViewFactory implements PlatformViewFactory {
    /**
     * The page this demo loads.
     *
     * <p>A {@code data:} URL on purpose: it renders with no network and no
     * INTERNET permission, so what appears on screen is unambiguously this
     * WebView rendering a document rather than a cached bitmap or the window
     * behind it.
     *
     * <p><b>This is the demo's page, not the API's.</b> Upstream's WebView plugin
     * takes its URL from {@code PlatformViewCreationParams}, which the framework
     * encodes and the host decodes. This tree has no codec call on this side of
     * the C ABI, so {@code PlatformViewCreationParams} carries an id and a type
     * name and nothing else -- the same gap {@code android_host} records. Wiring
     * params properly means reinstating the {@code PlatformViewsChannel} that
     * already sits unused in {@code shell/platform/android}.
     */
    private static final String DEMO_PAGE =
        "data:text/html;charset=utf-8,"
            + "<html><body style='margin:0;font:600 26px system-ui;background:%23121a24;"
            + "color:%237dd3fc;padding:24px'>"
            + "<div style='font:400 15px system-ui;color:%2394a3b8'>a live Android View,"
            + " drawn over the Flutter surface</div>"
            + "<p>not a Flutter widget</p>"
            + "<p style='color:%2394a3b8'>scrolls with the list, keeps its state</p>"
            + "</body></html>";

    @Override
    public View create(Context context) {
      android.webkit.WebView view = new android.webkit.WebView(context);
      view.getSettings().setJavaScriptEnabled(true);
      view.getSettings().setDomStorageEnabled(true);
      view.setBackgroundColor(0xFF121A24);
      view.setLayoutParams(new LayoutParams(0, 0));
      // Loaded after the view is added and measured, not here: a page loaded
      // against a zero-sized view lays out once at zero and never reflows, which
      // upstream also works around by loading on attach.
      view.post(
          new Runnable() {
            @Override
            public void run() {
              view.loadUrl(DEMO_PAGE);
            }
          });
      return view;
    }
  }
}