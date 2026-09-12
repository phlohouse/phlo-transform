//! The unified model selector engine.
//!
//! One selector language is shared by `plan`, `apply`, `run`, `test`,
//! `lineage`, `impact` and `list`. There is exactly one parser and one
//! resolution implementation; commands only differ in what they do with the
//! resolved [`Selection`].
//!
//! Grammar, per term:
//!
//! ```text
//! term    := "+"? body "+"?
//! body    := "tag:" value            model carries the tag
//!         |  "namespace:" value      model's namespace (first name segment)
//!         |  "source:" value         model reads a matching source
//!         |  "changed"               desired version differs from state
//!         |  "all" | "*"             every model
//!         |  pattern                 name, `model://` URI, `prefix.*` glob,
//!                                  or a unique name suffix
//! ```
//!
//! A leading `+` adds every transitive model dependency of the matched set;
//! a trailing `+` adds every transitive dependent. Both may be combined
//! (`+assay.results+`).
//!
//! Include terms union; `--exclude` terms subtract (applied last, with their
//! own `+` expansion); `--tag`/`--workflow` intersect the included set.
//! Selection is deterministic: members are returned in topological order.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::compiled::Compilation;
use crate::graph::Dependency;
use crate::identity::ModelId;

/// What a selector term's `body` matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectorKind {
    /// Model name (`assay.results`), `model://` URI, `prefix.*` glob, or a
    /// unique `local`/`last-segment` suffix.
    Name(String),
    /// `-- @tags` membership.
    Tag(String),
    /// The model's namespace (first segment of its logical name).
    Namespace(String),
    /// The model reads a source matching the value: exact logical name,
    /// a `value.` prefix, or any single part.
    Source(String),
    /// Desired version differs from the recorded materialised version.
    Changed,
    /// Every model in the workspace (`*` or `all`).
    All,
}

/// One parsed selector term such as `+assay.results` or `tag:qc+`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorTerm {
    /// Include every transitive model dependency (leading `+`).
    pub upstream: bool,
    /// Include every transitive dependent (trailing `+`).
    pub downstream: bool,
    pub kind: SelectorKind,
    /// The term exactly as written, for reporting.
    pub text: String,
}

impl fmt::Display for SelectorTerm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

/// The selector sets a command resolves. Built once per invocation from
/// positional selectors, `--select`, `--exclude`, `--tag`, `--workflow`,
/// `--changed`, `--upstream` and `--downstream`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectorSet {
    /// Terms unioned into the selection.
    pub include: Vec<SelectorTerm>,
    /// Terms subtracted from the resolved selection (applied last).
    pub exclude: Vec<SelectorTerm>,
    /// Terms intersected with the included base before expansion flags.
    /// Used by the orthogonal `--tag`/`--workflow` filters.
    pub filter: Vec<SelectorTerm>,
    /// Expand the filtered include-set through every transitive dependency.
    pub expand_upstream: bool,
    /// Expand the filtered include-set through every transitive dependent.
    pub expand_downstream: bool,
}

impl SelectorSet {
    /// Parse raw CLI terms into a `SelectorSet`.
    pub fn parse(
        include: &[String],
        exclude: &[String],
        filter: &[String],
        expand_upstream: bool,
        expand_downstream: bool,
    ) -> Result<Self, SelectorError> {
        Ok(Self {
            include: parse_terms(include)?,
            exclude: parse_terms(exclude)?,
            filter: parse_terms(filter)?,
            expand_upstream,
            expand_downstream,
        })
    }

    /// True when nothing constrains the selection.
    pub fn is_unrestricted(&self) -> bool {
        self.include.is_empty()
            && self.exclude.is_empty()
            && self.filter.is_empty()
            && !self.expand_upstream
            && !self.expand_downstream
    }

    /// Whether any term anywhere uses the `changed` predicate, which needs
    /// a resolved change set to evaluate.
    pub fn uses_changed(&self) -> bool {
        self.include
            .iter()
            .chain(self.exclude.iter())
            .chain(self.filter.iter())
            .any(|term| term.kind == SelectorKind::Changed)
    }
}

fn parse_terms(raw: &[String]) -> Result<Vec<SelectorTerm>, SelectorError> {
    raw.iter()
        .map(|text| parse_selector(text))
        .collect::<Result<_, _>>()
}

