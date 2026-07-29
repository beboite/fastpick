//! The picker: harness, then provider, then model, then what to launch it with.
//!
//! The model list comes from the provider itself, fetched on a background thread so the
//! menu stays usable while the network is slow or dead. No screen blocks on a socket.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::DefaultTerminal;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crate::catalog::{self, Source};
use crate::config::{Config, Model};
use crate::prompts::{self, PromptFile};

#[derive(Clone, Copy, PartialEq)]
pub enum Screen {
    Harness,
    Provider,
    Model,
    Options,
}

/// What the picker hands back once the user presses Enter on the options screen.
pub struct Picked {
    pub harness_idx: usize,
    pub provider_idx: usize,
    pub model: Model,
    pub effort: Option<String>,
    pub prompts: Vec<PathBuf>,
}

/// Where the menu should open.
///
/// A remembered choice and a choice made on the command line are not the same thing: the
/// first only moves the cursor, the second skips the screen entirely. Mixing them would
/// mean the menu silently stopped asking a question the user never answered.
#[derive(Default)]
pub struct Start {
    pub harness_idx: Option<usize>,
    pub provider_idx: Option<usize>,
    pub model_id: Option<String>,
    pub skip_harness: bool,
    pub skip_provider: bool,
    pub refresh: bool,
}

type Fetched = (Vec<Model>, Source);

pub struct App<'a> {
    cfg: &'a Config,
    screen: Screen,

    harness_idx: usize,

    /// Indices into `cfg.providers`, filtered to those the current harness can reach.
    provider_rows: Vec<usize>,
    provider_row: usize,
    /// How many models each of those serves, when that is known without a fetch. Computed
    /// once per harness switch rather than per frame: it reads files.
    provider_counts: Vec<Option<usize>>,

    models: Vec<Model>,
    source: Option<Source>,
    rx: Option<Receiver<Fetched>>,
    spinner: usize,
    force_refresh: bool,

    visible_models: Vec<usize>,
    filter: String,
    model_idx: usize,

    prompt_files: Vec<PromptFile>,
    prompt_matches: usize,
    prompt_show_all: bool,
    checked: BTreeSet<usize>,

    efforts: Vec<String>,
    effort_idx: Option<usize>,

    opt_row: usize,
    /// Set when the harness cannot do something the model offers, so the options screen
    /// explains the absence rather than silently hiding a row.
    unsupported: Vec<String>,
}

