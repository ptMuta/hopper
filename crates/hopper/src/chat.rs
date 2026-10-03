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
const HISTORY_WINDOW_S: u64 = 24 * 3600;

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

    // The last day first, then the log is followed from now on. Messages sent from here go
    // out with tellraw, which the game never logs, so they are recorded separately and merged
    // back in by time.
    let sent_log = SentLog::for_dir(&dir);
    match &unit {
        Some(u) => {
            let mut past: Vec<(u64, String)> = journal_history(u)?
                .into_iter()
                .filter_map(|(received, line)| {
                    let event = chat::parse(&line)?;
                    // Ordered by when the server wrote the line, not when journald got it: the
                    // console reaches the journal through a buffered pipe, so a line can arrive
                    // well after a message sent from here, and would sort after it.
                    let at = written_at(&line, received, utc_offset_s()).unwrap_or(received);
                    Some((at, render(&event, time_of(&line), &style)))
                })
                .collect();
            past.extend(
                sent_log
                    .recent()
                    .into_iter()
                    .map(|(at, name, msg)| (at, own_line(&name, &msg, &clock_at(at), &style))),
            );
            // Stable, so lines from the same second keep their order.
            past.sort_by_key(|(at, _)| *at);
            for (_, text) in past {
                screen.show(&text);
            }
        }
        None => {
            for line in file_lines(&dir.join("logs/latest.log")) {
                if let Some(event) = chat::parse(&line) {
                    screen.show(&render(&event, time_of(&line), &style));
                }
            }
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
            Some(text) => {
                sent_log.record(&me, text);
                screen.show(&own_line(&me, text, &now_hhmm(), &style));
            }
            // say and me come back through the log as server lines; showing them here too
            // would print them twice.
            None if echoes_through_log(&command) => screen.prompt(),
            None => screen.show(&command_block(&command, &strip_codes(&reply), &style)),
        }
    }
    if screen.interactive {
        println!();
    }
    Ok(exit::OK)
}

/// A command and its output: the command on a timestamped line, the output indented beneath
/// it behind a gutter, so it reads as the command's answer rather than as chat.
fn command_block(command: &str, output: &str, style: &Style) -> String {
    let mut block = format!(
        "{} {}",
        style.paint("90", &now_hhmm()),
        style.paint("36", &format!("/{command}"))
    );
    for line in output.trim_end().lines() {
        block.push('\n');
        block.push_str(&style.paint("90", "      │ "));
        block.push_str(&style.paint("2", line));
    }
    block
}

/// A message sent from this chat, as shown: same shape as a player's, in your own colour.
fn own_line(name: &str, message: &str, time: &str, style: &Style) -> String {
    format!(
        "{} {} {message}",
        style.paint("90", time),
        style.paint("1;33", &format!("<{name}>"))
    )
}

/// Messages sent from `hopper chat` to one server, kept for history: the game does not log
/// tellraw, so without this they would vanish from every later session.
///
/// One tab-separated line each: unix seconds, name, message. Entries older than the history
/// window are dropped the next time the file is read.
struct SentLog {
    path: Option<PathBuf>,
}

impl SentLog {
    fn for_dir(dir: &Path) -> Self {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")));
        let key: String = dir
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        Self {
            path: state.map(|s| {
                s.join("hopper/chat")
                    .join(format!("{}.log", key.trim_matches('-')))
            }),
        }
    }

