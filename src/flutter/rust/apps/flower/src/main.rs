// Copyright 2013 The Flutter Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A flower that blooms, on a loop, and can be tapped to bloom again.
//!
//! Nothing here is a widget doing the animating. The bloom is a **pure function
//! of one number in 0..1**, and that number comes out of a clock: [`advance`]
//! adds the frame's delta to it and asks for another frame, [`build`] reads it
//! and hands it to a render object that turns it into petals. There is no
//! `AnimationController`, no `Tween` chain and no widget rebuilt in the middle
//! of a transition -- one number, one draw.
//!
//! That is the shape a bloom actually has. A flower does not interpolate
//! between two pictures of itself; eight petals open in sequence, two rings a
//! third of a beat apart, and the staging is four overlapping windows on the
//! same timeline. Encoding that as `stage(t, from, to)` calls on one `t` says
//! it in one line each, where a controller chain would say it in a builder
//! nobody can read back.
//!
//! The petals are [`RenderPath`]s, not sprites or an image: a petal is two
//! cubics, and drawing it fifteen times a frame costs nothing next to the
//! frames it would cost to fetch.
//!
//! [`advance`]: BloomPage::advance

use std::cell::RefCell;
use std::f32::consts::PI;
use std::os::raw::{c_char, c_int};
use std::rc::Rc;

use rustflutter::animation::Curve;
use rustflutter::engine::{Canvas, Style};
use rustflutter::framework::StateHandle;
use rustflutter::gestures::PointerHandlers;
use rustflutter::painting::RenderPath;
use rustflutter::prelude::*;
use rustflutter::render::{
    BoxConstraints, MainAxisSize, Offset, PaintContext, RenderBox, Size, UpdateEffect,
};
use rustflutter::widgets::{Center, Column, Expanded, Pointer, Text};

// -- The clock -----------------------------------------------------------------

/// How long the bloom itself takes, in seconds.
const BLOOM_SECONDS: f32 = 3.4;
/// How long the open flower is held before it closes and starts again.
const HOLD_SECONDS: f32 = 1.4;
/// One whole cycle: open, hold, and the instant the loop restarts.
const PERIOD_SECONDS: f32 = BLOOM_SECONDS + HOLD_SECONDS;

// -- The flower ----------------------------------------------------------------

/// Petals in the outer ring. Nine reads as a flower; eight reads as a gear.
const OUTER_PETALS: usize = 9;
/// Petals in the inner ring, offset half a step so the gaps do not line up.
const INNER_PETALS: usize = 6;
/// Stamens around the disc. Only ever seen for the last third of the bloom.
const STAMENS: usize = 16;
/// Motes drifting past the flower. Purely so the frame is not still.
const MOTES: usize = 14;

const SKY_TOP: Color = Color::rgb(0x0A, 0x10, 0x1C);
const SKY_BOTTOM: Color = Color::rgb(0x16, 0x1B, 0x2E);
const STEM: Color = Color::rgb(0x3F, 0x7D, 0x4E);
const STEM_DARK: Color = Color::rgb(0x2A, 0x5C, 0x39);
const LEAF: Color = Color::rgb(0x4C, 0x93, 0x59);
const LEAF_DARK: Color = Color::rgb(0x2F, 0x6B, 0x3E);

const PETAL_OUTER_BASE: Color = Color::rgb(0xB0, 0x2C, 0x54);
const PETAL_OUTER_MID: Color = Color::rgb(0xE8, 0x54, 0x7C);
const PETAL_OUTER_TIP: Color = Color::rgb(0xF9, 0x9C, 0xB4);

const PETAL_INNER_BASE: Color = Color::rgb(0xD1, 0x45, 0x63);
const PETAL_INNER_MID: Color = Color::rgb(0xF3, 0x76, 0x92);
const PETAL_INNER_TIP: Color = Color::rgb(0xFD, 0xC9, 0xD4);

