//! Turning a plan into something a person can scan in one glance.
//!
//! ASCII symbols carry the meaning and colour is only reinforcement, so the output is equally
//! readable piped through `less`, captured in CI, or read by someone who cannot distinguish
//! red from green. Unchanged files collapse to a count — a 300-mod pack must not print 300
//! lines to say nothing happened.

use hopper_core::apply::Summary;
use hopper_core::model::{Digest, RelPath};
use hopper_core::plan::{ConflictResolution, Decision};

/// `+` added, `~` upgraded, `-` removed, `!` needs attention.
pub fn symbol(decision: &Decision) -> Option<char> {
    match decision {
        Decision::Add { restore: false, .. } => Some('+'),
        Decision::Add { restore: true, .. } => Some('+'),
        Decision::Replace { .. } => Some('~'),
        Decision::Remove { .. } => Some('-'),
        Decision::Conflict { .. } => Some('!'),
        Decision::Preserve { first_seen, .. } => first_seen.then_some('!'),
        Decision::Reject { .. } => Some('!'),
        Decision::Keep { .. }
        | Decision::Adopt { .. }
        | Decision::Untrack { .. }
        | Decision::LeaveAlone => None,
    }
}

/// Sort key: things needing a decision first, then additions, upgrades, removals.
fn rank(decision: &Decision) -> u8 {
    match symbol(decision) {
        Some('!') => 0,
        Some('+') => 1,
        Some('~') => 2,
        Some('-') => 3,
        _ => 4,
    }
}

fn short(d: &Digest) -> String {
    d.hex()[..8].to_owned()
}

/// A single line describing one change.
pub fn line(path: &RelPath, decision: &Decision) -> Option<String> {
    let sym = symbol(decision)?;
    let name = path.as_str();
    let detail = match decision {
        Decision::Add { restore: true, .. } => "  restored".to_owned(),
        Decision::Add { .. } => String::new(),
        Decision::Replace { from, to } => format!("  {} -> {}", short(from), short(to)),
        Decision::Remove { .. } => "  removed by the pack".to_owned(),
        Decision::Preserve { .. } => "  you edited this; your version is kept".to_owned(),
        Decision::Conflict { resolution, .. } => match resolution {
            ConflictResolution::KeepOursWriteNew => {
                "  you edited this; yours kept, the pack's saved as .new".to_owned()
            }
            ConflictResolution::BackupThenWrite => {
                "  you edited this; backed up, then replaced".to_owned()
            }
            ConflictResolution::Overwrite => "  replaced".to_owned(),
            ConflictResolution::Skip => "  left as it is".to_owned(),
        },
        Decision::Reject { reason } => format!("  refused: {reason:?}"),
        _ => String::new(),
    };
    Some(format!("  {sym} {name}{detail}"))
}

/// The whole diff.
pub fn plan(decisions: &[(RelPath, Decision)], summary: &Summary, user_files: usize) -> String {
    let mut out = String::new();

    let mut shown: Vec<&(RelPath, Decision)> = decisions
        .iter()
        .filter(|(_, d)| symbol(d).is_some())
        .collect();
    shown.sort_by(|a, b| rank(&a.1).cmp(&rank(&b.1)).then(a.0.cmp(&b.0)));

    if shown.is_empty() {
        return "Already up to date.\n".to_owned();
    }

    out.push_str(&format!(
        "  +{}  ~{}  -{}  !{}\n\n",
        summary.added + summary.restored,
        summary.upgraded,
        summary.removed,
        summary.conflicts + summary.rejected
    ));

    for (path, decision) in shown {
        if let Some(l) = line(path, decision) {
            out.push_str(&l);
            out.push('\n');
        }
    }

    out.push('\n');
    out.push_str(&format!("  {} files unchanged", summary.unchanged));
    if summary.adopted > 0 {
        out.push_str(&format!(", {} adopted", summary.adopted));
    }
    // Restated every run: it is the promise the tool exists to keep.
    out.push_str(&format!(", {user_files} files added by you, untouched\n"));
    out
}

