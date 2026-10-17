//! Upstream `RenderPlatformView` and `AndroidView`, and the glue that puts a
//! real Android View into a window.
//!
//! A platform view is not something the framework draws. It is a live Android
//! `View` -- a WebView, a map, a video player -- that the host parents into the
//! window *beside* the Flutter surface, and Android composites over the top.
//! The framework's entire job is to say where it goes and whether a press lands
//! on it.
//!
//! ## Why nothing in this file has ever run
//!
//! This tree has no platform-view channel. Until now there was no
//! `PlatformViewsDelegate` anywhere in the shell and `Compositor` had no
//! platform-view members at all, so the rules were ported and the producer was
//! missing -- the same shape this tree's own log keeps hitting.
//!
//! What changed is the *host*, not this file:
//!
//! - `rf_layer_tree_push_platform_view` puts a `PlatformViewLayer` in the tree.
//! - The host composites it, which needs no framework help.
//! - `rf_platform_view_create` / `destroy` / `set_bounds` / `set_visible` are
//!   the lifecycle.
//!
//! So the interesting part is not the drawing, and it is not the hit testing
//! either -- [`PlatformViewHitTestBehavior`] in [`crate::platform_tree`] already
//! has that ported, including the part that is easy to get backwards. It is
//! that **creation has a lifetime and layout has a frequency**: a view is
//! created when the render object first needs it and destroyed when it goes
//! away, while its bounds are pushed every frame it moves, which during a
//! scroll is every frame.

use crate::engine::Rect;
use crate::platform_tree::{
    AndroidViewSurface, PlatformViewCreationParams, PlatformViewHitTestBehavior,
};
use crate::render::{
    BoxConstraints, BoxedRender, HitTestResult, Offset, PaintContext, RenderBox, RenderRef, Size,
    UpdateEffect,
};
use std::cell::RefCell;
use std::rc::Rc;

/// Hands platform-view work to whichever host is running.
///
/// Real engine: the C ABI. Under `--cfg rustflutter_stubs`, or in this crate's
/// own tests, the recording stand-ins. Both go through this so that nothing
/// above it has to know which is which -- a test asserting "the bounds were sent
/// on the second frame" must not be asserting something the real host does
/// differently.
pub trait PlatformViewHost {
    /// Creates `params`. `Ok(())` means the host knows the view type; `Err` is
    /// what an unregistered factory looks like, which is what a plugin that was
    /// never installed returns.
    fn create(&mut self, params: &PlatformViewCreationParams, encoded: &[u8])
    -> Result<(), String>;

    fn destroy(&mut self, view_id: i64);

    /// Moves and resizes. Sent every frame the rectangle changes.
    fn set_bounds(&mut self, view_id: i64, rect: Rect);

    /// Hides without destroying: a view scrolled out of a viewport is still
    /// there.
    fn set_visible(&mut self, view_id: i64, visible: bool);
}

/// The C ABI, which is what a real application talks to.
pub struct EngineHost;

impl PlatformViewHost for EngineHost {
    fn create(
        &mut self,
        params: &PlatformViewCreationParams,
        encoded: &[u8],
    ) -> Result<(), String> {
        let mut kind = params.view_type.clone().into_bytes();
        kind.push(0);
        let status = unsafe {
            crate::engine::sys::rf_platform_view_create(
                params.id,
                kind.as_ptr() as *const std::ffi::c_char,
                if encoded.is_empty() {
                    std::ptr::null()
                } else {
                    encoded.as_ptr()
                },
                encoded.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(format!(
                "no factory registered for view type {:?}",
                params.view_type
            ))
        }
    }

    fn destroy(&mut self, view_id: i64) {
        unsafe { crate::engine::sys::rf_platform_view_destroy(view_id) };
    }

    fn set_bounds(&mut self, view_id: i64, rect: Rect) {
        unsafe {
            crate::engine::sys::rf_platform_view_set_bounds(
                view_id,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
            )
        };
    }

    fn set_visible(&mut self, view_id: i64, visible: bool) {
        unsafe { crate::engine::sys::rf_platform_view_set_visible(view_id, visible) };
    }
}

/// Where creation params are encoded on the way to the host.
///
/// Upstream hands the framework a `Map` and lets a `StandardMessageCodec`
/// produce the bytes. There is no codec call on the C ABI in this tree, so the
/// bytes are a parameter rather than a product, and a caller that has none
/// passes an empty slice -- which the host reads as "no params", exactly as a
/// null codec does upstream.
pub type CreationParamsBytes = Vec<u8>;

/// Shared between the render object that owns a view and whoever configured
/// it.
///
/// Upstream passes a `PlatformViewCreationParams` down and keeps no handle;
/// the host keeps the View. Here the handle exists because the render object
/// may be rebuilt while the view is not -- element reuse rebuilds render objects
/// every frame -- so "who is responsible for destroying this" cannot be answered
/// from a field on the render object alone.
#[derive(Clone, Default)]
pub struct PlatformViewController {
    inner: Rc<RefCell<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// The host, installed once. `None` before `attach`.
    host: Option<Box<dyn PlatformViewHost>>,
    /// Encoded creation params, kept so a view can be re-created after the
    /// host lost it.
    encoded: CreationParamsBytes,
    /// The ids that exist, and whether this controller is responsible for them.
    ///
    /// A set rather than a count: two views sharing a type must not tear each
    /// other down, and "did I already destroy this one" is the question a
    /// counter cannot answer.
    live: Vec<i64>,
    visible: Vec<(i64, bool)>,
}

impl PlatformViewController {
    pub fn new() -> PlatformViewController {
        PlatformViewController::default()
    }