const DISC_OUTER: Color = Color::rgb(0xC9, 0x7B, 0x1E);
const DISC_INNER: Color = Color::rgb(0xF2, 0xC0, 0x4C);
const STAMEN: Color = Color::rgb(0xFF, 0xE8, 0x9C);
const SEPAL: Color = Color::rgb(0x3A, 0x73, 0x46);
const GLOW: Color = Color::rgb(0xF6, 0x8A, 0xA8);

const LABEL: Color = Color::rgb(0x8F, 0xA3, 0xBD);

/// How far the flower head reaches from its centre, in design units. The petals'
/// length is this at full open, and it is also the scene's half-width, so one
/// number decides both the flower's size and how the whole plant is scaled.
const PETAL_RADIUS: f32 = 104.0;
/// How far below the head the stem runs, in design units. It reaches exactly
/// here and no further, so the plant's extent is known and the stem cannot run
/// off the bottom of a viewport it was not drawn for.
const STEM_BOTTOM: f32 = 250.0;
/// The stem's width at the bottom, tapering as it rises.
const STEM_WIDTH: f32 = 8.0;

/// How the bloom opens: a long slow start and a decisive finish, which is what
/// a petal does -- it hesitates and then gives way all at once.
const OPEN_CURVE: Curve = Curve::Cubic(0.32, 0.02, 0.18, 1.0);
/// The head lifts as it opens. A separate, slower window from the petals', so
/// the flower raises itself while it is still mostly a bud.
const LIFT_CURVE: Curve = Curve::Cubic(0.25, 0.10, 0.25, 1.0);

/// Where `t` is within `from..to`, clamped to 0..1.
///
/// The whole staging of the bloom is built from this: four calls with
/// overlapping windows are the entire difference between a flower opening and
/// eight shapes appearing.
fn stage(t: f32, from: f32, to: f32) -> f32 {
    ((t - from) / (to - from)).clamp(0.0, 1.0)
}

/// Interpolates two angles the short way round, in degrees.
///
/// Not `a + (b - a) * k`: the petals fan out from -90 through +270, and a naive
/// lerp sends the last one the long way round while the rest do not.
fn lerp_angle(a: f32, b: f32, k: f32) -> f32 {
    let mut delta = (b - a) % 360.0;
    if delta > 180.0 {
        delta -= 360.0;
    }
    if delta < -180.0 {
        delta += 360.0;
    }
    a + delta * k
}

/// A petal: two cubics from the base at the origin out to a point at `-len`.
///
/// Drawn pointing straight up from `(0, 0)`, so a ring is a loop that rotates
/// about the flower's centre. The shoulders are at 0.30 and 0.74 of the length
/// rather than evenly spaced, which is what keeps the tip pointed instead of
/// rounding it off like a leaf.
fn petal_path(len: f32, width: f32) -> RenderPath {
    let mut path = RenderPath::new();
    path.move_to(0.0, 0.0);
    path.cubic_to(-width, -len * 0.30, -width * 0.92, -len * 0.74, 0.0, -len);
    path.cubic_to(width * 0.92, -len * 0.74, width, -len * 0.30, 0.0, 0.0);
    path.close();
    path
}

/// A leaf, as two cubics. Pointed at both ends, which is what separates it
/// from the petal at a glance.
///
/// Cubics rather than quadratics, and not for the look of it: a quadratic is a
/// cubic with its control points at two thirds of the way to the quadratic's, so
/// `cubic_to` can draw one exactly. That matters because this engine build does
/// not resolve `rf_path_quadratic_to` at load time -- `dlopen` fails with
/// "cannot locate symbol" and the application never starts -- while every other
/// path entry point is fine. [`quadratic_as_cubic`] is the conversion.
fn leaf_path(len: f32, width: f32) -> RenderPath {
    let mut path = RenderPath::new();
    path.move_to(0.0, 0.0);
    quadratic_as_cubic(&mut path, (0.0, 0.0), (-width, -len * 0.45), (0.0, -len));
    quadratic_as_cubic(&mut path, (0.0, -len), (width, -len * 0.45), (0.0, 0.0));
    path.close();
    path
}

