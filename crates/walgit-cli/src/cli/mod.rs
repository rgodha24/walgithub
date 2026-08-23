//! Shared CLI helpers: repo-id parsing, printing.

use std::sync::Arc;

use walgit_config::Config;

/// Parse `owner/name` or `owner/name.git` into a validated `(owner, name)` pair.
/// Rules from D5: each part ASCII `[A-Za-z0-9._-]`, no leading `.`, not `..`,
/// 1..=100 chars.
pub fn parse_repo_id(s: &str) -> anyhow::Result<(String, String)> {
    let s = s.trim_end_matches(".git");
    let (owner, name) = s
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("repo id must be `owner/name`, got `{s}`"))?;

    validate_part(owner, "owner")?;
    validate_part(name, "name")?;
    Ok((owner.to_string(), name.to_string()))
}

fn validate_part(part: &str, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!part.is_empty(), "{label} is empty");
    anyhow::ensure!(part.len() <= 100, "{label} exceeds 100 chars");
    anyhow::ensure!(part != "..", "{label} is `..`");
    anyhow::ensure!(!part.starts_with('.'), "{label} must not start with a dot");
    for ch in part.chars() {
        anyhow::ensure!(
            ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'),
            "{label} contains invalid character `{ch}` (allowed: ASCII alphanumeric, . - _)"
        );
    }
    Ok(())
}

/// Print a labeled key-value line to stdout.
pub fn println_kv(key: &str, value: impl std::fmt::Display) {
    println!("{key:<20} {value}");
}

/// Convenience to build an `Arc<Config>`.
#[allow(dead_code)]
pub fn arc_cfg(cfg: Config) -> Arc<Config> {
    Arc::new(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_valid() {
        assert_eq!(
            parse_repo_id("acme/monorepo").unwrap(),
            ("acme".into(), "monorepo".into())
        );
        assert_eq!(
            parse_repo_id("acme/monorepo.git").unwrap(),
            ("acme".into(), "monorepo".into())
        );
        assert_eq!(
            parse_repo_id("a.b-c_d/1.2-3_4").unwrap(),
            ("a.b-c_d".into(), "1.2-3_4".into())
        );
    }

    #[test]
    fn repo_id_invalid() {
        assert!(parse_repo_id("noway").is_err());
        assert!(parse_repo_id("/name").is_err());
        assert!(parse_repo_id("owner/").is_err());
        assert!(parse_repo_id("../etc").is_err());
        assert!(parse_repo_id(".hidden/repo").is_err());
        assert!(parse_repo_id("owner/näme").is_err());
    }
}
