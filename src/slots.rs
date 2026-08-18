//! The lever nobody asked for.
//!
//! Three reels, one per question the picker asks: harness, then provider, then model. All
//! three go at once on a pull and stop left to right, which is both what a real cabinet does
//! and the order the answers depend on each other in: the provider reel is refilled while it
//! is still turning, from the harness that has just landed, and the model reel spins on
//! casino symbols until the catalogue lookup behind it comes back.
//!
//! Nothing turns on its own. The cabinet comes up dark, lights itself, and waits for the
//! handle, because a machine that starts spinning by itself is a loading screen.
//!
//! Nothing here has a fixed size either. Provider and model ids run long, so the cabinet is
//! measured against the terminal every frame and takes the whole of it.
//!
//! This module owns the pixels, the dice and the physics. The state machine that turns a
//! landing into a selection lives with the picker, which is the only thing allowed to touch
//! its rows.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::time::{SystemTime, UNIX_EPOCH};

/// splitmix64 seeded off the clock. A menu does not need a crypto source to pick a row, and
/// a dependency for three `%` operations is a dependency to audit for ever.
pub struct Rng(u64);

impl Rng {
    pub fn new() -> Rng {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Rng(seed ^ 0xD1B5_4A32_D192_ED03)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A row index in `0..n`, and 0 for an empty list rather than a panic: an empty reel is
    /// a menu with nothing to offer, which the caller already has to handle.
    pub fn below(&mut self, n: usize) -> usize {
        match n {
            0 => 0,
            n => (self.next_u64() % n as u64) as usize,
        }
    }
}

/// What a drum is doing. The picker never sets this, it pulls the lever and reads the
/// landings back, so the whole spin lives in one place.
#[derive(PartialEq, Clone, Copy)]
enum Spin {
    /// Switched on, nothing at stake: barely turning.
    Rest,
    /// Winding up to full speed, and holding there until told where to stop.
    Free,
    /// Easing into the row it was given, overshooting it and settling back.
    Braking,
    Stopped,
}

/// Rows a drum crosses per second at full tilt. Fast enough that the names blur into
/// symbols, slow enough that the eye still reads it as a wheel and not as noise.
const TOP_SPEED: f64 = 24.0;
/// Rows per second it drifts at while waiting to be played.
const REST_SPEED: f64 = 1.1;
/// Rows per second squared on the way up. A drum that reaches full speed instantly has no
/// weight, and weight is most of what makes a spin look real.
const SPIN_UP: f64 = 46.0;
/// Above this the drum shows casino symbols instead of names: nothing is readable at speed,
/// and pretending otherwise is what made the old machine look like a list being replaced.
const BLUR_SPEED: f64 = 9.0;
/// What a blurred drum shows. Three cells wide at most, so a narrow cabinet keeps them.
const SYMBOLS: [&str; 6] = ["7 7 7", "$ $ $", "* * *", "B A R", "- - -", "$ 7 $"];

/// One column of the machine. `items` is what it can land on, `offset` where it is right
/// now, in rows, fractional so the motion has somewhere to live between frames.
pub struct Reel {
    pub title: &'static str,
    pub items: Vec<String>,
    offset: f64,
    speed: f64,
    state: Spin,
    /// Where the current brake started, how far it runs, and how far through it is. An eased
    /// interpolation rather than a deceleration: it has to land exactly on the row the dice
    /// chose, and a physical brake would need a correction at the end that reads as a stutter.
    from: f64,
    dist: f64,
    t: f64,
    dur: f64,
    /// Seconds of landing flash left. The drum hitting its stop is the payout of the spin.
    flash: f64,
}

impl Reel {
    /// A reel whose question has not been answered yet: it still turns, on nothing.
    pub fn teaser(title: &'static str) -> Reel {
        Reel {
            title,
            items: SYMBOLS.iter().map(|s| s.to_string()).collect(),
            offset: 0.0,
            speed: 0.0,
            state: Spin::Rest,
            from: 0.0,
            dist: 0.0,
            t: 0.0,
            dur: 0.0,
            flash: 0.0,
        }
    }

    /// New contents under a drum that may well be turning: the motion is kept and only the
    /// position is brought back into range, so a reel refilled mid-spin never jumps.
    pub fn load(&mut self, items: Vec<String>) {
        self.items = match items.is_empty() {
            true => vec!["nothing".to_string()],
            false => items,
        };
        self.offset = self.offset.rem_euclid(self.items.len() as f64);
    }