/// One quadratic Bezier segment, written as the cubic it already is.
///
/// A cubic's two inner control points sit at `p0 + 2/3 (c - p0)` and
/// `p2 + 2/3 (c - p2)`, which is the standard conversion and is exact, not an
/// approximation.
fn quadratic_as_cubic(path: &mut RenderPath, p0: (f32, f32), c: (f32, f32), p2: (f32, f32)) {
    let c1 = (p0.0 + 2.0 / 3.0 * (c.0 - p0.0), p0.1 + 2.0 / 3.0 * (c.1 - p0.1));
    let c2 = (p2.0 + 2.0 / 3.0 * (c.0 - p2.0), p2.1 + 2.0 / 3.0 * (c.1 - p2.1));
    path.cubic_to(c1.0, c1.1, c2.0, c2.1, p2.0, p2.1);
}

/// A cheap deterministic 0..1 from an integer.
///
/// The motes need positions that are stable across frames -- a mote that jumps
/// every frame is a strobe, not drift -- and pulling in a generator to produce
/// fifteen numbers once would be more machinery than the effect is worth.
fn scatter(seed: u32) -> f32 {
    let mut x = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
    x ^= x >> 16;
    x = x.wrapping_mul(2_246_822_519);
    x ^= x >> 13;
    x = x.wrapping_mul(3_266_489_917);
    x ^= x >> 16;
    (x & 0xFFFF) as f32 / 65_535.0
}

// -- The page -----------------------------------------------------------------

/// The clock, and the two things a tap can change about it.
#[derive(Default)]
struct Bloom {
    /// Seconds into the current cycle. Bloom runs to [`BLOOM_SECONDS`], then the
    /// flower is held open until [`PERIOD_SECONDS`] and the cycle restarts.
    clock: f32,
    /// Blooms finished, so the caption can count them.
    cycles: u32,
    /// Set by the tap handler, consumed by the next [`advance`]. A frame is on
    /// its way, so the reset lands on this frame and the bloom starts again
    /// without the tap having to ask for one itself.
    replay: bool,
    /// The previous frame's timestamp, so `advance` can difference it. `None` on
    /// the first frame, which is also why the first frame does not move.
    last: Option<f32>,
}

struct BloomPage {
    io: Rc<RefCell<Bloom>>,
}

impl StatefulComponent for BloomPage {
    type State = ();

    /// Advances the clock and asks for the next frame.
    ///
    /// Returning true is what keeps the animation alive: frames are on demand,
    /// so an animation that stops asking stops running. Everything else about
    /// the bloom is derived from `clock` in `build`.
    fn advance(&self, _state: &mut (), frame_time_micros: i64) -> bool {
        let mut io = self.io.borrow_mut();
        // **This is a timestamp, not a duration.** `FrameContext` documents it as
        // "the time the frame is targeted at, in microseconds since epoch", so
        // adding it to the clock advances the bloom by however long ago the
        // epoch was -- about two and a quarter million cycles in the time it
        // takes to read this comment. `last` turns it back into a delta.
        let now = frame_time_micros as f32 / 1_000_000.0;
        if io.replay {
            io.replay = false;
            io.clock = 0.0;
            io.last = None;
        }
        let delta = io.last.replace(now).map(|then| (now - then).min(0.1)).unwrap_or(0.0);
        // Clamped to 100ms: a frame that took longer than that is a stall, and
        // integrating it whole would jump the bloom forward rather than let it
        // play.
        io.clock += delta;
        if io.clock >= PERIOD_SECONDS {
            // Subtracting rather than resetting keeps the remainder, so the
            // period is exactly PERIOD_SECONDS rather than PERIOD_SECONDS plus
            // however long the last frame happened to take.
            io.clock -= PERIOD_SECONDS;
            io.cycles += 1;
        }
        true
    }

