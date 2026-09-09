//! Outbound GitHub webhooks (`docs/GITHUB.md` §Webhooks).
//!
//! Two producers, one signed sender:
//!
//! * **Ref events come from the WAL.** [`GithubSink`] is a [`crate::events::Sink`]
//!   registered on the bridge (`crate::bridge`), so every `push` / `create` /
//!   `delete` delivery is rendered from a committed log entry read from a
//!   durable cursor — never from a step of a write (principle III). A delivery
//!   that fails leaves the cursor where it was and the batch is retried.
//! * **`pull_request` deliveries come from the PR handlers**, because PR state
//!   is the facade's own and is not in the WAL. They are best effort with a
//!   couple of retries, always spawned: nothing blocks a request handler.
//!
//! The wire is GitHub's: a JSON body, `x-github-event`, `x-github-delivery`
//! (uuid v4) and `x-hub-signature-256: sha256=<hex HMAC-SHA256 of the raw
//! body>` — the same HMAC helper the walgit-native signature uses.

use std::sync::{Arc, OnceLock, Weak};

use serde_json::{Value, json};
use walgit_git::RepoId;

use super::auth::USER_LOGIN;
use super::models::{self, Urls};
use super::repo::{self, View};
use crate::AppState;
use crate::events::{RefEvent, Sink};

/// GitHub truncates a push's `commits[]`; the consumer reads
/// `commits.len() < size` as "incomplete", which is what we want it to see.
const MAX_COMMITS: usize = 20;

/// Delivery attempts for a `pull_request` event (the WAL-driven ones retry
/// through the cursor instead).
const PR_ATTEMPTS: u32 = 3;

// ---- the sender --------------------------------------------------------------

/// One signed POST to `github.webhook_url`.
pub struct Sender {
    url: String,
    secret: Option<Vec<u8>>,
    client: reqwest::Client,
}

impl Sender {
    /// `None` unless `github.webhook_url` is set.
    pub fn from_cfg(cfg: &walgit_config::Config) -> Option<Sender> {
        let url = cfg.github.webhook_url.clone()?;
        Some(Sender {
            url,
            secret: cfg
                .github
                .webhook_secret
                .clone()
                .filter(|s| !s.is_empty())
                .map(String::into_bytes),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        })
    }

    /// POST one event. Non-2xx and timeouts are errors; the caller decides
    /// whether that fails a batch or is logged and dropped.
    pub async fn deliver(&self, event: &str, payload: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_vec(payload)?;
        let delivery = uuid::Uuid::new_v4().to_string();
        let mut req = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, "GitHub-Hookshot/walgit")
            .header("x-github-event", event)
            .header("x-github-delivery", &delivery);
        if let Some(secret) = &self.secret {
            req = req.header(
                "x-hub-signature-256",
                crate::events::WebhookSink::signature(secret, &body),
            );
        }
        let resp = req.body(body).send().await?;
        let status = resp.status();
        anyhow::ensure!(
            status.is_success(),
            "github webhook {event} ({delivery}) returned {status}"
        );
        tracing::debug!(event, delivery, "github webhook delivered");
        Ok(())
    }
}