    /// Let go of the brake and wind up.
    pub fn kick(&mut self) {
        self.state = Spin::Free;
        self.flash = 0.0;
    }

    /// Bring it down onto `row`, after `laps` more turns so the stop is watched rather than
    /// noticed. Ignored unless the drum is actually free, which is what keeps a second call
    /// from restarting a brake already under way.
    pub fn brake_to(&mut self, row: usize, laps: f64) {
        if self.state != Spin::Free {
            return;
        }
        let n = self.items.len().max(1) as f64;
        let ahead = (row as f64 - self.offset).rem_euclid(n);
        self.from = self.offset;
        self.dist = ahead + laps * n;
        self.t = 0.0;
        // Long enough for the eye to follow the last few rows in, and scaled by the distance
        // so a long brake is not a slow one.
        self.dur = (0.55 + self.dist / TOP_SPEED).min(2.2);
        self.state = Spin::Braking;
    }

    /// One frame of physics. True on the frame the drum comes to rest, which is the picker's
    /// cue to read the row and fill the next reel.
    pub fn tick(&mut self, dt: f64) -> bool {
        let n = self.items.len().max(1) as f64;
        self.flash = (self.flash - dt).max(0.0);
        match self.state {
            Spin::Rest => {
                self.speed = REST_SPEED;
                self.offset = (self.offset + self.speed * dt).rem_euclid(n);
                false
            }
            Spin::Free => {
                self.speed = (self.speed + SPIN_UP * dt).min(TOP_SPEED);
                self.offset = (self.offset + self.speed * dt).rem_euclid(n);
                false
            }
            Spin::Braking => {
                self.t = (self.t + dt).min(self.dur);
                let x = self.t / self.dur;
                let before = self.offset;
                self.offset = (self.from + self.dist * settle(x)).rem_euclid(n);
                // Speed is what the drawing reads to decide whether the names are legible,
                // so it is measured off the curve rather than carried alongside it.
                self.speed = ((self.offset - before).rem_euclid(n) / dt.max(1e-6)).min(TOP_SPEED);
                match self.t >= self.dur {
                    true => {
                        self.offset = self.offset.round().rem_euclid(n);
                        self.speed = 0.0;
                        self.state = Spin::Stopped;
                        self.flash = 0.45;
                        true
                    }
                    false => false,
                }
            }
            Spin::Stopped => false,
        }
    }

    pub fn stopped(&self) -> bool {
        self.state == Spin::Stopped
    }

    /// The row on the payline, which is only meaningful once it has stopped.
    pub fn row(&self) -> usize {
        (self.offset.round() as isize).rem_euclid(self.items.len().max(1) as isize) as usize
    }

    /// What sits `offset` rows off the payline, as the eye would see it: names when the drum
    /// is slow enough to read, symbols when it is not.
    fn face(&self, offset: isize) -> &str {
        if self.items.is_empty() {
            return "";
        }
        let i = self.offset.floor() as isize + offset;
        match self.speed > BLUR_SPEED {
            true => SYMBOLS[i.rem_euclid(SYMBOLS.len() as isize) as usize],
            false => &self.items[i.rem_euclid(self.items.len() as isize) as usize],
        }
    }
}

/// Eased landing with a bounce: fast in, then past the row and back onto it. `x` runs 0 to 1
/// and the curve ends at exactly 1, which is what lets the drum stop on the chosen row.
fn settle(x: f64) -> f64 {
    // A back-out curve. The overshoot is deliberately under a row, so the drum is seen to
    // strain past its stop rather than to skip one.
    const OVERSHOOT: f64 = 1.30;
    let u = x - 1.0;
    1.0 + u * u * ((OVERSHOOT + 1.0) * u + OVERSHOOT)
}

/// What the machine is doing this frame. Drawing reads it, nothing else does.
pub struct View<'a> {
    pub reels: &'a [Reel; 3],
    /// Frame counter. Drives the marquee, the blinking and the jackpot colours, so the
    /// animation needs no clock of its own.
    pub tick: usize,
    /// The handle, 0 up and 1 fully down. Fractional, because the pull and the slower return
    /// stroke are both eased.
    pub lever: f64,
    /// How lit the cabinet is, 0 dark and 1 fully on. Runs once when the machine is opened.
    pub boot: f64,
    /// Waiting for a pull. The handle pulses and the banner says how to reach it.
    pub idle: bool,
    pub jackpot: bool,
    pub status: String,
}