    fn build(
        &self,
        _state: &(),
        _handle: StateHandle<()>,
        _context: &mut rustflutter::framework::BuildContext,
    ) -> AnyWidget {
        let clock = self.io.borrow().clock;
        // The bloom is the first BLOOM_SECONDS of the cycle and then holds. The
        // petal window below is 0..1 of *this*, so the hold costs nothing and
        // needs no second timeline.
        let t = (clock / BLOOM_SECONDS).clamp(0.0, 1.0);
        let cycles = self.io.borrow().cycles;

        let replay = Rc::clone(&self.io);
        let handlers = PointerHandlers::new().with_tap(move |_| {
            if let Ok(mut io) = replay.try_borrow_mut() {
                io.replay = true;
            }
        });

        // `leaf`, not `component`: a `Column` is a render object, and `leaf` is
        // what turns a render object into a widget. The column is built once per
        // build and handed the element's render slot.
        leaf(move || {
            // **`Expanded`, not a bare child.** In a `Column` the main axis is
            // unbounded, and a render object that answers a layout with
            // `constraints.biggest()` then has an *infinite* height to work
            // with -- which this tree clamps to the minimum, so the scene laid
            // out to nothing and drew off the bottom of the screen. `Expanded`
            // hands it the space that is actually left over instead, and the
            // caption below keeps its own height.
            //
            // `Expanded` also supplies the main axis; the cross axis is tight by
            // default, so the scene is as wide as the column.
            Column::new()
                .with_main_axis_size(MainAxisSize::Max)
                .with_spacing(18.0)
                .push_flex(
                    Expanded::new(
                        Pointer::new(1, FlowerRender::new(t)).with_handlers(handlers.clone()),
                    ),
                )
                .push(Center::new(
                    Text::new(if cycles == 0 {
                        "tap to bloom".to_string()
                    } else {
                        format!("bloom {cycles}  -  tap again")
                    })
                    .with_size(14.0)
                    .with_color(LABEL),
                ))
        })
    }
}

// -- The drawing ---------------------------------------------------------------

/// Everything on screen, as a function of `t`.
///
/// One render object rather than a tree of them: the petals are all the same
/// shape at different angles, and a `Stack` of fifteen leaves would mean fifteen
/// elements to rebuild every frame instead of one number to change.
struct FlowerRender {
    progress: f32,
    size: Size,
}

impl FlowerRender {
    fn new(progress: f32) -> FlowerRender {
        FlowerRender {
            progress,
            size: Size::ZERO,
        }
    }
}

impl RenderBox for FlowerRender {
    fn update_from(&mut self, fresh: &mut dyn RenderBox) -> Option<UpdateEffect> {
        // The whole scene is a function of `progress`, so a new value repaints
        // and there is nothing else to carry over.
        let fresh = fresh.as_any_mut().downcast_mut::<FlowerRender>()?;
        self.progress = fresh.progress;
        Some(UpdateEffect::repaint_if(true))
    }

    fn layout(&mut self, constraints: BoxConstraints) -> Size {
        self.size = constraints.biggest();
        self.size
    }

    fn size(&self) -> Size {
        self.size
    }

    fn paint(&self, context: &mut PaintContext, _offset: Offset) {
        context
            .canvas()
            .saved(|canvas| draw_scene(canvas, self.size, self.progress));
    }
}

/// The whole scene: sky, then the plant in a design space centred on the head.
///
/// The plant is drawn once, in its own units, and mapped onto whatever it is
/// actually being shown on by a single scale and offset. Design space puts the
/// **flower head's centre at the origin**, the stem running down to
/// [`STEM_BOTTOM`], and the petals reaching `radius` in every direction -- so
/// the whole scene is one box of known extent and fitting it is arithmetic
/// rather than guesswork. Authoring it in screen coordinates instead is what
/// puts the stem through the bottom of the frame on any viewport but the one it
/// was written on.
fn draw_scene(canvas: &mut Canvas, size: Size, t: f32) {
    if size.width <= 0.0 || size.height <= 0.0 {
        return;
    }
    draw_sky(canvas, size);

    // The plant's extent: the head reaches `radius` up and sideways, and the
    // stem runs `STEM_BOTTOM` down. Width is two radii; height is a radius above
    // the head plus the stem below it.
    let radius = PETAL_RADIUS;
    let wide = 2.0 * radius + 24.0;
    let tall = radius + STEM_BOTTOM + 24.0;
    let unit = (size.width / wide).min(size.height / tall);

    // The head sits above the middle rather than on it: the stem is longer than
    // the flower is tall, so centring the whole box would push the bloom into
    // the top third.
    let head_y = size.height * 0.38;

    canvas.save();
    canvas.translate(size.width / 2.0, head_y);
    canvas.scale(unit, unit);

    draw_stem(canvas, t);
    draw_leaves(canvas, t);
    draw_head(canvas, t);
    draw_motes(canvas, t);
    canvas.restore();
}

