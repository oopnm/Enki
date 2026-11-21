//! The Abzu drawings: waves along the bottom, the two rivers that meet when the phone is linked, and the faint
//! cuneiform-style dividers. All of it is Cairo, drawn from the shared colors in `tokens.rs`.
//!
//! One `Motion` drives every animated widget. It runs a single timer, capped at 30 frames a second (15 when the
//! water is calm), and the timer is not running at all while the window is not focused or "reduce motion" is on:
//! the widgets then draw one still frame. That keeps the idle cost near zero.

use crate::tokens;
use gtk::cairo::{Context, LinearGradient, RadialGradient};
use gtk::glib;
use gtk::prelude::*;
use gtk::DrawingArea;
use std::cell::{Cell, RefCell};
use std::f64::consts::TAU;
use std::rc::Rc;
use std::time::{Duration, Instant};

type Rgb = (f64, f64, f64);

/// What the water shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visual {
    /// Nothing is running: calm water, the two rivers apart.
    Idle,
    /// The link is being set up or recovered: the rivers reach for each other.
    Linking,
    /// The phone is linked and streaming: one river, lively waves.
    Live,
}

impl Visual {
    /// How lively the waves are for this state (0 calm .. 1 streaming).
    fn target_level(self) -> f64 {
        match self {
            Visual::Idle => 0.2,
            Visual::Linking => 0.55,
            Visual::Live => 1.0,
        }
    }
}

const SINE_STEPS: usize = 1024;

pub struct Motion {
    t: Cell<f64>,
    level: Cell<f64>,
    /// 1.0 right after the link came up, fading out: the one place the gold color is used.
    glow: Cell<f64>,
    visual: Cell<Visual>,
    reduce: Cell<bool>,
    active: Cell<bool>,
    last: Cell<Instant>,
    timer: RefCell<Option<glib::SourceId>>,
    areas: RefCell<Vec<glib::WeakRef<DrawingArea>>>,
    sine: Vec<f64>,
}

impl Motion {
    pub fn new() -> Rc<Motion> {
        Rc::new(Motion {
            t: Cell::new(0.0),
            level: Cell::new(Visual::Idle.target_level()),
            glow: Cell::new(0.0),
            visual: Cell::new(Visual::Idle),
            reduce: Cell::new(false),
            active: Cell::new(true),
            last: Cell::new(Instant::now()),
            timer: RefCell::new(None),
            areas: RefCell::new(Vec::new()),
            sine: (0..SINE_STEPS)
                .map(|i| (i as f64 / SINE_STEPS as f64 * TAU).sin())
                .collect(),
        })
    }

    /// Sine from the precomputed table (the waves are drawn many times a second).
    fn sin(&self, theta: f64) -> f64 {
        let turns = (theta / TAU).rem_euclid(1.0);
        self.sine[(turns * SINE_STEPS as f64) as usize % SINE_STEPS]
    }

    pub fn set_visual(self: &Rc<Self>, visual: Visual) {
        let before = self.visual.replace(visual);
        if visual == Visual::Live && before != Visual::Live {
            self.glow.set(1.0);
        }
        self.sync();
    }

    pub fn set_reduce(self: &Rc<Self>, reduce: bool) {
        self.reduce.set(reduce);
        self.sync();
    }

    /// The window has the focus (and is not minimised).
    pub fn set_active(self: &Rc<Self>, active: bool) {
        self.active.set(active);
        if active {
            self.last.set(Instant::now());
        }
        self.sync();
    }

    fn register(&self, area: &DrawingArea) {
        let mut areas = self.areas.borrow_mut();
        areas.retain(|w| w.upgrade().is_some());
        areas.push(area.downgrade());
    }

    fn redraw(&self) {
        for weak in self.areas.borrow().iter() {
            if let Some(area) = weak.upgrade() {
                area.queue_draw();
            }
        }
    }

