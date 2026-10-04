// Copyright 2013 The Flutter Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A vertical scroller of photographs fetched over the network.
//!
//! The interesting part is not the list. It is where the work happens, because
//! the framework is single-threaded by construction -- every widget callback
//! runs on the UI task runner -- and a blocking HTTPS request or a JPEG decode
//! on that thread is a dropped frame.
//!
//! So the work is split by what each half is allowed to touch:
//!
//!   fetch   `std::thread`, many at once, no framework calls at all. Produces
//!           `Vec<u8>`, which is `Send`, so it crosses a thread boundary freely.
//!   decode  the UI thread, metered to a couple of images per frame. It has to
//!           be there: `Image` is a raw engine handle with no `Send`, so it
//!           cannot be built on a worker and handed over.
//!
//! Nothing posts back across the thread boundary. A frame is asked for from
//! `build` -- on the UI thread, where `request_frame` is allowed to be called --
//! for as long as anything is outstanding, which is what a progressive image
//! list wants anyway.
//!
//! ```text
//!   build     retire any fling, decode up to DECODES_PER_FRAME arrivals,
//!             start a fetch per photo not already asked for, ask for a frame
//!             if anything is outstanding, then lay the list out at `offset`
//!   drag      the offset follows the finger
//!   release   the leftover velocity integrates per frame and decays
//! ```
//!
//! State is `()` and everything lives in an `Rc<RefCell<Io>>`. The offset could
//! have been `State`, and drag would then have been one `set_state`; it is not,
//! because `build` is handed `&State` and this needs to *write* during a build --
//! to drain the queue, to decode, to claim a photo. `set_state` is the rebuild
//! signal either way.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::raw::{c_char, c_int};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustflutter::gestures::PointerHandlers;
use rustflutter::painting::Image;
use rustflutter::prelude::*;
use rustflutter::render::{CrossAxisAlignment, MainAxisAlignment, MainAxisSize, RenderBox};
use rustflutter::widgets::{
    ClipRRect, ColoredBox, Column, Expanded, ImageView, ListView, Padding, Pointer, Row, SizedBox,
    Text,
};

/// Asked for at the size they are drawn at, so the decode is cheap: a decode
/// costs the UI thread milliseconds, and this is the width that decides how
/// many pixels that is.
const IMG_WIDTH: u32 = 480;
const IMG_HEIGHT: u32 = 320;

const CARD_WIDTH: f32 = 320.0;
const CARD_HEIGHT: f32 = 213.0;
const CARD_SPACING: f32 = 14.0;
const CORNER: f32 = 14.0;

/// The expensive half, on the frame's thread. Two per frame is about what a
/// 60Hz budget absorbs without a visible hitch.
const DECODES_PER_FRAME: usize = 2;

/// How fast a fling loses speed, as the exponent of a time constant: the
/// velocity is multiplied by e^-FRICTION for every second it runs. At 4.0 a
/// fling is down to a tenth in about half a second, which reads as a coast
/// rather than a stop.
const FRICTION: f32 = 4.0;

/// Below this many logical pixels per second a fling is over.
const FLING_CUTOFF: f32 = 12.0;

/// The fastest a release is believed to have been. A flick the gesture
/// recogniser scored from two noisy samples can come out absurd.
const MAX_VELOCITY: f32 = 6000.0;

/// The most a single frame may scroll, in logical pixels -- so one long frame,
/// a decode's worth, does not throw the list off the screen.
const MAX_STEP: f32 = 48.0;

/// Ceiling on one response, so a wrong URL cannot fill memory.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// A permission worth showing, because it is one the engine's own host cannot
/// ask for: the platform channels cover clipboard, settings and lifecycle, but a
/// runtime permission request is a dialog plus a result, and nothing in the
/// embedder provides one. `android.permission.CAMERA` is the example.
const CAMERA: &str = "android.permission.CAMERA";

/// Named so the packager declares it, not read anywhere: the photographs load
/// over the network, and Android refuses every socket an app has not asked
/// for. A requirement that lives with the code that has it, rather than on a
/// build command line nobody re-reads.
#[allow(dead_code)]
const INTERNET: &str = "android.permission.INTERNET";