/// The background: a vertical gradient, so the flower has something to be
/// lighter than.
fn draw_sky(canvas: &mut Canvas, size: Size) {
    let gradient = Gradient::new(&[SKY_TOP, SKY_BOTTOM]);
    let paint = Paint::new(SKY_TOP)
        .with_linear_gradient((0.0, 0.0), (0.0, size.height), &gradient);
    canvas.draw_rect(Rect::xywh(0.0, 0.0, size.width, size.height), &paint);
}

/// The stem, stroked, growing upward from the bottom of the scene.
///
/// Drawn first so it is behind everything: the flower head covers the join, and
/// the join is the one place a stem that does not quite meet a head shows.
fn draw_stem(canvas: &mut Canvas, t: f32) {
    // The stem grows before the flower does. It reaches full length at the same
    // point the head stops rising, so by the time any petal moves there is
    // something for it to be at the top of.
    let grown = LIFT_CURVE.transform(stage(t, 0.0, 0.55));
    let top = STEM_BOTTOM - STEM_BOTTOM * grown;

    let mut stem = RenderPath::new();
    stem.move_to(4.0, STEM_BOTTOM);
    // Two cubics standing in for the S a cut stem leans in. See
    // `quadratic_as_cubic` for why this is not a quadratic call.
    quadratic_as_cubic(&mut stem, (4.0, STEM_BOTTOM), (-12.0, STEM_BOTTOM * 0.55), (0.0, top));

    let paint = Paint::new(STEM)
        .with_style(Style::Stroke { width: STEM_WIDTH })
        .with_anti_alias(true)
        .with_linear_gradient(
            (0.0, STEM_BOTTOM),
            (0.0, top),
            &Gradient::new(&[STEM_DARK, STEM]),
        );
    canvas.draw_path(&stem, &paint);
}

/// Two leaves, unfurling a beat after the stem is up.
///
/// Set at fixed fractions of the stem's length rather than at pixels, so they
/// stay on the stem whatever the scene is scaled to.
fn draw_leaves(canvas: &mut Canvas, t: f32) {
    // Left leaf lower on the stem than the right, which is what stops the two
    // from reading as one symmetric pair.
    let left = LIFT_CURVE.transform(stage(t, 0.10, 0.66));
    let right = LIFT_CURVE.transform(stage(t, 0.18, 0.76));

    draw_leaf(canvas, -6.0, STEM_BOTTOM * (0.72 - 0.10 * left), -54.0 - 16.0 * left, left, 74.0, LEAF_DARK);
    draw_leaf(canvas, 6.0, STEM_BOTTOM * (0.44 - 0.08 * right), 58.0 + 18.0 * right, right, 60.0, LEAF);
}

/// One leaf, at a point on the stem, swung out to one side.
fn draw_leaf(
    canvas: &mut Canvas,
    x: f32,
    y: f32,
    angle: f32,
    grown: f32,
    len: f32,
    colour: Color,
) {
    canvas.save();
    canvas.translate(x, y);
    canvas.rotate(angle);
    canvas.draw_path(
        &leaf_path(len * (0.5 + 0.5 * grown), len * 0.26 * (0.5 + 0.5 * grown)),
        &Paint::new(colour),
    );
    canvas.restore();
}

