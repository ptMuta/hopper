//! Interactive prompts: asked only when a person is at a terminal, never in scripts.
//!
//! One grid, shared with `hopper status`: a two-column gutter (`? ` while asking), a
//! 12-column key, then the value. An answered prompt collapses to that single row.
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use crate::style::Paint;

mod line;
pub mod search;
pub mod sys;

use sys::Key;

pub const KEY_WIDTH: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No one to ask: scripts, CI, pipes, background jobs, workers.
    Off,
    /// Drawn in place with arrow keys.
    Term,
    /// Numbered questions answered by lines: `TERM=dumb`, `HOPPER_ACCESSIBLE`, tiny windows.
    Line,
}

#[derive(Debug, Clone, Copy)]
struct Interaction {
    mode: Mode,
    yes: bool,
}

static INTERACTION: OnceLock<Interaction> = OnceLock::new();
static FOOTER_SHOWN: AtomicBool = AtomicBool::new(false);
/// Whether the prompt on screen is the first of this run, which alone explains the keys.
static FOOTER_NOW: AtomicBool = AtomicBool::new(false);

/// Called as each prompt opens.
fn opening() {
    FOOTER_NOW.store(
        !FOOTER_SHOWN.swap(true, Ordering::Relaxed),
        Ordering::Relaxed,
    );
}

fn truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0" && v != "false")
}

/// Decide once whether anyone can be asked. Unset means `Off`: a path that forgets to call
/// this can never prompt.
pub fn init(no_input: bool, json: bool, yes: bool, hidden: bool) {
    let terminals = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal();
    let mode = if no_input
        || json
        || hidden
        || !terminals
        || truthy("HOPPER_NO_INPUT")
        || truthy("CI")
        || !sys::foreground()
    {
        Mode::Off
    } else if std::env::var("TERM").is_ok_and(|t| t == "dumb")
        || truthy("HOPPER_ACCESSIBLE")
        || sys::width() < 40
    {
        Mode::Line
    } else {
        Mode::Term
    };
    let _ = INTERACTION.set(Interaction { mode, yes });
}

pub fn mode() -> Mode {
    INTERACTION.get().map_or(Mode::Off, |i| i.mode)
}

pub fn enabled() -> bool {
    mode() != Mode::Off
}

/// `-y`: take the default of every question that has one.
pub fn assume_defaults() -> bool {
    INTERACTION.get().is_some_and(|i| i.yes)
}

/// Ctrl-C or Esc at a prompt. Nothing has changed; exit 130.
#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}
impl std::error::Error for Cancelled {}

/// Esc with nothing typed: the caller may ask the previous question again. Uncaught, it is a
/// cancellation.
#[derive(Debug)]
pub struct Back;
impl std::fmt::Display for Back {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}
impl std::error::Error for Back {}

/// A consent or confirmation answered no (or impossible to ask). Exit 3.
#[derive(Debug)]
pub struct Declined(pub String);
impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Declined {}

/// Required input that nobody could be asked for. Exit 2, listing all of it at once.
#[derive(Debug)]
pub struct Missing {
    /// `NAME`, `--provider`, ...
    pub args: Vec<String>,
    /// A full example invocation.
    pub usage: String,
}
impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let args = match self.args.as_slice() {
            [] => String::new(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        };
        write!(f, "missing {args}\nhelp: {}", self.usage)?;
        f.write_str("\n      run in a terminal to be asked instead")
    }
}
impl std::error::Error for Missing {}

/// `·` where the terminal can show it.
pub fn sep() -> &'static str {
    let utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .is_some_and(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("utf-8") || l.contains("utf8")
        });
    if utf8 { "·" } else { "-" }
}

pub fn paint() -> Paint {
    Paint {
        on: crate::style::stderr(),
    }
}

/// `  Key         value` — the row an answered prompt leaves behind.
pub fn row(key: &str, value: &str) -> String {
    format!("  {}{value}", pad_key(key))
}

fn pad_key(key: &str) -> String {
    if key.chars().count() < KEY_WIDTH {
        format!("{key:<KEY_WIDTH$}")
    } else {
        format!("{key}  ")
    }
}

/// Print an answer row for a value that was decided without asking (one candidate).
pub fn note(key: &str, value: &str) {
    if mode() == Mode::Off {
        return;
    }
    eprintln!("{}", row(key, value));
}

