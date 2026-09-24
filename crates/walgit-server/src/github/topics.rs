//! Repository topics: `GET`/`PUT /repos/{o}/{r}/topics`, and the list every
//! other surface reads (the REST repository, webhook payloads, GraphQL's
//! `repositoryTopics` and `search … in:topics`).
//!
//! The list lives at `github/topics.json` under the repository's prefix in the
//! bucket, next to `github/protection.json`, and is written the same way: a
//! CAS against the object's current version, so two concurrent PUTs cannot
//! silently lose one.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use walgit_git::RepoId;
use walgit_store::{ObjectStoreExt, PutMode};

use super::error::{FieldError, GhError, GhResult};
use super::repo;
use crate::AppState;

/// GitHub's limits: at most 20 topics, each at most 50 characters.
const MAX_TOPICS: usize = 20;
const MAX_TOPIC_LEN: usize = 50;

/// The request and response body of both routes, and the stored object.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Topics {
    #[serde(default)]
    pub names: Vec<String>,
}

fn topics_key(id: &RepoId) -> String {
    format!("{}github/topics.json", id.store_prefix())
}

/// GitHub's topic rule: lowercase letters, digits and hyphens, starting with a
/// letter or a digit.
fn is_valid_topic(name: &str) -> bool {
    let starts_alphanumeric = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let only_allowed = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    starts_alphanumeric && only_allowed && name.len() <= MAX_TOPIC_LEN
}

/// What GitHub stores for a PUT: uppercase folded to lowercase, repeats
/// dropped, order kept. An invalid name or too many names is a 422.
fn normalize(names: &[String]) -> GhResult<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for raw in names {
        let name = raw.trim().to_lowercase();
        if !is_valid_topic(&name) {
            return Err(GhError::validation(
                "Validation Failed",
                FieldError::invalid(
                    "Repository",
                    "topics",
                    format!(
                        "{raw:?} is not a valid topic. Topics must start with a lowercase letter or number, \
                         consist of {MAX_TOPIC_LEN} characters or less, and can include hyphens."
                    ),
                ),
            ));
        }
        if !out.contains(&name) {
            out.push(name);
        }
    }
    if out.len() > MAX_TOPICS {
        return Err(GhError::validation(
            "Validation Failed",
            FieldError::invalid(
                "Repository",
                "topics",
                format!("A repository cannot have more than {MAX_TOPICS} topics."),
            ),
        ));
    }
    Ok(out)
}

/// The repository's topics. An absent or unparseable object is none: topics
/// are metadata a developer sets, not something a read should fail on.
pub async fn load(st: &Arc<AppState>, id: &RepoId) -> GhResult<Vec<String>> {
    let key = topics_key(id);
    let got = st
        .store
        .get_bytes(&key)
        .await
        .map_err(|e| GhError::Internal(format!("read {key}: {e}")))?;
    let Some((_, bytes)) = got else {
        return Ok(Vec::new());
    };
    Ok(serde_json::from_slice::<Topics>(&bytes)
        .map(|t| t.names)
        .unwrap_or_default())
}

/// `GET /api/v3/repos/{o}/{r}/topics`.
pub async fn get_topics(
    State(st): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    repo::open(&st, &id).await?;
    let names = load(&st, &id).await?;
    Ok(axum::Json(Topics { names }).into_response())
}

/// `PUT /api/v3/repos/{o}/{r}/topics` — replace the list; `{"names": []}`
/// clears it. Answers the stored list, as GitHub does.
pub async fn replace_topics(
    State(st): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    axum::Json(body): axum::Json<Topics>,
) -> GhResult<Response> {
    let id = repo::repo_id(&owner, &name)?;
    repo::open(&st, &id).await?;
    let stored = Topics {
        names: normalize(&body.names)?,
    };
    let key = topics_key(&id);
    let current = st
        .store
        .head(&key)
        .await
        .map_err(|e| GhError::Internal(format!("head {key}: {e}")))?;
    let mode = current.map_or(PutMode::Create, |m| PutMode::Update(m.version));
    let encoded = serde_json::to_vec(&stored)
        .map_err(|e| GhError::Internal(format!("encode topics: {e}")))?;
    st.store
        .put_bytes(&key, encoded, mode)
        .await
        .map_err(|e| match e {
            walgit_store::StoreError::PreconditionFailed { .. } => {
                GhError::Conflict(format!("{key} changed under this write"))
            }
            other => GhError::Internal(format!("write {key}: {other}")),
        })?;
    Ok(axum::Json(stored).into_response())
}

#[cfg(test)]
mod tests {
    use super::{MAX_TOPICS, is_valid_topic, normalize};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn topics_fold_to_lowercase_and_drop_repeats_in_order() {
        let got = normalize(&names(&["Docs", "public-api", "docs", "v2"])).ok();
        assert_eq!(got, Some(names(&["docs", "public-api", "v2"])));
    }

    #[test]
    fn a_topic_follows_githubs_character_rule() {
        assert!(is_valid_topic("docs"));
        assert!(is_valid_topic("2fa"));
        assert!(is_valid_topic("public-api"));
        assert!(!is_valid_topic("-docs"));
        assert!(!is_valid_topic("docs_site"));
        assert!(!is_valid_topic("docs site"));
        assert!(!is_valid_topic(""));
        assert!(!is_valid_topic(&"a".repeat(51)));
    }

    #[test]
    fn an_invalid_name_or_too_many_names_is_rejected() {
        assert!(normalize(&names(&["docs", "not valid"])).is_err());
        let too_many: Vec<String> = (0..=MAX_TOPICS).map(|i| format!("t{i}")).collect();
        assert!(normalize(&too_many).is_err());
    }
}