/// Narrowest a reel can be and still say anything: `douane` and a short model id fit, a long
/// one is trimmed. Below this the cabinet stops shrinking and lets the terminal cut it off,
/// since a column three characters wide answers no question.
const MIN_CELL: usize = 10;
/// Widest a reel gets. Past a full model id and its suffix the column is only stretching,
/// and three columns that wide already fill a very large terminal.
const MAX_CELL: usize = 60;
/// The right-hand margin the handle lives in, and the click target the picker reads back.
const MARGIN: usize = 9;
/// Columns the cabinet spends on something other than the reels: two walls, and the four
/// spaces framing and separating the three columns.
const GUTTERS: usize = 6;
/// Rows the cabinet spends on something other than the drum: sign, banner, titles, edges,
/// status, marquees, borders, plus the payline itself.
const CHROME: usize = 13;
/// Deepest drum. Past this the payline is so far from the edges that the eye loses it.
const MAX_REACH: isize = 16;

/// Row the handle starts on. Level with the reel titles, so it stands beside the drum.
const LEVER_TOP: usize = 5;

/// The casino palette, cycled by the tick so the frame never sits still.
const LIGHTS: [Color; 4] = [
    Color::LightRed,
    Color::LightYellow,
    Color::LightMagenta,
    Color::LightCyan,
];

/// The cabinet measured against the terminal it has to live in. Everything drawn reads its
/// sizes from here rather than from a constant, which is what lets a wide window show a full
/// model id instead of a stump.
pub struct Geo {
    /// Width of one reel column, borders included.
    pub cell: usize,
    /// Width between the two walls.
    pub inner: usize,
    /// Rows of drum above and below the payline.
    pub reach: isize,
    /// Width of the handle margin, and 0 when the terminal cannot spare it: on a narrow
    /// window the reels are worth more than the lever, which the space bar replaces.
    pub margin: usize,
    lever_base: usize,
}

impl Geo {
    pub fn fit(area: Rect) -> Geo {
        let w = area.width as usize;
        // The handle is the first thing dropped, and only once keeping it would squeeze the
        // reels under the point where an id is legible.
        let margin = match w.saturating_sub(GUTTERS + MARGIN) / 3 >= MIN_CELL {
            true => MARGIN,
            false => 0,
        };
        let cell = (w.saturating_sub(GUTTERS + margin) / 3).clamp(MIN_CELL, MAX_CELL);
        // A taller terminal buys deeper drums, which is the one part of the machine that
        // reads better big: more symbols in flight, more of a spin.
        let reach = ((area.height as isize - CHROME as isize) / 2).clamp(1, MAX_REACH);
        Geo {
            cell,
            inner: cell * 3 + 4,
            reach,
            margin,
            lever_base: (2 * reach as usize) + 9,
        }
    }

    /// Total rows, which is also what the caller has to have to see the whole cabinet.
    pub fn height(&self) -> usize {
        CHROME + 2 * self.reach as usize
    }

    /// Rows the ball travels on a full pull: everything between its rest and the plinth.
    fn throw(&self) -> usize {
        self.lever_base.saturating_sub(LEVER_TOP + 2)
    }
}

/// Draws the cabinet centred in `area` and answers with the handle's rectangle on screen,
/// which is the only part of it a click means anything on.
pub fn draw(f: &mut Frame, area: Rect, v: &View) -> Rect {
    let g = Geo::fit(area);
    let lines = lines(v, &g);
    let width = (g.inner as u16 + 2 + g.margin as u16).min(area.width);
    let height = (lines.len() as u16).min(area.height);
    // Centred, and clamped rather than skipped on a small terminal: a machine with its top
    // row cut off is still playable, a machine that refuses to draw is a broken easter egg.
    let box_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Paragraph::new(lines), box_area);

    if g.margin == 0 {
        return Rect::default();
    }
    // The whole margin, not the three cells the ball sits on: a handle you have to hit
    // exactly is a handle nobody pulls twice.
    let handle = Rect {
        x: box_area.x + g.inner as u16 + 2,
        y: box_area.y + LEVER_TOP as u16,
        width: g.margin as u16,
        height: (g.lever_base - LEVER_TOP + 1) as u16,
    };
    // On a terminal too short for the handle it is simply not on screen, and an empty
    // rectangle is what says so: `intersection` keeps the off-screen corner when it has
    // nothing to keep, which would leave a click target hanging past the last row.
    match handle.intersection(box_area) {
        r if r.width == 0 || r.height == 0 => Rect::default(),
        r => r,
    }
}