// ---------------------------------------------------------------- rendering

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Dim,
    Accent,
    Cursor,
    Bold,
    Warn,
    Inverse,
}

/// One screen row, clipped to the window so nothing wraps and redraws stay exact.
#[derive(Default, Clone)]
pub struct Line(Vec<(String, Tone)>);

impl Line {
    pub fn push(mut self, text: impl Into<String>, tone: Tone) -> Self {
        self.0.push((text.into(), tone));
        self
    }
    fn render(&self, width: usize, paint: Paint) -> String {
        let mut budget = width.saturating_sub(1);
        let mut out = String::new();
        for (text, tone) in &self.0 {
            if budget == 0 {
                break;
            }
            let count = text.chars().count();
            let part: String = if count > budget {
                let keep = budget.saturating_sub(3);
                let mut s: String = text.chars().take(keep).collect();
                s.push_str(&"..."[..budget.min(3)]);
                s
            } else {
                text.clone()
            };
            budget -= part.chars().count();
            out.push_str(&match tone {
                Tone::Plain => part,
                Tone::Dim => paint.dim(&part),
                Tone::Accent => paint.accent(&part),
                Tone::Cursor => paint.cursor(&part),
                Tone::Bold => paint.bold(&part),
                Tone::Warn => paint.warn(&part),
                Tone::Inverse => paint.inverse(&part),
            });
        }
        out
    }
}

/// The block of rows a prompt occupies while it is being answered.
struct Screen {
    drawn: usize,
    paint: Paint,
}

impl Screen {
    fn new() -> Self {
        Self {
            drawn: 0,
            paint: paint(),
        }
    }
    fn erase(&self, out: &mut impl Write) {
        if self.drawn > 1 {
            let _ = write!(out, "\x1b[{}A", self.drawn - 1);
        }
        if self.drawn > 0 {
            let _ = write!(out, "\r\x1b[J");
        }
    }
    fn draw(&mut self, lines: &[Line]) {
        let width = sys::width();
        let mut out = std::io::stderr().lock();
        self.erase(&mut out);
        let text: Vec<String> = lines.iter().map(|l| l.render(width, self.paint)).collect();
        let _ = write!(out, "{}", text.join("\n"));
        let _ = out.flush();
        self.drawn = lines.len();
    }
    /// Replace the block with its one-row summary and move below it.
    fn finish(&mut self, summary: &Line) {
        self.draw(std::slice::from_ref(summary));
        let _ = writeln!(std::io::stderr());
        self.drawn = 0;
    }
    fn clear(&mut self) {
        let mut out = std::io::stderr().lock();
        self.erase(&mut out);
        let _ = out.flush();
        self.drawn = 0;
    }
}

fn header(key: &str, input: &[(String, Tone)], hint: &str) -> Line {
    let mut line = Line::default()
        .push("? ", Tone::Accent)
        .push(pad_key(key), Tone::Bold);
    for (text, tone) in input {
        line = line.push(text.clone(), *tone);
    }
    if !hint.is_empty() {
        line = line.push(format!("  {hint}"), Tone::Dim);
    }
    line
}

fn answered(key: &str, value: &str, tone: Tone) -> Line {
    Line::default()
        .push(format!("  {}", pad_key(key)), Tone::Plain)
        .push(value, tone)
}

fn footer(indent: usize, multi: bool) -> Option<Line> {
    if !multi && !FOOTER_NOW.load(Ordering::Relaxed) {
        return None;
    }
    let s = sep();
    let text = if multi {
        format!("up/down {s} space toggle {s} ctrl-a all {s} enter {s} esc back")
    } else {
        format!("up/down {s} enter {s} type to filter {s} esc back")
    };
    Some(
        Line::default()
            .push(" ".repeat(indent), Tone::Plain)
            .push(text, Tone::Dim),
    )
}

/// What a key did to a prompt.
enum Step<T> {
    Continue,
    Done(T),
    Back,
}

trait Widget {
    type Out;
    fn render(&self, width: usize) -> Vec<Line>;
    fn handle(&mut self, key: Key) -> Step<Self::Out>;
    fn summary(&self, out: &Self::Out) -> Line;
    fn key(&self) -> &str;
}