/// Selector parse/resolution failures. Messages are written for the CLI.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SelectorError {
    #[error("empty selector")]
    Empty,
    #[error("invalid selector `{term}`: {message}")]
    Invalid { term: String, message: String },
    #[error("selector `{term}` matched no models{suggestions}")]
    NoMatches { term: String, suggestions: String },
    #[error("selector `{term}` is ambiguous\n{candidates}")]
    Ambiguous { term: String, candidates: String },
    #[error("the `changed` selector needs recorded state; no change set was supplied")]
    ChangedUnavailable,
}

/// Parse one selector term.
///
/// `+` is only valid as the first and/or last character; `+` anywhere else
/// is an error so `a+b` is never silently misparsed.
pub fn parse_selector(text: &str) -> Result<SelectorTerm, SelectorError> {
    let original = text.to_string();
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(SelectorError::Empty);
    }

    let upstream = rest.starts_with('+');
    if upstream {
        rest = &rest[1..];
    }
    let downstream = rest.ends_with('+');
    if downstream {
        rest = &rest[..rest.len() - 1];
    }
    if rest.contains('+') {
        return Err(SelectorError::Invalid {
            term: original,
            message: "`+` is only valid at the start and/or end of a selector".to_string(),
        });
    }
    if rest.is_empty() {
        return Err(SelectorError::Invalid {
            term: original,
            message: "missing selector body".to_string(),
        });
    }

    let kind = match rest.split_once(':') {
        // `model://...` is a URI, not a `kind:` term.
        Some((kind, value)) if !value.starts_with("//") => {
            if value.is_empty() {
                return Err(SelectorError::Invalid {
                    term: original.clone(),
                    message: format!("`{kind}:` needs a value, e.g. `{kind}:qc`"),
                });
            }
            match kind {
                "tag" => SelectorKind::Tag(value.to_string()),
                "namespace" => SelectorKind::Namespace(value.to_string()),
                "source" => SelectorKind::Source(value.to_string()),
                other => {
                    return Err(SelectorError::Invalid {
                        term: original,
                        message: format!(
                            "unknown selector kind `{other}:` (expected tag:, namespace:, source: or changed)"
                        ),
                    });
                }
            }
        }
        _ => match rest {
            "changed" => SelectorKind::Changed,
            "all" | "*" => SelectorKind::All,
            name => SelectorKind::Name(name.to_string()),
        },
    };

    Ok(SelectorTerm {
        upstream,
        downstream,
        kind,
        text: original,
    })
}

/// A model in a resolved selection, with the terms responsible for it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SelectedModel {
    /// Logical model name, e.g. `assay.results`.
    pub id: String,
    /// Include terms whose base match contained the model.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched: Vec<String>,
    /// Include terms that pulled the model in through `+` expansion.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expanded: Vec<String>,
}

impl SelectedModel {
    /// The model matched at least one term directly rather than only via
    /// graph expansion.
    pub fn is_direct(&self) -> bool {
        !self.matched.is_empty()
    }
}

/// Why a `changed`-style term selected a model — caller-supplied
/// provenance, e.g. the Git provider's per-model cause. Selection records
/// *how a model got here*; plan reasons still decide *why it builds*.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SelectionCause {
    /// The workspace-relative path responsible for the change.
    pub path: String,
    /// A complete phrase, e.g. `transforms/assay/raw.sql modified since
    /// main` or `consumes seed raw.events, which changed since main`.
    pub detail: String,
}

/// The resolved selection: which models are in, and why.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Selection {
    /// Selected models in topological order where the graph allows it.
    pub members: Vec<SelectedModel>,
    /// Models removed by exclude terms (in their expansion), sorted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<String>,
    /// The include terms as written.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<String>,
    /// The exclude terms as written.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_terms: Vec<String>,
    /// Per-model change provenance supplied by the caller (the Git change
    /// provider under `--since`), keyed by logical model name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub causes: BTreeMap<String, Vec<SelectionCause>>,
}

impl Selection {
    /// Every model in the workspace, as an unqualified selection.
    pub fn all(compilation: &Compilation) -> Self {
        Self {
            members: ordered_members(
                compilation,
                compilation
                    .models
                    .iter()
                    .map(|model| SelectedModel {
                        id: model.id.logical_name(),
                        matched: vec!["*".to_string()],
                        expanded: Vec::new(),
                    })
                    .collect(),
            ),
            excluded: Vec::new(),
            terms: Vec::new(),
            exclude_terms: Vec::new(),
            causes: BTreeMap::new(),
        }
    }

