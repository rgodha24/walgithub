//! `blame(path:)` on a commit, out of `git blame --porcelain`.
//!
//! Two documents in the contract select it (`docs/GITHUB_SHAPES.md`,
//! `BlameAuthors` and `Blame`): one reads
//! `ranges[].commit.author.user.email ?? .author.email`, the other reads
//! `ranges[].commit.committedDate` and *rethrows* on failure, so this must
//! answer a real date that `new Date(...)` parses.
//!
//! `git blame` needs no work tree — it walks the object database — so it runs
//! against the serving copy like every other read here.

use serde_json::{Value, json};

use super::error::GqlError;
use super::ops::{Ctx, git};
use super::parse::Field;
use crate::github::repo::View;

/// One contiguous run of lines attributed to one commit.
#[derive(Debug, PartialEq, Eq)]
struct Range {
    oid: String,
    start: u64,
    end: u64,
}

/// Everything the porcelain header tells us about a commit, once.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Meta {
    author_name: String,
    author_email: String,
    author_time: String,
    author_tz: String,
    author_date: String,
    committer_name: String,
    committer_email: String,
    committer_time: String,
    committer_tz: String,
    committer_date: String,
    summary: String,
}

pub async fn blame(ctx: &Ctx, view: &View, f: &Field, tip: &str) -> Result<Value, GqlError> {
    let Some(path) = f.str_arg("path").filter(|p| !p.is_empty()) else {
        return Err(GqlError::bad_request("blame(path:) is required"));
    };
    // A path that is not in the tree is an empty blame, not an error: the
    // authors variant swallows failures and the date variant rethrows, and a
    // renamed page must not take a deployment down.
    let Ok(out) = git(
        view,
        // `--end-of-options` is not accepted before `git blame`'s rev; the
        // `--` separator already ends the options.
        &["blame", "--porcelain", tip, "--", path],
    )
    .await
    else {
        return Ok(json!({ "ranges": [] }));
    };
    let (ranges, meta) = parse(&String::from_utf8_lossy(&out));
    let empty = Meta::default();
    Ok(json!({
        "ranges": ranges
            .iter()
            .map(|r| {
                let m = meta.get(&r.oid).unwrap_or(&empty);
                json!({
                    "startingLine": r.start,
                    "endingLine": r.end,
                    "age": 1,
                    "commit": {
                        "oid": r.oid,
                        "abbreviatedOid": r.oid.chars().take(7).collect::<String>(),
                        "message": m.summary,
                        "messageHeadline": m.summary,
                        "authoredDate": m.author_date,
                        "committedDate": m.committer_date,
                        "url": format!("{}/{}/commit/{}", ctx.urls.html, view.full_name, r.oid),
                        "author": person(ctx, &m.author_name, &m.author_email, &m.author_date),
                        "committer": person(
                            ctx,
                            &m.committer_name,
                            &m.committer_email,
                            &m.committer_date,
                        ),
                    },
                })
            })
            .collect::<Vec<_>>(),
    }))
}

fn person(ctx: &Ctx, name: &str, email: &str, date: &str) -> Value {
    let login = email
        .split('@')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    json!({
        "name": name,
        "email": email,
        "date": date,
        "user": {
            "login": login,
            "email": email,
            "url": format!("{}/{login}", ctx.urls.html),
        },
    })
}

/// The porcelain format: a group header `<oid> <orig> <final> [<count>]`,
/// then `key value` lines that appear once per commit, then the line itself
/// prefixed with a tab. Adjacent groups from the same commit are merged —
/// GitHub's ranges are per commit, not per hunk.
fn parse(out: &str) -> (Vec<Range>, std::collections::HashMap<String, Meta>) {
    let mut ranges: Vec<Range> = Vec::new();
    let mut meta: std::collections::HashMap<String, Meta> = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in out.lines() {
        if let Some(header) = group_header(line) {
            let (oid, start, count) = header;
            match ranges.last_mut() {
                // git repeats a header for every line of a group (the second
                // and later ones without a count), and consecutive groups can
                // share a commit; both extend the range in place.
                Some(last) if last.oid == oid && start <= last.end + 1 => {
                    last.end = last.end.max(start + count - 1);
                }
                _ => ranges.push(Range {
                    oid: oid.clone(),
                    start,
                    end: start + count - 1,
                }),
            }
            current = Some(oid);
            continue;
        }
        let Some(oid) = current.as_ref() else { continue };
        let Some((key, rest)) = line.split_once(' ') else {
            continue;
        };
        let entry = meta.entry(oid.clone()).or_default();
        match key {
            "author" => entry.author_name = rest.to_string(),
            "author-mail" => entry.author_email = unangle(rest),
            "author-time" => entry.author_time = rest.to_string(),
            "author-tz" => entry.author_tz = rest.to_string(),
            "committer" => entry.committer_name = rest.to_string(),
            "committer-mail" => entry.committer_email = unangle(rest),
            "committer-time" => entry.committer_time = rest.to_string(),
            "committer-tz" => entry.committer_tz = rest.to_string(),
            "summary" => entry.summary = rest.to_string(),
            _ => {}
        }
    }
    for entry in meta.values_mut() {
        entry.author_date = iso(&entry.author_time, &entry.author_tz);
        entry.committer_date = iso(&entry.committer_time, &entry.committer_tz);
    }
    (ranges, meta)
}