fn drive<W: Widget>(widget: &mut W) -> Result<W::Out> {
    opening();
    let mut raw = sys::Raw::enter()?;
    let mut keys = sys::Keys::default();
    let mut screen = Screen::new();
    loop {
        screen.draw(&widget.render(sys::width()));
        let key = keys.read()?;
        match key {
            Key::CtrlZ => {
                screen.clear();
                raw.suspend()?;
                continue;
            }
            Key::CtrlC => {
                screen.finish(&answered(widget.key(), "cancelled", Tone::Dim));
                return Err(Cancelled.into());
            }
            Key::Resize => continue,
            _ => {}
        }
        match widget.handle(key) {
            Step::Continue => {}
            Step::Done(out) => {
                screen.finish(&widget.summary(&out));
                return Ok(out);
            }
            Step::Back => {
                screen.clear();
                return Err(Back.into());
            }
        }
    }
}

/// A placeholder row while something loads before a question can be asked.
pub fn busy(key: &str) {
    if mode() != Mode::Term {
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "{}", paint().dim(&row(key, "...")));
    let _ = err.flush();
}

pub fn unbusy() {
    if mode() != Mode::Term {
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\r\x1b[2K");
    let _ = err.flush();
}

/// Undo the row left by the previous answer, when going back to its question.
pub fn rewind(rows: usize) {
    if mode() != Mode::Term || rows == 0 {
        return;
    }
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "\x1b[{rows}A\r\x1b[J");
    let _ = err.flush();
}

// ---------------------------------------------------------------- choices

#[derive(Debug, Clone)]
pub struct Item<T> {
    pub label: String,
    pub hint: String,
    pub value: T,
    /// Shown dimmed with this reason, and never selectable.
    pub disabled: Option<String>,
}

impl<T> Item<T> {
    pub fn new(label: impl Into<String>, value: T) -> Self {
        Self {
            label: label.into(),
            hint: String::new(),
            value,
            disabled: None,
        }
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }
}

const WINDOW: usize = 7;

/// Filter and cursor over a list, shared by select, multiselect and search.
struct List {
    filter: String,
    /// Indices into the items, in display order.
    shown: Vec<usize>,
    cursor: usize,
    top: usize,
}

impl List {
    fn new() -> Self {
        Self {
            filter: String::new(),
            shown: vec![],
            cursor: 0,
            top: 0,
        }
    }

    fn refilter<T>(&mut self, items: &[Item<T>], keep: Option<usize>) {
        let needle = self.filter.to_lowercase();
        self.shown = (0..items.len())
            .filter(|&i| {
                needle.is_empty()
                    || items[i].label.to_lowercase().contains(&needle)
                    || items[i].hint.to_lowercase().contains(&needle)
            })
            .collect();
        self.cursor = keep
            .and_then(|k| self.shown.iter().position(|&i| i == k))
            .or_else(|| self.shown.iter().position(|&i| items[i].disabled.is_none()))
            .unwrap_or(0);
        self.top = 0;
        self.scroll();
    }

    fn current(&self) -> Option<usize> {
        self.shown.get(self.cursor).copied()
    }

    fn scroll(&mut self) {
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + WINDOW {
            self.top = self.cursor + 1 - WINDOW;
        }
    }

    fn step<T>(&mut self, items: &[Item<T>], down: bool, by: usize) {
        if self.shown.is_empty() {
            return;
        }
        let mut cursor = self.cursor;
        for _ in 0..by {
            let mut next = cursor;
            loop {
                next = if down {
                    (next + 1).min(self.shown.len() - 1)
                } else {
                    next.saturating_sub(1)
                };
                if items[self.shown[next]].disabled.is_none() {
                    cursor = next;
                    break;
                }
                if next == 0 || next == self.shown.len() - 1 {
                    break;
                }
            }
        }
        self.cursor = cursor;
        self.scroll();
    }

    /// Editing keys for the filter. True if the key was consumed.
    fn edit<T>(&mut self, items: &[Item<T>], key: Key) -> bool {
        let keep = self.current();
        match key {
            Key::Char(c) if !c.is_control() => self.filter.push(c),
            Key::Backspace => {
                if self.filter.pop().is_none() {
                    return true;
                }
            }
            Key::CtrlU => self.filter.clear(),
            Key::CtrlW => {
                let trimmed = self.filter.trim_end();
                let cut = trimmed.rfind(' ').map_or(0, |i| i + 1);
                self.filter.truncate(cut);
            }
            _ => return false,
        }
        self.refilter(items, keep);
        true
    }

    fn rows<T>(
        &self,
        items: &[Item<T>],
        width: usize,
        mark: impl Fn(usize) -> Option<bool>,
    ) -> Vec<Line> {
        let indent = indent(width);
        let visible = &self.shown[self.top..(self.top + WINDOW).min(self.shown.len())];
        let label_width = visible
            .iter()
            .map(|&i| items[i].label.chars().count())
            .max()
            .unwrap_or(0)
            .min(36);
        let narrow = width < 60;
        let mut lines = vec![];
        for (offset, &i) in visible.iter().enumerate() {
            let item = &items[i];
            let here = self.top + offset == self.cursor;
            let mut line = Line::default()
                .push(" ".repeat(indent), Tone::Plain)
                .push(if here { "> " } else { "  " }, Tone::Cursor);
            if let Some(checked) = mark(i) {
                line = line.push(if checked { "[x] " } else { "[ ] " }, Tone::Plain);
            }
            let label = if item.label.chars().count() > label_width {
                let cut: String = item
                    .label
                    .chars()
                    .take(label_width.saturating_sub(3))
                    .collect();
                format!("{cut}...")
            } else {
                format!("{:<label_width$}", item.label)
            };
            line = match (&item.disabled, here) {
                (Some(_), _) => line.push(label, Tone::Dim),
                (None, true) => line.push(label, Tone::Bold),
                (None, false) => line.push(label, Tone::Plain),
            };
            let hint = item.disabled.as_deref().unwrap_or(&item.hint);
            if !narrow && !hint.is_empty() {
                line = line.push(format!("  {hint}"), Tone::Dim);
            }
            lines.push(line);
        }
        let rest = self.shown.len() - visible.len();
        if rest > 0 {
            lines.push(
                Line::default()
                    .push(" ".repeat(indent + 2), Tone::Plain)
                    .push(format!("... {rest} more"), Tone::Dim),
            );
        }
        if self.shown.is_empty() {
            lines.push(
                Line::default()
                    .push(" ".repeat(indent + 2), Tone::Plain)
                    .push("no matches", Tone::Dim),
            );
        }
        lines
    }

    fn input(&self) -> Vec<(String, Tone)> {
        vec![
            (self.filter.clone(), Tone::Plain),
            (" ".into(), Tone::Inverse),
        ]
    }
}

/// Options sit under the value column when there is room.
fn indent(width: usize) -> usize {
    if width >= 60 { 2 + KEY_WIDTH } else { 2 }
}

struct Select<'a, T> {
    key: &'a str,
    items: &'a [Item<T>],
    list: List,
}