impl<'a> App<'a> {
    pub fn new(cfg: &'a Config, start: &Start) -> App<'a> {
        let mut app = App {
            cfg,
            screen: Screen::Harness,
            harness_idx: start
                .harness_idx
                .unwrap_or(0)
                .min(cfg.harnesses.len().saturating_sub(1)),
            provider_rows: Vec::new(),
            provider_row: 0,
            provider_counts: Vec::new(),
            models: Vec::new(),
            source: None,
            rx: None,
            spinner: 0,
            force_refresh: start.refresh,
            visible_models: Vec::new(),
            filter: String::new(),
            model_idx: 0,
            prompt_files: Vec::new(),
            prompt_matches: 0,
            prompt_show_all: false,
            checked: BTreeSet::new(),
            efforts: Vec::new(),
            effort_idx: None,
            opt_row: 0,
            unsupported: Vec::new(),
        };
        app.rebuild_providers();
        if let Some(pi) = start.provider_idx {
            if let Some(row) = app.provider_rows.iter().position(|&i| i == pi) {
                app.provider_row = row;
            }
        }
        app
    }

    pub fn harness(&self) -> &crate::config::Harness {
        &self.cfg.harnesses[self.harness_idx]
    }

    fn provider_idx(&self) -> Option<usize> {
        self.provider_rows.get(self.provider_row).copied()
    }

    fn provider(&self) -> Option<&crate::config::Provider> {
        self.cfg.providers.get(self.provider_idx()?)
    }

    fn model(&self) -> Option<&Model> {
        self.models.get(*self.visible_models.get(self.model_idx)?)
    }

    fn rebuild_providers(&mut self) {
        self.provider_rows = self.cfg.providers_for(&self.harness().id);
        if self.provider_row >= self.provider_rows.len() {
            self.provider_row = self.provider_rows.len().saturating_sub(1);
        }
        self.provider_counts = self
            .provider_rows
            .iter()
            .map(|&i| {
                let p = &self.cfg.providers[i];
                match p.catalog {
                    // Nothing to fetch, so the config list is the count.
                    None => Some(p.models.len()),
                    Some(_) => catalog::cached_count(&p.models, &p.id),
                }
            })
            .collect();
    }

    /// Kicks off the catalogue lookup for the selected provider. Returns immediately.
    pub fn load_models(&mut self, force: bool) {
        let Some(p) = self.provider() else { return };
        let req = catalog::Request::new(self.cfg, p, force || self.force_refresh);
        self.force_refresh = false;
        self.models.clear();
        self.visible_models.clear();
        self.filter.clear();
        self.model_idx = 0;
        self.source = None;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(catalog::run(&req));
        });
        self.rx = Some(rx);
    }

    fn poll_models(&mut self) {
        let Some(rx) = &self.rx else { return };
        if let Ok((models, source)) = rx.try_recv() {
            self.models = models;
            self.source = Some(source);
            self.rx = None;
            self.rebuild_models();
        }
    }

    fn loading(&self) -> bool {
        self.rx.is_some()
    }

    fn rebuild_models(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible_models = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                needle.is_empty()
                    || m.id.to_lowercase().contains(&needle)
                    || m.display().to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();
        if self.model_idx >= self.visible_models.len() {
            self.model_idx = self.visible_models.len().saturating_sub(1);
        }
    }

    /// Selects a model by id without going through the list, for `--model`.
    pub fn select_model_id(&mut self, id: &str) -> bool {
        match self.models.iter().position(|m| m.id == id) {
            Some(i) => {
                self.model_idx = self.visible_models.iter().position(|&v| v == i).unwrap_or(0);
                true
            }
            None => false,
        }
    }

    /// Loads the prompt files for the selected model and pre-checks the best match.
    pub fn enter_options(&mut self) {
        self.opt_row = 0;
        self.checked.clear();
        self.prompt_show_all = false;
        self.prompt_files.clear();
        self.prompt_matches = 0;
        self.unsupported.clear();

        let kind = self.harness().kind;
        let (efforts, default) = match self.model() {
            Some(m) => (m.effort.clone(), m.effort_default.clone()),
            None => (Vec::new(), None),
        };

        if !efforts.is_empty() && !kind.supports_effort() {
            self.unsupported
                .push(format!("{} takes no effort level.", self.harness().name));
            self.efforts = Vec::new();
            self.effort_idx = None;
        } else {
            self.efforts = efforts;
            self.effort_idx = match (&default, self.efforts.is_empty()) {
                (_, true) => None,
                (Some(d), false) => Some(self.efforts.iter().position(|e| e == d).unwrap_or(0)),
                (None, false) => Some(0),
            };
        }

        if !kind.supports_system_prompts() {
            self.unsupported.push(format!(
                "{} has no way to append a system prompt at launch.",
                self.harness().name
            ));
            return;
        }

        let Some(dir) = self.cfg.prompts_dir() else { return };
        let Some(model) = self.model() else { return };
        let base = model.base_name().to_string();

        let matches = prompts::matches_for(&dir, &base);
        self.prompt_matches = matches.len();
        self.prompt_files = matches;
        if !self.prompt_files.is_empty() {
            self.checked.insert(0);
        }
    }

    fn toggle_show_all(&mut self) {
        if !self.harness().kind.supports_system_prompts() {
            return;
        }
        let Some(dir) = self.cfg.prompts_dir() else { return };
        let Some(model) = self.model() else { return };
        let base = model.base_name().to_string();

        self.prompt_show_all = !self.prompt_show_all;
        let previously: Vec<PathBuf> = self
            .checked
            .iter()
            .filter_map(|&i| self.prompt_files.get(i).map(|f| f.path.clone()))
            .collect();

        let matches = prompts::matches_for(&dir, &base);
        self.prompt_matches = matches.len();
        if self.prompt_show_all {
            let mut all = matches.clone();
            for f in prompts::all_in(&dir) {
                if !all.iter().any(|m| m.path == f.path) {
                    all.push(f);
                }
            }
            self.prompt_files = all;
        } else {
            self.prompt_files = matches;
        }

        self.checked = self
            .prompt_files
            .iter()
            .enumerate()
            .filter(|(_, f)| previously.contains(&f.path))
            .map(|(i, _)| i)
            .collect();
        self.opt_row = self.opt_row.min(self.rows().saturating_sub(1));
    }

    /// Row 0 is the effort selector when there is one, the prompt list follows.
    fn effort_rows(&self) -> usize {
        usize::from(self.effort_idx.is_some())
    }

    fn rows(&self) -> usize {
        self.effort_rows() + self.prompt_files.len()
    }

    pub fn picked(&self) -> Option<Picked> {
        Some(Picked {
            harness_idx: self.harness_idx,
            provider_idx: self.provider_idx()?,
            model: self.model()?.clone(),
            effort: self.effort_idx.and_then(|i| self.efforts.get(i)).cloned(),
            prompts: self
                .checked
                .iter()
                .filter_map(|&i| self.prompt_files.get(i).map(|f| f.path.clone()))
                .collect(),
        })
    }

    pub fn set_screen(&mut self, s: Screen) {
        self.screen = s;
    }
}

