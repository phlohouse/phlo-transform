//! Nessie reference management.
//!
//! Nessie operations are deliberately separate from SQL execution. The
//! [`NessieClient`] trait is implemented by a REST client and by an in-memory
//! client used in tests, so WAP orchestration can be exercised without a live
//! Nessie instance.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A Nessie reference (branch or tag).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceInfo {
    pub name: String,
    pub hash: String,
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    "BRANCH".to_string()
}

/// A merge conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub path: String,
    pub message: String,
}

/// The outcome of a merge or merge check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
}

impl MergeOutcome {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }

    pub fn clean(hash: impl Into<String>) -> Self {
        Self {
            hash: Some(hash.into()),
            conflicts: Vec::new(),
        }
    }

    pub fn conflict(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            hash: None,
            conflicts: vec![Conflict {
                path: path.into(),
                message: message.into(),
            }],
        }
    }
}

/// Nessie client failures.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum NessieError {
    #[error("reference `{0}` was not found")]
    NotFound(String),
    #[error("reference `{0}` already exists")]
    AlreadyExists(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("nessie error: {0}")]
    Remote(String),
}

/// The Nessie boundary.
#[async_trait]
pub trait NessieClient: Send + Sync {
    async fn get_reference(&self, name: &str) -> Result<Option<ReferenceInfo>, NessieError>;

    async fn create_branch(
        &self,
        name: &str,
        from_hash: Option<&str>,
    ) -> Result<ReferenceInfo, NessieError>;

    async fn delete_branch(&self, name: &str) -> Result<(), NessieError>;

    /// Merge `from_ref` into `to_ref`, optionally asserting the target hash.
    async fn merge(
        &self,
        from_ref: &str,
        to_ref: &str,
        expected_target_hash: Option<&str>,
    ) -> Result<MergeOutcome, NessieError>;

    /// Check whether `from_ref` can merge into `to_ref`.
    async fn can_merge(&self, from_ref: &str, to_ref: &str) -> Result<MergeOutcome, NessieError>;

    /// Move a reference to a specific hash (rollback/reset).
    async fn assign_reference(&self, name: &str, hash: &str) -> Result<ReferenceInfo, NessieError>;
}

/// An in-memory Nessie used for tests and offline planning.
#[derive(Default)]
pub struct InMemoryNessie {
    references: Mutex<BTreeMap<String, ReferenceInfo>>,
}

impl InMemoryNessie {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed(&self, name: &str, hash: &str) -> &Self {
        self.references.lock().unwrap().insert(
            name.to_string(),
            ReferenceInfo {
                name: name.to_string(),
                hash: hash.to_string(),
                kind: "BRANCH".to_string(),
            },
        );
        self
    }

    fn next_hash(seed: &str) -> String {
        // Deterministic placeholder for offline use when no base hash exists.
        format!("{seed}-00000000")
    }
}

#[async_trait]
impl NessieClient for InMemoryNessie {
    async fn get_reference(&self, name: &str) -> Result<Option<ReferenceInfo>, NessieError> {
        Ok(self.references.lock().unwrap().get(name).cloned())
    }

    async fn create_branch(
        &self,
        name: &str,
        from_hash: Option<&str>,
    ) -> Result<ReferenceInfo, NessieError> {
        let mut references = self.references.lock().unwrap();
        if references.contains_key(name) {
            return Err(NessieError::AlreadyExists(name.to_string()));
        }
        let hash = from_hash
            .map(str::to_string)
            .or_else(|| references.get("main").map(|main| main.hash.clone()))
            .unwrap_or_else(|| Self::next_hash(name));
        let reference = ReferenceInfo {
            name: name.to_string(),
            hash,
            kind: "BRANCH".to_string(),
        };
        references.insert(name.to_string(), reference.clone());
        Ok(reference)
    }

    async fn delete_branch(&self, name: &str) -> Result<(), NessieError> {
        self.references
            .lock()
            .unwrap()
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| NessieError::NotFound(name.to_string()))
    }

    async fn merge(
        &self,
        from_ref: &str,
        to_ref: &str,
        expected_target_hash: Option<&str>,
    ) -> Result<MergeOutcome, NessieError> {
        let mut references = self.references.lock().unwrap();
        let source = references
            .get(from_ref)
            .cloned()
            .ok_or_else(|| NessieError::NotFound(from_ref.to_string()))?;
        let target = references
            .get(to_ref)
            .cloned()
            .ok_or_else(|| NessieError::NotFound(to_ref.to_string()))?;
        if let Some(expected) = expected_target_hash {
            if expected != target.hash {
                return Ok(MergeOutcome::conflict(
                    to_ref,
                    format!(
                        "target advanced: expected {expected}, found {}",
                        target.hash
                    ),
                ));
            }
        }
        references.insert(
            to_ref.to_string(),
            ReferenceInfo {
                hash: source.hash.clone(),
                ..target
            },
        );
        Ok(MergeOutcome::clean(source.hash))
    }

    async fn can_merge(&self, from_ref: &str, _to_ref: &str) -> Result<MergeOutcome, NessieError> {
        let references = self.references.lock().unwrap();
        let source = references
            .get(from_ref)
            .ok_or_else(|| NessieError::NotFound(from_ref.to_string()))?;
        Ok(MergeOutcome::clean(source.hash.clone()))
    }

    async fn assign_reference(&self, name: &str, hash: &str) -> Result<ReferenceInfo, NessieError> {
        let mut references = self.references.lock().unwrap();
        let reference = references
            .get_mut(name)
            .ok_or_else(|| NessieError::NotFound(name.to_string()))?;
        reference.hash = hash.to_string();
        Ok(reference.clone())
    }
}