    fn record(&self, name: &str, message: &str) {
        let Some(path) = &self.path else { return };
        let now = unix_now();
        // Tabs and line breaks would break the format; a chat message has no use for them.
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let line = format!("{now}\t{}\t{}\n", clean(name), clean(message));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// Entries from the last day, oldest first. Rewrites the file without older ones.
    fn recent(&self) -> Vec<(u64, String, String)> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let cutoff = unix_now().saturating_sub(HISTORY_WINDOW_S);
        let all: Vec<(u64, String, String)> = text
            .lines()
            .filter_map(|l| {
                let mut parts = l.splitn(3, '\t');
                let at = parts.next()?.parse().ok()?;
                Some((at, parts.next()?.to_owned(), parts.next()?.to_owned()))
            })
            .collect();
        let kept: Vec<_> = all
            .iter()
            .filter(|(at, ..)| *at >= cutoff)
            .cloned()
            .collect();
        if kept.len() != all.len() {
            let body: String = kept
                .iter()
                .map(|(at, n, m)| format!("{at}\t{n}\t{m}\n"))
                .collect();
            let _ = hopper_core::fs::write_atomic(path, body.as_bytes(), false);
        }
        kept
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `HH:MM` local time of a unix timestamp.
fn clock_at(unix_s: u64) -> String {
    clock_of(unix_s as i64 + utc_offset_s())
}

/// The local wall-clock time as `HH:MM`, matching the server log's timestamps.
///
/// hopper has no time-zone database; the offset is asked of `date` once and applied to the
/// system clock.
fn now_hhmm() -> String {
    clock_at(unix_now())
}

/// The local UTC offset in seconds, asked of `date` once.
fn utc_offset_s() -> i64 {
    static OFFSET_S: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET_S.get_or_init(|| {
        Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .and_then(|o| parse_utc_offset(String::from_utf8_lossy(&o.stdout).trim()))
            .unwrap_or(0)
    })
}

/// `+0300` -> 10800 seconds.
fn parse_utc_offset(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if rest.len() != 4 {
        return None;
    }
    let h: i64 = rest[..2].parse().ok()?;
    let m: i64 = rest[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

/// `HH:MM` of a time of day given in seconds since the epoch, already shifted to local time.
fn clock_of(secs: i64) -> String {
    let day = secs.rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, day / 60 % 60)
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

/// When the server wrote `line`, as unix seconds: the time of day from the line's own
/// timestamp, on the day journald received it. A line stamped later in the day than it was
/// received was written the day before (received just after midnight).
fn written_at(line: &str, received: u64, utc_offset: i64) -> Option<u64> {
    let stamp = line.strip_prefix('[')?.split(']').next()?;
    let clock = stamp.rsplit(' ').next()?;
    let mut parts = clock.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let sec: i64 = parts.next()?.get(..2)?.parse().ok()?;
    let of_day = h * 3600 + m * 60 + sec;

    let local = received as i64 + utc_offset;
    let mut written = local - local.rem_euclid(86_400) + of_day;
    if written > local + 60 {
        written -= 86_400;
    }
    u64::try_from(written - utc_offset).ok()
}

/// The last day of the unit's journal, each line with its unix time in seconds.
fn journal_history(unit: &str) -> Result<Vec<(u64, String)>> {
    let out = Command::new("journalctl")
        .args(crate::service::scope_args())
        .args([
            "--unit",
            unit,
            "--since",
            HISTORY_SINCE,
            "--until",
            "now",
            "--output",
            "json",
            "--output-fields",
            "MESSAGE",
            "--no-pager",
        ])
        .stderr(Stdio::null())
        .output()
        .context("running journalctl")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_journal_json)
        .collect())
}

/// One `journalctl --output json` record: its time and message.
///
/// A message that is not valid UTF-8 comes as an array of bytes rather than a string.
fn parse_journal_json(line: &str) -> Option<(u64, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let at = v["__REALTIME_TIMESTAMP"].as_str()?.parse::<u64>().ok()? / 1_000_000;
    let message = match &v["MESSAGE"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(bytes) => {
            let bytes: Vec<u8> = bytes
                .iter()
                .filter_map(|b| b.as_u64().map(|b| b as u8))
                .collect();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => return None,
    };
    Some((at, message))
}

/// Follow the unit's journal from now on. The child is stopped when the follower is dropped.
fn follow_journal(unit: &str, tx: mpsc::Sender<String>) -> Result<Follower> {
    let mut child = Command::new("journalctl")
        .args(crate::service::scope_args())
        .args([
            "--unit", unit, "--follow", "--lines", "0", "--output", "cat",
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
    fn lines_are_ordered_by_the_servers_own_timestamp() {
        // Received at 00:21:30 UTC, offset +3h -> 03:21:30 local; stamped 03:21:10.
        let received = 1_790_900_000 - (1_790_900_000 % 86_400) + 21 * 60 + 30;
        let offset = 3 * 3600;
        let line = "[01Oct2026 03:21:10.553] [Server thread/INFO]: [Rcon] before";
        assert_eq!(written_at(line, received, offset), Some(received - 20));
        let vanilla = "[03:21:10] [Server thread/INFO]: [Rcon] before";
        assert_eq!(written_at(vanilla, received, offset), Some(received - 20));
        // Stamped 23:59:59 local but received at 00:00:05 local: the day before.
        let just_after_midnight = 1_790_900_000 - (1_790_900_000 % 86_400) - offset as u64 + 5;
        let late = "[23:59:59] [Server thread/INFO]: x";
        assert_eq!(
            written_at(late, just_after_midnight, offset),
            Some(just_after_midnight - 6)
        );
        assert_eq!(written_at("no stamp", received, offset), None);
    }

    #[test]
    fn journal_records_give_time_and_message() {
        let rec = r#"{"__REALTIME_TIMESTAMP":"1790891234567890","MESSAGE":"[12:00:00] [Server thread/INFO]: <Steve> hi"}"#;
        assert_eq!(
            parse_journal_json(rec),
            Some((
                1_790_891_234,
                "[12:00:00] [Server thread/INFO]: <Steve> hi".into()
            ))
        );
        let bytes = r#"{"__REALTIME_TIMESTAMP":"1000000","MESSAGE":[104,105]}"#;
        assert_eq!(parse_journal_json(bytes), Some((1, "hi".into())));
        assert_eq!(parse_journal_json("not json"), None);
    }

    #[test]
    fn sent_messages_are_kept_for_a_day_and_survive_the_format() {
        let dir = tempfile::tempdir().unwrap();
        let log = SentLog {
            path: Some(dir.path().join("chat/x.log")),
        };
        log.record("muta", "hello\tthere\nfriend");
        let old = unix_now() - HISTORY_WINDOW_S - 60;
        std::fs::write(
            dir.path().join("chat/x.log"),
            format!(
                "{old}\tmuta\tstale\n{}",
                std::fs::read_to_string(dir.path().join("chat/x.log")).unwrap()
            ),
        )
        .unwrap();
        let recent = log.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].1, "muta");
        assert_eq!(recent[0].2, "hello there friend");
        // The stale entry is gone from the file too.
        assert!(
            !std::fs::read_to_string(dir.path().join("chat/x.log"))
                .unwrap()
                .contains("stale")
        );
    }

    #[test]
    fn command_output_is_indented_under_its_command() {
        let plain = Style { on: false };
        let block = command_block(
            "list",
            "There are 0 of a max of 20 players online:\n",
            &plain,
        );
        let lines: Vec<&str> = block.lines().collect();
        assert!(lines[0].ends_with(" /list"), "{block}");
        assert_eq!(
            lines[1],
            "      │ There are 0 of a max of 20 players online:"
        );
        // A command with no output is just its line.
        assert_eq!(command_block("save-all", "", &plain).lines().count(), 1);
    }

    #[test]
    fn local_times_use_the_utc_offset() {
        assert_eq!(parse_utc_offset("+0300"), Some(10_800));
        assert_eq!(parse_utc_offset("-0530"), Some(-19_800));
        assert_eq!(parse_utc_offset("UTC"), None);
        assert_eq!(clock_of(0), "00:00");
        assert_eq!(clock_of(23 * 3600 + 59 * 60 + 59), "23:59");
        assert_eq!(clock_of(-60), "23:59");
    }

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
