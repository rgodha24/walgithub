//! The mutation side: `createCommitOnBranch` and the three pull-request
//! mutations.
//!
//! `createCommitOnBranch` is the editor's write path — it is how every page
//! the client saves reaches a branch — and it goes through
//! [`crate::github::write::commit_on_ref`], so it is walgit's publish path and
//! nothing else. The pull-request mutations only move state that lives in the
//! bucket as JSON ([`crate::github::pr_store`]) — the one store the REST
//! pulls endpoints write, so a review opened over REST is addressable here.

use base64::Engine;
use bytes::Bytes;
use serde_json::{Value, json};

use super::error::GqlError;
use super::ops::{self, Ctx};
use super::parse::Field;
use crate::github::auth::USER_LOGIN;
use crate::github::models::node_id;
use crate::github::pr_store::{self, NodeId, PullRequest, ReviewThread};
use crate::github::write;

/// The author and committer of every commit the facade builds. The facade has
/// one user (`docs/GITHUB.md` §1) and a commit must not claim to be anybody
/// else's.
const AUTHOR_EMAIL: &str = "mintlify-dev@localhost";

pub async fn mutation(ctx: &Ctx, label: &str, fields: &[Field]) -> Result<Value, GqlError> {
    let mut data = serde_json::Map::new();
    for f in fields {
        let value = match f.name.as_str() {
            "createCommitOnBranch" => create_commit_on_branch(ctx, f).await?,
            "markPullRequestReadyForReview" => set_draft(ctx, f, false).await?,
            "convertPullRequestToDraft" => set_draft(ctx, f, true).await?,
            "addPullRequestReviewThread" => add_review_thread(ctx, f).await?,
            other => return Err(GqlError::not_implemented(format!("{label}.{other}"))),
        };
        data.insert(f.name.clone(), value);
    }
    Ok(Value::Object(data))
}

// ---- createCommitOnBranch ----------------------------------------------------

fn input_str(input: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `createCommitOnBranch(input:)`. Additions win over deletions on one path,
/// which is GitHub's own rule and the only ordering that makes a rename
/// (delete old, add new) safe when the two paths collide.
async fn create_commit_on_branch(ctx: &Ctx, f: &Field) -> Result<Value, GqlError> {
    let input = f.input();
    let branch = input
        .get("branch")
        .and_then(Value::as_object)
        .ok_or_else(|| GqlError::bad_request("createCommitOnBranch: input.branch is required"))?;
    let full_name = input_str(branch, "repositoryNameWithOwner").ok_or_else(|| {
        GqlError::bad_request("createCommitOnBranch: branch.repositoryNameWithOwner is required")
    })?;
    let branch_name = input_str(branch, "branchName").ok_or_else(|| {
        GqlError::bad_request("createCommitOnBranch: branch.branchName is required")
    })?;
    let (owner, name) = full_name.split_once('/').ok_or_else(|| {
        GqlError::not_found(format!(
            "Could not resolve to a Repository with the name '{full_name}'."
        ))
    })?;
    let id = ctx.repo_id(owner, name)?;

    let message = input
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| GqlError::bad_request("createCommitOnBranch: input.message is required"))?;
    let headline = input_str(message, "headline").unwrap_or_default();
    let body = input_str(message, "body").unwrap_or_default();
    let message = if body.trim().is_empty() {
        headline
    } else {
        format!("{headline}\n\n{body}")
    };

    let changes = file_changes(input.get("fileChanges"))?;

    let view = ctx.objects(&id).await?;
    let ref_name = format!("refs/heads/{branch_name}");
    let head = view
        .index
        .branch(&branch_name)
        .map(str::to_string)
        .ok_or_else(|| {
            GqlError::not_found(format!(
                "Could not resolve to a Ref with the name '{ref_name}'."
            ))
        })?;
    if let Some(expected) = input_str(input, "expectedHeadOid")
        && expected != head
    {
        return Err(GqlError::unprocessable(format!(
            "Expected branch to point to \"{expected}\" but it did not. It points to \"{head}\"."
        ))
        .at(["createCommitOnBranch"]));
    }

    let author = write::Signature::new(USER_LOGIN, AUTHOR_EMAIL);
    let written = write::commit_on_ref(
        &ctx.st,
        &id,
        write::CommitOnRef {
            ref_name: ref_name.clone(),
            base: Some(head.clone()),
            expected_head: Some(head),
            changes,
            message,
            author: author.clone(),
            committer: author,
        },
    )
    .await?;

    let view = ctx.objects(&id).await?;
    let facts = ops::commit_facts(&view, &written.oid).await?;
    Ok(json!({
        "commit": ops::commit_node(&ctx.urls, &view.full_name, &facts),
        "ref": {
            "id": node_id("Ref", &format!("{}:{ref_name}", view.full_name)),
            "name": branch_name,
            "prefix": "refs/heads/",
            "target": { "oid": written.oid },
        },
        "clientMutationId": input.get("clientMutationId").cloned().unwrap_or(Value::Null),
    }))
}