/// The flower head: glow, sepals, two rings of petals, disc, stamens.
///
/// Drawn back to front in that order, which is the only ordering that works --
/// the disc has to cover the bases of both rings, and the rings have to cover
/// the sepals.
fn draw_head(canvas: &mut Canvas, t: f32) {
    canvas.save();
    // The head itself swells out of the bud. Without this the bloom is only a
    // rotation -- the petals change where they point but the flower never
    // becomes bigger, which reads as a spinning disc rather than something
    // opening.
    let swell = LIFT_CURVE.transform(stage(t, 0.05, 0.85));
    let scale = 0.46 + 0.54 * swell;
    canvas.scale(scale, scale);

    let open = OPEN_CURVE.transform(stage(t, 0.18, 1.0));
    let radius = PETAL_RADIUS;

    // The glow: a soft disc behind the flower that grows with the bloom, so the
    // opening has something to push against.
    let glow_r = radius * (0.55 + 0.75 * open);
    let glow = Paint::new(GLOW)
        .with_opacity(0.10 + 0.22 * open)
        .with_blur(24.0);
    canvas.draw_circle(0.0, 0.0, glow_r, &glow);

    // The calyx: the green cup the petals come out of. Wider than the disc, so
    // it reads as a base rather than a ring.
    let mut cup = RenderPath::new();
    cup.add_oval(Rect::xywh(-26.0, -14.0, 52.0, 46.0));
    canvas.draw_path(&cup, &Paint::new(SEPAL));

    draw_sepals(canvas, t, radius);

    // Two rings. The outer one is behind and larger; the inner is smaller,
    // lighter, and offset half a step so its petals sit in the gaps.
    draw_ring(
        canvas,
        OUTER_PETALS,
        t,
        0.22,
        radius,
        0.44,
        &[
            (PETAL_OUTER_BASE, 0.0),
            (PETAL_OUTER_MID, 0.55),
            (PETAL_OUTER_TIP, 1.0),
        ],
        0.0,
    );
    draw_ring(
        canvas,
        INNER_PETALS,
        t,
        0.34,
        radius * 0.66,
        0.40,
        &[
            (PETAL_INNER_BASE, 0.0),
            (PETAL_INNER_MID, 0.55),
            (PETAL_INNER_TIP, 1.0),
        ],
        (180.0 / INNER_PETALS as f32).min(30.0),
    );

    draw_disc(canvas, t, radius);
    canvas.restore();
}

/// The sepals: five green points that peel back as the bloom opens.
fn draw_sepals(canvas: &mut Canvas, t: f32, radius: f32) {
    let peel = LIFT_CURVE.transform(stage(t, 0.30, 0.88));
    let paint = Paint::new(SEPAL);
    // Five, fanned around the back of the head. Closed they stand up and hug the
    // bud; open they have fallen back past horizontal and hide behind the
    // petals, which is what a sepal does and what stops them reading as a green
    // leaf sticking out of the side of the flower.
    for i in 0..5 {
        let spread = (i as f32 - 2.0) * 34.0;
        let swung = lerp_angle(spread, spread + 74.0, peel);
        let len = radius * (0.44 + 0.16 * peel);
        canvas.save();
        canvas.rotate(swung);
        canvas.draw_path(&leaf_path(len, len * 0.26), &paint);
        canvas.restore();
    }
}

