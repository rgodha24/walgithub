//! Git commit trailers, parsed with `git interpret-trailers --parse`'s rules
//! (trailer.c), in Rust: the **trailer block** is the last paragraph of the
//! message when every line of it is either a `Key: value` trailer (token =
//! `[A-Za-z0-9-]+`, optionally followed by spaces before the separator, the
//! only separator being `:`), a continuation line (starts with whitespace),
//! or — git's allowance — a non-trailer line, as long as at least one line is
//! a trailer and the first line of the paragraph is one. (git also requires
//! a blank line before the block unless the whole message is the block; the
//! subject is never part of it.) A large repository's merge-queue commits carry 8–14 of
//! them (`Merge-Queue-*`, `Co-authored-by`, …); the UI renders them
//! as a table and links what it knows.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trailer {
    pub key: String,
    pub value: String,
}

/// Split a commit **body** (everything after the subject, already trimmed)
/// into `(body without the trailer block, trailers)`.
pub fn split_trailers(body: &str) -> (String, Vec<Trailer>) {
    let body = body.trim_end();
    if body.is_empty() {
        return (String::new(), Vec::new());
    }
    // Last paragraph = after the last blank line (a line that is empty once
    // trailing spaces are stripped).
    let lines: Vec<&str> = body.lines().collect();
    let mut start = 0usize;
    for (i, l) in lines.iter().enumerate() {
        if l.trim().is_empty() {
            start = i + 1;
        }
    }
    let block = &lines[start..];
    if block.is_empty() {
        return (body.to_string(), Vec::new());
    }
    // Parse the candidate block.
    let mut trailers: Vec<Trailer> = Vec::new();
    let mut non_trailer = 0usize;
    let mut first_is_trailer = false;
    for (i, line) in block.iter().enumerate() {
        if let Some((k, v)) = parse_trailer_line(line) {
            trailers.push(Trailer { key: k, value: v });
            if i == 0 {
                first_is_trailer = true;
            }
        } else if line.starts_with([' ', '\t']) && !trailers.is_empty() {
            // Continuation (RFC 822 folding) of the previous trailer's value.
            let last = trailers.last_mut().unwrap();
            if !last.value.is_empty() {
                last.value.push(' ');
            }
            last.value.push_str(line.trim());
        } else {
            non_trailer += 1;
        }
    }
    // git: the block counts if it has at least one trailer, its first line is
    // one, and trailers are not outnumbered by arbitrary lines (we keep git's
    // "at least 25 % trailers" rule as "first line + majority").
    let ok = first_is_trailer && !trailers.is_empty() && non_trailer * 3 < block.len() * 2;
    if !ok {
        return (body.to_string(), Vec::new());
    }
    let rest = lines[..start].join("\n");
    (rest.trim_end().to_string(), trailers)
}

/// `Key: value` where the key is `[A-Za-z0-9-]+` (git's token chars) and the
/// separator is `:` (optionally preceded by spaces). `Key:value` (no space)
/// is accepted like git does; a URL (`https://…`) is not a trailer (its token
/// would be `https`, value `//…` — git would accept that, but every commit
/// body with a bare URL as its last line would then grow a bogus trailer; we
/// require the value not to start with `//`).
fn parse_trailer_line(line: &str) -> Option<(String, String)> {
    let (key, value) = line.split_once(':')?;
    let key = key.trim_end();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    if value.starts_with("//") {
        return None;
    }
    Some((key.to_string(), value.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(k: &str, v: &str) -> Trailer {
        Trailer {
            key: k.into(),
            value: v.into(),
        }
    }

    #[test]
    fn merge_queue_commit() {
        let body = "Fix the thing.\n\nMore detail here: https://github.com/acme/monorepo/pull/42\n\nMerge-Queue-Phase: target-publish\nMerge-Queue-Batch: 41\nMerge-Queue-Node: 41.2\nMerge-Queue-Pull-Request: 42\nMerge-Queue-Generated: true\nCo-authored-by: Jane Doe <jane@example.com>\nAssisted-By: agent/123e4567-e89b-12d3-a456-426614174000";
        let (rest, tr) = split_trailers(body);
        assert_eq!(
            rest,
            "Fix the thing.\n\nMore detail here: https://github.com/acme/monorepo/pull/42"
        );
        assert_eq!(tr.len(), 7);
        assert_eq!(tr[0], t("Merge-Queue-Phase", "target-publish"));
        assert_eq!(tr[5], t("Co-authored-by", "Jane Doe <jane@example.com>"));
        assert_eq!(tr[6].value, "agent/123e4567-e89b-12d3-a456-426614174000");
    }

    #[test]
    fn no_blank_line_means_the_whole_body_may_be_the_block_or_not_at_all() {
        // A body that is only trailers (no blank line): the block.
        let (rest, tr) = split_trailers("Signed-off-by: A <a@x>\nReviewed-by: B <b@x>");
        assert_eq!(rest, "");
        assert_eq!(tr.len(), 2);
        // Prose whose last line happens to contain a colon is not a block.
        let (rest, tr) =
            split_trailers("We changed the rule.\nNote: this is prose, not a trailer.");
        assert!(tr.is_empty(), "{tr:?}");
        assert_eq!(
            rest,
            "We changed the rule.\nNote: this is prose, not a trailer."
        );
    }

    #[test]
    fn key_without_space_continuations_and_non_trailer_last_paragraph() {
        let (_, tr) =
            split_trailers("Body.\n\nFixes:#12\nContext: spans two\n  lines here\nChangelog: none");
        assert_eq!(
            tr,
            vec![
                t("Fixes", "#12"),
                t("Context", "spans two lines here"),
                t("Changelog", "none")
            ]
        );
        // Last paragraph is prose with one accidental colon line: not trailers.
        let (rest, tr) = split_trailers(
            "Body.\n\nThis paragraph explains\nthe reason: it was slow\nand we fixed it.",
        );
        assert!(tr.is_empty());
        assert!(rest.ends_with("fixed it."));
        // A bare URL as the last line is never a trailer.
        let (_, tr) = split_trailers("See\n\nhttps://bot.example.com/x");
        assert!(tr.is_empty());
    }

    #[test]
    fn empty_and_subjectless() {
        assert_eq!(split_trailers(""), (String::new(), vec![]));
        assert_eq!(split_trailers("   \n\n"), (String::new(), vec![]));
    }
}