fn lines(v: &View, g: &Geo) -> Vec<Line<'static>> {
    let frame = frame_colour(v);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(g.height());

    out.push(row(
        out.len(),
        vec![Span::styled(
            format!("╔{}╗", "═".repeat(g.inner)),
            Style::new().fg(frame),
        )],
        v,
        g,
    ));
    out.push(row(
        out.len(),
        vec![wall(frame), marquee(v.tick, frame, g), wall(frame)],
        v,
        g,
    ));
    out.push(row(
        out.len(),
        vec![
            wall(frame),
            Span::styled(
                centred("F A S T P I C K   S L O T S", g.inner),
                Style::new()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            wall(frame),
        ],
        v,
        g,
    ));
    out.push(row(
        out.len(),
        vec![wall(frame), banner(v, g), wall(frame)],
        v,
        g,
    ));
    out.push(row(
        out.len(),
        vec![Span::styled(
            format!("╠{}╣", "═".repeat(g.inner)),
            Style::new().fg(frame),
        )],
        v,
        g,
    ));

    // The three questions, over the reel that answers them.
    let mut titles = String::from(" ");
    for r in v.reels {
        titles.push_str(&centred(r.title, g.cell));
        titles.push(' ');
    }
    out.push(row(
        out.len(),
        vec![
            wall(frame),
            Span::styled(pad(&titles, g.inner), Style::new().fg(Color::DarkGray)),
            wall(frame),
        ],
        v,
        g,
    ));

    out.push(row(out.len(), edge('┌', '┐', frame, g), v, g));
    for offset in -g.reach..=g.reach {
        out.push(row(out.len(), band(v, offset, g), v, g));
    }
    out.push(row(out.len(), edge('└', '┘', frame, g), v, g));

    out.push(row(
        out.len(),
        vec![wall(frame), blank(g), wall(frame)],
        v,
        g,
    ));
    out.push(row(
        out.len(),
        vec![
            wall(frame),
            Span::styled(
                pad(&format!("  {}", v.status), g.inner),
                match v.jackpot {
                    true => Style::new()
                        .fg(Color::LightGreen)
                        .add_modifier(Modifier::BOLD),
                    false => Style::new().fg(Color::Gray),
                },
            ),
            wall(frame),
        ],
        v,
        g,
    ));
    out.push(row(
        out.len(),
        vec![wall(frame), marquee(v.tick + 3, frame, g), wall(frame)],
        v,
        g,
    ));
    out.push(row(
        out.len(),
        vec![Span::styled(
            format!("╚{}╝", "═".repeat(g.inner)),
            Style::new().fg(frame),
        )],
        v,
        g,
    ));
    out
}

fn frame_colour(v: &View) -> Color {
    match v.jackpot {
        // The whole cabinet joins in once it has paid out.
        true => LIGHTS[(v.tick / 2) % LIGHTS.len()],
        false => Color::LightMagenta,
    }
}

/// The line under the sign: what the machine wants from you, or what it has just done.
fn banner(v: &View, g: &Geo) -> Span<'static> {
    let (text, style) = match (v.jackpot, v.idle) {
        (true, _) => (
            "*  J A C K P O T  *",
            Style::new()
                .fg(LIGHTS[(v.tick / 2) % LIGHTS.len()])
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        (false, true) => (
            // Without a margin there is no handle to point at, so the invitation names the
            // key that is left.
            match g.margin {
                0 => "insert coin  >>  press space",
                _ => "insert coin  >>  pull the handle",
            },
            // Blinks slower than the marquee, which is what makes the eye go to it.
            match (v.tick / 6) % 2 {
                0 => Style::new()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
                _ => Style::new().fg(Color::DarkGray),
            },
        ),
        (false, false) => (
            "no refunds  --  the house picks",
            Style::new().fg(Color::DarkGray),
        ),
    };
    Span::styled(centred(text, g.inner), style)
}

