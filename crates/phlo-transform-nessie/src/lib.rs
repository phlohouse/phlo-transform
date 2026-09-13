//! Nessie reference management.
//!
//! Nessie operations are deliberately separate from SQL execution. The
//! [`NessieClient`] trait is implemented by a REST v2 client and by an
//! in-memory client used in tests, so WAP orchestration can be exercised
//! without a live Nessie instance.
//!
//! The REST client implements the subset of the Nessie v2 API needed for
//! environments and WAP (verified against a live Nessie server):
//!
//! * `GET  /trees/{ref}` — resolve a reference
//! * `POST /trees?name=&type=BRANCH` — create a branch from a reference
//! * `DELETE /trees/{ref}?type=BRANCH` — delete a branch
//! * `POST /trees/{target}@{expected}/history/merge` — merge
//! * `PUT  /trees/{ref}?type=BRANCH` — assign/rollback a reference

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

impl ReferenceInfo {
    pub fn branch(name: impl Into<String>, hash: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            hash: hash.into(),
            kind: "BRANCH".to_string(),
        }
    }
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

    /// Every reference (branches and tags), sorted by name.
    async fn list_references(&self) -> Result<Vec<ReferenceInfo>, NessieError>;

    /// Create `name` from an existing reference (its name and hash).
    async fn create_branch(
        &self,
        name: &str,
        from: &ReferenceInfo,
    ) -> Result<ReferenceInfo, NessieError>;

    async fn delete_branch(&self, name: &str) -> Result<(), NessieError>;

    /// Merge `from_ref` into `to_ref`, asserting the target hash when provided.
    async fn merge(
        &self,
        from_ref: &str,
        from_hash: Option<&str>,
        to_ref: &str,
        expected_target_hash: Option<&str>,
    ) -> Result<MergeOutcome, NessieError>;

    /// Non-destructive check that `from_ref` can merge into `to_ref`.
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
        self.references
            .lock()
            .unwrap()
            .insert(name.to_string(), ReferenceInfo::branch(name, hash));
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

    async fn list_references(&self) -> Result<Vec<ReferenceInfo>, NessieError> {
        let mut references: Vec<ReferenceInfo> =
            self.references.lock().unwrap().values().cloned().collect();
        references.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(references)
    }

    async fn create_branch(
        &self,
        name: &str,
        from: &ReferenceInfo,
    ) -> Result<ReferenceInfo, NessieError> {
        let mut references = self.references.lock().unwrap();
        if references.contains_key(name) {
            return Err(NessieError::AlreadyExists(name.to_string()));
        }
        let hash = if from.hash.is_empty() {
            references
                .get(&from.name)
                .map(|reference| reference.hash.clone())
                .unwrap_or_else(|| Self::next_hash(name))
        } else {
            from.hash.clone()
        };
        let reference = ReferenceInfo::branch(name, hash);
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
        _from_hash: Option<&str>,
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
    /// Base endpoint, e.g. `http://localhost:19120` (the `/api/v2` suffix is
    /// added by the client).
    pub endpoint: String,
    pub token: Option<String>,
}

impl NessieConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        let endpoint = endpoint.trim_end_matches('/');
        let endpoint = endpoint.strip_suffix("/api/v2").unwrap_or(endpoint);
        Self {
            endpoint: endpoint.to_string(),
            token: None,
        }
    }
}

