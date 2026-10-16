//! Conservative three-way text merge. Ambiguous or overlapping edits need review.
#[derive(Debug, PartialEq, Eq)]
struct Edit<'a> {
    start: usize,
    end: usize,
    lines: Vec<&'a str>,
}

fn edits<'a>(base: &[&str], other: &[&'a str]) -> Option<Vec<Edit<'a>>> {
    let width = other.len().checked_add(1)?;
    let cells = (base.len() + 1).checked_mul(width)?;
    if cells > 4_000_000 {
        return None;
    }
    let mut lcs = vec![0u32; cells];
    for i in (0..base.len()).rev() {
        for j in (0..other.len()).rev() {
            lcs[i * width + j] = if base[i] == other[j] {
                1 + lcs[(i + 1) * width + j + 1]
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < base.len() || j < other.len() {
        if i < base.len() && j < other.len() && base[i] == other[j] {
            i += 1;
            j += 1;
            continue;
        }
        let start = i;
        let mut lines = Vec::new();
        while i < base.len() || j < other.len() {
            if i < base.len() && j < other.len() && base[i] == other[j] {
                break;
            }
            if j < other.len()
                && (i == base.len() || lcs[i * width + j + 1] >= lcs[(i + 1) * width + j])
            {
                lines.push(other[j]);
                j += 1;
            } else {
                i += 1;
            }
        }
        out.push(Edit {
            start,
            end: i,
            lines,
        });
    }
    Some(out)
}

pub fn three_way(base: &[u8], local: &[u8], incoming: &[u8]) -> Option<Vec<u8>> {
    if local == base || local == incoming {
        return Some(incoming.to_vec());
    }
    if incoming == base {
        return Some(local.to_vec());
    }
    if [base, local, incoming]
        .iter()
        .any(|s| s.len() > 1024 * 1024 || s.contains(&0))
    {
        return None;
    }
    let base: Vec<_> = std::str::from_utf8(base)
        .ok()?
        .split_inclusive('\n')
        .collect();
    let local: Vec<_> = std::str::from_utf8(local)
        .ok()?
        .split_inclusive('\n')
        .collect();
    let incoming: Vec<_> = std::str::from_utf8(incoming)
        .ok()?
        .split_inclusive('\n')
        .collect();
    let mut combined = edits(&base, &local)?;
    for edit in edits(&base, &incoming)? {
        if combined.contains(&edit) {
            continue;
        }
        if combined.iter().any(|e| {
            if e.start == e.end || edit.start == edit.end {
                e.start <= edit.end && edit.start <= e.end
            } else {
                e.start < edit.end && edit.start < e.end
            }
        }) {
            return None;
        }
        combined.push(edit);
    }
    combined.sort_by_key(|e| e.start);
    let mut out = String::new();
    let mut position = 0;
    for edit in combined {
        for line in &base[position..edit.start] {
            out.push_str(line);
        }
        for line in edit.lines {
            out.push_str(line);
        }
        position = edit.end;
    }
    for line in &base[position..] {
        out.push_str(line);
    }
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_settings_and_new_upstream_defaults_merge() {
        assert_eq!(
            three_way(
                b"a=1\nb=1\nc=1\n",
                b"a=2\nb=1\nc=1\n",
                b"a=1\nb=1\nc=3\nd=4\n"
            )
            .unwrap(),
            b"a=2\nb=1\nc=3\nd=4\n"
        );
    }
    #[test]
    fn conflicting_values_and_insertions_need_review() {
        assert!(three_way(b"a=1\n", b"a=2\n", b"a=3\n").is_none());
        assert!(three_way(b"a\n", b"local\na\n", b"incoming\na\n").is_none());
    }
    #[test]
    fn newline_and_binary_bytes_are_not_lost() {
        assert_eq!(
            three_way(b"a\nb", b"a\nlocal", b"incoming\nb").unwrap(),
            b"incoming\nlocal"
        );
        assert!(three_way(b"a\0", b"b\0", b"c\0").is_none());
        assert_eq!(three_way(b"a\0", b"a\0", b"c\0").unwrap(), b"c\0");
    }
}