    /// Installs the host. Idempotent by way of `Some`, so a second call is a
    /// no-op rather than a silent replacement that orphans live views.
    pub fn attach(&self, host: Box<dyn PlatformViewHost>) {
        let mut inner = self.inner.borrow_mut();
        if inner.host.is_none() {
            inner.host = Some(host);
        }
    }

    pub fn with_creation_params(&self, encoded: CreationParamsBytes) {
        self.inner.borrow_mut().encoded = encoded;
    }

    pub fn has_host(&self) -> bool {
        self.inner.borrow().host.is_some()
    }

    /// Whether `view_id` is live and not destroyed.
    pub fn is_live(&self, view_id: i64) -> bool {
        self.inner.borrow().live.contains(&view_id)
    }

    /// Ids this controller has created and not destroyed.
    pub fn live_ids(&self) -> Vec<i64> {
        self.inner.borrow().live.clone()
    }

    /// The ids whose visibility was last set to `false`.
    pub fn hidden_ids(&self) -> Vec<i64> {
        self.inner
            .borrow()
            .visible
            .iter()
            .filter(|(_, visible)| !*visible)
            .map(|(id, _)| *id)
            .collect()
    }

    fn create(&self, params: &PlatformViewCreationParams) -> Result<(), String> {
        let mut inner = self.inner.borrow_mut();
        if inner.live.contains(&params.id) {
            return Ok(());
        }
        // Read the bytes before borrowing the host: both live in the same
        // RefCell, so asking for one while the other is borrowed is a
        // re-entrant borrow rather than two fields.
        let encoded = inner.encoded.clone();
        let Some(host) = inner.host.as_mut() else {
            return Err("no host attached".to_string());
        };
        host.create(params, &encoded)?;
        inner.live.push(params.id);
        Ok(())
    }

    fn destroy(&self, view_id: i64) {
        let mut inner = self.inner.borrow_mut();
        // Guard before the call, not after: a second destroy would otherwise
        // reach the host and tear down whatever took the id.
        let Some(at) = inner.live.iter().position(|id| *id == view_id) else {
            return;
        };
        inner.live.remove(at);
        inner.visible.retain(|(id, _)| *id != view_id);
        if let Some(host) = inner.host.as_mut() {
            host.destroy(view_id);
        }
    }

    fn set_bounds(&self, view_id: i64, rect: Rect) {
        if let Some(host) = self.inner.borrow_mut().host.as_mut() {
            host.set_bounds(view_id, rect);
        }
    }

    fn set_visible(&self, view_id: i64, visible: bool) {
        let mut inner = self.inner.borrow_mut();
        // Work out what changed before reaching for the host, so the borrow of
        // `visible` ends before `host` starts. Both are in the same RefCell.
        //
        // Only a change is sent. Bounds go out every frame; visibility does
        // not, and a host that hides and shows on every frame of a scroll is
        // doing work it was not asked for.
        let at = inner.visible.iter().position(|(id, _)| *id == view_id);
        let send = match at {
            Some(at) if inner.visible[at].1 == visible => false,
            Some(at) => {
                inner.visible[at].1 = visible;
                true
            }
            None => {
                inner.visible.push((view_id, visible));
                true
            }
        };
        if send {
            if let Some(host) = inner.host.as_mut() {
                host.set_visible(view_id, visible);
            }
        }
    }
}

/// Upstream `RenderPlatformView`.
///
/// Sized by its constraints and never by a child, because there is no child:
/// what it points at is not in this tree. That is why it takes no child, and
/// why `AndroidViewSurface::requires_bounded_constraints` is true -- a
/// texture has to be given a size and has no opinion of its own.
pub struct RenderPlatformView {
    params: PlatformViewCreationParams,
    controller: PlatformViewController,
    behavior: PlatformViewHitTestBehavior,
    surface: AndroidViewSurface,
    size: Size,
}

impl RenderPlatformView {
    pub fn new(
        params: PlatformViewCreationParams,
        controller: PlatformViewController,
    ) -> RenderPlatformView {
        RenderPlatformView {
            params,
            controller,
            behavior: PlatformViewHitTestBehavior::Opaque,
            surface: AndroidViewSurface::new(),
            size: Size::ZERO,
        }
    }

