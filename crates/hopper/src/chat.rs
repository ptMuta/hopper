//! `hopper chat`: talk with the players on a running server, and run console commands.
//!
//! What players say comes from the server's console log: the systemd journal when hopper runs
//! the server, `logs/latest.log` otherwise. What you type goes out over RCON: a plain line is
//! shown to everyone with `tellraw`, a line starting with `/` runs as a console command and its
//! output is printed. `/quit` or Ctrl-D leaves.

use std::io::{BufRead, BufReader, IsTerminal, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context, Result};

use hopper_core::server::chat::{self, Event};
use hopper_core::server::ping::strip_codes;

use crate::cli::exit;

/// How much past chat to show on opening.
const HISTORY_SINCE: &str = "-24h";

struct Style {
    on: bool,
}

impl Style {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// Everything shown goes through here, so the log thread and the input prompt never interleave.
///
/// In a terminal, input sits on a `> ` prompt at the bottom. A log line arriving meanwhile
/// clears the prompt line, prints above it and draws the prompt again; after Enter, the
/// terminal's own echo of the typed line is replaced by its rendered form, so each message
/// appears once.
#[derive(Clone)]
struct Screen {
    interactive: bool,
    prompting: Arc<AtomicBool>,
    lock: Arc<Mutex<()>>,
}

impl Screen {
    const PROMPT: &str = "> ";

    fn show(&self, text: &str) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = std::io::stdout().lock();
        let prompting = self.interactive && self.prompting.load(Ordering::Relaxed);
        if prompting {
            let _ = write!(out, "\r\x1b[2K");
        }
        let _ = writeln!(out, "{text}");
        if prompting {
            let _ = write!(out, "{}", Self::PROMPT);
        }
        let _ = out.flush();
    }

    /// Start, or return to, waiting for input.
    fn prompt(&self) {
        if !self.interactive {
            return;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.prompting.store(true, Ordering::Relaxed);
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "{}", Self::PROMPT);
        let _ = out.flush();
    }