/// One ring of petals.
///
/// The petals do not open together: petal `i` starts `stagger` after petal 0
/// and the ring takes `stagger * count` to finish, so the flower opens in a
/// sweep rather than all at once. That stagger is the difference between a
/// shape animating and a flower blooming.
fn draw_ring(
    canvas: &mut Canvas,
    count: usize,
    t: f32,
    from: f32,
    radius: f32,
    width_ratio: f32,
    colors: &[(Color, f32)],
    angle_offset: f32,
) {
    let gradient = Gradient::new(&[colors[0].0, colors[1].0, colors[2].0])
        .with_stops(&[colors[0].1, colors[1].1, colors[2].1]);
    // One paint for the whole ring: every petal is drawn after translating to
    // the centre, so the radial gradient is in petal-local coordinates and is
    // identical for all of them. Building it per petal would be fifteen
    // identical gradients a frame.
    let paint = Paint::new(colors[2].0)
        .with_anti_alias(true)
        .with_radial_gradient((0.0, 0.0), radius, &gradient);

    let stagger = 0.035;
    for i in 0..count {
        // Each petal has its own window into the timeline.
        let at = stage(t, from + i as f32 * stagger, from + 0.62 + i as f32 * stagger);
        let open = OPEN_CURVE.transform(at);

        // Closed, every petal points straight up and the ring is a bud. Open,
        // petal `i` has reached its own angle around the circle.
        let target = -90.0 + (i as f32 / count as f32) * 360.0 + angle_offset;
        let angle = lerp_angle(-90.0, target, open);

        // Length and width both grow, but the width more slowly: a bud is a
        // narrow point, and widening it as fast as it lengthens would read as
        // scaling rather than opening.
        let len = radius * (0.62 + 0.38 * open);
        let width = radius * width_ratio * (0.42 + 0.58 * open);

        canvas.save();
        canvas.rotate(angle);
        canvas.draw_path(&petal_path(len, width), &paint);
        canvas.restore();
    }
}

/// The disc, and the stamens around it, both late.
fn draw_disc(canvas: &mut Canvas, t: f32, radius: f32) {
    let grow = LIFT_CURVE.transform(stage(t, 0.58, 0.98));
    if grow <= 0.0 {
        return;
    }
    let disc_r = radius * 0.27 * grow;

    // Darker ring first, so the disc has a rim rather than being a flat dot.
    canvas.draw_circle(
        0.0,
        0.0,
        disc_r,
        &Paint::new(DISC_OUTER).with_anti_alias(true),
    );
    canvas.draw_circle(
        0.0,
        0.0,
        disc_r * 0.78,
        &Paint::new(DISC_INNER)
            .with_anti_alias(true)
            .with_radial_gradient(
                (0.0, -disc_r * 0.2),
                disc_r,
                &Gradient::new(&[DISC_INNER, DISC_OUTER]),
            ),
    );

    // Stamens: dots on a ring, each on its own schedule, so they arrive last
    // and from the outside in.
    for i in 0..STAMENS {
        let at = stage(t, 0.74 + (i % 4) as f32 * 0.045, 0.97);
        if at <= 0.0 {
            continue;
        }
        let angle = (i as f32 / STAMENS as f32) * 2.0 * PI;
        let r = disc_r * (0.72 + 0.42 * at);
        canvas.draw_circle(
            angle.cos() * r,
            angle.sin() * r,
            2.4 * at,
            &Paint::new(STAMEN).with_anti_alias(true),
        );
    }
}

/// Motes drifting up past the flower.
///
/// Positions come from [`scatter`] on the mote's index and its clock, so they
/// are stable between frames -- a mote that jumps every frame is a strobe.
fn draw_motes(canvas: &mut Canvas, t: f32) {
    for i in 0..MOTES {
        let seed = i as u32 * 7 + 3;
        // Spread across the scene's own width, so they are not clustered in the middle
        // of a viewport much wider than the flower.
        let x = (scatter(seed) - 0.5) * (2.0 * PETAL_RADIUS + 220.0);
        // `t` is the bloom progress, which runs 0..1 every cycle, so the drift
        // loops with the bloom rather than needing a second clock.
        let phase = (t + scatter(seed + 11)) % 1.0;
        // Up past the head and off the top, starting from the stem's foot.
        let y = STEM_BOTTOM * 0.9 - phase * (STEM_BOTTOM + PETAL_RADIUS * 1.6);
        let mote = Paint::new(STAMEN).with_opacity(0.30 * (1.0 - phase).min(1.0));
        canvas.draw_circle(
            x + (scatter(seed + 29) - 0.5) * 20.0,
            y,
            1.6 + 1.4 * scatter(seed + 41),
            &mote,
        );
    }
}