/// `fileChanges: {additions: [{path, contents}], deletions: [{path}]}`.
/// `contents` is base64 — GitHub's API takes nothing else — and may carry the
/// line breaks a client's encoder inserts.
fn file_changes(changes: Option<&Value>) -> Result<Vec<write::Change>, GqlError> {
    let Some(changes) = changes.and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let list = |key: &str| -> Vec<&Value> {
        changes
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    };
    let additions = list("additions");
    let added: std::collections::HashSet<&str> = additions
        .iter()
        .filter_map(|a| a.get("path").and_then(Value::as_str))
        .collect();
    let mut out = Vec::new();
    for deletion in list("deletions") {
        let path = deletion
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| GqlError::bad_request("fileChanges.deletions[].path is required"))?;
        if added.contains(path) {
            continue;
        }
        out.push(write::Change::delete(path));
    }
    for addition in additions {
        let path = addition
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| GqlError::bad_request("fileChanges.additions[].path is required"))?;
        let encoded = addition
            .get("contents")
            .and_then(Value::as_str)
            .ok_or_else(|| GqlError::bad_request("fileChanges.additions[].contents is required"))?;
        let stripped: String = encoded
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(stripped)
            .map_err(|e| {
                GqlError::unprocessable(format!("{path}: contents is not valid base64: {e}"))
            })?;
        out.push(write::Change::put(path, Bytes::from(bytes)));
    }
    Ok(out)
}

// ---- pull requests -----------------------------------------------------------

fn pull_request_node(ctx: &Ctx, pr: &PullRequest) -> Value {
    json!({
        "id": pr.node_id,
        "number": pr.number,
        "title": pr.title,
        "body": pr.body,
        "isDraft": pr.draft,
        "state": if pr.merged {
            "MERGED"
        } else if pr.state == "closed" {
            "CLOSED"
        } else {
            "OPEN"
        },
        "url": pr.html_url,
        "baseRefName": pr.base.ref_name,
        "headRefName": pr.head.ref_name,
        "author": { "login": pr.user, "url": format!("{}/{}", ctx.urls.html, pr.user) },
        "updatedAt": pr.updated_at,
    })
}

/// `markPullRequestReadyForReview` (`draft = false`) and
/// `convertPullRequestToDraft` (`draft = true`). Response unused by the
/// client; the state change is the point.
async fn set_draft(ctx: &Ctx, f: &Field, draft: bool) -> Result<Value, GqlError> {
    let input = f.input();
    let node = input_str(input, "pullRequestId")
        .ok_or_else(|| GqlError::bad_request("input.pullRequestId is required"))?;
    let parsed = NodeId::parse(&node).ok_or_else(|| {
        GqlError::not_found(format!(
            "Could not resolve to a PullRequest with the id {node}."
        ))
    })?;
    let (id, number) = parsed.target();
    let view = ctx.refs(id).await?;
    let store = view.handle.store().clone();
    let pr = pr_store::set_draft(&store, number, draft).await?;
    let action = if draft {
        "converted_to_draft"
    } else {
        "ready_for_review"
    };
    crate::github::prs::emit_for(&ctx.st, &view, &ctx.urls, action, &pr);
    Ok(json!({
        "pullRequest": pull_request_node(ctx, &pr),
        "clientMutationId": input.get("clientMutationId").cloned().unwrap_or(Value::Null),
    }))
}

/// `addPullRequestReviewThread(input:)`. The client addresses it by
/// `pullRequestReviewId` — the `node_id` of the pending review it just opened
/// over REST — and reads back `thread.id`, throwing when it is null
/// (`docs/GITHUB_SHAPES.md` §9). `pullRequestId` is accepted too, since the
/// same mutation takes either on GitHub.
async fn add_review_thread(ctx: &Ctx, f: &Field) -> Result<Value, GqlError> {
    let input = f.input();
    let node = input_str(input, "pullRequestReviewId")
        .or_else(|| input_str(input, "pullRequestId"))
        .ok_or_else(|| GqlError::bad_request("input.pullRequestReviewId is required"))?;
    let parsed = NodeId::parse(&node).ok_or_else(|| {
        GqlError::not_found(format!("Could not resolve to a node with the id {node}."))
    })?;
    let (id, number) = parsed.target();
    let review = match &parsed {
        NodeId::Review { review, .. } => Some(review.clone()),
        NodeId::PullRequest { .. } | NodeId::ReviewThread { .. } => None,
    };
    let path =
        input_str(input, "path").ok_or_else(|| GqlError::bad_request("input.path is required"))?;
    let thread = ReviewThread {
        id: String::new(),
        review_id: review,
        path,
        line: input.get("line").and_then(Value::as_u64),
        start_line: input.get("startLine").and_then(Value::as_u64),
        side: input_str(input, "side"),
        subject_type: input_str(input, "subjectType"),
        body: input_str(input, "body").unwrap_or_default(),
        created_at: pr_store::now(),
    };
    let view = ctx.refs(id).await?;
    let store = view.handle.store().clone();
    let stored = pr_store::add_review_thread(&store, &view.full_name, number, thread).await?;
    Ok(json!({
        "thread": {
            "id": stored.id,
            "path": stored.path,
            "line": stored.line,
            "startLine": stored.start_line,
            "isResolved": false,
            "isOutdated": false,
            "comments": {
                "nodes": [{
                    "body": stored.body,
                    "createdAt": stored.created_at,
                    "author": {
                        "login": USER_LOGIN,
                        "url": format!("{}/{USER_LOGIN}", ctx.urls.html),
                    },
                }],
            },
        },
        "clientMutationId": input.get("clientMutationId").cloned().unwrap_or(Value::Null),
    }))
}