/// One line of the cabinet, with the lever drawn in the margin beside it, dimmed while the
/// machine is still lighting up.
fn row(index: usize, mut spans: Vec<Span<'static>>, v: &View, g: &Geo) -> Line<'static> {
    spans.push(lever(index, v, g));
    if !lit(index, v, g) {
        // Everything is drawn either way: the cabinet is there in the dark and the current
        // reaches it, which is a machine warming up rather than a menu being assembled.
        for s in spans.iter_mut() {
            s.style = Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM);
        }
    }
    Line::from(spans)
}

/// Power runs out from the payline, so the reels come up first and the frame last.
fn lit(index: usize, v: &View, g: &Geo) -> bool {
    if v.boot >= 1.0 {
        return true;
    }
    let middle = (g.height() / 2) as f64;
    let dist = (index as f64 - middle).abs();
    dist <= v.boot * (middle + 1.0)
}

fn wall(c: Color) -> Span<'static> {
    Span::styled("║", Style::new().fg(c))
}

fn blank(g: &Geo) -> Span<'static> {
    Span::raw(" ".repeat(g.inner))
}

fn edge(left: char, right: char, c: Color, g: &Geo) -> Vec<Span<'static>> {
    let mut s = String::from(" ");
    for _ in 0..3 {
        s.push(left);
        s.push_str(&"─".repeat(g.cell - 2));
        s.push(right);
        s.push(' ');
    }
    vec![
        wall(c),
        Span::styled(pad(&s, g.inner), Style::new().fg(Color::DarkGray)),
        wall(c),
    ]
}

/// A horizontal slice through all three reels: the payline in the middle, drum above and
/// below it, fading with distance so the column reads as curved.
fn band(v: &View, offset: isize, g: &Geo) -> Vec<Span<'static>> {
    let lit = offset == 0;
    let frame = frame_colour(v);
    // The payline, marked in the margin the other bands leave blank rather than in a column
    // of its own: the result of a pull is the middle row and nothing else.
    let arrow = |c: &'static str| match lit {
        true => Span::styled(
            c,
            Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ),
        false => Span::raw(" "),
    };
    let mut spans = vec![wall(frame), arrow("▶")];
    for r in v.reels {
        spans.push(Span::styled("│", Style::new().fg(Color::DarkGray)));
        let text = fit(r.face(offset), g.cell - 2);
        let moving = r.speed > BLUR_SPEED;
        let style = match (lit, r.stopped(), v.jackpot) {
            // Landed, and the machine has paid: the answer flashes.
            (true, true, true) => Style::new()
                .fg(LIGHTS[(v.tick / 2) % LIGHTS.len()])
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            // The frames right after a drum hits its stop, so the eye is told which one just
            // landed rather than having to find it.
            (true, true, false) if r.flash > 0.0 => Style::new()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            (true, true, false) => Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            (true, false, _) => match moving {
                true => Style::new().fg(Color::LightRed),
                false => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            },
            // Off the payline, and the further off the fainter: the drum falls away. A
            // moving drum fades harder, which is the smear the terminal cannot draw.
            _ => match (offset.abs(), moving) {
                (1, false) => Style::new().fg(Color::Gray),
                (1, true) => Style::new().fg(Color::DarkGray),
                _ => Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            },
        };
        spans.push(Span::styled(text, style));
        spans.push(Span::styled("│", Style::new().fg(Color::DarkGray)));
        spans.push(Span::raw(" "));
    }
    spans.pop();
    spans.push(arrow("◀"));
    spans.push(wall(frame));
    spans
}

/// The chase lights, one cell shifted per frame.
fn marquee(tick: usize, frame: Color, g: &Geo) -> Span<'static> {
    let mut s = String::with_capacity(g.inner);
    for i in 0..g.inner {
        s.push(match (i + tick) % 6 {
            0 => '*',
            3 => '.',
            _ => ' ',
        });
    }
    Span::styled(s, Style::new().fg(frame))
}