    /// A selection containing exactly `ids`, each marked as directly matched.
    /// Used by callers that already know the model set (e.g. `explain`).
    pub fn of(compilation: &Compilation, ids: &[ModelId]) -> Self {
        let members = ids
            .iter()
            .map(|id| SelectedModel {
                id: id.logical_name(),
                matched: vec![id.logical_name()],
                expanded: Vec::new(),
            })
            .collect();
        Self {
            members: ordered_members(compilation, members),
            excluded: Vec::new(),
            terms: Vec::new(),
            exclude_terms: Vec::new(),
            causes: BTreeMap::new(),
        }
    }

    /// The selected model ids.
    pub fn ids(&self) -> Vec<ModelId> {
        self.members
            .iter()
            .filter_map(|member| ModelId::parse(&member.id).ok())
            .collect()
    }

    /// Look up a member by model id.
    pub fn get(&self, id: &ModelId) -> Option<&SelectedModel> {
        self.members
            .iter()
            .find(|member| member.id == id.logical_name())
    }

    /// True when `id` is part of the selection.
    pub fn contains(&self, id: &ModelId) -> bool {
        self.get(id).is_some()
    }

    /// The set of models removed by `--exclude` terms.
    pub fn excluded_ids(&self) -> BTreeSet<ModelId> {
        self.excluded
            .iter()
            .filter_map(|name| ModelId::parse(name).ok())
            .collect()
    }
}

/// Resolve a `SelectorSet` against the compiled workspace.
///
/// `changed` supplies the set of models whose desired version differs from
/// the recorded materialised version for the target environment; it is
/// required only when a `changed` term is used (the caller decides how the
/// set is computed — state comparison today, Git diff in a later batch).
pub fn resolve_selection(
    compilation: &Compilation,
    set: &SelectorSet,
    changed: Option<&BTreeSet<ModelId>>,
) -> Result<Selection, SelectorError> {
    // Origins per selected model: which include terms matched it directly
    // and which pulled it in through expansion.
    let mut matched_by: BTreeMap<ModelId, Vec<String>> = BTreeMap::new();
    let mut expanded_by: BTreeMap<ModelId, Vec<String>> = BTreeMap::new();

    if set.include.is_empty() {
        for model in &compilation.models {
            matched_by
                .entry(model.id.clone())
                .or_default()
                .push("*".to_string());
        }
    }

    for term in &set.include {
        let base = base_match(compilation, term, changed)?;
        for id in &base {
            matched_by
                .entry(id.clone())
                .or_default()
                .push(term.text.clone());
        }
        expand_term(compilation, term, &base, &mut expanded_by);
    }

    // Intersection filters (--tag / --workflow) narrow the included set.
    // An absent include list means "all models", so filters alone suffice.
    if !set.filter.is_empty() {
        let mut keep: Option<BTreeSet<ModelId>> = None;
        for term in &set.filter {
            let filter_set = base_match(compilation, term, changed)?;
            keep = Some(match keep {
                None => filter_set,
                Some(keep) => keep.intersection(&filter_set).cloned().collect(),
            });
        }
        if let Some(keep) = keep {
            matched_by.retain(|id, _| keep.contains(id));
            expanded_by.retain(|id, _| keep.contains(id));
        }
    }

    // The legacy --upstream/--downstream flags expand the filtered base.
    if set.expand_upstream {
        let seed: BTreeSet<ModelId> = matched_by.keys().cloned().collect();
        let term_text = "--upstream".to_string();
        for id in expand(compilation, &seed, true) {
            if !seed.contains(&id) {
                expanded_by.entry(id).or_default().push(term_text.clone());
            }
        }
    }
    if set.expand_downstream {
        let seed: BTreeSet<ModelId> = matched_by.keys().cloned().collect();
        let term_text = "--downstream".to_string();
        for id in expand(compilation, &seed, false) {
            if !seed.contains(&id) {
                expanded_by.entry(id).or_default().push(term_text.clone());
            }
        }
    }

    // Exclusions apply last and are absolute: an excluded model stays out
    // even if another term's expansion would have pulled it in.
    let mut excluded: BTreeSet<ModelId> = BTreeSet::new();
    for term in &set.exclude {
        let base = base_match(compilation, term, changed)?;
        let mut removed = base;
        if term.upstream {
            removed.extend(expand(compilation, &removed.clone(), true));
        }
        if term.downstream {
            removed.extend(expand(compilation, &removed.clone(), false));
        }
        excluded.extend(removed);
    }
    matched_by.retain(|id, _| !excluded.contains(id));
    expanded_by.retain(|id, _| !excluded.contains(id));

    let mut members: Vec<SelectedModel> = Vec::new();
    for (id, matched) in &matched_by {
        members.push(SelectedModel {
            id: id.logical_name(),
            matched: matched.clone(),
            expanded: expanded_by.get(id).cloned().unwrap_or_default(),
        });
    }
    for (id, expanded) in expanded_by {
        if matched_by.contains_key(&id) {
            continue;
        }
        members.push(SelectedModel {
            id: id.logical_name(),
            matched: Vec::new(),
            expanded,
        });
    }

    Ok(Selection {
        members: ordered_members(compilation, members),
        excluded: excluded
            .iter()
            .map(|id| id.logical_name())
            .collect::<Vec<_>>(),
        terms: set.include.iter().map(|term| term.text.clone()).collect(),
        exclude_terms: set.exclude.iter().map(|term| term.text.clone()).collect(),
        causes: BTreeMap::new(),
    })
}