/// Fire one `pull_request` (or any handler-side) delivery in the background.
/// Never awaited by a request handler: a slow consumer must not slow a write.
pub fn spawn(st: &Arc<AppState>, event: &'static str, payload: Value) {
    let Some(sender) = Sender::from_cfg(&st.cfg) else {
        return;
    };
    tokio::spawn(async move {
        for attempt in 1..=PR_ATTEMPTS {
            match sender.deliver(event, &payload).await {
                Ok(()) => return,
                Err(e) if attempt == PR_ATTEMPTS => {
                    tracing::warn!(event, error = %e, attempts = attempt,
                        "github webhook delivery failed");
                }
                Err(e) => {
                    tracing::warn!(event, error = %e, attempt, "github webhook delivery failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(200 * u64::from(attempt)))
                        .await;
                }
            }
        }
    });
}

// ---- shared payload pieces ---------------------------------------------------

/// The origins a bridge-rendered payload's URLs are built from. There is no
/// request here, so `server.public_url` is the only source; without it the
/// listener's own address is the best guess.
pub fn urls(cfg: &walgit_config::Config) -> Urls {
    let html = cfg.server.public_url.clone().unwrap_or_else(|| {
        let scheme = if cfg.tls_enabled() { "https" } else { "http" };
        format!("{scheme}://{}", cfg.server.listen)
    });
    let html = html.trim_end_matches('/').to_string();
    let api = format!("{html}/api/v3");
    Urls { html, api }
}

fn installation(cfg: &walgit_config::Config) -> Value {
    json!({ "id": cfg.github.installation_id })
}

fn sender_user(urls: &Urls) -> Value {
    serde_json::to_value(models::named_user(urls, USER_LOGIN)).unwrap_or(Value::Null)
}

fn all_zeros(oid: &str) -> bool {
    !oid.is_empty() && oid.bytes().all(|b| b == b'0')
}

fn short_ref(name: &str) -> &str {
    name.strip_prefix("refs/heads/")
        .or_else(|| name.strip_prefix("refs/tags/"))
        .unwrap_or(name)
}

// ---- commits -----------------------------------------------------------------

/// `added` / `modified` / `removed` for one commit, from `--name-status`
/// against its first parent (`--root` so an initial commit has its whole tree).
async fn paths(local: &walgit_git::LocalRepo, sha: &str) -> anyhow::Result<[Vec<String>; 3]> {
    let out = repo::git(
        local,
        &[
            "log",
            "-1",
            "--format=",
            "--name-status",
            "--first-parent",
            "--root",
            "--no-renames",
            "--end-of-options",
            sha,
        ],
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let (mut added, mut modified, mut removed) = (Vec::new(), Vec::new(), Vec::new());
    for line in String::from_utf8_lossy(&out).lines() {
        let mut f = line.split('\t');
        let Some(status) = f.next().and_then(|s| s.bytes().next()) else {
            continue;
        };
        let Some(path) = f.next().filter(|p| !p.is_empty()) else {
            continue;
        };
        match status {
            b'A' => added.push(path.to_string()),
            b'D' => removed.push(path.to_string()),
            _ => modified.push(path.to_string()),
        }
    }
    Ok([added, modified, removed])
}

/// GitHub's commit shape inside a `push`, `head_commit` included.
async fn commit_json(
    local: &walgit_git::LocalRepo,
    urls: &Urls,
    full_name: &str,
    sha: &str,
) -> anyhow::Result<Value> {
    let facts = repo::commit_facts(local, sha)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let [added, modified, removed] = paths(local, sha).await?;
    Ok(json!({
        "id": facts.sha,
        "tree_id": facts.tree,
        "message": facts.message,
        "timestamp": facts.author_date,
        "url": format!("{}/{full_name}/commit/{}", urls.html, facts.sha),
        "distinct": true,
        "author": {
            "name": facts.author_name,
            "email": facts.author_email,
            "username": USER_LOGIN,
        },
        "committer": {
            "name": facts.committer_name,
            "email": facts.committer_email,
            "username": USER_LOGIN,
        },
        "added": added,
        "modified": modified,
        "removed": removed,
    }))
}

// ---- the WAL-driven sink -----------------------------------------------------

/// The bridge sink that renders each `RefEvent` as GitHub deliveries. It reads
/// the repository to build the payload, so it needs the instance it belongs to
/// — which does not exist yet when `Bridge::new` runs; `attach_state` closes
/// that loop with a `Weak`, so the sink never keeps the state alive.
pub struct GithubSink {
    state: OnceLock<Weak<AppState>>,
    sender: Sender,
}

impl GithubSink {
    pub fn new(sender: Sender) -> Self {
        GithubSink {
            state: OnceLock::new(),
            sender,
        }
    }

    fn state(&self) -> anyhow::Result<Arc<AppState>> {
        self.state
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("github webhook sink has no server state attached"))
    }

    async fn deliver_ref(&self, st: &Arc<AppState>, ev: &RefEvent) -> anyhow::Result<()> {
        if ev.ref_type.is_empty() {
            return Ok(());
        }
        let (owner, name) = ev
            .repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("bad repo id {}", ev.repo))?;
        let id = RepoId::new(owner, name)?;
        let urls = urls(&st.cfg);
        let view = repo::objects_view(st, &id)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let repository = super::prs::repo_json(&view, &urls);

        let created = all_zeros(&ev.old);
        let deleted = all_zeros(&ev.new);
        if ev.ref_type == "branch" {
            let push = self.push_payload(st, &view, &urls, &repository, ev).await?;
            self.sender.deliver("push", &push).await?;
        }
        if created || deleted {
            let event = if created { "create" } else { "delete" };
            let payload = json!({
                "ref": short_ref(&ev.ref_name),
                "ref_type": ev.ref_type,
                "master_branch": default_branch(&view),
                "description": Value::Null,
                "pusher_type": "user",
                "repository": repository,
                "installation": installation(&st.cfg),
                "sender": sender_user(&urls),
            });
            self.sender.deliver(event, &payload).await?;
        }
        if ev.ref_type == "branch" && !created && !deleted {
            self.synchronize_prs(st, &view, &urls, &repository, ev)
                .await?;
        }
        Ok(())
    }

    async fn push_payload(
        &self,
        st: &Arc<AppState>,
        view: &View,
        urls: &Urls,
        repository: &Value,
        ev: &RefEvent,
    ) -> anyhow::Result<Value> {
        let local = &view.local;
        let full = &view.full_name;
        let created = all_zeros(&ev.old);
        let deleted = all_zeros(&ev.new);
        // A create renders the tip only (its history is not "pushed commits"
        // for any consumer here); an update renders the range, newest 20, so
        // `head_commit` is always in `commits[]`.
        let (shas, size, forced) = if deleted {
            (Vec::new(), 0u64, false)
        } else if created {
            (vec![ev.new.clone()], 1u64, false)
        } else {
            let all = repo::commits_between(local, &ev.old, &ev.new)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let size = all.len() as u64;
            let keep = all.len().saturating_sub(MAX_COMMITS);
            let forced = !local.is_ancestor(&ev.old, &ev.new).await.unwrap_or(false);
            (all.into_iter().skip(keep).collect(), size, forced)
        };
        let mut commits = Vec::with_capacity(shas.len());
        for sha in &shas {
            commits.push(commit_json(local, urls, full, sha).await?);
        }
        let head_commit = commits.last().cloned().unwrap_or(Value::Null);
        let pusher = if ev.pusher.is_empty() {
            USER_LOGIN
        } else {
            ev.pusher.as_str()
        };
        Ok(json!({
            "ref": ev.ref_name,
            "before": ev.old,
            "after": ev.new,
            "created": created,
            "deleted": deleted,
            "forced": forced,
            "size": size,
            "base_ref": Value::Null,
            "compare": format!("{}/{full}/compare/{}...{}", urls.html, ev.old, ev.new),
            "repository": repository,
            "installation": installation(&st.cfg),
            // The WAL's principal, not the facade's one user: a consumer that
            // attributes a push wants who actually pushed.
            "pusher": {
                "name": pusher,
                "email": format!("{pusher}@walgit.localhost"),
            },
            "sender": sender_user(urls),
            "head_commit": head_commit,
            "commits": commits,
        }))
    }

    /// A push to an open PR's head branch is a `synchronize`. The index row
    /// carries the head ref, so this is one GET of the index plus one per PR
    /// actually affected.
    async fn synchronize_prs(
        &self,
        st: &Arc<AppState>,
        view: &View,
        urls: &Urls,
        repository: &Value,
        ev: &RefEvent,
    ) -> anyhow::Result<()> {
        let Some(branch) = ev.ref_name.strip_prefix("refs/heads/") else {
            return Ok(());
        };
        let store = view.handle.store().clone();
        let Ok((index, _)) = super::pr_store::read_index(&store).await else {
            return Ok(());
        };
        let numbers: Vec<u64> = index
            .prs
            .iter()
            .filter(|r| r.state == "open" && r.head_ref == branch)
            .map(|r| r.number)
            .collect();
        for number in numbers {
            let Ok(Some(pr)) = super::pr_store::try_read(&store, number).await else {
                continue;
            };
            let mut payload = super::prs::event_payload(
                view,
                urls,
                repository,
                "synchronize",
                &pr,
                &ev.new,
                &installation(&st.cfg),
                &sender_user(urls),
            );
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("before".into(), json!(ev.old));
                obj.insert("after".into(), json!(ev.new));
            }
            self.sender.deliver("pull_request", &payload).await?;
        }
        Ok(())
    }
}