/// The handle, in the right margin: a knob riding a rod down a track into its housing. Every
/// row of the margin is drawn, blank ones included, so the click target is a solid block.
fn lever(index: usize, v: &View, g: &Geo) -> Span<'static> {
    if g.margin == 0 || !(LEVER_TOP..=g.lever_base).contains(&index) {
        return Span::raw("");
    }
    let knob = LEVER_TOP + (v.lever.clamp(0.0, 1.0) * g.throw() as f64).round() as usize;
    let pulling = v.lever > 0.05;
    let (art, style) = match index {
        // The housing the rod disappears into, bolted to the side of the cabinet.
        i if i == g.lever_base => (
            " ▐█████▌ ",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        i if i == g.lever_base - 1 => ("  ▄▄▄▄▄  ", Style::new().fg(Color::Gray)),
        i if i == knob => (
            "  ((◉))  ",
            Style::new()
                .fg(match (pulling, v.idle && (v.tick / 5) % 2 == 1) {
                    // Under the hand, so it goes hot.
                    (true, _) => Color::LightYellow,
                    // Idle, so it pulses: the one thing on screen asking to be touched.
                    (false, true) => Color::LightRed,
                    (false, false) => Color::Red,
                })
                .add_modifier(Modifier::BOLD),
        ),
        // The rod, thicker just under the knob so the handle has a direction.
        i if i == knob + 1 => ("    ┃    ", Style::new().fg(Color::White)),
        i if i > knob => ("    ┃    ", Style::new().fg(Color::Gray)),
        // The track above it, which is what says how far the thing still has to travel.
        _ => (
            "    ┊    ",
            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        ),
    };
    Span::styled(art.to_string(), style)
}

/// Trimmed to the cell, ellipsis included, so a long model id cannot push the cabinet open.
fn fit(text: &str, width: usize) -> String {
    let n = text.chars().count();
    if n > width {
        let mut s: String = text.chars().take(width.saturating_sub(1)).collect();
        s.push('~');
        return s;
    }
    let left = (width - n) / 2;
    format!(
        "{}{}{}",
        " ".repeat(left),
        text,
        " ".repeat(width - n - left)
    )
}

fn centred(text: &str, width: usize) -> String {
    fit(text, width)
}

