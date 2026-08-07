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
use std::time::{Duration, Instant};

use crate::catalog::{self, Listed, Source};
use crate::config::{Config, Model};
use crate::prompts::{self, PromptFile};

#[derive(Clone, Copy, PartialEq)]
pub enum Screen {
    Harness,
    Provider,
    Model,
}

/// What the picker hands back once the user presses Enter on the options screen.
pub struct Picked {
    pub harness_idx: usize,
    pub provider_idx: usize,
    /// Index of the key that serves this model inside its provider. Carried rather than
    /// looked up again, because a provider can hold several keys and only the row the user
    /// landed on knows which one listed the model.
    pub key: usize,
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
    /// Index of the key `--key` named, inside `provider_idx`. Narrows the model list to that
    /// one credential, so a model two keys both serve resolves without a guess.
    pub key: Option<usize>,
    pub model_id: Option<String>,
    pub skip_harness: bool,
    pub skip_provider: bool,
    pub refresh: bool,
}

type Fetched = (Vec<Listed>, Source);

pub struct App<'a> {
    cfg: &'a Config,
    screen: Screen,

    /// Indices into `cfg.harnesses`, filtered to the ones installed on this machine.
    harness_rows: Vec<usize>,
    harness_row: usize,

    /// Indices into `cfg.providers`, filtered to those the current harness can reach.
    provider_rows: Vec<usize>,
    provider_row: usize,
    /// How many models each of those serves, when that is known without a fetch. Computed
    /// once per harness switch rather than per frame: it reads files.
    provider_counts: Vec<Option<catalog::Count>>,

    /// The provider and key `--key` named, while the user is still on that provider. Moving
    /// to another one is a new question and its keys were never the ones filtered out.
    only_key: Option<(usize, usize)>,

    /// Every key's models in one list, each row remembering the key that listed it.
    models: Vec<Listed>,
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

    /// The options panel, opened next to the model list rather than after it. Enter on a
    /// model launches with what was detected, so tuning is a detour and never a step.
    options_open: bool,
    /// Which model the current options were computed for, so closing and reopening the
    /// panel does not throw away what was ticked by hand.
    options_model: Option<String>,
    opt_row: usize,
    /// Set when the harness cannot do something the model offers, so the options screen
    /// explains the absence rather than silently hiding a row.
    unsupported: Vec<String>,

    /// One line explaining why the menu opened when the command line asked it not to.
    /// Carried here rather than printed: anything written to the terminal before
    /// `ratatui::init()` is wiped by the switch to the alternate screen a moment later.
    notice: Option<String>,

    /// A newer release, if the last check found one. Read once here rather than per frame,
    /// which also means a check that lands during this run shows up on the next one. That
    /// is the right side to err on for a line nobody asked for.
    update_available: Option<String>,
}

