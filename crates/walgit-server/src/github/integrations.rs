//! App registrations owned by the facade, durably stored in the bucket.
//! Secrets never appear in API responses or Debug output. The `_dev` API has
//! the facade's existing network trust boundary; it is not authentication.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use walgit_git::RepoId;
use walgit_store::{DynStore, ObjectStoreExt, PutMode, Version};

use super::error::{GhError, GhResult};
use crate::AppState;

const KEY: &str = "github/integrations.json";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Integration {
    pub app_id: u64,
    pub webhook_url: String,
    pub webhook_secret: String,
    pub installations: Vec<Installation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub id: u64,
    pub owner: String,
    pub repository_selection: Selection,
    #[serde(default)]
    pub repositories: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Selection {
    All,
    Selected,
}

impl Installation {
    pub fn matches(&self, id: &RepoId) -> bool {
        self.owner == id.owner()
            && (self.repository_selection == Selection::All
                || self.repositories.iter().any(|name| name == id.name()))
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Registration {
    pub config: Integration,
    /// A new subscription must never inherit a removed subscription's cursor.
    generations: BTreeMap<u64, String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Registry {
    pub integrations: BTreeMap<String, Registration>,
}

pub struct Subscription<'a> {
    pub integration: &'a Integration,
    pub installation: &'a Installation,
    pub generation: &'a str,
}

impl Registry {
    pub fn subscriptions<'a>(&'a self, id: &RepoId) -> GhResult<Vec<Subscription<'a>>> {
        let mut subscriptions = Vec::new();
        for r in self.integrations.values() {
            for i in r.config.installations.iter().filter(|i| i.matches(id)) {
                subscriptions.push(Subscription {
                    integration: &r.config,
                    installation: i,
                    generation: r
                        .generations
                        .get(&i.id)
                        .ok_or_else(|| store_error("missing generation"))?,
                });
            }
        }
        Ok(subscriptions)
    }

    pub fn installation(&self, id: u64) -> GhResult<(&Integration, &Installation)> {
        self.integrations
            .values()
            .find_map(|r| {
                r.config
                    .installations
                    .iter()
                    .find(|i| i.id == id)
                    .map(|i| (&r.config, i))
            })
            .ok_or_else(|| GhError::not_found("installation"))
    }
}

fn store_error<T>(_: T) -> GhError {
    // Registry bodies contain secrets; do not reflect parser/store internals.
    GhError::Internal("could not read or write GitHub integrations".into())
}

pub async fn read(store: &DynStore) -> GhResult<Registry> {
    Ok(read_version(store).await?.0)
}

async fn read_version(store: &DynStore) -> GhResult<(Registry, Option<Version>)> {
    match store.get_bytes(KEY).await.map_err(store_error)? {
        Some((meta, bytes)) => {
            let registry: Registry = serde_json::from_slice(&bytes).map_err(store_error)?;
            let mut generations = BTreeSet::new();
            for (name, registration) in &registry.integrations {
                validate(name, &registration.config, &registry).map_err(store_error)?;
                for installation in &registration.config.installations {
                    if registration
                        .generations
                        .get(&installation.id)
                        .and_then(|g| uuid::Uuid::parse_str(g).ok())
                        .is_none()
                        || !generations.insert(registration.generations.get(&installation.id))
                    {
                        return Err(store_error("invalid subscription generation"));
                    }
                }
            }
            Ok((registry, Some(meta.version)))
        }
        None => Ok((Registry::default(), None)),
    }
}

fn validate(name: &str, cfg: &Integration, registry: &Registry) -> GhResult<()> {
    let invalid = |message: &str| GhError::BadRequest(message.into());
    if name.is_empty()
        || name.len() > 100
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid(
            "integration name must contain 1–100 letters, digits, hyphens or underscores",
        ));
    }
    if cfg.app_id == 0 || cfg.webhook_secret.is_empty() {
        return Err(invalid("app_id and a nonempty webhook_secret are required"));
    }
    let url = reqwest::Url::parse(&cfg.webhook_url)
        .map_err(|_| invalid("webhook_url must be an absolute http(s) URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "webhook_url must be an absolute http(s) URL without credentials or fragment",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut owners = BTreeSet::new();
    for i in &cfg.installations {
        if i.id == 0 || !ids.insert(i.id) || !owners.insert(&i.owner) {
            return Err(invalid(
                "installation IDs must be positive and unique; an app has one installation per owner",
            ));
        }
        RepoId::new(&i.owner, "validation").map_err(|_| invalid("invalid installation owner"))?;
        if i.repository_selection == Selection::All && !i.repositories.is_empty() {
            return Err(invalid(
                "repositories must be empty when repository_selection is all",
            ));
        }
        let mut names = BTreeSet::new();
        for name in &i.repositories {
            RepoId::new(&i.owner, name).map_err(|_| invalid("invalid repository name"))?;
            if !names.insert(name) {
                return Err(invalid("repository names must be unique"));
            }
        }
    }
    for (other_name, other) in &registry.integrations {
        if other_name == name {
            continue;
        }
        if other.config.app_id == cfg.app_id
            || other
                .config
                .installations
                .iter()
                .any(|i| ids.contains(&i.id))
        {
            return Err(GhError::Conflict(
                "app IDs and installation IDs must be unique across integrations".into(),
            ));
        }
    }
    Ok(())
}

/// One atomic registry mutation. Setup is infrequent; CAS retries protect
/// concurrent registrations without adding a registry write to any Git path.
enum Mutation<'a> {
    Put(&'a str, &'a Integration),
    Delete(&'a str),
    RemoveInstallation(u64, u64),
}

async fn mutate(store: &DynStore, mutation: Mutation<'_>) -> GhResult<()> {
    for attempt in 0..8u64 {
        let (mut registry, version) = read_version(store).await?;
        match mutation {
            Mutation::Put(name, cfg) => {
                validate(name, cfg, &registry)?;
                let old = registry.integrations.get(name);
                let generations = cfg
                    .installations
                    .iter()
                    .map(|i| {
                        let prior = old
                            .filter(|r| r.config.app_id == cfg.app_id)
                            .filter(|r| r.config.installations.iter().any(|previous| previous == i))
                            .and_then(|r| r.generations.get(&i.id))
                            .cloned();
                        (
                            i.id,
                            prior.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        )
                    })
                    .collect();
                registry.integrations.insert(
                    name.into(),
                    Registration {
                        config: cfg.clone(),
                        generations,
                    },
                );
            }
            Mutation::Delete(name) => {
                if registry.integrations.remove(name).is_none() {
                    return Err(GhError::not_found("integration"));
                }
            }
            Mutation::RemoveInstallation(app_id, id) => {
                let registration = registry
                    .integrations
                    .values_mut()
                    .find(|r| r.config.app_id == app_id)
                    .ok_or_else(|| GhError::not_found("installation"))?;
                if !registration.config.installations.iter().any(|i| i.id == id) {
                    return Err(GhError::not_found("installation"));
                }
                registration.config.installations.retain(|i| i.id != id);
                registration.generations.remove(&id);
            }
        }
        let mode = version.map_or(PutMode::Create, PutMode::Update);
        let body = serde_json::to_vec(&registry).map_err(store_error)?;
        match store.put_bytes(KEY, body, mode).await {
            Ok(_) => return Ok(()),
            Err(e) if e.is_precondition_failed() => {
                tokio::time::sleep(std::time::Duration::from_millis(
                    5 * (attempt + 1) + u64::from(rand::random::<u8>() % 10),
                ))
                .await;
            }
            Err(e) => return Err(store_error(e)),
        }
    }
    Err(GhError::Conflict(
        "GitHub integrations are being updated concurrently".into(),
    ))
}

fn public_config(name: &str, cfg: &Integration) -> Value {
    json!({"name": name, "app_id": cfg.app_id, "webhook_url": cfg.webhook_url,
        "webhook_secret_configured": !cfg.webhook_secret.is_empty(), "installations": cfg.installations})
}

pub async fn list(State(st): State<Arc<AppState>>) -> GhResult<Response> {
    let registry = read(st.registry.store()).await?;
    Ok(axum::Json(
        registry
            .integrations
            .iter()
            .map(|(name, r)| public_config(name, &r.config))
            .collect::<Vec<_>>(),
    )
    .into_response())
}

pub async fn get(State(st): State<Arc<AppState>>, Path(name): Path<String>) -> GhResult<Response> {
    let registry = read(st.registry.store()).await?;
    let r = registry
        .integrations
        .get(&name)
        .ok_or_else(|| GhError::not_found("integration"))?;
    Ok(axum::Json(public_config(&name, &r.config)).into_response())
}

pub async fn put(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    axum::Json(cfg): axum::Json<Integration>,
) -> GhResult<Response> {
    mutate(st.registry.store(), Mutation::Put(&name, &cfg)).await?;
    Ok(axum::Json(public_config(&name, &cfg)).into_response())
}

pub async fn delete(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> GhResult<StatusCode> {
    mutate(st.registry.store(), Mutation::Delete(&name)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// JWT issuer selects an app. It is deliberately NOT signature verification:
/// this private development facade remains auth-free. Missing/invalid issuer
/// is an error, never an implicit choice of another integration.
pub fn app_id(headers: &HeaderMap) -> GhResult<u64> {
    let token = bearer(headers).ok_or_else(|| {
        GhError::BadRequest("an App JWT with a registered numeric iss is required".into())
    })?;
    let claims = token
        .split('.')
        .nth(1)
        .and_then(|p| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(p)
                .ok()
        })
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    claims
        .and_then(|v| {
            let issuer = v.get("iss")?;
            issuer.as_u64().or_else(|| issuer.as_str()?.parse().ok())
        })
        .ok_or_else(|| {
            GhError::BadRequest("an App JWT with a registered numeric iss is required".into())
        })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token")).then_some(token)
}

pub fn token_installation(headers: &HeaderMap) -> GhResult<u64> {
    bearer(headers)
        .and_then(|t| t.strip_prefix("ghs_walgit_"))
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| {
            GhError::BadRequest(
                "an installation token returned by access_tokens is required".into(),
            )
        })
}

pub async fn remove_installation(store: &DynStore, app_id: u64, id: u64) -> GhResult<()> {
    mutate(store, Mutation::RemoveInstallation(app_id, id)).await
}