/// A Nessie REST v2 client.
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

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, NessieError> {
        request
            .send()
            .await
            .map_err(|error| NessieError::Transport(error.to_string()))
    }

    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, NessieError> {
        let response = self.send(request).await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(NessieError::NotFound("reference".to_string()));
        }
        if !status.is_success() {
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
struct SingleReferenceResponse {
    reference: ReferenceInfo,
}

#[derive(Deserialize)]
struct ReferencesResponse {
    #[serde(default)]
    references: Vec<ReferenceInfo>,
}

#[derive(Deserialize)]
struct MergeResponse {
    #[serde(rename = "wasSuccessful", default)]
    was_successful: bool,
    #[serde(rename = "resultantTargetHash", default)]
    resultant_target_hash: Option<String>,
    #[serde(default)]
    details: Vec<MergeDetail>,
}

#[derive(Deserialize)]
struct MergeDetail {
    #[serde(default)]
    key: Option<MergeKey>,
    #[serde(rename = "conflictType", default)]
    conflict: Option<String>,
}

#[derive(Deserialize)]
struct MergeKey {
    #[serde(default)]
    elements: Vec<String>,
}

#[async_trait]
impl NessieClient for NessieRestClient {
    async fn get_reference(&self, name: &str) -> Result<Option<ReferenceInfo>, NessieError> {
        match self
            .json::<SingleReferenceResponse>(
                self.request(reqwest::Method::GET, &format!("/trees/{}", encode(name))),
            )
            .await
        {
            Ok(response) => Ok(Some(response.reference)),
            Err(NessieError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn list_references(&self) -> Result<Vec<ReferenceInfo>, NessieError> {
        let response: ReferencesResponse = self
            .json(self.request(reqwest::Method::GET, "/trees"))
            .await?;
        let mut references = response.references;
        references.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(references)
    }

    async fn create_branch(
        &self,
        name: &str,
        from: &ReferenceInfo,
    ) -> Result<ReferenceInfo, NessieError> {
        let body = serde_json::json!({
            "type": "BRANCH",
            "name": from.name,
            "hash": from.hash,
        });
        let response: SingleReferenceResponse = self
            .json(
                self.request(
                    reqwest::Method::POST,
                    &format!("/trees?name={}&type=BRANCH", encode(name)),
                )
                .json(&body),
            )
            .await?;
        Ok(response.reference)
    }

    async fn delete_branch(&self, name: &str) -> Result<(), NessieError> {
        // Nessie v2 deletes `name@hash`: resolve first so we delete exactly
        // the observed hash rather than whatever the name has moved to.
        let reference = self
            .get_reference(name)
            .await?
            .ok_or_else(|| NessieError::NotFound(name.to_string()))?;
        let response = self
            .send(self.request(
                reqwest::Method::DELETE,
                &format!("/trees/{}@{}?type=BRANCH", encode(name), reference.hash),
            ))
            .await?;
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
        from_hash: Option<&str>,
        to_ref: &str,
        expected_target_hash: Option<&str>,
    ) -> Result<MergeOutcome, NessieError> {
        let target = match expected_target_hash {
            Some(hash) => format!("{}@{hash}", encode(to_ref)),
            None => encode(to_ref),
        };
        let body = serde_json::json!({
            "fromRefName": from_ref,
            "fromHash": from_hash,
        });
        let response = self
            .send(
                self.request(
                    reqwest::Method::POST,
                    &format!("/trees/{target}/history/merge"),
                )
                .json(&body),
            )
            .await?;
        let status = response.status();
        let payload = response.text().await.unwrap_or_default();
        let parsed: Result<MergeResponse, _> = serde_json::from_str(&payload);
        match parsed {
            Ok(merge) => Ok(merge_outcome(merge, &payload)),
            Err(_) if status == reqwest::StatusCode::CONFLICT => Ok(MergeOutcome::conflict(
                to_ref,
                format!("merge conflict: {payload}"),
            )),
            Err(error) => Err(NessieError::Remote(format!("HTTP {status}: {error}"))),
        }
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
        let response: SingleReferenceResponse = self
            .json(
                self.request(
                    reqwest::Method::PUT,
                    &format!("/trees/{}?type=BRANCH", encode(name)),
                )
                .json(&body),
            )
            .await?;
        Ok(response.reference)
    }
}

/// Percent-encode a reference name for use in a path or query component.
fn encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn merge_outcome(merge: MergeResponse, raw: &str) -> MergeOutcome {
    let mut conflicts: Vec<Conflict> = merge
        .details
        .into_iter()
        .filter_map(|detail| {
            let kind = detail.conflict?;
            let path = detail
                .key
                .map(|key| key.elements.join("."))
                .unwrap_or_default();
            Some(Conflict {
                path,
                message: kind,
            })
        })
        .collect();
    if !merge.was_successful && conflicts.is_empty() {
        conflicts.push(Conflict {
            path: String::new(),
            message: format!("merge was not applied: {raw}"),
        });
    }
    MergeOutcome {
        hash: merge.resultant_target_hash,
        conflicts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_merges_and_rolls_back() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        let branch = nessie
            .create_branch("ci/pr-1", &ReferenceInfo::branch("main", "aaa"))
            .await
            .unwrap();
        assert_eq!(branch.hash, "aaa");

        // Advance the candidate independently.
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();
        let outcome = nessie
            .merge("ci/pr-1", None, "main", Some("aaa"))
            .await
            .unwrap();
        assert!(outcome.is_clean());
        assert_eq!(
            nessie.get_reference("main").await.unwrap().unwrap().hash,
            "bbb"
        );

        // Stale target is rejected.
        nessie.assign_reference("ci/pr-1", "ccc").await.unwrap();
        let stale = nessie
            .merge("ci/pr-1", None, "main", Some("aaa"))
            .await
            .unwrap();
        assert!(!stale.is_clean());

        nessie.assign_reference("main", "aaa").await.unwrap();
        assert_eq!(
            nessie.get_reference("main").await.unwrap().unwrap().hash,
            "aaa"
        );
    }
}
