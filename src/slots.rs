//! The lever nobody asked for.
//!
//! Three reels, one per question the picker asks: harness, then provider, then model. They
//! stop left to right because that is the order the answers depend on each other in, so the
//! machine is not only decoration: a provider reel cannot hold anything until the harness
//! reel has landed, and the model reel spins on whatever the catalogue lookup is still
//! fetching behind it.
//!
//! Nothing turns on its own. The cabinet comes up idle and waits for the handle to be
//! pulled, because a machine that starts spinning by itself is a loading screen.
//!
//! Nothing here has a fixed size either. Provider and model ids run long, so the cabinet is
//! measured against the terminal every frame and takes all the room it is given, down to a
//! window that cannot hold the handle at all.
//!
//! This module owns the pixels and the dice. The state machine that turns a landing into a
//! selection lives with the picker, which is the only thing allowed to touch its rows.

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

/// One column of the machine. `items` is what it can land on, `pos` where it is right now.
pub struct Reel {
    pub title: &'static str,
    pub items: Vec<String>,
    pub pos: usize,
    pub stopped: bool,
}

impl Reel {
    /// A reel whose question has not been answered yet: it still turns, on nothing.
    pub fn teaser(title: &'static str) -> Reel {
        Reel {
            title,
            items: ["? ? ?", "$ $ $", "7 7 7", "* * *", "B A R", "- - -"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            pos: 0,
            stopped: false,
        }
    }

    pub fn load(&mut self, items: Vec<String>) {
        self.items = match items.is_empty() {
            true => vec!["nothing".to_string()],
            false => items,
        };
        self.pos = 0;
    }

    fn at(&self, offset: isize) -> &str {
        if self.items.is_empty() {
            return "";
        }
        let n = self.items.len() as isize;
        let i = (self.pos as isize + offset).rem_euclid(n) as usize;
        &self.items[i]
    }
}

/// What the machine is doing this frame. Drawing reads it, nothing else does.
pub struct View<'a> {
    pub reels: &'a [Reel; 3],
    /// Frame counter. Drives the marquee, the blinking and the jackpot colours, so the
    /// animation needs no clock of its own.
    pub tick: usize,
    /// 0 up, `LEVER_THROW` fully pulled.
    pub lever: u8,
    /// Waiting for a pull. The handle pulses and the banner says how to reach it.
    pub idle: bool,
    pub jackpot: bool,
    pub status: String,
}

/// Narrowest a reel can be and still say anything: `douane` and a short model id fit, a long
/// one is trimmed. Below this the cabinet stops shrinking and lets the terminal cut it off,
/// since a column three characters wide answers no question.
const MIN_CELL: usize = 10;
/// Widest a reel gets. Past this the ids are long since fully readable and the machine is
/// only stretching, which reads as a bug rather than a bigger prize.
const MAX_CELL: usize = 34;
/// The right-hand margin the handle lives in, and the click target the picker reads back.
const MARGIN: usize = 7;
/// Columns the cabinet spends on something other than the reels: two walls, and the four
/// spaces framing and separating the three columns.
const GUTTERS: usize = 6;
/// Rows the cabinet spends on something other than the drum: sign, banner, titles, edges,
/// status, marquees, borders, plus the payline itself.
const CHROME: usize = 13;

