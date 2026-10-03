//! Search as you type, with results that arrive from the network while you keep typing.
//!
//! Requests start 300ms after the last keystroke and at two characters; a newer query drops
//! the older request (dropping its future cancels it); answers are cached per query; the
//! previous results stay on screen while the next ones load, so the list never flashes.
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::sys::{self, Key};
use super::{Back, Cancelled, Item, Line, List, Mode, Screen, Tone, header, indent};

pub type Literal<'a, T> = Box<dyn Fn(&str) -> Option<Item<T>> + 'a>;
pub type Fetch<'a, T> =
    Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<Vec<Item<T>>>> + 'a>> + 'a>;

pub struct Search<'a, T> {
    pub key: &'a str,
    /// Text already typed, e.g. a query given on the command line.
    pub initial: String,
    /// Always offered, filtered locally (e.g. a single-pack provider).
    pub pinned: Vec<Item<T>>,
    /// Remote lookup; the empty query asks for something worth showing before typing.
    pub fetch: Fetch<'a, T>,
    /// What typed text means on its own: a path, a URL, a literal slug.
    pub literal: Literal<'a, T>,
}

const DEBOUNCE: Duration = Duration::from_millis(300);
const DEADLINE: Duration = Duration::from_secs(4);
const SPINNER_AFTER: Duration = Duration::from_millis(150);
const MIN_CHARS: usize = 2;

type Pending<'a, T> = Pin<Box<dyn Future<Output = Result<Vec<Item<T>>>> + 'a>>;

struct State<'a, T> {
    items: Vec<Item<T>>,
    list: List,
    cache: HashMap<String, Vec<Item<T>>>,
    /// The query the remote rows on screen belong to.
    shown_for: Option<String>,
    error: Option<String>,
    key: &'a str,
}