/// How long a tap on the permission card keeps frames coming: the grant
/// arrives asynchronously, and past this the card stops spending frames on an
/// answer that is not coming (the user dismissed the dialog, or worse).
const PERMISSION_WATCH: Duration = Duration::from_secs(30);

/// Photographs, by id. `picsum.photos` serves real photographs and takes the
/// size in the path, so the bytes on the wire are the size drawn.
const PHOTOS: &[(&str, &str)] = &[
    ("1015", "a river running through the mountains"),
    ("1016", "a canoe on a lake, and a person in it"),
    ("1018", "a mountain range under cloud"),
    ("1024", "a bear, which is the sort of thing that happens"),
    ("1025", "a pug in a blanket"),
    ("1036", "a bay, from above"),
    ("1043", "the last frames of a film camera"),
    ("1050", "a coastline, and the sea working at it"),
    ("1051", "a panda, eating"),
    ("1062", "a city, blurred by a long exposure"),
];

fn url_for(id: &str) -> String {
    format!("https://picsum.photos/id/{id}/{IMG_WIDTH}/{IMG_HEIGHT}")
}

const BACKGROUND: Color = Color::rgb(0x0D, 0x11, 0x17);
const TEXT: Color = Color::rgb(0xE6, 0xED, 0xF3);
const MUTED: Color = Color::rgb(0x9A, 0xAB, 0xC0);
const ACCENT: Color = Color::rgb(0x54, 0xC5, 0xF8);
const PLACEHOLDER: Color = Color::rgb(0x1B, 0x22, 0x2C);

// -- Crossing the thread boundary ---------------------------------------------

/// What the workers hand back, and the only thing shared with them. Both halves
/// are `Send`; nothing framework-shaped crosses.
#[derive(Default)]
struct Wire {
    arrived: Vec<(String, Vec<u8>)>,
    running: usize,
    /// Reclaimed by the UI thread, which is the only side that can show them.
    failed: Vec<String>,
}

/// Starts a fetch for `url` on a thread of its own.
///
/// The framework's own answer to this is `ImageProvider::Network`, which takes a
/// `fetch` closure and calls it *inline* -- on whichever thread resolved the
/// image, which is the UI thread. That is the wrong thread for a socket, so the
/// request is made here instead and the framework is handed bytes.
fn start_fetch(wire: Arc<Mutex<Wire>>, url: String) {
    wire.lock().expect("wire").running += 1;
    std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(10))
            .timeout_write(Duration::from_secs(10))
            .build();

        let outcome = match agent.get(&url).call() {
            Err(_) => Err(()),
            Ok(response) => {
                use std::io::Read;
                let mut bytes = Vec::new();
                match response
                    .into_reader()
                    .take(MAX_BYTES)
                    .read_to_end(&mut bytes)
                {
                    Err(_) => Err(()),
                    // An HTML error page decodes to nothing; treat it as a miss
                    // rather than handing the decoder something it will reject
                    // one frame later.
                    Ok(_) if bytes.is_empty() => Err(()),
                    Ok(_) => Ok(bytes),
                }
            }
        };

        let mut wire = wire.lock().expect("wire");
        wire.running -= 1;
        match outcome {
            Ok(bytes) => wire.arrived.push((url, bytes)),
            Err(()) => wire.failed.push(url),
        }
    });
}

// -- Everything the page owns -------------------------------------------------

struct Io {
    /// The offset the list is drawn at.
    offset: f32,
    /// Decoded handles by url. UI thread only: `Image` is a raw engine handle
    /// with no `Send`, so this could not be shared even if it wanted to be.
    images: HashMap<String, Rc<Image>>,
    /// Asked for already, so a photo is requested once and not once a frame.
    asked: std::collections::HashSet<String>,
    wire: Arc<Mutex<Wire>>,
    /// Velocity still integrating, in logical pixels per second. Kept as a
    /// velocity rather than a per-frame step so the same flick travels the same
    /// distance at 90Hz as at 60Hz.
    fling: f32,
    /// The window's inner height, handed in by the application each frame --
    /// `Component::build` is given the element, not the view, so it has no size.
    viewport: f32,
    /// Only so a fling can notice a long gap between frames and not jump.
    last: Option<Instant>,
    /// Watching the camera permission after the user tapped the card: the
    /// grant arrives asynchronously (the system dialog), so builds keep
    /// coming -- the same `set_state` + `request_frame` mechanism a fling or
    /// an outstanding fetch uses -- until the status flips or this deadline
    /// passes. `None` means nobody tapped and no frames are spent on it.
    watch_permission_until: Option<Instant>,
}

