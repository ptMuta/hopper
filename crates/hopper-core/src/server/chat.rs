//! Reading what happens on a server from its console log, for `hopper chat`.
//!
//! The server logs chat, joins and leaves to its console, which systemd keeps in the journal.
//! Line shapes differ by loader: vanilla writes `[12:00:00] [Server thread/INFO]: <Steve> hi`,
//! Forge and NeoForge add a logger column, `[12:00:00] [Server thread/INFO]
//! [minecraft/MinecraftServer]: <Steve> hi`. Both are read the same way: the message is whatever
//! follows the header's last `]: `.

/// Something worth showing in a chat view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Chat {
        player: String,
        message: String,
    },
    Join {
        player: String,
    },
    Leave {
        player: String,
    },
    /// `say` from the console or over RCON.
    Server {
        message: String,
    },
}

pub fn parse(line: &str) -> Option<Event> {
    let message = body(line)?;
    // Chat that is not signed is marked; the marker is noise in a chat view.
    let message = message.strip_prefix("[Not Secure] ").unwrap_or(message);

    if let Some(rest) = message.strip_prefix('<')
        && let Some((player, text)) = rest.split_once("> ")
        && is_player_name(player)
    {
        return Some(Event::Chat {
            player: player.to_owned(),
            message: text.to_owned(),
        });
    }
    if let Some(player) = message.strip_suffix(" joined the game")
        && is_player_name(player)
    {
        return Some(Event::Join {
            player: player.to_owned(),
        });
    }
    if let Some(player) = message.strip_suffix(" left the game")
        && is_player_name(player)
    {
        return Some(Event::Leave {
            player: player.to_owned(),
        });
    }
    for prefix in ["[Rcon] ", "[Server] "] {
        if let Some(text) = message.strip_prefix(prefix) {
            return Some(Event::Server {
                message: text.to_owned(),
            });
        }
    }
    None
}

/// The message part of an INFO line from the server, or `None` for anything else.
fn body(line: &str) -> Option<&str> {
    let line = line.trim_end();
    // Only the game's own INFO output; warnings and errors are never chat.
    let level = line.find("/INFO]")?;
    let after = &line[level..];
    // The header may carry a logger column after the level; the message follows the last
    // `]: ` of the header, which always comes before any text a player could type.
    let header_end = after.find("]: ")?;
    let rest = &after[header_end + 3..];
    // Forge/NeoForge: `[Server thread/INFO] [minecraft/MinecraftServer]: msg`; the first `]: `
    // found above may close the logger column instead of the level, which is what we want.
    Some(rest)
}

/// Minecraft names are 3 to 16 letters, digits and underscores. Checking keeps arbitrary log
/// lines that happen to contain `<...>` or end in "left the game" out of the chat.
fn is_player_name(s: &str) -> bool {
    (1..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A `tellraw` command that shows `message` to every player as `<from> message`.
pub fn tellraw(from: &str, message: &str) -> String {
    let json = serde_json::json!([
        "",
        {"text": format!("<{from}> "), "color": "gold"},
        {"text": message}
    ]);
    format!("tellraw @a {json}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_vanilla_and_forge_chat_lines() {
        let vanilla = "[12:00:00] [Server thread/INFO]: <Steve> hello there";
        let forge =
            "[12:00:00] [Server thread/INFO] [minecraft/MinecraftServer]: <Steve> hello there";
        let neo = "[01Oct2026 23:14:40.553] [Server thread/INFO] [net.minecraft.server.MinecraftServer/]: <Steve> hello there";
        for line in [vanilla, forge, neo] {
            assert_eq!(
                parse(line),
                Some(Event::Chat {
                    player: "Steve".into(),
                    message: "hello there".into()
                }),
                "{line}"
            );
        }
    }

    #[test]
    fn unsigned_chat_is_read_without_its_marker() {
        let line = "[12:00:00] [Server thread/INFO]: [Not Secure] <Alex> hi";
        assert_eq!(
            parse(line),
            Some(Event::Chat {
                player: "Alex".into(),
                message: "hi".into()
            })
        );
    }

    #[test]
    fn joins_leaves_and_server_messages() {
        assert_eq!(
            parse(
                "[12:00:00] [Server thread/INFO] [minecraft/MinecraftServer]: Steve joined the game"
            ),
            Some(Event::Join {
                player: "Steve".into()
            })
        );
        assert_eq!(
            parse("[12:00:00] [Server thread/INFO]: Steve left the game"),
            Some(Event::Leave {
                player: "Steve".into()
            })
        );
        assert_eq!(
            parse(
                "[01Oct2026 23:14:40.553] [Server thread/INFO] [net.minecraft.server.MinecraftServer/]: [Rcon] hello from hopper"
            ),
            Some(Event::Server {
                message: "hello from hopper".into()
            })
        );
    }

    #[test]
    fn everything_else_is_ignored() {
        for line in [
            "[12:00:00] [Server thread/WARN]: <Steve> not chat, a warning",
            "[12:00:00] [Server thread/INFO]: Done (5.560s)! For help, type \"help\"",
            "[12:00:00] [Server thread/INFO]: Some mod left the game in a weird state",
            "[12:00:00] [Server thread/INFO]: <not a name!> x",
            "random journal noise",
        ] {
            assert_eq!(parse(line), None, "{line}");
        }
    }

    #[test]
    fn tellraw_escapes_whatever_is_typed() {
        let cmd = tellraw("muta", "a \"quoted\" \\ thing");
        assert!(cmd.starts_with("tellraw @a ["));
        let json: serde_json::Value = serde_json::from_str(&cmd["tellraw @a ".len()..]).unwrap();
        assert_eq!(json[2]["text"], "a \"quoted\" \\ thing");
        assert_eq!(json[1]["text"], "<muta> ");
    }
}