/// Runs the picker. `Ok(None)` means the user quit without choosing.
///
/// With all three steps already named on the command line there is no question left to
/// ask, so the terminal is never switched to the alternate screen: no flicker, and the
/// output of `--dry-run` is not framed by escape sequences.
pub fn run(cfg: &Config, start: &Start) -> Result<Option<Picked>> {
    let mut app = App::new(cfg, start);
    let mut pending_model = start.model_id.clone();

    // Skip whatever the command line already decided.
    if start.skip_harness {
        app.set_screen(Screen::Provider);
    }
    if start.skip_harness && start.skip_provider {
        app.set_screen(Screen::Model);
        app.load_models(false);

        if let Some(id) = pending_model.clone() {
            while app.loading() {
                app.poll_models();
                std::thread::sleep(Duration::from_millis(20));
            }
            pending_model = None;
            if app.select_model_id(&id) {
                app.enter_options();
                return Ok(app.picked());
            }
            // Unknown id. Rather than launching something else, open the list with the
            // name already typed into the filter so the near misses are on screen.
            eprintln!("fastpick: `{id}` is not in this provider's catalogue, pick from the list.");
            app.filter = id;
            app.rebuild_models();
        }
    }

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, pending_model.as_deref());
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    wanted_model: Option<&str>,
) -> Result<Option<Picked>> {
    // A model named on the command line still needs the catalogue to resolve it, so it is
    // applied once the fetch lands rather than before.
    let mut pending_model = wanted_model.map(|s| s.to_string());

    loop {
        app.poll_models();

        if let Some(id) = pending_model.clone() {
            if app.screen == Screen::Model && !app.loading() {
                pending_model = None;
                if app.select_model_id(&id) {
                    app.enter_options();
                    app.set_screen(Screen::Options);
                    return Ok(app.picked());
                }
                // Unknown id: fall back to the list rather than launching something else.
                // The filter is pre-filled so the near misses are already on screen.
                app.filter = id;
                app.rebuild_models();
            }
        }

        terminal.draw(|f| draw(f, app))?;

        if !event::poll(Duration::from_millis(if app.loading() { 100 } else { 500 }))? {
            app.spinner = app.spinner.wrapping_add(1);
            continue;
        }

        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(None);
        }

        match app.screen {
            Screen::Harness => match key.code {
                KeyCode::Char('q') => return Ok(None),
                KeyCode::Up | KeyCode::Char('k') => {
                    app.harness_idx = prev(app.harness_idx, app.cfg.harnesses.len());
                    app.provider_row = 0;
                    app.rebuild_providers();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.harness_idx = next(app.harness_idx, app.cfg.harnesses.len());
                    app.provider_row = 0;
                    app.rebuild_providers();
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    app.rebuild_providers();
                    if !app.provider_rows.is_empty() {
                        app.set_screen(Screen::Provider);
                    }
                }
                _ => {}
            },

            Screen::Provider => match key.code {
                KeyCode::Char('q') => return Ok(None),
                KeyCode::Esc | KeyCode::Left => app.set_screen(Screen::Harness),
                KeyCode::Up | KeyCode::Char('k') => {
                    app.provider_row = prev(app.provider_row, app.provider_rows.len())
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.provider_row = next(app.provider_row, app.provider_rows.len())
                }
                KeyCode::Enter | KeyCode::Right => {
                    if app.provider().is_some() {
                        app.set_screen(Screen::Model);
                        app.load_models(false);
                    }
                }
                _ => {}
            },

            Screen::Model => match key.code {
                KeyCode::Esc | KeyCode::Left => {
                    if app.filter.is_empty() {
                        app.set_screen(Screen::Provider);
                    } else {
                        app.filter.clear();
                        app.rebuild_models();
                    }
                }
                KeyCode::Up => app.model_idx = prev(app.model_idx, app.visible_models.len()),
                KeyCode::Down => app.model_idx = next(app.model_idx, app.visible_models.len()),
                KeyCode::Backspace => {
                    app.filter.pop();
                    app.rebuild_models();
                }
                KeyCode::Tab => app.load_models(true),
                KeyCode::Enter => {
                    if app.model().is_some() {
                        app.enter_options();
                        app.set_screen(Screen::Options);
                    }
                }
                KeyCode::Char(c) => {
                    app.filter.push(c);
                    app.model_idx = 0;
                    app.rebuild_models();
                }
                _ => {}
            },

            Screen::Options => match key.code {
                KeyCode::Esc => app.set_screen(Screen::Model),
                KeyCode::Char('q') => return Ok(None),
                KeyCode::Up | KeyCode::Char('k') => {
                    let n = app.rows();
                    if n > 0 {
                        app.opt_row = prev(app.opt_row, n);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let n = app.rows();
                    if n > 0 {
                        app.opt_row = next(app.opt_row, n);
                    }
                }
                KeyCode::Left | KeyCode::Right => {
                    if app.opt_row < app.effort_rows() {
                        if let (Some(i), false) = (app.effort_idx, app.efforts.is_empty()) {
                            app.effort_idx = Some(if key.code == KeyCode::Left {
                                prev(i, app.efforts.len())
                            } else {
                                next(i, app.efforts.len())
                            });
                        }
                    }
                }
                KeyCode::Char(' ') => {
                    let base = app.effort_rows();
                    if app.opt_row >= base {
                        let i = app.opt_row - base;
                        if !app.checked.insert(i) {
                            app.checked.remove(&i);
                        }
                    }
                }
                KeyCode::Char('a') => app.toggle_show_all(),
                KeyCode::Enter => return Ok(app.picked()),
                _ => {}
            },
        }
    }
}