    /// After Enter: remove the terminal's echo of `> typed line`.
    fn erase_input(&self) {
        if !self.interactive {
            return;
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\x1b[1A\x1b[2K\r");
        let _ = out.flush();
    }
}

pub fn run(dir: &Path, name: Option<&str>) -> Result<i32> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{} does not exist", dir.display()))?;
    let me = name
        .map(str::to_owned)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "server".to_owned());
    let style = Style {
        on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };
    let screen = Screen {
        interactive: style.on && std::io::stdin().is_terminal(),
        prompting: Arc::new(AtomicBool::new(false)),
        lock: Arc::new(Mutex::new(())),
    };

    let mut rcon = crate::service::connect_waiting(&dir)?;
    let unit = crate::service::unit_for(&dir);

    screen.show(&style.paint(
        "90",
        &format!(
            "Chatting as <{me}> on {}. /command runs a console command; /quit or Ctrl-D leaves.",
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
    ));

    // The last day first, then the log is followed from now on.
    let history: Box<dyn Iterator<Item = String>> = match &unit {
        Some(u) => journal_history(u)?,
        None => Box::new(file_lines(&dir.join("logs/latest.log"))),
    };
    for line in history {
        if let Some(event) = chat::parse(&line) {
            screen.show(&render(&event, time_of(&line), &style));
        }
    }

    let (tx, rx) = mpsc::channel::<String>();
    // Held until we leave; dropping it stops the journal follower.
    let _follower = match &unit {
        Some(u) => follow_journal(u, tx)?,
        None => follow_file(dir.join("logs/latest.log"), tx),
    };
    let incoming = screen.clone();
    let incoming_style = Style { on: style.on };
    std::thread::spawn(move || {
        for line in rx {
            if let Some(event) = chat::parse(&line) {
                incoming.show(&render(&event, time_of(&line), &incoming_style));
            }
        }
    });

    screen.prompt();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        screen.erase_input();
        let line = line.trim();
        if line == "/quit" || line == "/exit" {
            break;
        }
        if line.is_empty() {
            screen.prompt();
            continue;
        }
        let (command, echo) = match line.strip_prefix('/') {
            Some(cmd) => (cmd.to_owned(), None),
            None => (chat::tellraw(&me, line), Some(line)),
        };
        let reply = match rcon.command(&command) {
            Ok(r) => r,
            // The server may have restarted underneath a long session; reconnect once.
            Err(_) => {
                rcon = crate::service::connect_waiting(&dir)?;
                rcon.command(&command)?
            }
        };
        match echo {
            // tellraw does not reach the console log, so our own message is shown here.
            Some(text) => screen.show(&format!(
                "{} {text}",
                style.paint("1;33", &format!("<{me}>"))
            )),
            // say and me come back through the log as server lines; showing them here too
            // would print them twice.
            None if echoes_through_log(&command) => screen.prompt(),
            None => {
                let out = strip_codes(&reply);
                let out = out.trim_end();
                screen.show(&style.paint("90", &format!("/{command}")));
                if !out.is_empty() {
                    screen.show(out);
                }
            }
        }
    }
    if screen.interactive {
        println!();
    }
    Ok(exit::OK)
}

/// Commands whose effect the console log already shows.
fn echoes_through_log(command: &str) -> bool {
    matches!(command.split_whitespace().next(), Some("say" | "me"))
}

fn render(event: &Event, time: Option<&str>, style: &Style) -> String {
    let time = time.map(|t| style.paint("90", t) + " ").unwrap_or_default();
    match event {
        Event::Chat { player, message } => {
            format!(
                "{time}{} {message}",
                style.paint("1;36", &format!("<{player}>"))
            )
        }
        Event::Join { player } => {
            format!("{time}{}", style.paint("32", &format!("→ {player} joined")))
        }
        Event::Leave { player } => {
            format!("{time}{}", style.paint("33", &format!("← {player} left")))
        }
        Event::Server { message } => {
            format!("{time}{} {message}", style.paint("35", "[server]"))
        }
    }
}

/// `HH:MM` from a console line's leading timestamp, whatever the loader's date format.
fn time_of(line: &str) -> Option<&str> {
    let stamp = line.strip_prefix('[')?.split(']').next()?;
    let clock = stamp.rsplit(' ').next()?;
    (clock.len() >= 5 && clock.as_bytes()[2] == b':').then(|| &clock[..5])
}

/// The last day of the unit's journal, read to the end.
fn journal_history(unit: &str) -> Result<Box<dyn Iterator<Item = String>>> {
    let mut child = Command::new("journalctl")
        .args([
            "--user",
            "--unit",
            unit,
            "--since",
            HISTORY_SINCE,
            "--until",
            "now",
            "--output",
            "cat",
            "--no-pager",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running journalctl")?;
    let out = child.stdout.take().expect("piped");
    let lines = BufReader::new(out).lines().map_while(Result::ok);
    // Reaped once the history has been read.
    Ok(Box::new(lines.chain(std::iter::from_fn(move || {
        let _ = child.wait();
        None
    }))))
}

/// Follow the unit's journal from now on. The child is stopped when the follower is dropped.
fn follow_journal(unit: &str, tx: mpsc::Sender<String>) -> Result<Follower> {
    let mut child = Command::new("journalctl")
        .args([
            "--user", "--unit", unit, "--follow", "--lines", "0", "--output", "cat",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running journalctl")?;
    let out = child.stdout.take().expect("piped");
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    Ok(Follower(Some(child)))
}

/// Every line of a log file, for history when there is no journal. `latest.log` covers the
/// server's current run, which is as far back as the game keeps it uncompressed.
fn file_lines(path: &Path) -> impl Iterator<Item = String> + use<> {
    std::fs::File::open(path)
        .ok()
        .into_iter()
        .flat_map(|f| BufReader::new(f).lines().map_while(Result::ok))
}

/// Follow `logs/latest.log` for a server hopper does not run, from its recent end. The game
/// starts a new file on each start, so a file that shrinks is read again from the top.
fn follow_file(path: PathBuf, tx: mpsc::Sender<String>) -> Follower {
    std::thread::spawn(move || {
        // History was already shown from the file; follow from its current end.
        let mut pos: u64 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut partial = String::new();
        loop {
            if let Ok(mut f) = std::fs::File::open(&path) {
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                if len < pos {
                    pos = 0;
                }
                if len > pos && f.seek(std::io::SeekFrom::Start(pos)).is_ok() {
                    let mut buf = String::new();
                    if f.read_to_string(&mut buf).is_ok() {
                        pos = len;
                        partial.push_str(&buf);
                        while let Some(i) = partial.find('\n') {
                            let line: String = partial.drain(..=i).collect();
                            if tx.send(line).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
    Follower(None)
}

/// The log source. A journal follower is a child process, stopped when this is dropped: left
/// alone, `journalctl --follow` would outlive the chat until the server next logged a line.
struct Follower(Option<std::process::Child>);

impl Drop for Follower {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn say_and_me_are_not_echoed_twice() {
        assert!(echoes_through_log("say hello"));
        assert!(echoes_through_log("me waves"));
        assert!(!echoes_through_log("list"));
        assert!(!echoes_through_log("sayings"));
    }

    #[test]
    fn times_are_read_from_either_timestamp_format() {
        assert_eq!(time_of("[12:34:56] [Server thread/INFO]: x"), Some("12:34"));
        assert_eq!(
            time_of("[01Oct2026 23:14:40.553] [Server thread/INFO]: x"),
            Some("23:14")
        );
        assert_eq!(time_of("no stamp"), None);
    }

    #[test]
    fn events_render_without_colour_when_asked() {
        let plain = Style { on: false };
        let chat = Event::Chat {
            player: "Steve".into(),
            message: "hi".into(),
        };
        assert_eq!(render(&chat, Some("12:34"), &plain), "12:34 <Steve> hi");
        assert_eq!(
            render(
                &Event::Join {
                    player: "Alex".into()
                },
                None,
                &plain
            ),
            "→ Alex joined"
        );
    }
}