/// A warning when an update removes mods, listing what will survive.
pub fn removal_warning(summary: &Summary) -> Option<String> {
    (summary.removed > 0).then(|| {
        format!(
            "\n  Removing {} file(s) can break worlds that use their blocks or items.\n  \
             Back up world/ before continuing.\n\n  \
             hopper only deletes files it installed. Your own files, world/, logs/\n  \
             and server.properties are not touched.\n",
            summary.removed
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::model::HashAlgo;
    use hopper_core::plan::ConflictReason;

    fn d(seed: u8) -> Digest {
        Digest::new(HashAlgo::Sha512, &format!("{seed:0128x}")).unwrap()
    }

    fn p(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }

    #[test]
    fn unchanged_files_produce_no_line() {
        // A 300-mod pack must not print 300 lines to say nothing happened.
        assert_eq!(
            symbol(&Decision::Keep {
                digest: d(1),
                disabled: false
            }),
            None
        );
        assert_eq!(symbol(&Decision::LeaveAlone), None);
        assert_eq!(
            symbol(&Decision::Adopt {
                content: d(1),
                reconciled: false
            }),
            None
        );
    }

    #[test]
    fn an_acknowledged_edit_stops_being_reported() {
        assert_eq!(
            symbol(&Decision::Preserve {
                lock: d(1),
                on_disk: d(2),
                first_seen: true
            }),
            Some('!')
        );
        assert_eq!(
            symbol(&Decision::Preserve {
                lock: d(1),
                on_disk: d(2),
                first_seen: false
            }),
            None
        );
    }

    #[test]
    fn an_upgrade_shows_both_versions() {
        let l = line(
            &p("mods/a.jar"),
            &Decision::Replace {
                from: d(1),
                to: d(2),
            },
        )
        .unwrap();
        assert!(l.contains('~'));
        assert!(l.contains("->"), "got {l}");
    }

    #[test]
    fn a_conflict_explains_what_will_happen_to_their_file() {
        let l = line(
            &p("config/a.toml"),
            &Decision::Conflict {
                reason: ConflictReason::LocalModification,
                resolution: ConflictResolution::KeepOursWriteNew,
                desired: Some(d(2)),
                on_disk: Some(d(3)),
            },
        )
        .unwrap();
        assert!(l.contains("yours kept"), "got {l}");
        assert!(l.contains(".new"), "got {l}");
    }

    #[test]
    fn things_needing_attention_sort_first() {
        let decisions = vec![
            (
                p("mods/z.jar"),
                Decision::Add {
                    content: d(1),
                    restore: false,
                },
            ),
            (p("mods/y.jar"), Decision::Remove { was: d(2) }),
            (
                p("config/a.toml"),
                Decision::Conflict {
                    reason: ConflictReason::LocalModification,
                    resolution: ConflictResolution::KeepOursWriteNew,
                    desired: Some(d(3)),
                    on_disk: Some(d(4)),
                },
            ),
        ];
        let out = plan(&decisions, &Summary::of(&decisions), 0);
        let conflict = out.find("config/a.toml").unwrap();
        let add = out.find("mods/z.jar").unwrap();
        let remove = out.find("mods/y.jar").unwrap();
        assert!(conflict < add && add < remove, "got:\n{out}");
    }

    #[test]
    fn a_settled_directory_says_so_in_one_line() {
        let decisions = vec![(
            p("mods/a.jar"),
            Decision::Keep {
                digest: d(1),
                disabled: false,
            },
        )];
        assert_eq!(
            plan(&decisions, &Summary::of(&decisions), 0),
            "Already up to date.\n"
        );
    }

    #[test]
    fn the_safety_promise_is_restated_every_run() {
        let decisions = vec![(
            p("mods/a.jar"),
            Decision::Add {
                content: d(1),
                restore: false,
            },
        )];
        let out = plan(&decisions, &Summary::of(&decisions), 3);
        assert!(
            out.contains("3 files added by you, untouched"),
            "got:\n{out}"
        );
    }

    #[test]
    fn the_header_counts_every_category() {
        let decisions = vec![
            (
                p("a"),
                Decision::Add {
                    content: d(1),
                    restore: false,
                },
            ),
            (
                p("b"),
                Decision::Replace {
                    from: d(1),
                    to: d(2),
                },
            ),
            (p("c"), Decision::Remove { was: d(3) }),
        ];
        let out = plan(&decisions, &Summary::of(&decisions), 0);
        assert!(out.contains("+1"), "got:\n{out}");
        assert!(out.contains("~1"));
        assert!(out.contains("-1"));
    }

    #[test]
    fn removals_warn_about_worlds_and_say_what_survives() {
        let s = Summary {
            removed: 5,
            ..Default::default()
        };
        let w = removal_warning(&s).unwrap();
        assert!(w.contains("Back up world/"));
        assert!(w.contains("only deletes files it installed"));

        assert!(removal_warning(&Summary::default()).is_none());
    }

    #[test]
    fn output_is_plain_ascii_so_it_survives_pipes_and_ci_logs() {
        let decisions = vec![
            (
                p("a"),
                Decision::Add {
                    content: d(1),
                    restore: false,
                },
            ),
            (p("b"), Decision::Remove { was: d(2) }),
        ];
        let out = plan(&decisions, &Summary::of(&decisions), 1);
        assert!(out.is_ascii(), "got:\n{out}");
        assert!(!out.contains('\u{1b}'), "no escape codes");
    }
}