impl<T> Widget for Select<'_, T> {
    type Out = usize;
    fn key(&self) -> &str {
        self.key
    }
    fn render(&self, width: usize) -> Vec<Line> {
        let hint = if self.list.filter.is_empty() {
            String::new()
        } else {
            format!("{}/{}", self.list.shown.len(), self.items.len())
        };
        let mut lines = vec![header(self.key, &self.list.input(), &hint)];
        lines.extend(self.list.rows(self.items, width, |_| None));
        lines.extend(footer(indent(width), false));
        lines
    }
    fn handle(&mut self, key: Key) -> Step<usize> {
        match key {
            Key::Up => self.list.step(self.items, false, 1),
            Key::Down | Key::Tab => self.list.step(self.items, true, 1),
            Key::PageUp => self.list.step(self.items, false, WINDOW),
            Key::PageDown => self.list.step(self.items, true, WINDOW),
            Key::Enter => {
                if let Some(i) = self.list.current() {
                    if self.items[i].disabled.is_none() {
                        return Step::Done(i);
                    }
                }
            }
            Key::Esc if !self.list.filter.is_empty() => {
                self.list.filter.clear();
                let keep = self.list.current();
                self.list.refilter(self.items, keep);
            }
            Key::Esc | Key::CtrlD => return Step::Back,
            other => {
                self.list.edit(self.items, other);
            }
        }
        Step::Continue
    }
    fn summary(&self, &i: &usize) -> Line {
        answered(self.key, &self.items[i].label, Tone::Plain)
    }
}

/// Pick one. `default` is preselected (and taken under `-y`).
pub fn select<T: Clone>(key: &str, items: &[Item<T>], default: Option<usize>) -> Result<T> {
    debug_assert!(enabled());
    if assume_defaults() {
        if let Some(i) = default.filter(|&i| items[i].disabled.is_none()) {
            note(key, &items[i].label);
            return Ok(items[i].value.clone());
        }
    }
    let index = match mode() {
        Mode::Line => line::select(key, items, default)?,
        _ => {
            let mut list = List::new();
            list.refilter(items, default);
            drive(&mut Select { key, items, list })?
        }
    };
    Ok(items[index].value.clone())
}

