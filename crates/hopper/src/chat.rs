//! `hopper chat`: talk with the players on a running server, and run console commands.
//!
//! What players say comes from the server's console log: the systemd journal when hopper runs
//! the server, `logs/latest.log` otherwise. What you type goes out over RCON: a plain line is
//! shown to everyone with `tellraw`, a line starting with `/` runs as a console command and its
//! output is printed. `/quit` or Ctrl-D leaves.

use std::io::{BufRead, BufReader, IsTerminal, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result};

use hopper_core::server::chat::{self, Event};
use hopper_core::server::ping::strip_codes;

use crate::cli::exit;

/// How much past chat to show on opening.
const HISTORY_LINES: &str = "300";

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

    let mut rcon = crate::service::connect_waiting(&dir)?;
    let unit = crate::service::unit_for(&dir);

    // Incoming: the log, followed in the background.
    let (tx, rx) = mpsc::channel::<String>();
    // Held until we leave; dropping it stops the journal follower.
    let _follower = match &unit {
        Some(u) => follow_journal(u, tx)?,
        None => follow_file(dir.join("logs/latest.log"), tx),
    };
    let printer_style = Style { on: style.on };
    std::thread::spawn(move || {
        for line in rx {
            if let Some(event) = chat::parse(&line) {
                println!("{}", render(&event, time_of(&line), &printer_style));
            }
        }
    });

    println!(
        "{}",
        style.paint(
            "90",
            &format!(
                "Chatting as <{me}> on {}. /command runs a console command; /quit or Ctrl-D leaves.",
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )
        )
    );

    // Outgoing: what you type.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "/quit" || line == "/exit" {
            break;
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
            Some(text) => println!("{} {}", style.paint("1;33", &format!("<{me}>")), text),
            None => {
                let out = strip_codes(&reply);
                let out = out.trim_end();
                if !out.is_empty() {
                    println!("{}", style.paint("90", out));
                }
            }
        }
        std::io::stdout().flush().ok();
    }
    Ok(exit::OK)
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

/// Follow the unit's journal, starting with recent history. The child dies with this process.
fn follow_journal(unit: &str, tx: mpsc::Sender<String>) -> Result<Follower> {
    let mut child = Command::new("journalctl")
        .args([
            "--user",
            "--unit",
            unit,
            "--follow",
            "--output",
            "cat",
            "--lines",
            HISTORY_LINES,
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

/// Follow `logs/latest.log` for a server hopper does not run, from its recent end. The game
/// starts a new file on each start, so a file that shrinks is read again from the top.
fn follow_file(path: PathBuf, tx: mpsc::Sender<String>) -> Follower {
    std::thread::spawn(move || {
        let mut pos: u64 = std::fs::metadata(&path)
            .map(|m| m.len().saturating_sub(16 * 1024))
            .unwrap_or(0);
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