fn next(i: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (i + 1) % len
    }
}

fn prev(i: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (i + len - 1) % len
    }
}

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

/// One line of the body. Headings and gaps are drawn but never land under the cursor, so a
/// selection index counts items only and grouping cannot desynchronise it.
enum Row {
    Gap,
    Heading(String),
    Item(Line<'static>),
}

/// A screen is either a list to move through, or a sentence saying why there is none.
enum Body {
    Rows(Vec<Row>, usize),
    Text(String),
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let body = body(app);

    // The body is sized to what it holds rather than to the terminal. Six providers take
    // six lines, not a bordered box the height of the window with six lines inside it.
    let wanted = match &body {
        Body::Rows(rows, _) => rows.len() as u16,
        Body::Text(t) => wrapped_height(t, area.width),
    };
    // title, blank, body, blank, status, help.
    let room = area.height.saturating_sub(5).max(1);
    let height = wanted.clamp(1, room);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title(app),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))),
        Rect { height: 1, ..area },
    );

    let body_area = Rect {
        y: area.y + 2,
        height,
        ..area
    };
    match body {
        Body::Rows(rows, selected) => render_rows(f, body_area, rows, selected),
        Body::Text(text) => f.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(Color::Gray)),
            body_area,
        ),
    }

    let footer = Rect {
        y: body_area.y + height + 1,
        height: 2,
        ..area
    };
    if footer.y + footer.height <= area.y + area.height {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    status(app),
                    Style::new().fg(match &app.source {
                        Some(Source::Failed(_)) => Color::Yellow,
                        _ => Color::DarkGray,
                    }),
                )),
                Line::from(Span::styled(help(app), Style::new().fg(Color::DarkGray))),
            ]),
            footer,
        );
    }
}

/// How tall a paragraph will be once wrapped, so the footer sits under it and not over it.
fn wrapped_height(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    text.lines()
        .map(|l| (l.chars().count().max(1)).div_ceil(width) as u16)
        .sum::<u16>()
        .max(1)
}

fn render_rows(f: &mut Frame, area: Rect, rows: Vec<Row>, selected: usize) {
    let mut items: Vec<ListItem> = Vec::with_capacity(rows.len());
    let mut cursor = 0;
    let mut seen = 0;
    for row in rows {
        match row {
            Row::Gap => items.push(ListItem::new(Line::default())),
            Row::Heading(t) => items.push(ListItem::new(Line::from(Span::styled(
                t,
                Style::new().fg(Color::DarkGray),
            )))),
            Row::Item(line) => {
                if seen == selected {
                    cursor = items.len();
                }
                seen += 1;
                items.push(ListItem::new(line));
            }
        }
    }

    let mut state = ListState::default().with_selected(Some(cursor));
    f.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        area,
        &mut state,
    );
}