/// Apply a term's own `+` expansion, recording origins.
fn expand_term(
    compilation: &Compilation,
    term: &SelectorTerm,
    base: &BTreeSet<ModelId>,
    expanded_by: &mut BTreeMap<ModelId, Vec<String>>,
) {
    if term.upstream {
        for id in expand(compilation, base, true) {
            if !base.contains(&id) {
                expanded_by.entry(id).or_default().push(term.text.clone());
            }
        }
    }
    if term.downstream {
        for id in expand(compilation, base, false) {
            if !base.contains(&id) {
                expanded_by.entry(id).or_default().push(term.text.clone());
            }
        }
    }
}

/// The set of models a term's body matches, before `+` expansion.
fn base_match(
    compilation: &Compilation,
    term: &SelectorTerm,
    changed: Option<&BTreeSet<ModelId>>,
) -> Result<BTreeSet<ModelId>, SelectorError> {
    let matched: BTreeSet<ModelId> = match &term.kind {
        SelectorKind::All => compilation
            .models
            .iter()
            .map(|model| model.id.clone())
            .collect(),
        SelectorKind::Changed => match changed {
            Some(changed) => changed
                .iter()
                .filter(|id| compilation.model(id).is_some())
                .cloned()
                .collect(),
            None => return Err(SelectorError::ChangedUnavailable),
        },
        SelectorKind::Tag(tag) => compilation
            .models
            .iter()
            .filter(|model| model.config.tags.iter().any(|model_tag| model_tag == tag))
            .map(|model| model.id.clone())
            .collect(),
        SelectorKind::Namespace(namespace) => compilation
            .models
            .iter()
            .filter(|model| model.id.namespace().as_str() == namespace)
            .map(|model| model.id.clone())
            .collect(),
        SelectorKind::Source(value) => compilation
            .models
            .iter()
            .filter(|model| {
                model.source_dependencies().any(|source| {
                    let name = source.logical_name();
                    name == *value
                        || name.starts_with(&format!("{value}."))
                        || source.parts().iter().any(|part| part == value)
                })
            })
            .map(|model| model.id.clone())
            .collect(),
        SelectorKind::Name(pattern) => return match_name(compilation, pattern, term),
    };

    // `changed` may legitimately resolve to nothing; every other empty match
    // is a user error worth reporting.
    if matched.is_empty() && term.kind != SelectorKind::Changed {
        return Err(no_match(term, compilation));
    }
    Ok(matched)
}