impl<'a> App<'a> {
    pub fn new(cfg: &'a Config, start: &Start) -> App<'a> {
        let mut harness_rows = cfg.harnesses_installed();
        // A harness named on the command line is kept whatever the detection said: the flag
        // is an explicit answer, and a binary this missed must not become unreachable.
        if let Some(i) = start.harness_idx {
            if i < cfg.harnesses.len() && !harness_rows.contains(&i) {
                harness_rows.push(i);
                harness_rows.sort_unstable();
            }
        }
        let harness_row = start
            .harness_idx
            .and_then(|i| harness_rows.iter().position(|&r| r == i))
            .unwrap_or(0);

        let mut app = App {
            cfg,
            screen: Screen::Harness,
            harness_rows,
            harness_row,
            provider_rows: Vec::new(),
            provider_row: 0,
            provider_counts: Vec::new(),
            only_key: start.provider_idx.zip(start.key),
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
            options_open: false,
            options_model: None,
            opt_row: 0,
            unsupported: Vec::new(),
            notice: None,
            update_available: crate::update::pending(),
        };
        app.rebuild_providers();
        if let Some(pi) = start.provider_idx {
            if let Some(row) = app.provider_rows.iter().position(|&i| i == pi) {
                app.provider_row = row;
            }
        }
        app
    }

    fn harness_idx(&self) -> usize {
        self.harness_rows
            .get(self.harness_row)
            .copied()
            .unwrap_or(0)
    }

    /// Selects a harness by its index in the config whether or not it was detected, the way
    /// `--harness` does.
    #[cfg(test)]
    fn force_harness(&mut self, idx: usize) {
        if !self.harness_rows.contains(&idx) {
            self.harness_rows.push(idx);
            self.harness_rows.sort_unstable();
        }
        self.harness_row = self
            .harness_rows
            .iter()
            .position(|&r| r == idx)
            .unwrap_or(0);
    }

    pub fn harness(&self) -> &crate::config::Harness {
        &self.cfg.harnesses[self.harness_idx()]
    }

    fn provider_idx(&self) -> Option<usize> {
        self.provider_rows.get(self.provider_row).copied()
    }

    fn provider(&self) -> Option<&crate::config::Provider> {
        self.cfg.providers.get(self.provider_idx()?)
    }

    /// The selected row, model and serving key together. Nothing splits the two apart, so a
    /// launch cannot end up with a model from one key and a token from another.
    fn listed(&self) -> Option<&Listed> {
        self.models.get(*self.visible_models.get(self.model_idx)?)
    }

    fn model(&self) -> Option<&Model> {
        Some(&self.listed()?.model)
    }

    fn rebuild_providers(&mut self) {
        let harness_id = self.harness().id.clone();
        self.provider_rows = self.cfg.providers_for(&harness_id);
        if self.provider_row >= self.provider_rows.len() {
            self.provider_row = self.provider_rows.len().saturating_sub(1);
        }
        self.provider_counts = self
            .provider_rows
            .iter()
            .map(|&i| catalog::cached_count(&self.cfg.providers[i], &harness_id))
            .collect();
    }

    /// Refetches for the provider already on screen, keeping what the user has typed.
    ///
    /// Separate from `load_models` because that one is entering the screen, where a filter
    /// from the previous provider makes no sense. Here the user asked for fresher data
    /// about the list in front of them and expects to still be looking at it.
    pub fn refresh_models(&mut self) {
        let filter = std::mem::take(&mut self.filter);
        self.load_models(true);
        self.filter = filter;
    }

    /// Kicks off the catalogue lookup for the selected provider. Returns immediately.
    pub fn load_models(&mut self, force: bool) {
        let Some(p) = self.provider() else { return };
        // One lookup per key that can serve this harness. A key answers for what it may use,
        // so asking the provider once would list models half the credentials cannot reach.
        let mut reqs = catalog::requests_for(
            self.cfg,
            p,
            Some(&self.harness().id),
            force || self.force_refresh,
        );
        // `--key` was an answer to "which credential", so the other keys of that site are not
        // fetched at all: no lookup, no cache write, nothing on screen to pick by mistake.
        if let Some((pi, ki)) = self.only_key {
            if self.provider_idx() == Some(pi) {
                reqs.retain(|r| r.key_idx == ki);
            }
        }
        self.force_refresh = false;
        self.models.clear();
        self.visible_models.clear();
        self.filter.clear();
        self.model_idx = 0;
        self.source = None;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(catalog::run_all(reqs));
        });
        self.rx = Some(rx);
    }

    fn poll_models(&mut self) {
        let Some(rx) = &self.rx else { return };
        match rx.try_recv() {
            Ok((models, source)) => {
                self.models = models;
                self.source = Some(source);
                self.rx = None;
                self.rebuild_models();
            }
            // The sender is gone without having sent, which means the fetch thread died.
            // Treated as a failure rather than ignored: leaving `rx` in place would spin
            // the loading indicator for ever with nothing left to answer it.
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                // The config list of every key that binds this harness, which is the same
                // fallback the lookup itself would have degraded to.
                let harness_id = self.harness().id.clone();
                self.models = self
                    .provider()
                    .map(|p| {
                        p.keys_for(&harness_id)
                            .into_iter()
                            .flat_map(|ki| {
                                let k = &p.keys[ki];
                                k.models.iter().map(move |m| catalog::Listed {
                                    key: ki,
                                    key_label: k.label.clone(),
                                    model: m.clone(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.source = Some(catalog::Source::Failed(
                    "the lookup stopped without answering".into(),
                ));
                self.rebuild_models();
            }
            Err(mpsc::TryRecvError::Empty) => {}
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
            .filter(|(_, l)| {
                // The key label is part of the row, so typing a subscription name narrows to
                // the models that key serves rather than matching nothing.
                needle.is_empty()
                    || l.model.id.to_lowercase().contains(&needle)
                    || l.model.display().to_lowercase().contains(&needle)
                    || l.key_label
                        .as_deref()
                        .is_some_and(|k| k.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        if self.model_idx >= self.visible_models.len() {
            self.model_idx = self.visible_models.len().saturating_sub(1);
        }
    }

    /// Selects a model by id without going through the list, for `--model`.
    ///
    /// `Ok(false)` when the id is not on screen, rather than falling back to the first row.
    /// `unwrap_or(0)` here used to answer "selected" while pointing at a different model, so
    /// `--model x` could launch y with nothing said about it.
    ///
    /// `Err` when the id is on screen more than once. Two keys of one site serving a model of
    /// the same name is ordinary: two subscriptions, two price lists, one name. Which one to
    /// spend is a question, and `position()` answered it by taking whichever key the config
    /// happened to declare first.
    pub fn select_model_id(&mut self, id: &str) -> std::result::Result<bool, String> {
        let hits: Vec<usize> = self
            .models
            .iter()
            .enumerate()
            .filter(|(_, l)| l.model.id == id)
            .map(|(i, _)| i)
            .collect();
        if hits.len() > 1 {
            let names: Vec<String> = match self.provider() {
                Some(p) => hits
                    .iter()
                    .filter_map(|&i| self.models.get(i))
                    .map(|l| format!("--key {}", p.route_id(l.key)))
                    .collect(),
                None => Vec::new(),
            };
            return Err(format!(
                "`{id}` is served by {} of this provider's keys, so name the one to use: {}",
                hits.len(),
                names.join(", ")
            ));
        }
        let Some(i) = hits.first().copied() else {
            return Ok(false);
        };
        // A filter left over from an earlier screen can hide a model that does exist. The
        // named one wins over the filter, and the cursor has to land on that exact row: the
        // key comes off the row, so settling for another one would launch the right model
        // with the wrong credential.
        if !self.visible_models.contains(&i) {
            self.filter.clear();
            self.rebuild_models();
        }
        match self.visible_models.iter().position(|&v| v == i) {
            Some(row) => {
                self.model_idx = row;
                Ok(true)
            }
            // Unreachable with the filter cleared, and answered with `false` rather than
            // row 0 anyway: the caller treats that as "not found" and opens the screen,
            // where landing on someone else's row would silently swap the credential.
            None => Ok(false),
        }
    }

    /// Recomputes the options only when they belong to another model.
    fn ensure_options(&mut self) {
        let id = self.model().map(|m| m.id.clone());
        if id.is_some() && id != self.options_model {
            self.enter_options();
        }
    }

    /// Loads the prompt files for the selected model and pre-checks the best match.
    pub fn enter_options(&mut self) {
        self.options_model = self.model().map(|m| m.id.clone());
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

        let Some(dir) = self.cfg.prompts_dir() else {
            return;
        };
        let Some(model) = self.model() else { return };
        let base = model.prompt_name().to_string();

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
        let Some(dir) = self.cfg.prompts_dir() else {
            return;
        };
        let Some(model) = self.model() else { return };
        let base = model.prompt_name().to_string();

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

    /// Changes whatever the cursor is on: the effort row steps to the next level, a prompt
    /// file toggles.
    fn act_on_row(&mut self) {
        let base = self.effort_rows();
        if self.opt_row < base {
            if let (Some(i), false) = (self.effort_idx, self.efforts.is_empty()) {
                self.effort_idx = Some(next(i, self.efforts.len()));
            }
            return;
        }
        let i = self.opt_row - base;
        if !self.checked.insert(i) {
            self.checked.remove(&i);
        }
    }

    pub fn picked(&self) -> Option<Picked> {
        let listed = self.listed()?;
        Some(Picked {
            harness_idx: self.harness_idx(),
            provider_idx: self.provider_idx()?,
            key: listed.key,
            model: listed.model.clone(),
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
            // Bounded at the consumer rather than trusting the client timeout. ureq applies
            // its deadline to the connection and the transfer but not to the DNS lookup, so
            // a wedged resolver blocks the fetch thread for the OS timeout, minutes, and
            // this loop would show a blank terminal for all of it with no way out.
            let began = Instant::now();
            let deadline = began + Duration::from_secs(20);
            let mut said = false;
            while app.loading() && Instant::now() < deadline {
                // No alternate screen on this path, so a line here survives. Anything
                // slower than a blink needs to say what it is waiting for.
                if !said && began.elapsed() > Duration::from_secs(1) {
                    eprintln!("fastpick: asking the provider what it serves...");
                    said = true;
                }
                app.poll_models();
                std::thread::sleep(Duration::from_millis(20));
            }
            pending_model = None;
            if !app.loading() {
                match app.select_model_id(&id) {
                    Ok(true) => {
                        app.enter_options();
                        return Ok(app.picked());
                    }
                    // Real name, named too loosely. Everything on this path was answered on
                    // the command line, so it is a script, and a script gets the reason and
                    // a non-zero exit rather than a menu nobody is there to answer.
                    Err(e) => anyhow::bail!(e),
                    Ok(false) => {}
                }
            }
            // Unknown id, or the catalogue never answered. Rather than launching something
            // else, open the list with the name already in the filter so the near misses
            // are on screen. The reason goes on the status line: printing it here would be
            // wiped by the switch to the alternate screen two lines below.
            app.notice = Some(if app.loading() {
                format!("the catalogue did not answer in time, so `{id}` could not be checked")
            } else {
                format!("`{id}` is not in this provider's catalogue, pick from the list")
            });
            app.filter = id;
            app.rebuild_models();
        }
    }

    // Only now, once it is certain a menu will be drawn. A launch that named all three
    // steps on the command line is a script, and a script does not want a request to
    // github.com it never asked for.
    crate::update::check_in_background();

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
                match app.select_model_id(&id) {
                    Ok(true) => {
                        app.enter_options();
                        return Ok(app.picked());
                    }
                    // Unknown id: fall back to the list rather than launching something
                    // else. The filter is pre-filled so the near misses are on screen.
                    // Ambiguous is the same fallback with a different reason: the menu is
                    // already open, and picking a row is exactly the answer it wants.
                    Ok(false) => {
                        app.notice = Some(format!(
                            "`{id}` is not in this provider's catalogue, pick from the list"
                        ));
                    }
                    Err(e) => app.notice = Some(e),
                }
                app.filter = id;
                app.rebuild_models();
            }
        }

        terminal.draw(|f| draw(f, app))?;

        // Only the spinner needs a tick, and only while something is in flight. Waking
        // twice a second on an idle menu redrew the whole screen for nothing; a resize or
        // a keypress still wakes the poll immediately.
        let wait = if app.loading() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(3600)
        };
        if !event::poll(wait)? {
            app.spinner = app.spinner.wrapping_add(1);
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Read once, then gone: it explains the screen the user just landed on, and past
        // that it would sit on top of the catalogue line for the rest of the session.
        app.notice = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(None);
        }

        match app.screen {
            Screen::Harness => match key.code {
                KeyCode::Char('q') => return Ok(None),
                KeyCode::Up | KeyCode::Char('k') => {
                    app.harness_row = prev(app.harness_row, app.harness_rows.len());
                    app.provider_row = 0;
                    app.rebuild_providers();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.harness_row = next(app.harness_row, app.harness_rows.len());
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
                KeyCode::Enter | KeyCode::Right if app.provider().is_some() => {
                    app.set_screen(Screen::Model);
                    app.load_models(false);
                }
                _ => {}
            },

            // The options panel takes the keys while it is open: the model list keeps its
            // selection but not the cursor, so nothing moves behind the panel.
            Screen::Model if app.options_open => match key.code {
                KeyCode::Esc | KeyCode::Left => app.options_open = false,
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
                // One key acts on the current row whatever it holds, which leaves
                // left and right to mean back and forward on every screen.
                KeyCode::Char(' ') | KeyCode::Right => app.act_on_row(),
                KeyCode::Char('a') => app.toggle_show_all(),
                KeyCode::Enter => return Ok(app.picked()),
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
                KeyCode::Tab => app.refresh_models(),
                KeyCode::Right => {
                    if app.model().is_some() {
                        app.ensure_options();
                        app.options_open = true;
                    }
                }
                // Enter launches from here. What the panel would have shown is already
                // decided: the matching prompt file is checked and the effort is the
                // model's default, so the common case is one key.
                KeyCode::Enter => {
                    if app.model().is_some() {
                        app.ensure_options();
                        return Ok(app.picked());
                    }
                }
                // A modifier means a shortcut, not a character to filter on. Without the
                // guard, Ctrl+V and Alt+F typed `v` and `f` into the filter.
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    app.filter.push(c);
                    app.model_idx = 0;
                    app.rebuild_models();
                }
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
    let panel = app.options_open.then(|| options_body(app));

    // The panel opens beside the list, not over it and not after it: the model being
    // configured stays on screen, and closing it puts the cursor back where it was.
    let (left_width, right_width) = match panel {
        None => (area.width, 0),
        Some(_) => {
            // The panel keeps enough room for an effort row and a file name whatever the
            // model list is called.
            let split = (width_of(&body) + 5)
                .clamp(16, area.width.saturating_sub(34).max(16))
                .min(area.width);
            (split, area.width - split)
        }
    };

    // The body is sized to what it holds rather than to the terminal. Six providers take
    // six lines, not a bordered box the height of the window with six lines inside it.
    let wanted = height_of(&body, left_width).max(match &panel {
        Some(p) => height_of(p, right_width),
        None => 0,
    });
    // title, blank, body, blank, status, help, and the update line when there is one.
    let footer_height = if app.update_available.is_some() { 3 } else { 2 };
    let room = area.height.saturating_sub(3 + footer_height as u16).max(1);
    let height = wanted.clamp(1, room);

    f.render_widget(Paragraph::new(title(app)), Rect { height: 1, ..area });

    let body_area = Rect {
        y: area.y + 2,
        height,
        width: left_width,
        ..area
    };
    render_body(f, body_area, body, !app.options_open);
    if let Some(p) = panel {
        render_body(
            f,
            Rect {
                x: area.x + left_width,
                width: right_width,
                ..body_area
            },
            p,
            true,
        );
    }

    let footer = Rect {
        y: body_area.y + height + 1,
        height: footer_height as u16,
        ..area
    };
    if footer.y + footer.height <= area.y + area.height {
        let mut lines = vec![
            Line::from(Span::styled(
                status(app),
                Style::new().fg(match &app.source {
                    Some(Source::Failed(_)) => Color::Yellow,
                    // One key out of three being unreachable is a shorter list than the
                    // user asked for, so it is coloured like an outright failure.
                    Some(s) if !s.failures().is_empty() => Color::Yellow,
                    _ => Color::DarkGray,
                }),
            )),
            footer_line(app),
        ];
        if let Some(v) = &app.update_available {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} · {v} available, run `fastpick --update`",
                    crate::update::current_version()
                ),
                Style::new().fg(Color::DarkGray),
            )));
        }
        f.render_widget(Paragraph::new(lines), footer);
    }
}

fn height_of(body: &Body, width: u16) -> u16 {
    match body {
        Body::Rows(rows, _) => rows.len() as u16,
        Body::Text(t) => wrapped_height(t, width),
    }
}

/// The widest line, cursor marker aside. Only used to decide where the panel starts, so a
/// long model name pushes the split right instead of being cut in half.
fn width_of(body: &Body) -> u16 {
    let width = match body {
        Body::Rows(rows, _) => rows
            .iter()
            .map(|r| match r {
                Row::Gap => 0,
                Row::Heading(t) => t.chars().count(),
                Row::Item(l) => l.width(),
            })
            .max()
            .unwrap_or(0),
        Body::Text(t) => t.lines().map(|l| l.chars().count()).max().unwrap_or(0),
    };
    width.try_into().unwrap_or(u16::MAX)
}

fn render_body(f: &mut Frame, area: Rect, body: Body, focused: bool) {
    match body {
        Body::Rows(rows, selected) => render_rows(f, area, rows, selected, focused),
        Body::Text(text) => f.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(Color::Gray)),
            area,
        ),
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

fn render_rows(f: &mut Frame, area: Rect, rows: Vec<Row>, selected: usize, focused: bool) {
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

    // A default `ListState` starts at offset 0 on every frame, and ratatui then scrolls the
    // minimum needed to reveal the selection, which parks it on the last visible row: on a
    // catalogue longer than the viewport the user never sees what comes after the cursor.
    // Computing the offset keeps a few rows of context below it instead.
    let height = area.height.max(1) as usize;
    let margin = (height / 4).min(3);
    let offset = (cursor + margin + 1)
        .saturating_sub(height)
        .min(items.len().saturating_sub(height))
        .min(cursor);

    let mut state = ListState::default()
        .with_selected(Some(cursor))
        .with_offset(offset);
    f.render_stateful_widget(
        List::new(items).highlight_symbol("> ").highlight_style(
            // The marker stays on the unfocused list so the model being configured is
            // still named, but the colour follows the keys.
            match focused {
                true => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                false => Style::new(),
            },
        ),
        area,
        &mut state,
    );
}

const STEPS: [&str; 3] = ["harness", "provider", "model"];

/// The whole trail on one line: what is already answered, what is being asked, what is
/// still to come. A first-time user should see three questions coming without being told,
/// so the steps ahead are drawn as their own names rather than left out until reached.
fn title(app: &App) -> Line<'static> {
    let step = match (app.screen, app.options_open) {
        (Screen::Harness, _) => 0,
        (Screen::Provider, _) => 1,
        (Screen::Model, false) => 2,
        // The panel is a detour off the last step, not a fourth one, so the trail keeps
        // its three questions and the model stays the one being answered.
        (Screen::Model, true) => 3,
    };
    let answers = [
        Some(app.harness().name.clone()),
        app.provider().map(|p| p.name.clone()),
        app.model().map(|m| m.display().to_string()),
    ];

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(STEPS.len() * 2);
    for (i, name) in STEPS.iter().enumerate() {
        if i > 0 {
            spans.push(dim("  ·  ".to_string()));
        }
        match i.cmp(&step) {
            // Answered, so the answer takes the place of the question.
            std::cmp::Ordering::Less => spans.push(Span::styled(
                answers[i].clone().unwrap_or_else(|| "?".to_string()),
                Style::new().fg(Color::Gray),
            )),
            std::cmp::Ordering::Equal => spans.push(Span::styled(
                format!("{name} ?"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            std::cmp::Ordering::Greater => spans.push(dim(name.to_string())),
        }
    }

    if app.options_open {
        spans.push(dim("  ·  ".to_string()));
        spans.push(Span::styled(
            "options",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        if app.prompt_show_all {
            spans.push(dim("   every file".to_string()));
        }
    } else if app.screen == Screen::Model && !app.filter.is_empty() {
        spans.push(dim(format!("   /{}", app.filter)));
    }
    Line::from(spans)
}

/// The name sits with the keys rather than in the title: the title answers "what is being
/// asked", and which program is asking is a detail that belongs out of the way.
fn footer_line(app: &App) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "fastpick",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        dim(format!("   {}", help(app))),
    ])
}

fn help(app: &App) -> &'static str {
    match (app.screen, app.options_open) {
        (Screen::Harness, _) => "up/down move   right next   q quit",
        (Screen::Provider, _) => "up/down move   right next   left back   q quit",
        (Screen::Model, false) => {
            "up/down move   enter launch   right options   left back   type to filter   tab refetch"
        }
        (Screen::Model, true) => {
            "up/down move   space change   a every file   enter launch   left back"
        }
    }
}

fn status(app: &App) -> String {
    // Outranks the catalogue line: it is the answer to "why am I looking at a menu I asked
    // to skip", and the user has not seen it anywhere else.
    if let Some(n) = &app.notice {
        return n.clone();
    }
    match app.screen {
        Screen::Model => match (&app.source, app.loading()) {
            (_, true) => format!(
                "{} asking {} what it serves",
                SPINNER[app.spinner % SPINNER.len()],
                app.provider().map(|p| p.name.as_str()).unwrap_or("")
            ),
            (Some(s), false) => {
                let mut line = s.label();
                // Naming the key that did not answer is the difference between a short list
                // and a list the user believes is complete.
                if !s.failures().is_empty() {
                    line.push_str(&format!("   {}", s.failures().join("   ")));
                }
                line
            }
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

/// The model count for a provider row. `at least` when one of the site's keys has never been
/// fetched: the number is real and the list is longer, and neither half of that can be left
/// out without the row saying something untrue.
fn model_count(c: catalog::Count) -> String {
    match c.floor {
        false => count(c.total, "model"),
        true => format!("   at least {}", count(c.total, "model").trim_start()),
    }
}

fn harness_body(app: &App) -> Body {
    // Only what is installed is offered. A menu whose entries mostly fail is worse than a
    // short one, so a declared harness with no binary is left out rather than greyed.
    if app.harness_rows.is_empty() {
        let missing: Vec<String> = app
            .cfg
            .harnesses
            .iter()
            .map(|h| format!("{} ({})", h.name, h.bin))
            .collect();
        return Body::Text(format!(
            "None of the harnesses in the config is installed:\n{}\n\nInstall one, or point its `bin` at where it lives. `--harness <id>` runs one anyway.",
            missing.join("\n")
        ));
    }

    let rows = app
        .harness_rows
        .iter()
        .filter_map(|&i| app.cfg.harnesses.get(i))
        .map(|h| {
            let n = app.cfg.providers_for(&h.id).len();
            Row::Item(Line::from(vec![
                Span::raw(h.name.clone()),
                Span::styled(count(n, "provider"), Style::new().fg(Color::Blue)),
            ]))
        })
        .collect();
    Body::Rows(rows, app.harness_row)
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
        let Some(p) = app.cfg.providers.get(i) else {
            continue;
        };

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
        if let Some(c) = app.provider_counts.get(row).copied().flatten() {
            spans.push(Span::styled(model_count(c), Style::new().fg(Color::Blue)));
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
        .map(|l| {
            let m = &l.model;
            let mut spans = vec![Span::raw(m.display().to_string())];
            if m.label.is_some() {
                spans.push(dim(format!("  {}", m.id)));
            }
            // Only a provider holding several keys labels them, so a single-key row reads
            // exactly as it did before this existed.
            if let Some(key) = &l.key_label {
                spans.push(dim(format!("  {key}")));
            }
            if let Some(ctx) = m.context_window {
                spans.push(Span::styled(
                    format!("   {}K", ctx / 1000),
                    Style::new().fg(Color::Blue),
                ));
            }
            Row::Item(Line::from(spans))
        })
        .collect();
    Body::Rows(rows, app.model_idx)
}

fn options_body(app: &App) -> Body {
    let mut rows: Vec<Row> = Vec::new();

    if let Some(sel) = app.effort_idx {
        // Every level is drawn, the current one lit: what the key does is then visible
        // without a legend, which a `< high >` spinner cannot show.
        let mut spans = vec![dim("effort  ".to_string())];
        for (i, e) in app.efforts.iter().enumerate() {
            spans.push(Span::styled(
                format!(" {e}"),
                match i == sel {
                    true => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    false => Style::new().fg(Color::DarkGray),
                },
            ));
        }
        rows.push(Row::Item(Line::from(spans)));
    }

    if !app.prompt_files.is_empty() {
        if !rows.is_empty() {
            rows.push(Row::Gap);
        }
        rows.push(Row::Heading("system prompt".to_string()));
    }
    for (i, file) in app.prompt_files.iter().enumerate() {
        let mark = if app.checked.contains(&i) {
            "[x] "
        } else {
            "[ ] "
        };
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
        Config::parse(crate::config::DEFAULT_CONFIG).expect("the shipped config must parse")
    }

    /// One row as a provider holding a single key produces it: no label, key 0.
    fn listed(id: &str) -> Listed {
        Listed {
            key: 0,
            key_label: None,
            model: Model::new(id.into()),
        }
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

        // A hand-made model rather than a fetched one: the preview must not hit the network.
        app.set_screen(Screen::Model);
        let mut m = Model::new("claude-opus-5[1m]".into());
        m.label = Some("Opus 5".into());
        m.context_window = Some(1_000_000);
        m.effort = vec!["low".into(), "medium".into(), "high".into(), "max".into()];
        m.effort_default = Some("high".into());
        app.models = vec![
            Listed {
                key: 0,
                key_label: None,
                model: m,
            },
            listed("claude-sonnet-5"),
        ];
        app.rebuild_models();
        for open in [false, true] {
            app.options_open = open;
            if open {
                app.enter_options();
                app.options_open = true;
            }
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
        app.models = vec![listed("some-model")];
        app.rebuild_models();
        assert!(render(&app).contains("some-model"));

        app.enter_options();
        app.options_open = true;
        let drawn = render(&app);
        assert!(drawn.contains("Enter launches") || drawn.contains(".md"));
        // The panel sits beside the list, so the model is still on screen behind it.
        assert!(drawn.contains("some-model"));
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

    /// The menu offers what can actually run, so a config listing agents this machine does
    /// not have is not a menu of failures.
    #[test]
    fn a_harness_with_no_binary_is_not_offered() {
        let cfg = cfg();
        let app = App::new(&cfg, &Start::default());
        for &i in &app.harness_rows {
            assert!(
                crate::paths::locate(&cfg.harnesses[i].bin).is_some(),
                "`{}` is offered but is not installed",
                cfg.harnesses[i].bin
            );
        }
    }

    /// Named on the command line beats detected: a wrong answer from PATH must not make a
    /// harness unreachable.
    #[test]
    fn a_named_harness_is_offered_even_when_it_was_not_detected() {
        let cfg = cfg();
        let last = cfg.harnesses.len() - 1;
        let app = App::new(
            &cfg,
            &Start {
                harness_idx: Some(last),
                ..Start::default()
            },
        );
        assert!(app.harness_rows.contains(&last));
        assert_eq!(app.harness_idx(), last);
    }

    #[test]
    fn switching_harness_reshuffles_the_provider_list_without_leaving_a_stale_index() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());
        for i in 0..cfg.harnesses.len() {
            app.force_harness(i);
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
        app.models = vec![listed("a"), listed("b")];
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
        app.force_harness(codex);
        app.rebuild_providers();
        let mut m = Model::new("m".into());
        m.effort = vec!["high".into()];
        m.effort_default = Some("high".into());
        app.models = vec![Listed {
            key: 0,
            key_label: None,
            model: m,
        }];
        app.rebuild_models();
        app.enter_options();

        assert_eq!(app.effort_rows(), 0);
        assert!(app.picked().unwrap().effort.is_none());
        assert!(!app.unsupported.is_empty());
    }

    /// Closing the panel is not cancelling: what was ticked by hand survives, and only
    /// moving to another model starts the detection over.
    #[test]
    fn reopening_the_panel_keeps_what_was_ticked() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());
        app.set_screen(Screen::Model);
        app.models = vec![listed("a"), listed("b")];
        app.rebuild_models();
        app.ensure_options();

        app.prompt_files = vec![PromptFile {
            path: "some.md".into(),
            stem: "some".into(),
            score: 0,
        }];
        app.checked.clear();
        app.act_on_row();
        assert!(app.checked.contains(&0));

        app.options_open = false;
        app.ensure_options();
        assert!(
            app.checked.contains(&0),
            "the same model must not have its options recomputed"
        );

        app.model_idx = 1;
        app.ensure_options();
        assert!(app.checked.is_empty(), "another model starts over");
    }

    /// Enter on a model launches. The options are still resolved, so the file that matches
    /// is appended without the panel ever being opened.
    #[test]
    fn a_model_can_be_launched_without_opening_the_panel() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());
        app.set_screen(Screen::Model);
        app.models = vec![listed("a")];
        app.rebuild_models();
        app.ensure_options();
        assert!(!app.options_open);
        assert!(app.picked().is_some());
    }

    /// A provider holding two keys shows one list, and every row keeps the key that served
    /// it. The launch reads that key, so a row losing it would send one key's model to
    /// another key's endpoint.
    #[test]
    fn two_keys_share_one_list_and_each_row_keeps_its_own() {
        let cfg = cfg();
        let mut app = App::new(&cfg, &Start::default());
        app.set_screen(Screen::Model);
        app.models = vec![
            Listed {
                key: 0,
                key_label: Some("openai".into()),
                model: Model::new("gpt-5".into()),
            },
            Listed {
                key: 1,
                key_label: Some("anthropic".into()),
                model: Model::new("claude-opus-5".into()),
            },
        ];
        app.rebuild_models();
        assert_eq!(app.visible_models.len(), 2);
        assert_eq!(app.picked().unwrap().key, 0);

        app.model_idx = 1;
        let second = app.picked().unwrap();
        assert_eq!(second.model.id, "claude-opus-5");
        assert_eq!(second.key, 1);

        // The label is a row of its own as far as the filter is concerned.
        app.filter = "anthropic".into();
        app.model_idx = 0;
        app.rebuild_models();
        assert_eq!(app.visible_models, vec![1]);
        let only = app.picked().unwrap();
        assert_eq!(only.model.id, "claude-opus-5");
        assert_eq!(only.key, 1);
        assert!(render(&app).contains("anthropic"));
    }

    /// A provider with two keys and no catalogue anywhere, so `load_models` answers from the
    /// config alone and the test never reaches a network.
    fn two_keys() -> Config {
        Config::parse(
            r#"
            [[harness]]
            id = "claude-code"
            name = "Claude Code"
            kind = "claude-code"
            bin = "claude"

            [[provider]]
            id = "site"
            name = "Site"

            [[provider.key]]
            id = "openai"
            [provider.key.harness.claude-code]
            base_url = "https://site.invalid"
            [[provider.key.model]]
            id = "gpt-5"
            [[provider.key.model]]
            id = "shared"

            [[provider.key]]
            id = "anthropic"
            [provider.key.harness.claude-code]
            base_url = "https://site.invalid"
            [[provider.key.model]]
            id = "claude-opus-5"
            [[provider.key.model]]
            id = "shared"
            "#,
        )
        .unwrap()
    }

    /// Drains a fetch that answers from the config, so `models` is settled before it is read.
    fn settle(app: &mut App) {
        for _ in 0..500 {
            app.poll_models();
            if !app.loading() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the config-only lookup never landed");
    }

    /// `--key` was an answer to "which credential", so the other key is not fetched at all.
    /// Leaving its models on screen would let the next keypress pick one.
    #[test]
    fn a_named_key_is_the_only_one_that_lists_anything() {
        let cfg = two_keys();
        let start = Start {
            provider_idx: Some(0),
            key: Some(1),
            skip_harness: true,
            skip_provider: true,
            ..Start::default()
        };
        let mut app = App::new(&cfg, &start);
        app.set_screen(Screen::Model);
        app.load_models(false);
        settle(&mut app);

        assert!(
            app.models.iter().all(|l| l.key == 1),
            "a key nobody asked for listed something"
        );
        let ids: Vec<&str> = app.models.iter().map(|l| l.model.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-opus-5", "shared"]);
    }

    /// Without the flag both keys list, `shared` appears twice, and each row keeps its own.
    #[test]
    fn without_the_flag_every_key_lists_and_a_shared_id_appears_once_per_key() {
        let cfg = two_keys();
        let mut app = App::new(&cfg, &Start::default());
        app.set_screen(Screen::Model);
        app.load_models(false);
        settle(&mut app);

        let shared: Vec<usize> = app
            .models
            .iter()
            .filter(|l| l.model.id == "shared")
            .map(|l| l.key)
            .collect();
        assert_eq!(shared, vec![0, 1]);
    }

    /// One name, two subscriptions. Answering it by taking the first key would launch on a
    /// billing account the command line never named.
    #[test]
    fn a_model_id_two_keys_both_serve_is_refused_with_the_keys_named() {
        let cfg = two_keys();
        let mut app = App::new(&cfg, &Start::default());
        app.set_screen(Screen::Model);
        app.load_models(false);
        settle(&mut app);

        let err = app.select_model_id("shared").unwrap_err();
        assert!(err.contains("--key site.openai"), "{err}");
        assert!(err.contains("--key site.anthropic"), "{err}");

        // A name only one key serves is still answered, and the row keeps that key.
        assert_eq!(app.select_model_id("gpt-5"), Ok(true));
        assert_eq!(app.picked().unwrap().key, 0);
        assert_eq!(app.select_model_id("nothing-like-it"), Ok(false));
    }

    /// With --key there is only one candidate left, so the same id resolves.
    #[test]
    fn naming_the_key_makes_a_shared_model_id_answerable_again() {
        let cfg = two_keys();
        let start = Start {
            provider_idx: Some(0),
            key: Some(1),
            skip_harness: true,
            skip_provider: true,
            ..Start::default()
        };
        let mut app = App::new(&cfg, &start);
        app.set_screen(Screen::Model);
        app.load_models(false);
        settle(&mut app);

        assert_eq!(app.select_model_id("shared"), Ok(true));
        assert_eq!(app.picked().unwrap().key, 1);
        // The other key's own models went with it.
        assert_eq!(app.select_model_id("gpt-5"), Ok(false));
    }

    /// A count that is only part of the list has to say so on the row, in words, or it reads
    /// as the whole thing.
    #[test]
    fn a_partial_count_says_at_least() {
        let whole = |n| {
            model_count(catalog::Count {
                total: n,
                floor: false,
            })
        };
        let part = |n| {
            model_count(catalog::Count {
                total: n,
                floor: true,
            })
        };
        assert_eq!(whole(7).trim(), "7 models");
        assert_eq!(whole(1).trim(), "1 model");
        assert_eq!(part(7).trim(), "at least 7 models");
        assert_eq!(part(1).trim(), "at least 1 model");
    }

    #[test]
    fn wrapping_is_symmetric() {
        assert_eq!(next(2, 3), 0);
        assert_eq!(prev(0, 3), 2);
        assert_eq!(next(0, 0), 0);
        assert_eq!(prev(0, 0), 0);
    }
}