fn title(app: &App) -> String {
    match app.screen {
        Screen::Harness => "fastpick  ·  harness".to_string(),
        Screen::Provider => format!("fastpick  ·  {}  ·  provider", app.harness().name),
        Screen::Model => format!(
            "fastpick  ·  {}  ·  {}  ·  {}",
            app.harness().name,
            app.provider().map(|p| p.name.as_str()).unwrap_or("?"),
            match app.filter.is_empty() {
                true => "model".to_string(),
                false => format!("/{}", app.filter),
            }
        ),
        Screen::Options => format!(
            "fastpick  ·  {}  ·  {}  ·  {}{}",
            app.harness().name,
            app.provider().map(|p| p.name.as_str()).unwrap_or("?"),
            app.model().map(|m| m.display()).unwrap_or("?"),
            match app.prompt_show_all {
                true => "  ·  every file",
                false => "",
            }
        ),
    }
}

fn help(app: &App) -> &'static str {
    match app.screen {
        Screen::Harness => "up/down move   enter select   q quit",
        Screen::Provider => "up/down move   enter select   esc back   q quit",
        Screen::Model => "up/down move   type to filter   tab refetch   enter select   esc back",
        Screen::Options => {
            "up/down move   space toggle   left/right effort   a every file   enter launch   esc back"
        }
    }
}

fn status(app: &App) -> String {
    match app.screen {
        Screen::Model => match (&app.source, app.loading()) {
            (_, true) => format!(
                "{} asking {} what it serves",
                SPINNER[app.spinner % SPINNER.len()],
                app.provider().map(|p| p.name.as_str()).unwrap_or("")
            ),
            (Some(s), false) => s.label(),
            (None, false) => String::new(),
        },
        _ => String::new(),
    }
}

fn body(app: &App) -> Body {
    match app.screen {
        Screen::Harness => harness_body(app),
        Screen::Provider => provider_body(app),
        Screen::Model => model_body(app),
        Screen::Options => options_body(app),
    }
}

/// Grey trailing detail, kept to one style so a row is a name plus context and never a
/// column layout that breaks the moment a name is long.
fn dim(text: String) -> Span<'static> {
    Span::styled(text, Style::new().fg(Color::DarkGray))
}

fn count(n: usize, thing: &str) -> String {
    match n {
        1 => format!("   1 {thing}"),
        _ => format!("   {n} {thing}s"),
    }
}

fn harness_body(app: &App) -> Body {
    let rows = app
        .cfg
        .harnesses
        .iter()
        .map(|h| {
            let n = app.cfg.providers_for(&h.id).len();
            let mut spans = vec![
                Span::raw(h.name.clone()),
                Span::styled(count(n, "provider"), Style::new().fg(Color::Blue)),
            ];
            if let Some(note) = &h.note {
                spans.push(dim(format!("   {note}")));
            }
            Row::Item(Line::from(spans))
        })
        .collect();
    Body::Rows(rows, app.harness_idx)
}

fn provider_body(app: &App) -> Body {
    if app.provider_rows.is_empty() {
        return Body::Text(format!(
            "No provider declares a binding for {}.\nAdd a [provider.harness.{}] block to one in the config.",
            app.harness().name,
            app.harness().id
        ));
    }

    let mut rows = Vec::new();
    let mut current: Option<&str> = None;
    for (row, &i) in app.provider_rows.iter().enumerate() {
        let Some(p) = app.cfg.providers.get(i) else { continue };

        // A heading opens each block, and the blocks are separated by a blank line. Two
        // entries that are the same site with a different key belong under one heading, so
        // the list reads as three places rather than seven unrelated choices.
        let group = p.group.as_deref();
        if group != current || row == 0 {
            if row > 0 {
                rows.push(Row::Gap);
            }
            if let Some(name) = group {
                rows.push(Row::Heading(name.to_string()));
            }
            current = group;
        }

        let mut spans = vec![Span::raw(p.name.clone())];
        if let Some(n) = app.provider_counts.get(row).copied().flatten() {
            spans.push(Span::styled(count(n, "model"), Style::new().fg(Color::Blue)));
        }
        if let Some(note) = &p.note {
            spans.push(dim(format!("   {note}")));
        }
        rows.push(Row::Item(Line::from(spans)));
    }
    Body::Rows(rows, app.provider_row)
}