// -- Entry point --------------------------------------------------------------

struct Flower;

impl WidgetApplication for Flower {
    fn build(&mut self, _context: &BuildContext) -> AnyWidget {
        stateful(BloomPage {
            io: Rc::new(RefCell::new(Bloom::default())),
        })
    }

    /// The colour behind everything, and what `compose_frame` fills the root
    /// picture with. The scene paints its own gradient over the whole viewport,
    /// so this only shows for the frame or two before the first build.
    fn background(&self) -> Color {
        SKY_TOP
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rustflutter_app_main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args = collect_args(argc, argv);

    // One frame, at one point in the bloom, to a PNG.
    //
    // An animation that only exists while a device is showing it cannot be
    // checked in CI or looked at twice, so this is the same scene drawn at a
    // chosen `t`: `--at 0.2` is the bud, `--at 0.6` is it half open, `--at 1`
    // is the whole flower. Same code path as the screen -- the clock is simply
    // seeded instead of run.
    if let Some(path) = named_string(&args, "--png") {
        let at = named_value(&args, "--at").unwrap_or(1.0).clamp(0.0, 1.0);
        return render_png(&path, at);
    }

    register_application(|| Box::new(WidgetHost::new(Flower)));
    let options = RunOptions {
        width: 411,
        height: 891,
        title: String::from("Flower - rustflutter"),
        ..RunOptions::default()
    };
    match run(&options) {
        Ok(()) => 0,
        Err(code) => {
            eprintln!("flower: the host exited with {code}");
            1
        }
    }
}

/// One frame of the bloom at progress `at`, written to `path`.
fn render_png(path: &str, at: f32) -> c_int {
    use rustflutter::framework::ElementTree;
    use rustflutter::render::{BoxConstraints, RenderBox};

    // Text is shaped during the very first layout, and skparagraph cannot break
    // a line without ICU data.
    rustflutter::engine::initialize();

    let size = Size::new(411.0, 891.0);
    let mut tree = ElementTree::new();
    let io = Rc::new(RefCell::new(Bloom {
        clock: at * BLOOM_SECONDS,
        ..Bloom::default()
    }));
    tree.rebuild(stateful(BloomPage { io }));

    let mut built = tree.build_render_tree().expect("the tree has a root");
    built.layout(BoxConstraints::tight(size.width, size.height));

    let mut layer_tree = rustflutter::app::compose_frame(
        411,
        891,
        1.0,
        size,
        SKY_TOP,
        |context| built.paint(context, Offset::ZERO),
    );

    match layer_tree.write_png(std::path::Path::new(path)) {
        Ok(()) => {
            println!("flower: wrote {path} at t={at}");
            0
        }
        Err(err) => {
            eprintln!("flower: could not write {path}: {err}");
            1
        }
    }
}

fn named_string(args: &[String], flag: &str) -> Option<String> {
    let index = args.iter().position(|a| a == flag)?;
    args.get(index + 1).cloned()
}

fn named_value(args: &[String], flag: &str) -> Option<f32> {
    named_string(args, flag)?.parse().ok()
}

fn collect_args(argc: c_int, argv: *const *const c_char) -> Vec<String> {
    if argv.is_null() || argc <= 1 {
        return Vec::new();
    }
    // SAFETY: the C++ shim passes main()'s argc/argv unchanged, so argv holds
    // at least `argc` NUL-terminated pointers.
    (1..argc as usize)
        .filter_map(|i| {
            // SAFETY: `i` is inside `1..argc`, and argv holds at least `argc`
            // NUL-terminated pointers, because the C++ shim passes main()'s
            // own argc/argv through unchanged.
            unsafe {
                let ptr = *argv.add(i);
                if ptr.is_null() {
                    None
                } else {
                    std::ffi::CStr::from_ptr(ptr).to_str().ok().map(str::to_string)
                }
            }
        })
        .collect()
}
