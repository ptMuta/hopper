//! One colour decision per stream. ASCII carries the meaning; colour only reinforces it.
use std::io::IsTerminal;

/// Colour for a stream that is (or is not) a terminal, honouring `NO_COLOR` and `TERM=dumb`.
pub fn enabled(terminal: bool) -> bool {
    terminal
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

pub fn stdout() -> bool {
    enabled(std::io::stdout().is_terminal())
}

pub fn stderr() -> bool {
    enabled(std::io::stderr().is_terminal())
}

#[derive(Clone, Copy)]
pub struct Paint {
    pub on: bool,
}

impl Paint {
    pub fn code(&self, code: &str, text: &str) -> String {
        if self.on && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
    /// Secondary text: hints, defaults, the echoed command. The dim attribute stays legible
    /// on light themes where a fixed grey does not.
    pub fn dim(&self, text: &str) -> String {
        self.code("2", text)
    }
    pub fn accent(&self, text: &str) -> String {
        self.code("1;36", text)
    }
    pub fn cursor(&self, text: &str) -> String {
        self.code("36", text)
    }
    pub fn bold(&self, text: &str) -> String {
        self.code("1", text)
    }
    pub fn warn(&self, text: &str) -> String {
        self.code("33", text)
    }
    pub fn inverse(&self, text: &str) -> String {
        self.code("7", text)
    }
}