    /// Starts or stops the timer to match the state, and draws a still frame when it is not running.
    fn sync(self: &Rc<Self>) {
        let animate = self.active.get() && !self.reduce.get();
        if self.reduce.get() {
            self.level.set(self.visual.get().target_level());
            self.glow.set(0.0);
        }
        if !animate {
            if let Some(id) = self.timer.borrow_mut().take() {
                id.remove();
            }
            self.redraw();
            return;
        }
        if self.timer.borrow().is_some() {
            return;
        }
        self.last.set(Instant::now());
        let motion = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(33), move || {
            motion.tick();
            glib::ControlFlow::Continue
        });
        *self.timer.borrow_mut() = Some(id);
    }

    fn tick(&self) {
        let now = Instant::now();
        let dt = now
            .duration_since(self.last.replace(now))
            .as_secs_f64()
            .min(0.25);
        let visual = self.visual.get();
        // Calm water moves at half the frame rate: every other tick is skipped.
        if visual == Visual::Idle && self.glow.get() <= 0.0 {
            let calm_tick = (self.t.get() * 30.0) as u64 % 2 == 0;
            self.t.set(self.t.get() + dt);
            self.level
                .set(approach(self.level.get(), visual.target_level(), dt, 1.5));
            if calm_tick {
                return;
            }
        } else {
            self.t.set(self.t.get() + dt);
            self.level
                .set(approach(self.level.get(), visual.target_level(), dt, 1.5));
            self.glow.set((self.glow.get() - dt / 2.5).max(0.0));
        }
        self.redraw();
    }

    // ---- drawing -----------------------------------------------------------------------------------

    /// The wave strip along the bottom of the window.
    pub fn wave_area(self: &Rc<Self>, height: i32) -> DrawingArea {
        let area = DrawingArea::builder()
            .content_height(height)
            .hexpand(true)
            .can_target(false)
            .build();
        let motion = self.clone();
        area.set_draw_func(move |_, cr, w, h| motion.draw_waves(cr, w as f64, h as f64));
        self.register(&area);
        area
    }

    fn draw_waves(&self, cr: &Context, w: f64, h: f64) {
        let level = self.level.get();
        let t = self.t.get();
        // (base line as a fraction of the height, amplitude, wave length in px, speed, color, alpha)
        let layers: [(f64, f64, f64, f64, Rgb, f64); 3] = [
            (0.50, 0.10 + 0.12 * level, 260.0, 0.55, tokens::LAPIS, 0.40),
            (
                0.62,
                0.08 + 0.10 * level,
                190.0,
                -0.80,
                tokens::AQUA,
                0.14 + 0.10 * level,
            ),
            (0.74, 0.06 + 0.07 * level, 130.0, 1.10, tokens::LAPIS, 0.55),
        ];
        for (i, (base, amp, length, speed, color, alpha)) in layers.iter().enumerate() {
            let phase = t * speed * (0.6 + 0.8 * level) + i as f64 * 1.7;
            let y0 = h * base;
            let a = h * amp;
            cr.move_to(0.0, h);
            let mut x = 0.0;
            while x <= w + 6.0 {
                let theta = x / length * TAU + phase;
                let y = y0 + a * (self.sin(theta) + 0.45 * self.sin(theta * 2.3 - phase * 0.6));
                cr.line_to(x, y);
                x += 6.0;
            }
            cr.line_to(w + 6.0, h);
            cr.close_path();
            let fill = LinearGradient::new(0.0, y0 - a, 0.0, h);
            fill.add_color_stop_rgba(0.0, color.0, color.1, color.2, *alpha);
            fill.add_color_stop_rgba(1.0, tokens::ABYSS.0, tokens::ABYSS.1, tokens::ABYSS.2, 0.0);
            let _ = cr.set_source(&fill);
            let _ = cr.fill_preserve();
            // A faint foam line along the crest of the first layer.
            if i == 0 {
                cr.set_line_width(1.2);
                let (r, g, b) = tokens::FOAM;
                cr.set_source_rgba(r, g, b, 0.10 + 0.22 * level);
                let _ = cr.stroke();
            } else {
                cr.new_path();
            }
        }
    }

    /// The two rivers: the computer on one side, the phone on the other. Apart while idle, reaching for each
    /// other while linking, one river when linked.
    pub fn river_area(self: &Rc<Self>, height: i32) -> DrawingArea {
        let area = DrawingArea::builder()
            .content_height(height)
            .hexpand(true)
            .build();
        let motion = self.clone();
        area.set_draw_func(move |_, cr, w, h| motion.draw_rivers(cr, w as f64, h as f64));
        self.register(&area);
        area
    }

    fn draw_rivers(&self, cr: &Context, w: f64, h: f64) {
        let t = self.t.get();
        let visual = self.visual.get();
        let ym = h / 2.0;
        let (xl, xr) = (34.0, w - 34.0);
        let cx = w / 2.0;
        // How far each river reaches toward the middle (1.0 = they meet).
        let reach = match visual {
            Visual::Idle => 0.34,
            Visual::Linking => 0.55 + 0.17 * self.sin(t * 2.2),
            Visual::Live => 1.0,
        };
        let live = visual == Visual::Live;
        let amp = if live { 5.0 } else { 7.0 };
        let wavelength = 120.0;
        let flow = t * 3.2;
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);

        // One path from the starting node to `end_x`, bending like flowing water.
        let river = |from: f64, to: f64, sign: f64| {
            let steps = ((to - from).abs() / 4.0).max(2.0) as usize;
            for s in 0..=steps {
                let x = from + (to - from) * s as f64 / steps as f64;
                // The bend fades out near the nodes so the lines start and end on the icons.
                let edge = ((x - xl).min(xr - x) / 40.0).clamp(0.0, 1.0);
                let y = ym + amp * edge * self.sin((x / wavelength) * TAU + sign * flow);
                if s == 0 {
                    cr.move_to(x, y);
                } else {
                    cr.line_to(x, y);
                }
            }
        };
        let left_end = xl + 22.0 + (cx - xl - 22.0) * reach;
        let right_end = xr - 22.0 - (xr - cx - 22.0) * reach;

        if live {
            // One river from the phone to the computer, with a soft glow under it.
            for (width, alpha, color) in [
                (9.0, 0.10, tokens::AQUA),
                (4.0, 0.55, tokens::AQUA),
                (1.6, 0.95, tokens::FOAM),
            ] {
                river(xl + 22.0, xr - 22.0, -1.0);
                cr.set_line_width(width);
                cr.set_source_rgba(color.0, color.1, color.2, alpha);
                let _ = cr.stroke();
            }
        } else {
            let (color, alpha) = if visual == Visual::Linking {
                (tokens::AQUA, 0.75)
            } else {
                (tokens::LAPIS, 0.85)
            };
            cr.set_line_width(3.0);
            cr.set_source_rgba(color.0, color.1, color.2, alpha);
            cr.set_dash(&[9.0, 7.0], -t * 30.0);
            river(xl + 22.0, left_end, 1.0);
            let _ = cr.stroke();
            river(xr - 22.0, right_end, -1.0);
            let _ = cr.stroke();
            cr.set_dash(&[], 0.0);
        }

        // The gold flash where the rivers meet, once, when the link comes up.
        let glow = self.glow.get();
        if glow > 0.0 {
            let radius = 14.0 + 26.0 * (1.0 - glow);
            let g = RadialGradient::new(cx, ym, 0.0, cx, ym, radius);
            g.add_color_stop_rgba(
                0.0,
                tokens::GOLD.0,
                tokens::GOLD.1,
                tokens::GOLD.2,
                0.85 * glow,
            );
            g.add_color_stop_rgba(1.0, tokens::GOLD.0, tokens::GOLD.1, tokens::GOLD.2, 0.0);
            let _ = cr.set_source(&g);
            let _ = cr.arc(cx, ym, radius, 0.0, TAU);
            let _ = cr.fill();
        }

        // The two ends: a computer screen and a phone.
        let tint = if live { 0.95 } else { 0.55 };
        let (fr, fg, fb) = tokens::FOAM;
        cr.set_source_rgba(fr, fg, fb, tint);
        cr.set_line_width(2.0);
        rounded_rect(cr, xl - 16.0, ym - 12.0, 32.0, 21.0, 4.0);
        let _ = cr.stroke();
        cr.move_to(xl - 7.0, ym + 15.0);
        cr.line_to(xl + 7.0, ym + 15.0);
        let _ = cr.stroke();
        rounded_rect(cr, xr - 8.0, ym - 14.0, 16.0, 28.0, 4.0);
        let _ = cr.stroke();
        cr.arc(xr, ym + 9.5, 1.3, 0.0, TAU);
        let _ = cr.fill();
    }

    /// A thin line of abstract wedge marks, like writing pressed into clay. Shapes only, never text. Static: it is
    /// drawn once and again only when the window is resized.
    pub fn divider_area(seed: &str) -> DrawingArea {
        let seed = seed
            .bytes()
            .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
        let area = DrawingArea::builder()
            .content_height(14)
            .hexpand(true)
            .can_target(false)
            .build();
        area.set_draw_func(move |_, cr, w, h| draw_marks(cr, w as f64, h as f64, seed));
        area
    }
}

