//! Line mode: plain numbered questions for screen readers, `TERM=dumb` and tiny windows.
//! Nothing is redrawn; every answer is a typed line. `<` goes back.
use std::io::{BufRead, Write};

use anyhow::Result;

use super::{Back, Cancelled, Item, Validate};

/// Read one answer. EOF cancels; `<` goes back.
pub fn read(question: &str) -> Result<String> {
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "{question} ");
    let _ = err.flush();
    drop(err);
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        eprintln!();
        return Err(Cancelled.into());
    }
    let line = line.trim().to_owned();
    if line == "<" {
        return Err(Back.into());
    }
    Ok(line)
}

fn list<T>(items: &[Item<T>], numbers: &[usize]) {
    for (n, &i) in numbers.iter().enumerate() {
        let item = &items[i];
        let mut row = format!("  {}) {}", n + 1, item.label);
        if let Some(reason) = &item.disabled {
            row.push_str(&format!(" (unavailable: {reason})"));
        } else if !item.hint.is_empty() {
            row.push_str(&format!(" - {}", item.hint));
        }
        eprintln!("{row}");
    }
}

fn pick<T>(items: &[Item<T>], numbers: &[usize], answer: &str) -> Option<usize> {
    let n: usize = answer.parse().ok()?;
    let i = *numbers.get(n.checked_sub(1)?)?;
    items[i].disabled.is_none().then_some(i)
}

pub fn select<T>(key: &str, items: &[Item<T>], default: Option<usize>) -> Result<usize> {
    let mut numbers: Vec<usize> = (0..items.len()).collect();
    eprintln!("{key}:");
    list(items, &numbers);
    loop {
        let suffix = default
            .and_then(|d| numbers.iter().position(|&i| i == d))
            .map(|n| format!(" [{}]", n + 1))
            .unwrap_or_default();
        let answer = read(&format!("Number, or text to filter{suffix}:"))?;
        if answer.is_empty() {
            if let Some(d) = default {
                return Ok(d);
            }
            continue;
        }
        if let Some(i) = pick(items, &numbers, &answer) {
            return Ok(i);
        }
        let needle = answer.to_lowercase();
        let found: Vec<usize> = (0..items.len())
            .filter(|&i| {
                items[i].label.to_lowercase().contains(&needle)
                    || items[i].hint.to_lowercase().contains(&needle)
            })
            .collect();
        match found.as_slice() {
            [] => eprintln!("  no matches"),
            [one] if items[*one].disabled.is_none() => return Ok(*one),
            _ => {
                numbers = found;
                list(items, &numbers);
            }
        }
    }
}

pub fn multiselect<T>(key: &str, items: &[Item<T>], checked: &[bool]) -> Result<Vec<usize>> {
    let numbers: Vec<usize> = (0..items.len()).collect();
    eprintln!("{key}:");
    list(items, &numbers);
    let preset: Vec<String> = (0..items.len())
        .filter(|&i| checked.get(i).copied().unwrap_or(false))
        .map(|i| (i + 1).to_string())
        .collect();
    loop {
        let suffix = if preset.is_empty() {
            String::new()
        } else {
            format!(" [{}]", preset.join(" "))
        };
        let answer = read(&format!("Numbers separated by spaces{suffix}:"))?;
        let answer = if answer.is_empty() {
            preset.join(" ")
        } else {
            answer
        };
        let picked: Option<Vec<usize>> = answer
            .split([' ', ','])
            .filter(|s| !s.is_empty())
            .map(|s| pick(items, &numbers, s))
            .collect();
        match picked {
            Some(p) if !p.is_empty() => return Ok(p),
            _ => eprintln!("  enter numbers from the list"),
        }
    }
}

pub fn text(key: &str, default: Option<&str>, validate: Validate) -> Result<String> {
    loop {
        let suffix = default.map(|d| format!(" [{d}]")).unwrap_or_default();
        let answer = read(&format!("{key}{suffix}:"))?;
        let value = if answer.is_empty() {
            default.unwrap_or("").to_owned()
        } else {
            answer
        };
        match validate(&value) {
            Ok(()) => return Ok(value),
            Err(e) => eprintln!("  ! {e}"),
        }
    }
}

pub fn confirm(key: &str, default: bool) -> Result<bool> {
    loop {
        let answer = read(&format!("{key} [{}]:", if default { "Y/n" } else { "y/N" }))?;
        match answer.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => eprintln!("  answer y or n"),
        }
    }
}
