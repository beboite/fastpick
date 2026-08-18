//! The lever nobody asked for.
//!
//! Three reels, one per question the picker asks: harness, then provider, then model. They
//! stop left to right because that is the order the answers depend on each other in, so the
//! machine is not only decoration: a provider reel cannot hold anything until the harness
//! reel has landed, and the model reel spins on whatever the catalogue lookup is still
//! fetching behind it.
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
            items: ["? ? ?", "$ $ $", "7 7 7", "* * *"]
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
    /// 0 up, 1 halfway, 2 pulled.
    pub lever: u8,
    pub jackpot: bool,
    pub status: String,
}

/// Wide enough for a model id carrying a suffix, which is what the third reel lands on.
const CELL: usize = 18;
const INNER: usize = CELL * 3 + 4;

/// The casino palette, cycled by the tick so the frame never sits still.
const LIGHTS: [Color; 4] = [
    Color::LightRed,
    Color::LightYellow,
    Color::LightMagenta,
    Color::LightCyan,
];

pub fn draw(f: &mut Frame, area: Rect, v: &View) {
    let lines = lines(v);
    let width = (INNER as u16 + 2 + 6).min(area.width);
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
}

fn lines(v: &View) -> Vec<Line<'static>> {
    let frame = match v.jackpot {
        // The whole cabinet joins in once it has paid out.
        true => LIGHTS[(v.tick / 2) % LIGHTS.len()],
        false => Color::LightMagenta,
    };
    let mut out = Vec::with_capacity(16);

    out.push(row(
        0,
        vec![Span::styled(
            format!("╔{}╗", "═".repeat(INNER)),
            Style::new().fg(frame),
        )],
        v,
    ));
    out.push(row(
        1,
        vec![wall(frame), marquee(v.tick, frame), wall(frame)],
        v,
    ));
    out.push(row(
        2,
        vec![
            wall(frame),
            Span::styled(
                centred("F A S T P I C K   S L O T S", INNER),
                Style::new()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            wall(frame),
        ],
        v,
    ));
    out.push(row(
        3,
        vec![Span::styled(
            format!("╠{}╣", "═".repeat(INNER)),
            Style::new().fg(frame),
        )],
        v,
    ));

    // The three questions, over the reel that answers them.
    let mut titles = String::from(" ");
    for r in v.reels {
        titles.push_str(&centred(r.title, CELL));
        titles.push(' ');
    }
    out.push(row(
        4,
        vec![
            wall(frame),
            Span::styled(pad(&titles, INNER), Style::new().fg(Color::DarkGray)),
            wall(frame),
        ],
        v,
    ));

    out.push(row(5, edge('┌', '┐', frame), v));
    out.push(row(6, band(v, -1), v));
    out.push(row(7, band(v, 0), v));
    out.push(row(8, band(v, 1), v));
    out.push(row(9, edge('└', '┘', frame), v));

    out.push(row(10, vec![wall(frame), blank(), wall(frame)], v));
    out.push(row(
        11,
        vec![
            wall(frame),
            Span::styled(
                pad(&format!("  {}", v.status), INNER),
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
    ));
    out.push(row(
        12,
        vec![wall(frame), marquee(v.tick + 2, frame), wall(frame)],
        v,
    ));
    out.push(row(
        13,
        vec![Span::styled(
            format!("╚{}╝", "═".repeat(INNER)),
            Style::new().fg(frame),
        )],
        v,
    ));
    out
}

/// One line of the cabinet, with the lever drawn in the margin beside it.
fn row(index: usize, mut spans: Vec<Span<'static>>, v: &View) -> Line<'static> {
    spans.push(lever(index, v));
    Line::from(spans)
}

fn wall(c: Color) -> Span<'static> {
    Span::styled("║", Style::new().fg(c))
}

fn blank() -> Span<'static> {
    Span::raw(" ".repeat(INNER))
}

fn edge(left: char, right: char, c: Color) -> Vec<Span<'static>> {
    let mut s = String::from(" ");
    for _ in 0..3 {
        s.push(left);
        s.push_str(&"─".repeat(CELL - 2));
        s.push(right);
        s.push(' ');
    }
    vec![
        wall(c),
        Span::styled(pad(&s, INNER), Style::new().fg(Color::DarkGray)),
        wall(c),
    ]
}

/// A horizontal slice through all three reels: the row above the window, the lit one, the
/// row below. Three visible symbols per reel is what makes a spinning column read as a
/// spinning column and not as a label changing at random.
fn band(v: &View, offset: isize) -> Vec<Span<'static>> {
    let lit = offset == 0;
    let frame = match v.jackpot {
        true => LIGHTS[(v.tick / 2) % LIGHTS.len()],
        false => Color::LightMagenta,
    };
    // The payline, marked in the margin the other two bands leave blank rather than in a
    // column of its own: the result of a pull is the middle row and nothing else.
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
        let text = fit(r.at(offset), CELL - 2);
        let style = match (lit, r.stopped, v.jackpot) {
            // Landed, and the machine has paid: the answer flashes.
            (true, true, true) => Style::new()
                .fg(LIGHTS[(v.tick / 2) % LIGHTS.len()])
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            (true, true, false) => Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            (true, false, _) => Style::new().fg(Color::White),
            _ => Style::new().fg(Color::DarkGray),
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
fn marquee(tick: usize, frame: Color) -> Span<'static> {
    let mut s = String::with_capacity(INNER);
    for i in 0..INNER {
        s.push(match (i + tick) % 4 {
            0 => '*',
            2 => '.',
            _ => ' ',
        });
    }
    Span::styled(s, Style::new().fg(frame))
}

/// The handle, six rows tall in the right margin, the ball riding down as it is pulled.
fn lever(index: usize, v: &View) -> Span<'static> {
    let top = 5usize;
    let ball = top + v.lever as usize;
    if index < top || index > top + 4 {
        return Span::raw("");
    }
    let (art, colour) = match index {
        i if i == ball => ("  (O) ", Color::LightRed),
        i if i > top + 3 => ("  [=] ", Color::DarkGray),
        i if i > ball => ("   |  ", Color::Gray),
        _ => ("      ", Color::Reset),
    };
    Span::styled(
        art.to_string(),
        Style::new().fg(colour).add_modifier(match index == ball {
            true => Modifier::BOLD,
            false => Modifier::empty(),
        }),
    )
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

    #[test]
    fn every_line_is_the_same_width() {
        let reels = [
            reel(&["claude code"]),
            reel(&["a provider with a very long name"]),
            reel(&["gpt-5"]),
        ];
        let v = View {
            reels: &reels,
            tick: 3,
            lever: 2,
            jackpot: true,
            status: "JACKPOT".into(),
        };
        let widths: Vec<usize> = lines(&v).iter().map(|l| l.width()).collect();
        // The lever margin is empty on the rows it does not reach, so the cabinet itself is
        // what has to line up: every row is the frame plus, at most, the handle.
        for w in &widths {
            assert!(
                *w == INNER + 2 || *w == INNER + 8,
                "a row came out {w} wide, cabinet is {}",
                INNER + 2
            );
        }
    }

    /// A terminal smaller than the cabinet must still render something rather than panic.
    #[test]
    fn draws_into_a_short_terminal() {
        let reels = [reel(&["x"]), reel(&["y"]), reel(&["z"])];
        let v = View {
            reels: &reels,
            tick: 0,
            lever: 0,
            jackpot: false,
            status: String::new(),
        };
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), &v))
            .expect("a small terminal must not stop the machine");
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