fn approach(current: f64, target: f64, dt: f64, rate: f64) -> f64 {
    current + (target - current) * (1.0 - (-rate * dt).exp())
}

fn rounded_rect(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -TAU / 4.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, TAU / 4.0);
    cr.arc(x + r, y + h - r, r, TAU / 4.0, TAU / 2.0);
    cr.arc(x + r, y + r, r, TAU / 2.0, TAU * 0.75);
    cr.close_path();
}

/// One wedge: a small triangle head with a tail, turned by `angle`.
fn wedge(cr: &Context, x: f64, y: f64, angle: f64, size: f64) {
    cr.save().ok();
    cr.translate(x, y);
    cr.rotate(angle);
    cr.move_to(-size * 0.35, -size * 0.5);
    cr.line_to(size * 0.35, -size * 0.5);
    cr.line_to(0.0, size * 0.05);
    cr.close_path();
    let _ = cr.fill();
    cr.move_to(0.0, size * 0.0);
    cr.line_to(0.0, size * 0.55);
    cr.set_line_width(1.1);
    let _ = cr.stroke();
    cr.restore().ok();
}

fn draw_marks(cr: &Context, w: f64, h: f64, seed: u32) {
    let (r, g, b) = tokens::FOAM;
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    let mut x = 6.0;
    while x < w - 6.0 {
        // Marks fade out toward both ends of the line.
        let edge = (x.min(w - x) / 70.0).clamp(0.0, 1.0);
        cr.set_source_rgba(r, g, b, 0.22 * edge);
        let size = 7.0 + (next() % 4) as f64;
        match next() % 4 {
            0 => wedge(cr, x, h / 2.0, 0.0, size),
            1 => wedge(cr, x, h / 2.0, TAU / 4.0, size),
            2 => {
                wedge(cr, x - 2.5, h / 2.0, 0.0, size * 0.8);
                wedge(cr, x + 2.5, h / 2.0, 0.0, size * 0.8);
            }
            _ => wedge(cr, x, h / 2.0, TAU / 8.0, size),
        }
        x += 12.0 + (next() % 3) as f64 * 6.0;
    }
}