fn pad(text: &str, width: usize) -> String {
    let n = text.chars().count();
    match n >= width {
        true => text.chars().take(width).collect(),
        false => format!("{text}{}", " ".repeat(width - n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn reel(items: &[&str]) -> Reel {
        let mut r = Reel::teaser("harness");
        r.load(items.iter().map(|s| s.to_string()).collect());
        r
    }

    fn view(reels: &[Reel; 3], idle: bool, jackpot: bool) -> View<'_> {
        View {
            reels,
            tick: 3,
            lever: 0.5,
            boot: 1.0,
            idle,
            jackpot,
            status: "JACKPOT".into(),
        }
    }

    fn geo(width: u16, height: u16) -> Geo {
        Geo::fit(Rect::new(0, 0, width, height))
    }

    /// Every terminal the machine can be opened in, from a phone-sized pane to a wall.
    fn sizes() -> Vec<(u16, u16)> {
        vec![
            (20, 5),
            (40, 12),
            (60, 18),
            (80, 24),
            (100, 24),
            (120, 40),
            (200, 60),
            (400, 100),
        ]
    }

    #[test]
    fn every_line_is_the_same_width() {
        let reels = [
            reel(&["claude code"]),
            reel(&["a provider with a name that runs off the end of the cabinet"]),
            reel(&["cx-gpt-5-6-terra-with-a-long-suffix"]),
        ];
        for (w, h) in sizes() {
            let g = geo(w, h);
            // The lever margin is empty on the rows it does not reach, so the cabinet itself
            // is what has to line up: every row is the frame plus, at most, the handle.
            for line in lines(&view(&reels, false, true), &g) {
                let width = line.width();
                assert!(
                    width == g.inner + 2 || width == g.inner + 2 + g.margin,
                    "at {w}x{h} a row came out {width} wide, cabinet is {}",
                    g.inner + 2
                );
            }
        }
    }

    /// The reels grow with the terminal, which is the whole point of measuring it.
    #[test]
    fn a_wider_terminal_buys_wider_reels() {
        assert!(geo(200, 40).cell > geo(80, 24).cell);
        assert!(geo(200, 60).reach > geo(80, 24).reach);
        // Too narrow for the handle: the reels keep the room and the space bar takes over.
        assert_eq!(geo(34, 20).margin, 0);
        assert!(geo(100, 24).margin > 0);
    }

    /// A wide terminal has to be filled, not decorated with a small machine in the middle.
    #[test]
    fn a_big_terminal_is_mostly_cabinet() {
        for (w, h) in [(120u16, 30u16), (160, 45), (200, 50)] {
            let g = geo(w, h);
            let used = g.inner + 2 + g.margin;
            assert!(
                used + 6 >= w as usize,
                "at {w}x{h} the cabinet is {used} wide and leaves {} columns empty",
                w as usize - used
            );
            assert!(
                g.height() * 4 >= h as usize * 3,
                "at {w}x{h} the cabinet is only {} rows tall",
                g.height()
            );
        }
    }

    /// The handle has to fill the rectangle the click handler is handed, at every point of
    /// its travel and in every cabinet tall enough to carry one.
    #[test]
    fn the_handle_covers_its_whole_click_target() {
        let reels = [reel(&["x"]), reel(&["y"]), reel(&["z"])];
        for (w, h) in sizes() {
            let g = geo(w, h);
            if g.margin == 0 {
                continue;
            }
            for step in 0..=10 {
                let mut v = view(&reels, true, false);
                v.lever = step as f64 / 10.0;
                for index in LEVER_TOP..=g.lever_base {
                    assert_eq!(
                        lever(index, &v, &g).content.chars().count(),
                        g.margin,
                        "at {w}x{h}, row {index} of the handle is not the width of the target"
                    );
                }
            }
        }
    }

    /// The knob must never reach the housing, however short the cabinet is.
    #[test]
    fn the_knob_stays_on_its_rod() {
        for (w, h) in sizes() {
            let g = geo(w, h);
            assert!(
                g.throw() >= 1,
                "at {w}x{h} the handle has no rod left to run down"
            );
            assert!(LEVER_TOP + g.throw() <= g.lever_base - 2);
        }
    }

    /// A spin has to wind up, blur, and come to rest exactly on the row it was given.
    #[test]
    fn a_braked_reel_lands_on_its_row() {
        for row in 0..5 {
            let mut r = reel(&["a", "b", "c", "d", "e"]);
            r.kick();
            for _ in 0..40 {
                r.tick(1.0 / 30.0);
            }
            assert!(r.speed > BLUR_SPEED, "the drum never reached full speed");
            r.brake_to(row, 2.0);
            let mut frames = 0;
            while !r.tick(1.0 / 30.0) {
                frames += 1;
                assert!(frames < 300, "the drum never came to rest");
            }
            assert_eq!(r.row(), row);
            assert!(r.stopped());
            // And it stays put once it has: a stopped drum is an answer.
            let before = r.offset;
            r.tick(1.0 / 30.0);
            assert_eq!(r.offset, before);
        }
    }

    /// The landing curve overshoots, which is the bounce, but never by a whole row and it
    /// always comes back to exactly where it was sent.
    #[test]
    fn the_landing_bounces_without_skipping_a_row() {
        assert!(settle(0.0).abs() < 1e-9);
        assert!((settle(1.0) - 1.0).abs() < 1e-9);
        let peak = (0..=100)
            .map(|i| settle(i as f64 / 100.0))
            .fold(0.0f64, f64::max);
        assert!(peak > 1.0, "no overshoot, so no bounce");
        assert!(peak < 1.08, "the bounce is worth more than a whole row");
    }

    /// Refilling a turning drum keeps it turning, since the provider reel is loaded mid-spin
    /// from the harness that has just landed.
    #[test]
    fn a_reel_refilled_mid_spin_keeps_its_motion() {
        let mut r = reel(&["a", "b"]);
        r.kick();
        for _ in 0..20 {
            r.tick(1.0 / 30.0);
        }
        let speed = r.speed;
        r.load(vec!["one".into(), "two".into(), "three".into()]);
        assert_eq!(r.speed, speed);
        assert!(r.offset < 3.0, "the position was left out of range");
        assert!(!r.stopped());
    }

    /// A terminal smaller than the cabinet must still render something rather than panic.
    #[test]
    fn draws_into_any_terminal() {
        let reels = [reel(&["x"]), reel(&["y"]), reel(&["z"])];
        for (w, h) in sizes() {
            for boot in [0.0, 0.4, 1.0] {
                let mut v = view(&reels, true, false);
                v.boot = boot;
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                terminal
                    .draw(|f| {
                        let handle = draw(f, f.area(), &v);
                        // Clamped into the frame, so a click is never tested against a
                        // rectangle hanging off the screen.
                        assert!(handle.right() <= f.area().right());
                        assert!(handle.bottom() <= f.area().bottom());
                    })
                    .expect("a small terminal must not stop the machine");
            }
        }
    }

    #[test]
    fn below_stays_in_range() {
        let mut rng = Rng::new();
        for _ in 0..200 {
            assert!(rng.below(7) < 7);
        }
        assert_eq!(rng.below(0), 0);
    }
}