impl Io {
    fn new() -> Io {
        Io {
            offset: 0.0,
            images: HashMap::new(),
            asked: std::collections::HashSet::new(),
            wire: Arc::new(Mutex::new(Wire::default())),
            fling: 0.0,
            viewport: 800.0,
            last: None,
            watch_permission_until: None,
        }
    }

    fn max_offset(&self) -> f32 {
        let content = PHOTOS.len() as f32 * (CARD_HEIGHT + CARD_SPACING) + CARD_SPACING;
        (content - self.viewport).max(0.0)
    }

    fn clamp(&mut self) {
        self.offset = self.offset.clamp(0.0, self.max_offset());
    }

    /// Moves the list by the velocity still integrating, over `dt` seconds, and
    /// decays that velocity.
    ///
    /// The sign is the drag's. A finger moving up is a negative `dy` and carries
    /// the list further up, which is the offset growing -- so the offset
    /// *gains* this velocity rather than losing it, and a release has to keep
    /// the same sign the drag had.
    fn step_fling(&mut self, dt: f32) -> bool {
        if self.fling.abs() < FLING_CUTOFF {
            self.fling = 0.0;
            return false;
        }
        self.offset += (self.fling * dt).clamp(-MAX_STEP, MAX_STEP);
        self.fling *= (-FRICTION * dt).exp();
        self.clamp();
        true
    }
}

// -- The page -----------------------------------------------------------------

struct Page {
    io: Rc<RefCell<Io>>,
}

impl StatefulComponent for Page {
    type State = ();