fn group_header(line: &str) -> Option<(String, u64, u64)> {
    let mut parts = line.split(' ');
    let oid = parts.next()?;
    if oid.len() < 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let _orig: u64 = parts.next()?.parse().ok()?;
    let final_line: u64 = parts.next()?.parse().ok()?;
    let count: u64 = parts.next().and_then(|c| c.parse().ok()).unwrap_or(1);
    Some((oid.to_string(), final_line, count.max(1)))
}

fn unangle(s: &str) -> String {
    s.trim_start_matches('<').trim_end_matches('>').to_string()
}

/// `<epoch seconds>` + `+0100` → RFC 3339, which is what `new Date(...)` in
/// the client parses. An unparseable pair stays as it was read.
fn iso(seconds: &str, tz: &str) -> String {
    let Ok(secs) = seconds.trim().parse::<i64>() else {
        return seconds.to_string();
    };
    let offset = parse_tz(tz).unwrap_or(0);
    let Some(at) = chrono::DateTime::from_timestamp(secs, 0) else {
        return seconds.to_string();
    };
    match chrono::FixedOffset::east_opt(offset) {
        Some(fixed) => at
            .with_timezone(&fixed)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        None => at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }
}

fn parse_tz(tz: &str) -> Option<i32> {
    let tz = tz.trim();
    let (sign, digits) = match tz.as_bytes().first()? {
        b'-' => (-1, tz.get(1..)?),
        b'+' => (1, tz.get(1..)?),
        _ => (1, tz),
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits.get(0..2)?.parse().ok()?;
    let minutes: i32 = digits.get(2..4)?.parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::{Range, iso, parse, parse_tz};

    const PORCELAIN: &str = "\
1111111111111111111111111111111111111111 1 1 2
author Ada
author-mail <ada@example.com>
author-time 1700000000
author-tz +0000
committer Ada
committer-mail <ada@example.com>
committer-time 1700000000
committer-tz +0000
summary first
filename docs/index.mdx
\tline one
1111111111111111111111111111111111111111 2 2
\tline two
2222222222222222222222222222222222222222 3 3 1
author Bob
author-mail <bob@example.com>
author-time 1700000600
author-tz +0200
committer Bob
committer-mail <bob@example.com>
committer-time 1700000600
committer-tz +0200
summary second
filename docs/index.mdx
\tline three
";

    #[test]
    fn porcelain_groups_become_ranges_and_metadata() {
        let (ranges, meta) = parse(PORCELAIN);
        assert_eq!(
            ranges,
            vec![
                Range {
                    oid: "1".repeat(40),
                    start: 1,
                    end: 2
                },
                Range {
                    oid: "2".repeat(40),
                    start: 3,
                    end: 3
                },
            ]
        );
        let ada = &meta[&"1".repeat(40)];
        assert_eq!(ada.author_email, "ada@example.com");
        assert_eq!(ada.committer_date, "2023-11-14T22:13:20+00:00");
        let bob = &meta[&"2".repeat(40)];
        assert_eq!(bob.summary, "second");
        assert_eq!(bob.committer_date, "2023-11-15T00:23:20+02:00");
    }

    #[test]
    fn timezones_and_junk() {
        assert_eq!(parse_tz("+0530"), Some(19800));
        assert_eq!(parse_tz("-0800"), Some(-28800));
        assert_eq!(parse_tz("nonsense"), None);
        assert_eq!(iso("not a number", "+0000"), "not a number");
    }
}