struct Multi<'a, T> {
    key: &'a str,
    items: &'a [Item<T>],
    list: List,
    checked: Vec<bool>,
}

impl<T> Widget for Multi<'_, T> {
    type Out = Vec<usize>;
    fn key(&self) -> &str {
        self.key
    }
    fn render(&self, width: usize) -> Vec<Line> {
        let count = self.checked.iter().filter(|c| **c).count();
        let mut lines = vec![header(
            self.key,
            &self.list.input(),
            &format!("{count} selected"),
        )];
        lines.extend(self.list.rows(self.items, width, |i| Some(self.checked[i])));
        lines.extend(footer(indent(width), true));
        lines
    }
    fn handle(&mut self, key: Key) -> Step<Vec<usize>> {
        match key {
            Key::Up => self.list.step(self.items, false, 1),
            Key::Down | Key::Tab => self.list.step(self.items, true, 1),
            Key::PageUp => self.list.step(self.items, false, WINDOW),
            Key::PageDown => self.list.step(self.items, true, WINDOW),
            Key::Char(' ') => {
                if let Some(i) = self.list.current() {
                    if self.items[i].disabled.is_none() {
                        self.checked[i] = !self.checked[i];
                    }
                }
            }
            Key::CtrlA => {
                let shown: Vec<usize> = self
                    .list
                    .shown
                    .iter()
                    .copied()
                    .filter(|&i| self.items[i].disabled.is_none())
                    .collect();
                let all = shown.iter().all(|&i| self.checked[i]);
                for i in shown {
                    self.checked[i] = !all;
                }
            }
            Key::Enter => {
                let picked: Vec<usize> =
                    (0..self.items.len()).filter(|&i| self.checked[i]).collect();
                // Enter with nothing ticked takes the row under the cursor.
                return match (picked.is_empty(), self.list.current()) {
                    (false, _) => Step::Done(picked),
                    (true, Some(i)) if self.items[i].disabled.is_none() => Step::Done(vec![i]),
                    _ => Step::Continue,
                };
            }
            Key::Esc if !self.list.filter.is_empty() => {
                self.list.filter.clear();
                let keep = self.list.current();
                self.list.refilter(self.items, keep);
            }
            Key::Esc | Key::CtrlD => return Step::Back,
            other => {
                self.list.edit(self.items, other);
            }
        }
        Step::Continue
    }
    fn summary(&self, picked: &Vec<usize>) -> Line {
        let names: Vec<&str> = picked
            .iter()
            .map(|&i| self.items[i].label.as_str())
            .collect();
        answered(self.key, &names.join(", "), Tone::Plain)
    }
}

/// Pick one or more.
pub fn multiselect<T: Clone>(key: &str, items: &[Item<T>], checked: &[bool]) -> Result<Vec<T>> {
    debug_assert!(enabled());
    let picked = match mode() {
        Mode::Line => line::multiselect(key, items, checked)?,
        _ => {
            let mut list = List::new();
            list.refilter(items, None);
            let mut checked = checked.to_vec();
            checked.resize(items.len(), false);
            drive(&mut Multi {
                key,
                items,
                list,
                checked,
            })?
        }
    };
    Ok(picked.into_iter().map(|i| items[i].value.clone()).collect())
}

// ---------------------------------------------------------------- text

pub type Validate<'a> = &'a dyn Fn(&str) -> Result<()>;

struct Text<'a> {
    key: &'a str,
    default: Option<&'a str>,
    validate: Validate<'a>,
    input: String,
    error: Option<String>,
}