/// Row the handle starts on. Level with the reel titles, so it stands beside the drum.
const LEVER_TOP: usize = 5;
/// How far down the ball travels on a pull. Small enough that the shortest cabinet still has
/// rod above its plinth to run it down.
pub const LEVER_THROW: u8 = 3;

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
        let reach = ((area.height as isize - CHROME as isize) / 2).clamp(1, 4);
        Geo {
            cell,
            inner: cell * 3 + 4,
            reach,
            margin,
            lever_base: (2 * reach as usize) + 9,
        }
    }

    /// Total rows, which is also what the caller has to have to see the whole cabinet.
    fn height(&self) -> usize {
        CHROME + 2 * self.reach as usize
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

/// One line of the cabinet, with the lever drawn in the margin beside it.
fn row(index: usize, mut spans: Vec<Span<'static>>, v: &View, g: &Geo) -> Line<'static> {
    spans.push(lever(index, v, g));
    Line::from(spans)
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
        let text = fit(r.at(offset), g.cell - 2);
        let style = match (lit, r.stopped, v.jackpot) {
            // Landed, and the machine has paid: the answer flashes.
            (true, true, true) => Style::new()
                .fg(LIGHTS[(v.tick / 2) % LIGHTS.len()])
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            (true, true, false) => Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            (true, false, _) => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            // Off the payline, and the further off the fainter: the drum falls away.
            _ => match offset.abs() {
                1 => Style::new().fg(Color::Gray),
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

/// The handle, in the right margin, the ball riding down the rod as it is pulled. Every row
/// of the margin is drawn, blank ones included, so the click target is a solid block.
fn lever(index: usize, v: &View, g: &Geo) -> Span<'static> {
    if g.margin == 0 || !(LEVER_TOP..=g.lever_base).contains(&index) {
        return Span::raw("");
    }
    let ball = LEVER_TOP + v.lever as usize;
    let (art, style) = match index {
        i if i == g.lever_base => (
            " ▐███▌ ",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        i if i == g.lever_base - 1 => ("  ▄▄▄  ", Style::new().fg(Color::DarkGray)),
        i if i == ball => (
            "  (O)  ",
            Style::new()
                .fg(match v.idle && (v.tick / 5) % 2 == 1 {
                    // Idle, so it pulses: the one thing on screen asking to be touched.
                    true => Color::LightYellow,
                    false => Color::LightRed,
                })
                .add_modifier(Modifier::BOLD),
        ),
        i if i > ball => ("   ║   ", Style::new().fg(Color::Gray)),
        _ => ("       ", Style::new()),
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
        Reel {
            title: "harness",
            items: items.iter().map(|s| s.to_string()).collect(),
            pos: 0,
            stopped: true,
        }
    }

    fn view(reels: &[Reel; 3], idle: bool, jackpot: bool) -> View<'_> {
        View {
            reels,
            tick: 3,
            lever: 2,
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
        assert!(geo(200, 40).reach >= geo(80, 24).reach);
        assert_eq!(
            geo(200, 40).cell,
            MAX_CELL,
            "the cabinet has to stop somewhere"
        );
        // Too narrow for the handle: the reels keep the room and the space bar takes over.
        assert_eq!(geo(34, 20).margin, 0);
        assert!(geo(100, 24).margin > 0);
    }

    /// The handle has to fill the rectangle the click handler is handed, at every throw and
    /// in every cabinet tall enough to carry one.
    #[test]
    fn the_handle_covers_its_whole_click_target() {
        let reels = [reel(&["x"]), reel(&["y"]), reel(&["z"])];
        for (w, h) in sizes() {
            let g = geo(w, h);
            if g.margin == 0 {
                continue;
            }
            for throw in 0..=LEVER_THROW {
                let mut v = view(&reels, true, false);
                v.lever = throw;
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

    /// The ball must never reach the plinth, however short the cabinet is.
    #[test]
    fn the_ball_stays_on_its_rod() {
        for (w, h) in sizes() {
            let g = geo(w, h);
            assert!(
                LEVER_TOP + LEVER_THROW as usize <= g.lever_base - 2,
                "at {w}x{h} the handle has no rod left to run down"
            );
        }
    }

    /// A terminal smaller than the cabinet must still render something rather than panic.
    #[test]
    fn draws_into_any_terminal() {
        let reels = [reel(&["x"]), reel(&["y"]), reel(&["z"])];
        let v = view(&reels, true, false);
        for (w, h) in sizes() {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|f| {
                    let handle = draw(f, f.area(), &v);
                    // Clamped into the frame, so a click is never tested against a rectangle
                    // hanging off the screen.
                    assert!(handle.right() <= f.area().right());
                    assert!(handle.bottom() <= f.area().bottom());
                })
                .expect("a small terminal must not stop the machine");
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