fn model_body(app: &App) -> Body {
    if app.loading() {
        return Body::Text(
            "Asking the provider for its model list.\nesc goes back, the answer is cached once it lands."
                .to_string(),
        );
    }
    if app.visible_models.is_empty() {
        return Body::Text(match app.filter.is_empty() {
            true => "This provider listed no model.".to_string(),
            false => format!("Nothing matches `{}`. Backspace to widen.", app.filter),
        });
    }

    let rows = app
        .visible_models
        .iter()
        .filter_map(|&i| app.models.get(i))
        .map(|m| {
            let mut spans = vec![Span::raw(m.display().to_string())];
            if m.label.is_some() {
                spans.push(dim(format!("  {}", m.id)));
            }
            if let Some(ctx) = m.context_window {
                spans.push(Span::styled(
                    format!("   {}K", ctx / 1000),
                    Style::new().fg(Color::Blue),
                ));
            }
            if let Some(note) = &m.note {
                spans.push(dim(format!("   {note}")));
            }
            Row::Item(Line::from(spans))
        })
        .collect();
    Body::Rows(rows, app.model_idx)
}

fn options_body(app: &App) -> Body {
    let mut rows: Vec<Row> = Vec::new();

    if let Some(i) = app.effort_idx {
        let current = app.efforts.get(i).cloned().unwrap_or_default();
        rows.push(Row::Item(Line::from(vec![
            dim("effort   ".to_string()),
            Span::raw("< "),
            Span::styled(current, Style::new().fg(Color::Yellow)),
            Span::raw(" >"),
        ])));
    }

    if !app.prompt_files.is_empty() {
        if !rows.is_empty() {
            rows.push(Row::Gap);
        }
        rows.push(Row::Heading("system prompt".to_string()));
    }
    for (i, file) in app.prompt_files.iter().enumerate() {
        let mark = if app.checked.contains(&i) { "[x] " } else { "[ ] " };
        let matched = i < app.prompt_matches;
        let mut spans = vec![
            Span::styled(mark, Style::new().fg(Color::Green)),
            Span::styled(
                format!("{}.md", file.stem),
                match matched {
                    true => Style::new(),
                    false => Style::new().fg(Color::DarkGray),
                },
            ),
        ];
        if matched {
            spans.push(dim("   matches this model".to_string()));
        }
        rows.push(Row::Item(Line::from(spans)));
    }

    if rows.is_empty() {
        let mut text = String::new();
        for u in &app.unsupported {
            text.push_str(u);
            text.push('\n');
        }
        if app.harness().kind.supports_system_prompts() {
            let dir = app
                .cfg
                .prompts_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| "not configured".into());
            text.push_str(&format!(
                "No system prompt file matches this model.\nDrop a .md named after it in {dir}, or press `a` to list every file there.\n"
            ));
        }
        text.push_str("Enter launches.");
        return Body::Text(text);
    }

    Body::Rows(rows, app.opt_row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// The bundled default config, which is also what a fresh install starts from: if the
    /// picker cannot render it, nobody's first run works.
    fn cfg() -> Config {
        toml::from_str(crate::config::DEFAULT_CONFIG).expect("the shipped config must parse")
    }

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 22)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn render_lines(app: &App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(100, 22)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// Prints a screen as the terminal would draw it, from any config file:
    /// `FASTPICK_PREVIEW=<path> cargo test preview -- --ignored --nocapture`. Ignored by
    /// default, and the only way to look at a layout change without launching an agent.
    #[test]
    #[ignore]
    fn preview() {
        let path = std::env::var("FASTPICK_PREVIEW").unwrap();
        let cfg = Config::load(std::path::Path::new(&path)).unwrap();
        let mut app = App::new(&cfg, &Start::default());
        for screen in [Screen::Harness, Screen::Provider] {
            app.set_screen(screen);
            for l in render_lines(&app) {
                println!("|{l}|");
            }
            println!();
        }
    }

    #[test]
    fn renders_every_screen() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());

        assert!(render(&app).contains("fastpick"));
        assert!(render(&app).contains(cfg.harnesses[0].name.as_str()));

        app.set_screen(Screen::Provider);
        let provider = cfg.providers[app.provider_rows[0]].name.clone();
        assert!(render(&app).contains(&provider));

        app.set_screen(Screen::Model);
        app.models = vec![Model::new("some-model".into())];
        app.rebuild_models();
        assert!(render(&app).contains("some-model"));

        app.enter_options();
        app.set_screen(Screen::Options);
        assert!(render(&app).contains("Enter launches") || render(&app).contains(".md"));
    }

    /// A heading is drawn, and the cursor still lands on the provider it names rather than
    /// on the heading itself: the selection counts items, the render counts lines.
    #[test]
    fn a_group_heading_does_not_shift_the_selection() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());
        app.set_screen(Screen::Provider);

        let Some(grouped) = cfg.providers.iter().find_map(|p| p.group.clone()) else {
            panic!("the shipped config must show what a group looks like");
        };
        assert!(render(&app).contains(&grouped));

        for row in 0..app.provider_rows.len() {
            app.provider_row = row;
            let Body::Rows(rows, selected) = body(&app) else {
                panic!("the provider screen is a list")
            };
            let picked = rows
                .iter()
                .filter(|r| matches!(r, Row::Item(_)))
                .nth(selected)
                .map(|r| match r {
                    Row::Item(l) => l.spans[0].content.to_string(),
                    _ => unreachable!(),
                });
            assert_eq!(
                picked.as_deref(),
                Some(cfg.providers[app.provider_rows[row]].name.as_str())
            );
        }
    }

    /// The body is as tall as what it holds, never as tall as the window.
    #[test]
    fn the_body_is_sized_to_its_content() {
        let cfg = cfg();
        let app = App::new(&cfg, &Start::default());
        let drawn = render_lines(&app);
        let blank_tail = drawn
            .iter()
            .rev()
            .take_while(|l| l.trim().is_empty())
            .count();
        assert!(
            blank_tail > 0,
            "a short list must leave the rest of the window empty, got:\n{}",
            drawn.join("\n")
        );
    }

    #[test]
    fn every_harness_has_at_least_one_provider_in_the_shipped_config() {
        let cfg = cfg();
        for h in &cfg.harnesses {
            assert!(
                !cfg.providers_for(&h.id).is_empty(),
                "harness `{}` is declared with nothing able to serve it",
                h.id
            );
        }
    }

    #[test]
    fn switching_harness_reshuffles_the_provider_list_without_leaving_a_stale_index() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());
        for i in 0..cfg.harnesses.len() {
            app.harness_idx = i;
            app.rebuild_providers();
            assert!(app.provider_rows.iter().all(|&p| p < cfg.providers.len()));
            assert!(app.provider_row < app.provider_rows.len().max(1));
        }
    }

    #[test]
    fn a_loading_model_screen_draws_without_a_list() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());
        app.set_screen(Screen::Model);
        let (_tx, rx) = mpsc::channel();
        app.rx = Some(rx);
        assert!(app.loading());
        assert!(render(&app).contains("Asking the provider"));
    }

    #[test]
    fn filtering_to_nothing_leaves_no_selectable_model() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());
        app.models = vec![Model::new("a".into()), Model::new("b".into())];
        app.filter = "zzzz".into();
        app.rebuild_models();
        assert!(app.visible_models.is_empty());
        assert!(app.model().is_none());
        assert!(app.picked().is_none());
        app.set_screen(Screen::Model);
        let _ = render(&app);
    }

    #[test]
    fn a_harness_without_effort_support_drops_the_row_and_says_so() {
        let cfg = cfg();
        let Some(codex) = cfg
            .harnesses
            .iter()
            .position(|h| h.kind == crate::config::HarnessKind::Codex)
        else {
            return;
        };

        let mut app = App::new(&cfg, &Start::default());
        app.harness_idx = codex;
        app.rebuild_providers();
        let mut m = Model::new("m".into());
        m.effort = vec!["high".into()];
        m.effort_default = Some("high".into());
        app.models = vec![m];
        app.rebuild_models();
        app.enter_options();

        assert_eq!(app.effort_rows(), 0);
        assert!(app.picked().unwrap().effort.is_none());
        assert!(!app.unsupported.is_empty());
    }

    #[test]
    fn wrapping_is_symmetric() {
        assert_eq!(next(2, 3), 0);
        assert_eq!(prev(0, 3), 2);
        assert_eq!(next(0, 0), 0);
        assert_eq!(prev(0, 0), 0);
    }
}