impl Widget for Text<'_> {
    type Out = String;
    fn key(&self) -> &str {
        self.key
    }
    fn render(&self, width: usize) -> Vec<Line> {
        let input = if self.input.is_empty() {
            match self.default {
                Some(d) => vec![(" ".into(), Tone::Inverse), (d.into(), Tone::Dim)],
                None => vec![(" ".into(), Tone::Inverse)],
            }
        } else {
            vec![
                (self.input.clone(), Tone::Plain),
                (" ".into(), Tone::Inverse),
            ]
        };
        let mut lines = vec![header(self.key, &input, "")];
        if let Some(error) = &self.error {
            lines.push(
                Line::default()
                    .push(" ".repeat(indent(width)), Tone::Plain)
                    .push(format!("! {error}"), Tone::Warn),
            );
        }
        lines
    }
    fn handle(&mut self, key: Key) -> Step<String> {
        match key {
            Key::Enter => {
                let value = if self.input.is_empty() {
                    self.default.unwrap_or("").to_owned()
                } else {
                    self.input.trim().to_owned()
                };
                match (self.validate)(&value) {
                    Ok(()) => return Step::Done(value),
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            Key::Char(c) if !c.is_control() => {
                self.input.push(c);
                self.error = None;
            }
            Key::Backspace => {
                self.input.pop();
                self.error = None;
            }
            Key::CtrlU => self.input.clear(),
            Key::CtrlW => {
                let trimmed = self.input.trim_end();
                let cut = trimmed.rfind([' ', '/', '-']).map_or(0, |i| i + 1);
                self.input.truncate(cut);
            }
            Key::Esc if !self.input.is_empty() => self.input.clear(),
            Key::Esc => return Step::Back,
            Key::CtrlD if self.input.is_empty() => return Step::Back,
            _ => {}
        }
        Step::Continue
    }
    fn summary(&self, out: &String) -> Line {
        answered(self.key, out, Tone::Plain)
    }
}

/// Free text, validated on Enter with the same check the flag gets.
pub fn text(key: &str, default: Option<&str>, validate: Validate) -> Result<String> {
    debug_assert!(enabled());
    if assume_defaults() {
        if let Some(d) = default.filter(|d| validate(d).is_ok()) {
            note(key, d);
            return Ok(d.to_owned());
        }
    }
    match mode() {
        Mode::Line => line::text(key, default, validate),
        _ => drive(&mut Text {
            key,
            default,
            validate,
            input: String::new(),
            error: None,
        }),
    }
}

// ---------------------------------------------------------------- yes/no

struct YesNo<'a> {
    key: &'a str,
    detail: &'a str,
    default: bool,
    words: [&'a str; 2],
}

impl Widget for YesNo<'_> {
    type Out = bool;
    fn key(&self) -> &str {
        self.key
    }
    fn render(&self, _: usize) -> Vec<Line> {
        let choices = if self.default { "Y/n" } else { "y/N" };
        let mut input = vec![];
        if !self.detail.is_empty() {
            input.push((format!("{}  ", self.detail), Tone::Plain));
        }
        input.push((choices.into(), Tone::Dim));
        vec![header(self.key, &input, "")]
    }
    fn handle(&mut self, key: Key) -> Step<bool> {
        match key {
            Key::Char('y' | 'Y') => Step::Done(true),
            Key::Char('n' | 'N') => Step::Done(false),
            Key::Enter => Step::Done(self.default),
            Key::Esc | Key::CtrlD => Step::Back,
            _ => Step::Continue,
        }
    }
    fn summary(&self, &yes: &bool) -> Line {
        answered(self.key, self.words[usize::from(!yes)], Tone::Plain)
    }
}

fn yes_no(key: &str, detail: &str, default: bool, words: [&str; 2]) -> Result<bool> {
    match mode() {
        Mode::Line if detail.is_empty() => line::confirm(key, default),
        Mode::Line => line::confirm(&format!("{key}: {detail}"), default),
        _ => drive(&mut YesNo {
            key,
            detail,
            default,
            words,
        }),
    }
}

/// A question with a default: `-y` takes the default.
pub fn ask(key: &str, default: bool) -> Result<bool> {
    debug_assert!(enabled());
    if assume_defaults() {
        note(key, if default { "yes" } else { "no" });
        return Ok(default);
    }
    yes_no(key, "", default, ["yes", "no"])
}

/// Agreement only a person can give (EULA, client pack, firewall): never assumed, never
/// taken from `-y`, default no. False when nobody can be asked.
pub fn consent(question: &str) -> Result<bool> {
    agree(question, "")
}

/// [`consent`] in the grid: `? EULA        accept https://...  y/N`.
pub fn agree(key: &str, detail: &str) -> Result<bool> {
    if !enabled() {
        return Ok(false);
    }
    yes_no(key, detail, false, ["accepted", "declined"])
}