    pub fn boxed(
        params: PlatformViewCreationParams,
        controller: PlatformViewController,
    ) -> BoxedRender {
        RenderRef::new(RenderPlatformView::new(params, controller))
    }

    pub fn with_hit_test_behavior(mut self, behavior: PlatformViewHitTestBehavior) -> Self {
        self.behavior = behavior;
        self
    }

    pub fn view_id(&self) -> i64 {
        self.params.id
    }

    pub fn controller(&self) -> &PlatformViewController {
        &self.controller
    }

    pub fn created(&self) -> bool {
        self.controller.is_live(self.params.id)
    }
}

impl RenderBox for RenderPlatformView {
    /// Upstream's `updateFrom`. A change of view type or id is a different
    /// object, not an update to this one -- the old view belongs to the old
    /// identity and nothing here can hand it back.
    fn update_from(&mut self, fresh: &mut dyn RenderBox) -> Option<UpdateEffect> {
        let fresh = fresh.as_any_mut().downcast_mut::<RenderPlatformView>()?;
        if fresh.params.id != self.params.id || fresh.params.view_type != self.params.view_type {
            return None;
        }
        let effect = UpdateEffect::repaint_if(self.behavior != fresh.behavior);
        self.behavior = fresh.behavior;
        Some(effect)
    }

    /// Largest that fits, which is what a proxy box with no child answers.
    ///
    /// `biggest()` already answers `min` for an unbounded axis in this tree,
    /// which is the deliberate `BoxConstraints::biggest()` deviation recorded
    /// in `RenderDecoratedBox` -- so it is finite here and needs no guard. A
    /// View cannot be sized to infinity, and this is what keeps it from being.
    fn layout(&mut self, constraints: BoxConstraints) -> Size {
        self.size = constraints.biggest();
        self.size
    }

    fn size(&self) -> Size {
        self.size
    }

    fn paint(&self, context: &mut PaintContext, offset: Offset) {
        let rect = Rect::xywh(offset.dx, offset.dy, self.size.width, self.size.height);
        // Create before pushing. The layer tree is built ahead of the host
        // doing anything, so pushing a view the host has not made leaves the
        // tree referring to something that does not exist.
        let _ = self.controller.create(&self.params);
        context.in_layer(
            |tree| tree.push_platform_view(self.params.id, rect),
            |_| {
                // Nothing. A platform view draws no pixels here: the View is
                // composited by Android over the surface, and the layer carries
                // only where it goes.
            },
        );
        // Bounds go out whether or not the view was created, so that a frame
        // which positions an existing view is not lost when creation failed.
        self.controller.set_bounds(self.params.id, rect);
        self.controller.set_visible(self.params.id, true);
    }

    /// Upstream's `RenderPlatformView.hitTest`, and the reason it is not the
    /// default protocol.
    ///
    /// The default adds an object only once a child was hit or it is a target
    /// in its own right. There is no child, so the default would answer false
    /// everywhere and a WebView would be untappable -- which is the one thing it
    /// has to be good at.
    ///
    /// The bounds check comes first whatever the behaviour: `opaque` does not
    /// mean "everywhere".
    fn hit_test(&self, position: Offset, result: &mut HitTestResult) -> bool {
        let (recorded, _stop) = self.behavior.hit_test(self.size.contains(position));
        if !recorded {
            return false;
        }
        result.add(self.hit_test_id(), position);
        true
    }

    /// The instance's identity, as a hit-test target.
    ///
    /// The view id rather than a fresh number, because a WebView that is
    /// rebuilt every frame still has to be the same tap target to the gesture
    /// recognizer above it.
    fn hit_test_id(&self) -> u64 {
        self.params.id as u64
    }

    fn compute_dry_layout(&self, constraints: BoxConstraints) -> Size {
        // The same answer as `layout`, stated rather than delegated: a dry
        // layout gets `&self` and must not lay out, which is the entire point
        // of the protocol. Calling `layout` here would record a size on a tree
        // that is only being measured.
        constraints.biggest()
    }
}

impl Drop for RenderPlatformView {
    /// Destroy on the way out.
    ///
    /// Not on the last paint: a view scrolled out of a viewport is still
    /// alive, still holding whatever state the page had, and re-creating it
    /// would reload it.
    fn drop(&mut self) {
        self.controller.destroy(self.params.id);
    }
}

/// Upstream `AndroidView`: a platform view, with the surface strategy chosen
/// for you.
///
/// Its own doc steers you away from doing this by hand, and the advice is worth
/// repeating because the reason is not obvious -- there is no difference in
/// performance, only in how much you have to say.
pub struct AndroidView;

impl AndroidView {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        params: PlatformViewCreationParams,
        controller: PlatformViewController,
    ) -> RenderPlatformView {
        RenderPlatformView::new(params, controller)
    }
}