    fn build(
        &self,
        _state: &(),
        handle: StateHandle<()>,
        context: &mut rustflutter::framework::BuildContext,
    ) -> AnyWidget {
        let mut io = self.io.borrow_mut();

        // How long since the last frame, for the fling's step. Clamped, because
        // the first frame has no previous one.
        let elapsed = io
            .last
            .replace(Instant::now())
            .map(|then| then.elapsed().as_secs_f32().min(0.1))
            .unwrap_or(0.0);
        // No elapsed time means no motion: the first frame has no previous one,
        // and a fling stepped then would jump a whole frame's worth.
        let flinging = elapsed > 0.0 && io.step_fling(elapsed);

        // Decode what has arrived, metered. The only expensive thing on this
        // thread, so it is rationed.
        // The lock is taken, drained and dropped before `io` is touched: holding
        // a MutexGuard across a mutation of the thing it was taken from is a
        // borrow error, and the guard has no reason to outlive the drain.
        let (batch, retry, outstanding) = {
            let mut wire = io.wire.lock().expect("wire");
            let mut batch = Vec::new();
            while batch.len() < DECODES_PER_FRAME && !wire.arrived.is_empty() {
                batch.push(wire.arrived.remove(0));
            }
            let retry = std::mem::take(&mut wire.failed);
            let outstanding = wire.arrived.len() + wire.running;
            (batch, retry, outstanding)
        };
        for (url, bytes) in batch {
            // A format Skia has no codec for returns None rather than failing
            // the build; the placeholder stays, which is right.
            if let Some(image) = Image::decode(&bytes) {
                io.images.insert(url, Rc::new(image));
            }
        }
        // A photo that failed is asked for once more rather than written off.
        // Draining the list here is what keeps that from being every frame.
        for url in retry {
            io.asked.remove(&url);
        }

        // Ask for everything not already asked for, not only what is on screen.
        // The offset is known here but a request costs a thread and tens of
        // kilobytes, and fetching ahead is what keeps a flick through the list
        // from showing placeholders.
        for (id, _) in PHOTOS {
            let url = url_for(id);
            if io.images.contains_key(&url) || io.asked.contains(&url) {
                continue;
            }
            io.asked.insert(url.clone());
            start_fetch(io.wire.clone(), url);
        }

        // While anything is outstanding, or a fling is integrating, the next
        // frame is this build's to ask for. `set_state` rather than
        // `request_frame` because the offset moved and the list has to be built
        // again, not merely repainted.
        let busy = outstanding > 0 || flinging;
        if busy {
            handle.set_state(|()| ());
            context.request_frame();
        }

        // While the camera permission is being watched (the user tapped the
        // card and the system dialog is up), builds keep coming so the line
        // flips the frame the grant lands rather than on the next scroll. The
        // dialog is answered out-of-band, so without this the card would lie
        // until something else rebuilt.
        let watching = io
            .watch_permission_until
            .map(|until| {
                let granted = matches!(
                    mobile_sentinel::permissions::status(CAMERA),
                    mobile_sentinel::PermissionState::Granted
                );
                if granted || Instant::now() > until {
                    io.watch_permission_until = None;
                    false
                } else {
                    true
                }
            })
            .unwrap_or(false);
        if watching {
            handle.set_state(|()| ());
            context.request_frame();
        }

        // A snapshot rather than the render objects themselves: the closure
        // below is `Fn`, so it may be called more than once for one build, and
        // a ListView cannot be rebuilt from a moved-in one.
        let offset = io.offset;
        let cards: Vec<(&'static str, Option<Rc<Image>>)> = PHOTOS
            .iter()
            .map(|(id, caption)| (*caption, io.images.get(&url_for(id)).cloned()))
            .collect();
        drop(io);

        // One region over the whole list: a drag anywhere scrolls it.
        //
        // Built out here and cloned in, because the closure below is `Fn` and a
        // closure that owns its captures cannot move them out to the handlers.
        let on_drag = self.io.clone();
        let drag_handle = handle.clone();
        let on_release = self.io.clone();
        let release_handle = handle.clone();
        let handlers = PointerHandlers::new()
            .with_drag_update(move |event| {
                let mut io = on_drag.borrow_mut();
                // A finger moving up is a negative dy and must carry the list
                // further up, which is the offset growing. Hence the negation,
                // and it is why the release negates too.
                io.fling = 0.0;
                io.offset -= event.delta.dy;
                io.clamp();
                drag_handle.set_state(|()| ());
            })
            .with_drag_end(move |event| {
                let mut io = on_release.borrow_mut();
                // Negated for the same reason the drag is: a finger moving up is
                // a negative dy and the offset grows.
                io.fling = -event.velocity.dy.clamp(-MAX_VELOCITY, MAX_VELOCITY);
                release_handle.set_state(|()| ());
            });

        // The card is its own tap region, inside the scroll region: a tap does
        // not travel, so the drag handlers above never fire for it, and a drag
        // is not a tap, so this never fires for a scroll. Tapping asks the
        // system for the permission and starts the watch that flips this line
        // when the answer arrives. Cloned out here because the closure below
        // outlives this build and cannot borrow `self`.
        let tap_io = self.io.clone();

        leaf(move || {
            // Read on every build, so the caption is the truth at this frame rather
        // than whatever it was when the page was first built.
        let granted = matches!(
            mobile_sentinel::permissions::status(CAMERA),
            mobile_sentinel::PermissionState::Granted
        );
        let state_line = if granted {
            "camera: granted"
        } else {
            "camera: not granted — tap to request"
        };

        // The card is its own tap region, inside the scroll region: a tap does
        // not travel, so the drag handlers above never fire for it, and a drag
        // is not a tap, so this never fires for a scroll. Tapping asks the
        // system for the permission and starts the watch that flips this line
        // when the answer arrives.
        let tap_handle = handle.clone();
        // Cloned per build: the tap closure moves its own copy, and this
        // closure is `Fn`, so it cannot give its capture away.
        let tap_io = tap_io.clone();
        let card_tap = PointerHandlers::new().with_tap(move |_| {
            mobile_sentinel::permissions::request(CAMERA);
            tap_io.borrow_mut().watch_permission_until =
                Some(Instant::now() + PERMISSION_WATCH);
            tap_handle.set_state(|()| ());
        });

        let mut list = ListView::new().with_spacing(CARD_SPACING).with_offset(offset);
        for (caption, image) in &cards {
            list = list.push(card(caption, image.as_ref()));
        }
        // The permission banner sits above the scroll region, not in it: a
        // request control that scrolls off the top cannot be tapped, which is
        // exactly what happened before it was pinned here.
        Column::new()
            .with_spacing(CARD_SPACING)
            .push(Pointer::new(2, permission_card(state_line)).with_handlers(card_tap))
            .push_flex(Expanded::new(
                Pointer::new(
                    1,
                    Padding::new(EdgeInsets::all(12.0), list),
                )
                .with_handlers(handlers.clone()),
            ))
        })
    }
}

/// The permission line: a caption over a placeholder-sized box, so the card
/// rhythm of the photographs is not broken by it.
fn permission_card(state_line: &str) -> Box<dyn RenderBox> {
    Box::new(
        Column::new()
            .with_main_axis_size(MainAxisSize::Min)
            .with_cross_axis_alignment(CrossAxisAlignment::Start)
            .push(
                ClipRRect::new(
                    CORNER,
                    ColoredBox::new(
                        PLACEHOLDER,
                        Column::new()
                            .with_main_axis_size(MainAxisSize::Min)
                            .with_main_axis_alignment(MainAxisAlignment::Center)
                            .with_cross_axis_alignment(CrossAxisAlignment::Center)
                            .push(Text::new(state_line).with_size(13.0).with_color(ACCENT)),
                    ),
                ),
            )
            .push(SizedBox::new(1.0, 8.0))
            .push(
                Text::new("a permission, asked of the platform")
                    .with_size(13.0)
                    .with_color(TEXT),
            ),
    )
}

fn card(caption: &str, image: Option<&Rc<Image>>) -> Box<dyn RenderBox> {
    let picture: Box<dyn RenderBox> = match image {
        Some(handle) => Box::new(ImageView::new(handle.clone())),
        None => Box::new(
            ColoredBox::new(PLACEHOLDER, Column::new()
                .with_main_axis_size(MainAxisSize::Min)
                .with_main_axis_alignment(MainAxisAlignment::Center)
                .with_cross_axis_alignment(CrossAxisAlignment::Center)
                .push(Text::new("loading").with_size(13.0).with_color(MUTED))),
        ),
    };

    Box::new(
    Column::new()
        .with_main_axis_size(MainAxisSize::Min)
        .with_cross_axis_alignment(CrossAxisAlignment::Start)
        .push(ClipRRect::new(
            CORNER,
            SizedBox::new(CARD_WIDTH, CARD_HEIGHT).with_child(picture),
        ))
        .push(SizedBox::new(1.0, 8.0))
        .push(Text::new(caption).with_size(13.0).with_color(TEXT)),
    )
}

// -- Entry point --------------------------------------------------------------

/// Holds the page's `Io` so it can hand the viewport height down.
///
/// `WidgetApplication::build` is given the view -- it has a `size` -- where
/// `Component::build` is given the element, and does not. The alternative was a
/// static, which would be a lie about there being only ever one of these.
struct PhotoScroller {
    io: Rc<RefCell<Io>>,
}

impl WidgetApplication for PhotoScroller {
    /// The colour behind everything, and what `compose_frame` fills the root
    /// picture with. `Theme::dark()` does not do this: it publishes a Theme for
    /// widgets to read, and the window behind them is a separate decision.
    fn background(&self) -> Color {
        BACKGROUND
    }

    fn build(&mut self, context: &BuildContext) -> AnyWidget {
        self.io.borrow_mut().viewport = context.size.height;

        provide(
            Theme::dark(),
            component(SafeArea::new(stateful(Page {
                io: self.io.clone(),
            }))),
        )
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rustflutter_app_main(_argc: c_int, _argv: *const *const c_char) -> c_int {
    register_application(|| {
        Box::new(WidgetHost::new(PhotoScroller {
            io: Rc::new(RefCell::new(Io::new())),
        }))
    });
    let options = RunOptions {
        width: IMG_WIDTH as i32,
        height: 900,
        title: String::from("Photo scroller - rustflutter"),
        ..RunOptions::default()
    };
    run(&options).unwrap();
    0
}