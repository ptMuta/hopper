//! Reading and editing `server.properties` without disturbing anything else in it.
//!
//! hopper never writes this file during an install or update. The only writer is
//! `hopper service install`, which needs RCON on, and does it by changing exactly the keys it
//! needs: every other line, comment and ordering is kept byte for byte.

/// The value of `key`, if set.
pub fn get<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (k, v) = split(line)?;
        (k == key).then_some(v)
    })
}

/// `text` with each `(key, value)` set: replaced in place where the key exists, appended
/// otherwise.
pub fn set(text: &str, pairs: &[(&str, &str)]) -> String {
    let mut done = vec![false; pairs.len()];
    let mut out = String::with_capacity(text.len() + 64);
    for line in text.lines() {
        match split(line).and_then(|(k, _)| pairs.iter().position(|(pk, _)| *pk == k)) {
            Some(i) if !done[i] => {
                out.push_str(&format!("{}={}\n", pairs[i].0, escape(pairs[i].1)));
                done[i] = true;
            }
            // A duplicate of a key already written: drop it, or the later one would win.
            Some(_) => {}
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for (i, (k, v)) in pairs.iter().enumerate() {
        if !done[i] {
            out.push_str(&format!("{k}={}\n", escape(v)));
        }
    }
    out
}

fn split(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
        return None;
    }
    let (k, v) = trimmed
        .split_once('=')
        .or_else(|| trimmed.split_once(':'))?;
    Some((k.trim(), v.trim()))
}

/// Java properties treat `\` as an escape, so a literal one is doubled to read back unchanged.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# Minecraft server properties\n\
                        server-port=25565\n\
                        enable-rcon=false\n\
                        motd=A Minecraft Server\n";

    #[test]
    fn reads_keys_and_ignores_comments() {
        assert_eq!(get(TEXT, "server-port"), Some("25565"));
        assert_eq!(get(TEXT, "enable-rcon"), Some("false"));
        assert_eq!(get(TEXT, "rcon.port"), None);
        assert_eq!(get("#rcon.port=1\n", "rcon.port"), None);
    }

    #[test]
    fn sets_in_place_and_appends_the_rest() {
        let out = set(TEXT, &[("enable-rcon", "true"), ("rcon.port", "25575")]);
        assert_eq!(
            out,
            "# Minecraft server properties\n\
             server-port=25565\n\
             enable-rcon=true\n\
             motd=A Minecraft Server\n\
             rcon.port=25575\n"
        );
        // Idempotent.
        assert_eq!(
            set(&out, &[("enable-rcon", "true"), ("rcon.port", "25575")]),
            out
        );
    }

    #[test]
    fn a_duplicated_key_ends_up_once() {
        let out = set("a=1\na=2\n", &[("a", "3")]);
        assert_eq!(out, "a=3\n");
    }
}