impl<T: Clone> State<'_, T> {
    fn query(&self) -> String {
        let q = self.list.filter.trim();
        if q.chars().count() < MIN_CHARS {
            String::new()
        } else {
            q.to_owned()
        }
    }

    /// Pinned rows matching the input, the literal reading of it, then remote rows.
    fn rebuild(&mut self, search: &Search<T>, remote: Option<Vec<Item<T>>>) {
        let keep = self.list.current().map(|i| self.items[i].label.clone());
        let needle = self.list.filter.trim().to_lowercase();
        let mut items: Vec<Item<T>> = search
            .pinned
            .iter()
            .filter(|i| {
                needle.is_empty()
                    || i.label.to_lowercase().contains(&needle)
                    || i.hint.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();
        let remote = remote.or_else(|| {
            self.shown_for
                .as_ref()
                .and_then(|q| self.cache.get(q))
                .cloned()
        });
        items.extend(remote.unwrap_or_default());
        if let Some(literal) = (search.literal)(self.list.filter.trim()) {
            if !items.iter().any(|i| i.label == literal.label) {
                items.push(literal);
            }
        }
        self.items = items;
        // The remote side already filtered; only keep the cursor on the same row.
        let filter = std::mem::take(&mut self.list.filter);
        let keep = keep.and_then(|label| self.items.iter().position(|i| i.label == label));
        self.list.refilter(&self.items, keep);
        self.list.filter = filter;
    }

    fn render(&self, width: usize, loading: Option<usize>) -> Vec<Line> {
        let status = if let Some(frame) = loading {
            ["|", "/", "-", "\\"][frame % 4].to_owned()
        } else if let Some(e) = &self.error {
            format!("! {e}")
        } else if self.list.filter.trim().chars().count() == 1 {
            "keep typing".into()
        } else {
            let n = self.items.len();
            format!("{n} result{}", if n == 1 { "" } else { "s" })
        };
        let mut lines = vec![header(self.key, &self.list.input(), &status)];
        lines.extend(self.list.rows(&self.items, width, |_| None));
        lines.extend(super::footer(indent(width), false));
        lines
    }
}

/// Ask with live search. Under root (no network allowed) or in line mode the caller should
/// prefer [`search`]'s fallbacks, which this handles.
pub async fn search<T: Clone>(search: Search<'_, T>) -> Result<T> {
    debug_assert!(super::enabled());
    if super::mode() == Mode::Line {
        return line(&search).await;
    }
    super::opening();
    let fd = tokio::io::unix::AsyncFd::new(0)?;
    let mut raw = sys::Raw::enter()?;
    let mut keys = sys::Keys::default();
    let mut screen = Screen::new();
    let mut state = State {
        items: vec![],
        list: List::new(),
        cache: HashMap::new(),
        shown_for: None,
        error: None,
        key: search.key,
    };
    state.list.filter = search.initial.clone();
    state.rebuild(&search, None);

    let mut pending: Option<(String, Instant, Pending<T>)> = None;
    let mut due: Option<Instant> = Some(Instant::now());
    let mut frame = 0usize;

    let result = loop {
        let loading = pending
            .as_ref()
            .filter(|(_, started, _)| started.elapsed() >= SPINNER_AFTER)
            .map(|_| frame);
        screen.draw(&state.render(sys::width(), loading));

        let busy = pending.is_some();
        tokio::select! {
            key = keys.next(&fd) => {
                let key = key?;
                match key {
                    Key::CtrlC => {
                        screen.finish(&super::answered(search.key, "cancelled", Tone::Dim));
                        break Err(Cancelled.into());
                    }
                    Key::CtrlZ => {
                        screen.clear();
                        raw.suspend()?;
                    }
                    Key::Resize => {}
                    Key::Up => state.list.step(&state.items, false, 1),
                    Key::Down | Key::Tab => state.list.step(&state.items, true, 1),
                    Key::PageUp => state.list.step(&state.items, false, super::WINDOW),
                    Key::PageDown => state.list.step(&state.items, true, super::WINDOW),
                    Key::Enter => {
                        if let Some(i) = state.list.current() {
                            let item = &state.items[i];
                            if item.disabled.is_none() {
                                screen.finish(&super::answered(search.key, &item.label, Tone::Plain));
                                break Ok(item.value.clone());
                            }
                        }
                    }
                    Key::Esc if !state.list.filter.is_empty() => {
                        state.list.filter.clear();
                        due = Some(Instant::now());
                        state.rebuild(&search, None);
                    }
                    Key::Esc | Key::CtrlD => {
                        screen.clear();
                        break Err(Back.into());
                    }
                    other => {
                        let before = state.list.filter.clone();
                        state.list.edit(&state.items, other);
                        if state.list.filter != before {
                            state.error = None;
                            let query = state.query();
                            if let Some(hit) = state.cache.get(&query).cloned() {
                                state.shown_for = Some(query);
                                due = None;
                                pending = None;
                                state.rebuild(&search, Some(hit));
                            } else {
                                due = Some(Instant::now() + DEBOUNCE);
                                state.rebuild(&search, None);
                            }
                        }
                    }
                }
            }
            _ = async {
                match due {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => {
                due = None;
                let query = state.query();
                if let Some(hit) = state.cache.get(&query).cloned() {
                    state.shown_for = Some(query);
                    state.rebuild(&search, Some(hit));
                } else {
                    let fut = (search.fetch)(query.clone());
                    let fut: Pending<T> = Box::pin(async move {
                        tokio::time::timeout(DEADLINE, fut)
                            .await
                            .map_err(|_| anyhow::anyhow!("no answer"))?
                    });
                    pending = Some((query, Instant::now(), fut));
                }
            }
            result = async {
                match pending.as_mut() {
                    Some((_, _, fut)) => fut.await,
                    None => std::future::pending().await,
                }
            } => {
                let (query, _, _) = pending.take().expect("a response needs a request");
                match result {
                    Ok(rows) => {
                        state.cache.insert(query.clone(), rows.clone());
                        // Only the latest query reaches the screen.
                        if query == state.query() {
                            state.shown_for = Some(query);
                            state.rebuild(&search, Some(rows));
                        }
                    }
                    Err(e) => {
                        state.error = Some(short(&e));
                        state.rebuild(&search, None);
                    }
                }
            }
            _ = async {
                if busy {
                    tokio::time::sleep(Duration::from_millis(100)).await
                } else {
                    std::future::pending().await
                }
            } => frame += 1,
        }
    };
    drop(raw);
    result
}

fn short(e: &anyhow::Error) -> String {
    let text = e.to_string();
    if text.contains("dns") || text.contains("connect") || text.contains("no answer") {
        "offline".into()
    } else {
        text.chars().take(40).collect()
    }
}

/// Line mode: a query, numbered results, then a number or a new query.
async fn line<T: Clone>(search: &Search<'_, T>) -> Result<T> {
    let mut query = search.initial.clone();
    loop {
        let mut items: Vec<Item<T>> = search
            .pinned
            .iter()
            .filter(|i| query.is_empty() || i.label.to_lowercase().contains(&query.to_lowercase()))
            .cloned()
            .collect();
        match tokio::time::timeout(DEADLINE, (search.fetch)(query.clone())).await {
            Ok(Ok(rows)) => items.extend(rows),
            Ok(Err(e)) => eprintln!("  ! {}", short(&e)),
            Err(_) => eprintln!("  ! offline"),
        }
        if let Some(literal) = (search.literal)(&query) {
            items.push(literal);
        }
        eprintln!("{}:", search.key);
        for (n, item) in items.iter().enumerate() {
            let hint = if item.hint.is_empty() {
                String::new()
            } else {
                format!(" - {}", item.hint)
            };
            eprintln!("  {}) {}{hint}", n + 1, item.label);
        }
        let answer = super::line::read("Number, or a new search:")?;
        if let Some(item) = answer
            .parse::<usize>()
            .ok()
            .and_then(|n| items.get(n.checked_sub(1)?))
        {
            return Ok(item.value.clone());
        }
        if answer.is_empty() && items.len() == 1 {
            return Ok(items[0].value.clone());
        }
        query = answer;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_combine_pinned_remote_and_literal() {
        let search = Search {
            key: "Pack",
            initial: String::new(),
            pinned: vec![Item::new("GT New Horizons", 0).hint("gtnh")],
            fetch: Box::new(|_| Box::pin(async { Ok(vec![]) })),
            literal: Box::new(|q: &str| {
                (!q.is_empty()).then(|| Item::new(format!("use \"{q}\""), 9))
            }),
        };
        let mut state = State {
            items: vec![],
            list: List::new(),
            cache: HashMap::new(),
            shown_for: None,
            error: None,
            key: "Pack",
        };
        state.rebuild(&search, Some(vec![Item::new("atm9", 1)]));
        assert_eq!(state.items.len(), 2);
        state.list.filter = "atm".into();
        state.rebuild(
            &search,
            Some(vec![Item::new("atm9", 1), Item::new("atm10", 2)]),
        );
        let labels: Vec<_> = state.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            ["atm9", "atm10", "use \"atm\""],
            "pinned filtered out"
        );
        assert_eq!(state.query(), "atm");
        state.list.filter = "a".into();
        assert_eq!(state.query(), "", "one character is not a query yet");
    }
}