/// Name matching: exact logical name or URI first, then `prefix.*` globs,
/// then a unique suffix on the namespace-relative or final segment.
fn match_name(
    compilation: &Compilation,
    pattern: &str,
    term: &SelectorTerm,
) -> Result<BTreeSet<ModelId>, SelectorError> {
    if let Ok(id) = ModelId::parse(pattern) {
        if compilation.model(&id).is_some() {
            return Ok([id].into_iter().collect());
        }
    }

    if let Some(prefix) = pattern.strip_suffix(".*") {
        let matched: BTreeSet<ModelId> = compilation
            .models
            .iter()
            .filter(|model| {
                let name = model.id.logical_name();
                name == prefix || name.starts_with(&format!("{prefix}."))
            })
            .map(|model| model.id.clone())
            .collect();
        if matched.is_empty() {
            return Err(no_match(term, compilation));
        }
        return Ok(matched);
    }

    // A partially-qualified or bare name matches when it identifies exactly
    // one model — `results` or `staging.results` are fine when unambiguous.
    let suffix: Vec<ModelId> = compilation
        .models
        .iter()
        .filter(|model| model.id.local_name() == pattern || model.id.last_segment() == pattern)
        .map(|model| model.id.clone())
        .collect();
    match suffix.len() {
        0 => Err(no_match(term, compilation)),
        1 => Ok(suffix.into_iter().collect()),
        _ => {
            let candidates = suffix
                .iter()
                .map(|id| format!("  {}", id.logical_name()))
                .collect::<Vec<_>>()
                .join("\n");
            Err(SelectorError::Ambiguous {
                term: term.text.clone(),
                candidates: format!("candidates:\n{candidates}\nqualify the model name"),
            })
        }
    }
}

/// A "matched no models" error with near-miss suggestions.
fn no_match(term: &SelectorTerm, compilation: &Compilation) -> SelectorError {
    let mut suggestions: Vec<String> = Vec::new();
    if let SelectorKind::Name(pattern) = &term.kind {
        let qualified = pattern.contains('.');
        for model in &compilation.models {
            let name = model.id.logical_name();
            let same_namespace = qualified
                && pattern
                    .split('.')
                    .next()
                    .map(|namespace| model.id.namespace().as_str() == namespace)
                    .unwrap_or(false);
            if name.contains(pattern.as_str())
                || model.id.local_name().ends_with(pattern.as_str())
                || model.id.last_segment() == pattern
                || same_namespace
            {
                suggestions.push(name);
            }
        }
        if suggestions.is_empty()
            && !pattern.ends_with(".*")
            && compilation
                .models
                .iter()
                .any(|model| model.id.logical_name().starts_with(&format!("{pattern}.")))
        {
            suggestions.push(format!("{pattern}.*"));
        }
    }
    suggestions.sort();
    suggestions.dedup();
    suggestions.truncate(5);
    SelectorError::NoMatches {
        term: term.text.clone(),
        suggestions: if suggestions.is_empty() {
            String::new()
        } else {
            format!("\n  did you mean: {}", suggestions.join(", "))
        },
    }
}

/// The transitive model closure of `seed` in one direction.
///
/// `upstream` walks model dependencies; otherwise it walks dependents. The
/// returned set includes the seed itself.
fn expand(
    compilation: &Compilation,
    seed: &BTreeSet<ModelId>,
    upstream: bool,
) -> BTreeSet<ModelId> {
    let mut included: BTreeSet<ModelId> = seed.clone();
    let mut frontier: Vec<ModelId> = seed.iter().cloned().collect();
    while let Some(id) = frontier.pop() {
        let neighbours: Vec<ModelId> = if upstream {
            compilation
                .dependencies(&id)
                .into_iter()
                .filter_map(|dependency| match dependency {
                    Dependency::Model(id) => Some(id),
                    Dependency::Source(_) => None,
                })
                .collect()
        } else {
            compilation.dependents(&id)
        };
        for neighbour in neighbours {
            if included.insert(neighbour.clone()) {
                frontier.push(neighbour);
            }
        }
    }
    included
}