/// Configuration for the Nessie REST client.
#[derive(Clone, Debug)]
pub struct NessieConfig {
    pub endpoint: String,
    pub token: Option<String>,
}

impl NessieConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            token: None,
        }
    }
}

/// A minimal Nessie REST v2 client.
pub struct NessieRestClient {
    client: reqwest::Client,
    config: NessieConfig,
}

impl NessieRestClient {
    pub fn new(config: NessieConfig) -> Result<Self, NessieError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| NessieError::Transport(error.to_string()))?;
        Ok(Self { client, config })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/api/v2{}", self.config.endpoint, path);
        let request = self.client.request(method, url);
        match &self.config.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, NessieError> {
        let response = request
            .send()
            .await
            .map_err(|error| NessieError::Transport(error.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(NessieError::NotFound("reference".to_string()));
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(NessieError::Remote(format!("HTTP {status}: {body}")));
        }
        response
            .json()
            .await
            .map_err(|error| NessieError::Remote(error.to_string()))
    }
}

#[derive(Deserialize)]
struct TreeResponse {
    reference: ReferenceInfo,
}

#[async_trait]
impl NessieClient for NessieRestClient {
    async fn get_reference(&self, name: &str) -> Result<Option<ReferenceInfo>, NessieError> {
        match self
            .json::<TreeResponse>(self.request(reqwest::Method::GET, &format!("/trees/{name}")))
            .await
        {
            Ok(response) => Ok(Some(response.reference)),
            Err(NessieError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn create_branch(
        &self,
        name: &str,
        from_hash: Option<&str>,
    ) -> Result<ReferenceInfo, NessieError> {
        let body = serde_json::json!({
            "type": "BRANCH",
            "name": name,
            "hash": from_hash,
        });
        let response: TreeResponse = self
            .json(self.request(reqwest::Method::POST, "/trees").json(&body))
            .await?;
        Ok(response.reference)
    }

    async fn delete_branch(&self, name: &str) -> Result<(), NessieError> {
        let request = self.request(reqwest::Method::DELETE, &format!("/trees/{name}"));
        let response = request
            .send()
            .await
            .map_err(|error| NessieError::Transport(error.to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(NessieError::Remote(format!(
                "delete returned HTTP {}",
                response.status()
            )))
        }
    }

    async fn merge(
        &self,
        from_ref: &str,
        to_ref: &str,
        expected_target_hash: Option<&str>,
    ) -> Result<MergeOutcome, NessieError> {
        let body = serde_json::json!({
            "fromRefName": from_ref,
            "fromHash": expected_target_hash,
        });
        let value: serde_json::Value = self
            .json(
                self.request(reqwest::Method::POST, &format!("/trees/{to_ref}/merge"))
                    .json(&body),
            )
            .await?;
        Ok(merge_from_value(value))
    }

    async fn can_merge(&self, from_ref: &str, _to_ref: &str) -> Result<MergeOutcome, NessieError> {
        // Non-destructive optimistic check; the real merge reports conflicts.
        let from = self
            .get_reference(from_ref)
            .await?
            .ok_or_else(|| NessieError::NotFound(from_ref.to_string()))?;
        Ok(MergeOutcome::clean(from.hash))
    }

    async fn assign_reference(&self, name: &str, hash: &str) -> Result<ReferenceInfo, NessieError> {
        let body = serde_json::json!({
            "type": "BRANCH",
            "name": name,
            "hash": hash,
        });
        let response: TreeResponse = self
            .json(
                self.request(reqwest::Method::POST, &format!("/trees/{name}"))
                    .json(&body),
            )
            .await?;
        Ok(response.reference)
    }
}

fn merge_from_value(value: serde_json::Value) -> MergeOutcome {
    let hash = value
        .get("hash")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let conflicts = value
        .get("conflicts")
        .and_then(serde_json::Value::as_array)
        .map(|conflicts| {
            conflicts
                .iter()
                .map(|conflict| Conflict {
                    path: conflict
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    message: conflict
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("merge conflict")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    MergeOutcome { hash, conflicts }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_merges_and_rolls_back() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        let branch = nessie.create_branch("ci/pr-1", None).await.unwrap();
        assert_eq!(branch.hash, "aaa");

        // Advance the candidate independently.
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();
        let outcome = nessie.merge("ci/pr-1", "main", Some("aaa")).await.unwrap();
        assert!(outcome.is_clean());
        assert_eq!(
            nessie.get_reference("main").await.unwrap().unwrap().hash,
            "bbb"
        );

        // Stale target is rejected.
        nessie.assign_reference("ci/pr-1", "ccc").await.unwrap();
        let stale = nessie.merge("ci/pr-1", "main", Some("aaa")).await.unwrap();
        assert!(!stale.is_clean());

        nessie.assign_reference("main", "aaa").await.unwrap();
        assert_eq!(
            nessie.get_reference("main").await.unwrap().unwrap().hash,
            "aaa"
        );
    }
}