fn default_branch(view: &View) -> String {
    view.index
        .head_target
        .strip_prefix("refs/heads/")
        .unwrap_or("main")
        .to_string()
}

#[async_trait::async_trait]
impl Sink for GithubSink {
    fn name(&self) -> &'static str {
        "github"
    }

    fn attach_state(&self, st: &Arc<AppState>) {
        let _ = self.state.set(Arc::downgrade(st));
    }

    async fn deliver(&self, batch: &[RefEvent]) -> anyhow::Result<()> {
        let st = self.state()?;
        for ev in batch {
            self.deliver_ref(&st, ev).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_oids_and_short_refs() {
        assert!(all_zeros(&"0".repeat(40)));
        assert!(!all_zeros(""));
        assert!(!all_zeros("0000000000000000000000000000000000000001"));
        assert_eq!(short_ref("refs/heads/feature/x"), "feature/x");
        assert_eq!(short_ref("refs/tags/v1"), "v1");
        assert_eq!(short_ref("HEAD"), "HEAD");
    }

    /// The signature a consumer verifies is the walgit-native one over the raw
    /// body, with GitHub's header name.
    #[test]
    fn signature_is_hmac_sha256_of_the_body() {
        let sig = crate::events::WebhookSink::signature(b"secret", b"{\"a\":1}");
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), "sha256=".len() + 64);
        assert_eq!(
            sig,
            crate::events::WebhookSink::signature(b"secret", b"{\"a\":1}")
        );
        assert_ne!(
            sig,
            crate::events::WebhookSink::signature(b"other", b"{\"a\":1}")
        );
    }

    #[test]
    fn urls_prefer_public_url() {
        let mut cfg = walgit_config::Config::default();
        cfg.server.public_url = Some("http://127.0.0.1:8080/".into());
        let u = urls(&cfg);
        assert_eq!(u.html, "http://127.0.0.1:8080");
        assert_eq!(u.api, "http://127.0.0.1:8080/api/v3");
    }
}