/// Sort members topologically (dependencies first), falling back to logical
/// name order for cyclic graphs.
fn ordered_members(compilation: &Compilation, members: Vec<SelectedModel>) -> Vec<SelectedModel> {
    let by_name: BTreeMap<String, SelectedModel> = members
        .into_iter()
        .map(|member| (member.id.clone(), member))
        .collect();
    match compilation.topological_order() {
        Some(order) => order
            .iter()
            .filter_map(|id| by_name.get(&id.logical_name()).cloned())
            .collect(),
        None => by_name.into_values().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::model::{SemanticModel, SemanticProject};

    fn workspace() -> Compilation {
        let mut tagged = SemanticModel::in_memory(
            ModelId::parse("assay.results").unwrap(),
            "select * from assay.raw",
        );
        tagged.config.tags = vec!["qc".to_string()];
        let models = vec![
            SemanticModel::in_memory(
                ModelId::parse("assay.raw").unwrap(),
                "select * from external.raw_assay_results",
            ),
            tagged,
            SemanticModel::in_memory(
                ModelId::parse("reporting.monthly").unwrap(),
                "select * from assay.results",
            ),
        ];
        let compilation = compile(&SemanticProject::in_memory(models));
        assert!(compilation.is_ok());
        compilation
    }

    fn resolve(terms: &[&str]) -> Selection {
        try_resolve(terms).unwrap()
    }

    fn try_resolve(terms: &[&str]) -> Result<Selection, SelectorError> {
        let raw: Vec<String> = terms.iter().map(|term| term.to_string()).collect();
        let set = SelectorSet::parse(&raw, &[], &[], false, false).unwrap();
        resolve_selection(&workspace(), &set, None)
    }

    fn names(selection: &Selection) -> Vec<String> {
        selection
            .members
            .iter()
            .map(|member| member.id.clone())
            .collect()
    }

    #[test]
    fn empty_selection_selects_all() {
        let selection = resolve(&[]);
        assert_eq!(
            names(&selection),
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
    }

    #[test]
    fn selects_by_name_and_glob() {
        assert_eq!(names(&resolve(&["assay.results"])), vec!["assay.results"]);
        assert_eq!(
            names(&resolve(&["assay.*"])),
            vec!["assay.raw", "assay.results"]
        );
        assert_eq!(names(&resolve(&["model://assay/raw"])), vec!["assay.raw"]);
    }

    #[test]
    fn bare_unique_suffix_matches() {
        assert_eq!(names(&resolve(&["monthly"])), vec!["reporting.monthly"]);
        assert_eq!(
            names(&resolve(&["reporting.monthly"])),
            vec!["reporting.monthly"]
        );
    }

    #[test]
    fn trailing_plus_expands_downstream() {
        assert_eq!(
            names(&resolve(&["assay.raw+"])),
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
        let selection = resolve(&["assay.raw+"]);
        let monthly = selection
            .members
            .iter()
            .find(|member| member.id == "reporting.monthly")
            .unwrap();
        assert_eq!(monthly.expanded, vec!["assay.raw+".to_string()]);
    }

    #[test]
    fn leading_plus_expands_upstream() {
        assert_eq!(
            names(&resolve(&["+reporting.monthly"])),
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
        assert_eq!(
            names(&resolve(&["+reporting.monthly+"])),
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
    }

    #[test]
    fn tag_namespace_and_source_kinds() {
        assert_eq!(names(&resolve(&["tag:qc"])), vec!["assay.results"]);
        assert_eq!(
            names(&resolve(&["namespace:assay"])),
            vec!["assay.raw", "assay.results"]
        );
        assert_eq!(names(&resolve(&["source:external"])), vec!["assay.raw"]);
        assert_eq!(
            names(&resolve(&["source:external.raw_assay_results+"])),
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
    }

    #[test]
    fn union_and_exclusion() {
        let raw = vec!["assay.raw".to_string(), "reporting.monthly".to_string()];
        let exclude = vec!["assay.raw".to_string()];
        let set = SelectorSet::parse(&raw, &exclude, &[], false, false).unwrap();
        let selection = resolve_selection(&workspace(), &set, None).unwrap();
        assert_eq!(names(&selection), vec!["reporting.monthly"]);
        assert_eq!(selection.excluded, vec!["assay.raw".to_string()]);
    }

    #[test]
    fn exclude_removes_expanded_members() {
        let raw = vec!["assay.raw+".to_string()];
        let exclude = vec!["reporting.monthly".to_string()];
        let set = SelectorSet::parse(&raw, &exclude, &[], false, false).unwrap();
        let selection = resolve_selection(&workspace(), &set, None).unwrap();
        assert_eq!(names(&selection), vec!["assay.raw", "assay.results"]);
    }

    #[test]
    fn filter_terms_intersect() {
        let raw = vec!["assay.*".to_string()];
        let filter = vec!["tag:qc".to_string()];
        let set = SelectorSet::parse(&raw, &[], &filter, false, false).unwrap();
        let selection = resolve_selection(&workspace(), &set, None).unwrap();
        assert_eq!(names(&selection), vec!["assay.results"]);
    }

    #[test]
    fn legacy_expand_flags_apply_to_the_filtered_base() {
        let raw = vec!["assay.results".to_string()];
        let filter = vec!["tag:qc".to_string()];
        let set = SelectorSet::parse(&raw, &[], &filter, true, false).unwrap();
        let selection = resolve_selection(&workspace(), &set, None).unwrap();
        assert_eq!(names(&selection), vec!["assay.raw", "assay.results"]);
    }

    #[test]
    fn changed_term_uses_the_supplied_set() {
        let raw = vec!["changed".to_string()];
        let set = SelectorSet::parse(&raw, &[], &[], false, false).unwrap();
        let changed: BTreeSet<ModelId> =
            [ModelId::parse("assay.raw").unwrap()].into_iter().collect();
        let selection = resolve_selection(&workspace(), &set, Some(&changed)).unwrap();
        assert_eq!(names(&selection), vec!["assay.raw"]);
    }

    #[test]
    fn changed_without_a_change_set_is_an_error() {
        let raw = vec!["changed".to_string()];
        let set = SelectorSet::parse(&raw, &[], &[], false, false).unwrap();
        assert!(matches!(
            resolve_selection(&workspace(), &set, None),
            Err(SelectorError::ChangedUnavailable)
        ));
    }

    #[test]
    fn changed_empty_is_not_an_error() {
        let raw = vec!["changed+".to_string()];
        let set = SelectorSet::parse(&raw, &[], &[], false, false).unwrap();
        let changed = BTreeSet::new();
        let selection = resolve_selection(&workspace(), &set, Some(&changed)).unwrap();
        assert!(selection.members.is_empty());
    }

    #[test]
    fn unknown_kind_is_an_error() {
        let error = parse_selector("foo:bar").unwrap_err();
        assert!(matches!(error, SelectorError::Invalid { .. }), "{error}");
    }

    #[test]
    fn empty_value_and_stray_plus_are_errors() {
        assert!(matches!(
            parse_selector("tag:"),
            Err(SelectorError::Invalid { .. })
        ));
        assert!(matches!(
            parse_selector("a+b"),
            Err(SelectorError::Invalid { .. })
        ));
        assert!(matches!(
            parse_selector("+"),
            Err(SelectorError::Invalid { .. })
        ));
    }

    #[test]
    fn no_match_suggests_near_misses() {
        let error = try_resolve(&["assay.reslts"]).unwrap_err();
        match error {
            SelectorError::NoMatches { suggestions, .. } => {
                assert!(suggestions.contains("assay.results"), "{suggestions}");
            }
            other => panic!("expected NoMatches, got {other}"),
        }
    }

    #[test]
    fn ambiguous_suffix_lists_candidates() {
        let mut a = SemanticModel::in_memory(
            ModelId::parse("assay.results").unwrap(),
            "select * from assay.raw",
        );
        a.config.tags = Vec::new();
        let models = vec![
            SemanticModel::in_memory(
                ModelId::parse("assay.raw").unwrap(),
                "select * from external.x",
            ),
            a,
            SemanticModel::in_memory(
                ModelId::parse("reporting.results").unwrap(),
                "select * from assay.results",
            ),
        ];
        let compilation = compile(&SemanticProject::in_memory(models));
        assert!(compilation.is_ok());
        let raw = vec!["results".to_string()];
        let set = SelectorSet::parse(&raw, &[], &[], false, false).unwrap();
        let error = resolve_selection(&compilation, &set, None).unwrap_err();
        match error {
            SelectorError::Ambiguous { candidates, .. } => {
                assert!(candidates.contains("assay.results"), "{candidates}");
                assert!(candidates.contains("reporting.results"), "{candidates}");
            }
            other => panic!("expected Ambiguous, got {other}"),
        }
    }

    #[test]
    fn expansion_is_deterministic() {
        // Expansion order follows the topological order, not insertion order.
        let first = resolve(&["+reporting.monthly"]);
        let second = resolve(&["+reporting.monthly"]);
        assert_eq!(names(&first), names(&second));
    }
}