/// A pause before something disruptive. Scripts proceed exactly as before this prompt
/// existed; `-y` proceeds; a person at a terminal answers.
pub fn proceed(question: &str, default: bool) -> Result<bool> {
    if !enabled() || assume_defaults() {
        return Ok(true);
    }
    yes_no(question, "", default, ["yes", "no"])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn items(labels: &[&str]) -> Vec<Item<usize>> {
        labels
            .iter()
            .enumerate()
            .map(|(i, l)| Item::new(*l, i))
            .collect()
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.render(80, Paint { on: false }))
            .collect()
    }

    #[test]
    fn select_filters_skips_disabled_and_goes_back() {
        let mut items = items(&["alpha", "beta", "gamma"]);
        items[1].disabled = Some("running".into());
        let mut list = List::new();
        list.refilter(&items, None);
        let mut select = Select {
            key: "Instance",
            items: &items,
            list,
        };
        assert!(matches!(select.handle(Key::Down), Step::Continue));
        assert_eq!(select.list.current(), Some(2), "disabled row is skipped");
        let screen = plain(&select.render(80));
        assert_eq!(screen[0], "? Instance     ");
        assert_eq!(screen[1], "                alpha");
        assert_eq!(screen[2], "                beta   running");
        assert_eq!(screen[3], "              > gamma");
        for c in "alp".chars() {
            select.handle(Key::Char(c));
        }
        assert_eq!(plain(&select.render(80))[0], "? Instance    alp   1/3");
        assert!(
            matches!(select.handle(Key::Esc), Step::Continue),
            "esc clears"
        );
        assert!(
            matches!(select.handle(Key::Esc), Step::Back),
            "then goes back"
        );
        assert!(matches!(select.handle(Key::Enter), Step::Done(0)));
        assert_eq!(
            plain(&[select.summary(&0)])[0],
            "  Instance    alpha",
            "collapses into the status grid"
        );
    }

    #[test]
    fn long_lists_window_and_clip() {
        let labels: Vec<String> = (0..20).map(|i| format!("pack-{i}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let items = items(&refs);
        let mut list = List::new();
        list.refilter(&items, Some(12));
        let rows = plain(&list.rows(&items, 80, |_| None));
        assert_eq!(rows.len(), WINDOW + 1);
        assert!(rows.iter().any(|r| r.contains("> pack-12")));
        assert!(rows.last().unwrap().contains("... 13 more"));
        let narrow = list.rows(&items, 20, |_| None);
        assert!(
            narrow
                .iter()
                .all(|l| l.render(20, Paint { on: false }).chars().count() < 20)
        );
    }

    #[test]
    fn multiselect_toggles_and_defaults_to_cursor() {
        let items = items(&["a.jar", "b.jar", "c.jar"]);
        let mut list = List::new();
        list.refilter(&items, None);
        let mut multi = Multi {
            key: "Disable",
            items: &items,
            list,
            checked: vec![false; 3],
        };
        multi.handle(Key::Down);
        assert!(matches!(multi.handle(Key::Enter), Step::Done(ref v) if v == &[1]));
        multi.handle(Key::Char(' '));
        multi.handle(Key::CtrlA);
        assert!(matches!(multi.handle(Key::Enter), Step::Done(ref v) if v == &[0, 1, 2]));
        assert_eq!(
            plain(&[multi.summary(&vec![0, 2])])[0],
            "  Disable     a.jar, c.jar"
        );
    }

    #[test]
    fn text_validates_and_takes_default() {
        let check = |s: &str| {
            anyhow::ensure!(s.starts_with('a'), "must start with a");
            Ok(())
        };
        let mut text = Text {
            key: "Name",
            default: Some("atm9"),
            validate: &check,
            input: String::new(),
            error: None,
        };
        text.handle(Key::Char('x'));
        assert!(matches!(text.handle(Key::Enter), Step::Continue));
        assert_eq!(
            plain(&text.render(80))[1],
            "              ! must start with a"
        );
        text.handle(Key::Backspace);
        assert!(matches!(text.handle(Key::Enter), Step::Done(ref s) if s == "atm9"));
    }

    #[test]
    fn missing_lists_everything_once() {
        let e = Missing {
            args: vec!["NAME".into(), "--provider".into(), "--pack".into()],
            usage: "hopper install NAME --provider P".into(),
        };
        assert!(
            e.to_string()
                .starts_with("missing NAME, --provider and --pack\nhelp: hopper install")
        );
    }
}
