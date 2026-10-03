//! Raw terminal input without a terminal crate: termios, key decoding and the guarantees
//! that the terminal is given back on every exit path (Drop, panic, SIGTERM/SIGHUP, Ctrl-Z).
use std::io::Write;
use std::os::fd::RawFd;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;

const STDIN: RawFd = 0;
const STDERR: RawFd = 2;

/// The termios to restore. Written once before raw mode is first entered and only read
/// afterwards, including from signal handlers, which may only make async-signal-safe calls.
static mut ORIGINAL: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();
static SAVED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static RESIZED: AtomicBool = AtomicBool::new(false);
static HOOKS: Once = Once::new();

/// Show the cursor and put the line discipline back. Async-signal-safe.
pub fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) && SAVED.load(Ordering::SeqCst) {
        // SAFETY: ORIGINAL is initialised before SAVED is set and never written again.
        unsafe {
            libc::tcsetattr(STDIN, libc::TCSADRAIN, (&raw const ORIGINAL).cast());
        }
    }
    let show = b"\x1b[?25h";
    // SAFETY: plain write(2) of a static buffer.
    unsafe {
        libc::write(STDERR, show.as_ptr().cast(), show.len());
    }
}

extern "C" fn on_fatal(signal: libc::c_int) {
    restore();
    // SAFETY: resetting to the default disposition and re-raising is async-signal-safe.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

extern "C" fn on_resize(_: libc::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

fn install_hooks() {
    HOOKS.call_once(|| {
        // SAFETY: handlers only touch atomics and async-signal-safe calls. SA_RESTART is left
        // off for SIGWINCH so a blocked poll(2) returns and the prompt redraws at the new size.
        unsafe {
            for (signal, handler, flags) in [
                (libc::SIGTERM, on_fatal as usize, libc::SA_RESTART),
                (libc::SIGHUP, on_fatal as usize, libc::SA_RESTART),
                (libc::SIGQUIT, on_fatal as usize, libc::SA_RESTART),
                (libc::SIGWINCH, on_resize as usize, 0),
            ] {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handler;
                action.sa_flags = flags;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
        // Release builds abort on panic, so no Drop runs: restore before the report prints.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

/// Raw mode for as long as this lives.
pub struct Raw(());

impl Raw {
    pub fn enter() -> Result<Raw> {
        install_hooks();
        // SAFETY: termios calls on fd 0, which the caller checked is a terminal.
        unsafe {
            if !SAVED.load(Ordering::SeqCst) {
                let mut original = std::mem::zeroed::<libc::termios>();
                if libc::tcgetattr(STDIN, &mut original) != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                (&raw mut ORIGINAL).cast::<libc::termios>().write(original);
                SAVED.store(true, Ordering::SeqCst);
            }
            let mut raw = (&raw const ORIGINAL).cast::<libc::termios>().read();
            libc::cfmakeraw(&mut raw);
            // Keep output post-processing so "\n" still returns the carriage.
            raw.c_oflag |= libc::OPOST;
            if libc::tcsetattr(STDIN, libc::TCSADRAIN, &raw) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        ACTIVE.store(true, Ordering::SeqCst);
        let mut err = std::io::stderr();
        let _ = err.write_all(b"\x1b[?25l");
        let _ = err.flush();
        Ok(Raw(()))
    }

    /// Ctrl-Z: give the terminal back, stop like any job would, and take it again on `fg`.
    pub fn suspend(&mut self) -> Result<()> {
        restore();
        // SAFETY: SIGTSTP with its default action stops the process until SIGCONT.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
        std::mem::forget(Raw::enter()?);
        Ok(())
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        restore();
    }
}

/// Discard typed-ahead input, so keys meant for a prompt never reach a child process.
pub fn flush_input() {
    // SAFETY: tcflush on a terminal fd.
    unsafe {
        libc::tcflush(STDIN, libc::TCIFLUSH);
    }
}

/// Columns on stderr, where prompts draw; 80 when unknown.
pub fn width() -> usize {
    // SAFETY: TIOCGWINSZ fills a winsize struct.
    unsafe {
        let mut size = std::mem::zeroed::<libc::winsize>();
        if libc::ioctl(STDERR, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0 {
            return size.ws_col as usize;
        }
    }
    80
}

/// True when this process owns the terminal (not `hopper ... &`).
pub fn foreground() -> bool {
    // SAFETY: plain process-group queries.
    unsafe { libc::tcgetpgrp(STDIN) == libc::getpgrp() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Delete,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    CtrlA,
    CtrlC,
    CtrlD,
    CtrlU,
    CtrlW,
    CtrlZ,
    Resize,
}

/// Decode one key from the front of `buf`. `None` means more bytes are needed; `eof` says
/// none are coming, so a lone ESC is the Escape key rather than the start of a sequence.
pub fn parse(buf: &[u8], eof: bool) -> Option<(Option<Key>, usize)> {
    let first = *buf.first()?;
    let key = |k| Some((Some(k), 1));
    match first {
        b'\r' | b'\n' => key(Key::Enter),
        0x7f | 0x08 => key(Key::Backspace),
        b'\t' => key(Key::Tab),
        0x01 => key(Key::CtrlA),
        0x03 => key(Key::CtrlC),
        0x04 => key(Key::CtrlD),
        0x15 => key(Key::CtrlU),
        0x17 => key(Key::CtrlW),
        0x1a => key(Key::CtrlZ),
        0x0e => key(Key::Down),
        0x10 => key(Key::Up),
        0x1b => {
            if buf.len() == 1 {
                return if eof { key(Key::Esc) } else { None };
            }
            if buf[1] != b'[' && buf[1] != b'O' {
                // Alt+key or a stray ESC: report Escape and leave the rest.
                return key(Key::Esc);
            }
            // CSI/SS3: parameters, then a final byte in 0x40..=0x7e.
            let end = buf[2..].iter().position(|b| (0x40..=0x7e).contains(b));
            let Some(end) = end else {
                return if eof { Some((None, buf.len())) } else { None };
            };
            let len = end + 3;
            let params = &buf[2..len - 1];
            let k = match (buf[len - 1], params) {
                (b'A', _) => Some(Key::Up),
                (b'B', _) => Some(Key::Down),
                (b'C', _) => Some(Key::Right),
                (b'D', _) => Some(Key::Left),
                (b'H', _) => Some(Key::Home),
                (b'F', _) => Some(Key::End),
                (b'~', b"1" | b"7") => Some(Key::Home),
                (b'~', b"4" | b"8") => Some(Key::End),
                (b'~', b"3") => Some(Key::Delete),
                (b'~', b"5") => Some(Key::PageUp),
                (b'~', b"6") => Some(Key::PageDown),
                _ => None,
            };
            Some((k, len))
        }
        c if c < 0x20 => Some((None, 1)),
        _ => {
            let len = match first {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            if buf.len() < len {
                return if eof { Some((None, buf.len())) } else { None };
            }
            let ch = std::str::from_utf8(&buf[..len])
                .ok()
                .and_then(|s| s.chars().next());
            Some((ch.map(Key::Char), len))
        }
    }
}

/// Keys from stdin while in raw mode.
#[derive(Default)]
pub struct Keys {
    buf: Vec<u8>,
}

impl Keys {
    fn take(&mut self, eof: bool) -> Option<Key> {
        while let Some((key, used)) = parse(&self.buf, eof) {
            self.buf.drain(..used);
            if key.is_some() {
                return key;
            }
        }
        None
    }

    fn fill(&mut self) -> std::io::Result<usize> {
        let mut chunk = [0u8; 64];
        // SAFETY: read(2) into a stack buffer.
        let n = unsafe { libc::read(STDIN, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.buf.extend_from_slice(&chunk[..n as usize]);
        Ok(n as usize)
    }

    /// Wait up to `timeout` (forever when `None`) for stdin to be readable.
    fn wait(timeout: Option<Duration>) -> std::io::Result<bool> {
        let mut fd = libc::pollfd {
            fd: STDIN,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.map_or(-1, |t| t.as_millis() as i32);
        // SAFETY: poll(2) on one pollfd.
        let n = unsafe { libc::poll(&mut fd, 1, ms) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n > 0)
    }

    /// Block until a key (or a resize) arrives.
    pub fn read(&mut self) -> Result<Key> {
        loop {
            if RESIZED.swap(false, Ordering::SeqCst) {
                return Ok(Key::Resize);
            }
            if let Some(key) = self.take(false) {
                return Ok(key);
            }
            let partial = !self.buf.is_empty();
            match Self::wait(partial.then(|| Duration::from_millis(30))) {
                Ok(true) => {
                    if self.fill()? == 0 {
                        return Ok(Key::CtrlD);
                    }
                }
                // A lone ESC with nothing after it within 30ms is the Escape key.
                Ok(false) => {
                    if let Some(key) = self.take(true) {
                        return Ok(key);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// The same, without blocking the async runtime.
    pub async fn next(&mut self, fd: &tokio::io::unix::AsyncFd<RawFd>) -> Result<Key> {
        loop {
            if RESIZED.swap(false, Ordering::SeqCst) {
                return Ok(Key::Resize);
            }
            if let Some(key) = self.take(false) {
                return Ok(key);
            }
            // Readiness is edge-triggered, so look at what is already there before waiting.
            if Self::wait(Some(Duration::ZERO))? {
                if self.fill()? == 0 {
                    return Ok(Key::CtrlD);
                }
                continue;
            }
            // An incomplete escape sequence gets 30ms for the rest; otherwise wake now and
            // then to notice resizes, which arrive as signals.
            let partial = !self.buf.is_empty();
            let wait = Duration::from_millis(if partial { 30 } else { 100 });
            match tokio::time::timeout(wait, fd.readable()).await {
                Err(_) if partial => {
                    if let Some(key) = self.take(true) {
                        return Ok(key);
                    }
                }
                Err(_) => {}
                Ok(guard) => guard?.clear_ready(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(bytes: &[u8]) -> Vec<Key> {
        let mut keys = Keys {
            buf: bytes.to_vec(),
        };
        std::iter::from_fn(|| keys.take(true)).collect()
    }

    #[test]
    fn decodes_keys_and_sequences() {
        assert_eq!(
            all(b"a\x1b[A\x1b[B\r\x7f\x1b[3~\x1bOH\x03"),
            [
                Key::Char('a'),
                Key::Up,
                Key::Down,
                Key::Enter,
                Key::Backspace,
                Key::Delete,
                Key::Home,
                Key::CtrlC
            ]
        );
        assert_eq!(all("ä".as_bytes()), [Key::Char('ä')]);
        assert_eq!(all(b"\x1b"), [Key::Esc]);
        // Unknown sequences are swallowed whole, not typed as text.
        assert_eq!(all(b"\x1b[200~x"), [Key::Char('x')]);
        assert_eq!(all(b"\x1b[1;5C"), [Key::Right], "modifiers are ignored");
    }

    #[test]
    fn waits_for_the_rest_of_a_sequence() {
        assert_eq!(parse(b"\x1b", false), None);
        assert_eq!(parse(b"\x1b[", false), None);
        assert_eq!(parse(&"ä".as_bytes()[..1], false), None);
    }
}
