//! Candidate/base contract comparison.
//!
//! `contract_diff` turns the opaque `contract_hash` change into a structured,
//! machine-readable report: which columns the contract added, removed,
//! renamed, retightened or relaxed, and whether enforcement changed. Safety
//! uses the same three-level vocabulary as the physical schema diff so
//! promotion gates can treat both uniformly.

use phlo_transform_core::semantic::{DataType, ModelContract};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How dangerous a contract change is for downstream consumers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractSafety {
    /// Consumption stays valid without action.
    Safe,
    /// Deserves human review but doesn't inherently break consumers.
    Review,
    /// Existing consumers, tests or downstream models can break.
    Breaking,
}

impl ContractSafety {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Review => "review",
            Self::Breaking => "breaking",
        }
    }
}

/// One entry in a contract diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractChange {
    /// Column the change applies to. For `renamed`, this is the new name.
    pub column: String,
    /// `contract_added`, `contract_removed`, `enforcement_changed`, `added`,
    /// `removed`, `renamed`, `rename_ambiguous`, `type_changed`,
    /// `nullability_changed`, `key_added`, `key_removed` or `key_changed`.
    pub kind: String,
    /// Human-readable detail.
    pub detail: String,
    pub safety: ContractSafety,
}

/// Diff a previous contract against a desired contract.
///
/// `previous` may be `None` (no contract recorded on the base side) and
/// `desired` may be `None` (the model dropped its contract) — both are
/// reported as changes rather than being silently ignored.
pub fn contract_diff(
    previous: Option<&ModelContract>,
    desired: Option<&ModelContract>,
) -> Vec<ContractChange> {
    let mut changes = Vec::new();
    let (previous, desired) = match (previous, desired) {
        (None, None) => return changes,
        (None, Some(desired)) => {
            // A contract appearing where none existed tightens guarantees —
            // safe for consumers, worth noting.
            changes.push(ContractChange {
                column: String::new(),
                kind: "contract_added".to_string(),
                detail: format!("contract declared ({} columns)", desired.columns.len()),
                safety: ContractSafety::Safe,
            });
            return changes;
        }
        (Some(previous), None) => {
            // The contract disappeared entirely — guarantees are gone.
            changes.push(ContractChange {
                column: String::new(),
                kind: "contract_removed".to_string(),
                detail: format!("contract dropped ({} columns)", previous.columns.len()),
                safety: ContractSafety::Breaking,
            });
            return changes;
        }
        (Some(previous), Some(desired)) => (previous, desired),
    };

    if previous.enforced != desired.enforced {
        changes.push(ContractChange {
            column: String::new(),
            kind: "enforcement_changed".to_string(),
            detail: if desired.enforced {
                "contract now enforced — violations become errors".to_string()
            } else {
                "contract no longer enforced — violations downgrade to warnings".to_string()
            },
            safety: if desired.enforced {
                ContractSafety::Review
            } else {
                ContractSafety::Breaking
            },
        });
    }

    // Declared renames resolve a removed+added pair into a single rename —
    // but only when both names actually exist (`new` in the desired
    // contract, `old` in the previous contract). A stale rename declaration
    // resolves nothing and must not suppress a real removal or addition.
    let previous_columns: BTreeMap<&str, &phlo_transform_core::semantic::ColumnContract> = previous
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect();
    let desired_columns: BTreeMap<&str, &phlo_transform_core::semantic::ColumnContract> = desired
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect();
    // Rename resolution is one-to-one: an `old` name claimed by two new
    // columns is ambiguous — resolve none of them so the underlying
    // removal and additions report normally instead of half the claim
    // silently vanishing.
    let mut claims: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (new, old) in &desired.renames {
        claims.entry(old.as_str()).or_default().push(new.as_str());
    }
    let ambiguous: std::collections::BTreeSet<&str> = claims
        .iter()
        .filter(|(_, news)| news.len() > 1)
        .map(|(old, _)| *old)
        .collect();
    let resolved_renames: BTreeMap<&str, &str> = desired
        .renames
        .iter()
        .filter(|(new, old)| {
            !ambiguous.contains(old.as_str())
                && desired_columns.contains_key(new.as_str())
                && !previous_columns.contains_key(new.as_str())
                && previous_columns.contains_key(old.as_str())
        })
        .map(|(new, old)| (new.as_str(), old.as_str()))
        .collect();
    for old in &ambiguous {
        if previous_columns.contains_key(old) {
            changes.push(ContractChange {
                column: (*old).to_string(),
                kind: "rename_ambiguous".to_string(),
                detail: format!(
                    "declared renames for `{old}` are ambiguous: {}",
                    claims[*old].join(", ")
                ),
                safety: ContractSafety::Review,
            });
        }
    }

    for (new, old) in &resolved_renames {
        // A rename the previous contract already declared is acknowledged
        // history — only newly declared renames are changes.
        if previous.renames.get(*new).map(String::as_str) != Some(*old) {
            changes.push(ContractChange {
                column: (*new).to_string(),
                kind: "renamed".to_string(),
                detail: format!("renamed from `{old}`"),
                // A declared rename proves intent, not compatibility —
                // consumers selecting the old name break. Gated like a
                // removal; a waiver is the acknowledgement.
                safety: ContractSafety::Breaking,
            });
        }
    }

    for column in &previous.columns {
        let renamed_away = resolved_renames
            .values()
            .any(|old| *old == column.name.as_str());
        if !desired_columns.contains_key(column.name.as_str()) && !renamed_away {
            changes.push(ContractChange {
                column: column.name.clone(),
                kind: "removed".to_string(),
                detail: "contract column removed".to_string(),
                safety: ContractSafety::Breaking,
            });
        }
    }

    for column in &desired.columns {
        // A resolved rename compares the new column against its old-name
        // contract so a type or nullability change cannot hide behind the
        // rename; otherwise compare same-name columns.
        let previous_column = resolved_renames
            .get(column.name.as_str())
            .and_then(|old| previous_columns.get(*old).copied())
            .or_else(|| previous_columns.get(column.name.as_str()).copied());
        match previous_column {
            None => changes.push(ContractChange {
                column: column.name.clone(),
                kind: "added".to_string(),
                detail: format!("contract column added ({})", describe_column(column)),
                safety: ContractSafety::Safe,
            }),
            Some(previous_column) => {
                compare_column_contracts(&mut changes, &column.name, previous_column, column)
            }
        }
    }

    changes
}

/// Type and nullability changes between two contract columns, emitted under
/// the desired column's name (the rename target when renamed).
fn compare_column_contracts(
    changes: &mut Vec<ContractChange>,
    name: &str,
    previous: &phlo_transform_core::semantic::ColumnContract,
    desired: &phlo_transform_core::semantic::ColumnContract,
) {
    if let (Some(previous_type), Some(desired_type)) = (&previous.data_type, &desired.data_type) {
        if previous_type != desired_type {
            // Mirror `classify_schema_change`: numeric widening is
            // reviewable, anything else breaks consumers.
            let widening = previous_type.is_known()
                && desired_type.is_known()
                && previous_type.is_numeric()
                && desired_type.is_numeric()
                && DataType::widen(desired_type, previous_type) == *desired_type;
            changes.push(ContractChange {
                column: name.to_string(),
                kind: "type_changed".to_string(),
                detail: format!("contract type `{previous_type}` → `{desired_type}`"),
                safety: if widening {
                    ContractSafety::Review
                } else {
                    ContractSafety::Breaking
                },
            });
        }
    }
    if let (Some(was_nullable), Some(is_nullable)) = (previous.nullable, desired.nullable) {
        if was_nullable != is_nullable {
            changes.push(ContractChange {
                column: name.to_string(),
                kind: "nullability_changed".to_string(),
                detail: if is_nullable {
                    "contract relaxed to nullable".to_string()
                } else {
                    "contract tightened to non-null".to_string()
                },
                // Downstream direction: relaxing a NOT NULL guarantee breaks
                // consumers that relied on it; tightening can reject producer
                // data but doesn't break downstream compatibility.
                safety: if is_nullable {
                    ContractSafety::Breaking
                } else {
                    ContractSafety::Review
                },
            });
        }
    }
}

/// The effective row-identity key of a model — the semantic concept that
/// incremental `key` columns and native unique assertions collapse onto.
/// Each claim contributes one column set: a `Key` strategy's columns, and
/// each `unique` assertion's columns. Sets stay distinct — `unique(a) ∧
/// unique(b)` is a stronger identity claim than `unique(a,b)`. `None` when
/// the model declares no key at all.
pub fn effective_key(model: &phlo_transform_core::CompiledModel) -> Option<Vec<Vec<String>>> {
    use phlo_transform_core::semantic::Assertion;
    let mut key: Vec<Vec<String>> = Vec::new();
    if let Some(phlo_transform_core::IncrementalStrategy::Key { columns }) =
        &model.config.incremental
    {
        let mut columns = columns.clone();
        columns.sort();
        key.push(columns);
    }
    for assertion in &model.assertions {
        if let Assertion::Unique { columns } = assertion {
            let mut columns = columns.clone();
            columns.sort();
            key.push(columns);
        }
    }
    key.sort();
    key.dedup();
    (!key.is_empty()).then_some(key)
}

/// What a materialisation record can prove about the key it was written
/// with. `MaterializedRecord::recorded_key` produces this from the persisted
/// columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedKey {
    /// The key is known: `Some` claims, or `None` meaning the record
    /// definitely proves the materialised model had no key.
    Known(Option<Vec<Vec<String>>>),
    /// The row predates effective-key persistence and carries no
    /// incremental key either — its historical key cannot be told from
    /// "no key". Promotion evidence must not read this as keyless.
    Unknown,
}

impl RecordedKey {
    /// The claims the record proves — `None` for an unverifiable record.
    pub fn as_deref(&self) -> Option<&[Vec<String>]> {
        match self {
            RecordedKey::Known(key) => key.as_deref(),
            RecordedKey::Unknown => None,
        }
    }
}

/// Compare a recorded key against the desired effective key. Changing or
/// dropping the key is breaking: it changes row identity for merges and any
/// consumer relying on the uniqueness guarantee. A newly declared key is a
/// new guarantee — safe direction, worth reviewing. A record whose
/// historical key cannot be verified fails closed: it may be hiding a
/// changed or dropped key.
pub fn key_change(
    recorded: &RecordedKey,
    desired: Option<&[Vec<String>]>,
) -> Option<ContractChange> {
    let render = |key: &[Vec<String>]| {
        key.iter()
            .map(|columns| columns.join("+"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let known = match recorded {
        RecordedKey::Unknown => {
            return Some(ContractChange {
                column: String::new(),
                kind: "key_unverifiable".to_string(),
                detail: "the base materialisation predates key persistence — its \
                         historical key cannot be verified; rematerialise the base \
                         environment to bind it"
                    .to_string(),
                safety: ContractSafety::Breaking,
            });
        }
        RecordedKey::Known(key) => key.as_deref(),
    };
    match (known, desired) {
        (Some(was), Some(now)) if was != now => Some(ContractChange {
            column: String::new(),
            kind: "key_changed".to_string(),
            detail: format!("key changed: `{}` → `{}`", render(was), render(now)),
            safety: ContractSafety::Breaking,
        }),
        (Some(was), None) => Some(ContractChange {
            column: String::new(),
            kind: "key_removed".to_string(),
            detail: format!("key `{}` dropped", render(was)),
            safety: ContractSafety::Breaking,
        }),
        (None, Some(now)) => Some(ContractChange {
            column: String::new(),
            kind: "key_added".to_string(),
            detail: format!("key `{}` declared", render(now)),
            safety: ContractSafety::Review,
        }),
        _ => None,
    }
}

/// The breaking subset of a contract diff — what promotion must gate on.
pub fn breaking_contract_changes(changes: &[ContractChange]) -> Vec<ContractChange> {
    changes
        .iter()
        .filter(|change| change.safety == ContractSafety::Breaking)
        .cloned()
        .collect()
}

fn describe_column(column: &phlo_transform_core::semantic::ColumnContract) -> String {
    match (&column.data_type, column.nullable) {
        (Some(data_type), Some(nullable)) => format!(
            "{data_type}, {}",
            if nullable { "nullable" } else { "non-null" }
        ),
        (Some(data_type), None) => data_type.to_string(),
        (None, Some(nullable)) => {
            if nullable {
                "nullable".to_string()
            } else {
                "non-null".to_string()
            }
        }
        (None, None) => "untyped".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phlo_transform_core::semantic::ColumnContract;

    fn column(name: &str, data_type: Option<DataType>, nullable: Option<bool>) -> ColumnContract {
        ColumnContract {
            name: name.to_string(),
            data_type,
            nullable,
        }
    }

    fn contract(enforced: bool, columns: Vec<ColumnContract>) -> ModelContract {
        ModelContract {
            enforced,
            columns,
            renames: BTreeMap::new(),
        }
    }

    fn kind<'a>(changes: &'a [ContractChange], column: &str) -> Option<&'a ContractChange> {
        changes.iter().find(|change| change.column == column)
    }

    #[test]
    fn identical_contracts_diff_empty() {
        let contract = contract(
            true,
            vec![column("id", Some(DataType::BigInt), Some(false))],
        );
        assert!(contract_diff(Some(&contract), Some(&contract.clone())).is_empty());
    }

    #[test]
    fn added_column_is_safe() {
        let previous = contract(true, vec![column("id", None, None)]);
        let desired = contract(
            true,
            vec![column("id", None, None), column("note", None, Some(true))],
        );
        let changes = contract_diff(Some(&previous), Some(&desired));
        let change = kind(&changes, "note").unwrap();
        assert_eq!(change.kind, "added");
        assert_eq!(change.safety, ContractSafety::Safe);
    }

    #[test]
    fn removed_column_is_breaking() {
        let previous = contract(
            true,
            vec![column("id", None, None), column("legacy", None, None)],
        );
        let desired = contract(true, vec![column("id", None, None)]);
        let changes = contract_diff(Some(&previous), Some(&desired));
        let change = kind(&changes, "legacy").unwrap();
        assert_eq!(change.kind, "removed");
        assert_eq!(change.safety, ContractSafety::Breaking);
        assert_eq!(breaking_contract_changes(&changes).len(), 1);
    }

    #[test]
    fn declared_rename_is_breaking() {
        // Declaring a rename proves intent, not compatibility — consumers
        // selecting `old_name` break, so the gate must see it.
        let previous = contract(
            true,
            vec![column("id", None, None), column("old_name", None, None)],
        );
        let mut desired = contract(
            true,
            vec![column("id", None, None), column("new_name", None, None)],
        );
        desired
            .renames
            .insert("new_name".to_string(), "old_name".to_string());
        let changes = contract_diff(Some(&previous), Some(&desired));
        let change = kind(&changes, "new_name").unwrap();
        assert_eq!(change.kind, "renamed");
        assert_eq!(change.safety, ContractSafety::Breaking);
        assert!(kind(&changes, "old_name").is_none());
        assert_eq!(breaking_contract_changes(&changes).len(), 1);
    }

    #[test]
    fn ambiguous_renames_resolve_nothing() {
        // Two new columns claiming the same old name cannot both be the
        // rename — resolve neither: the removal stays breaking and both
        // additions report.
        let previous = contract(
            true,
            vec![column("id", None, None), column("old_name", None, None)],
        );
        let mut desired = contract(
            true,
            vec![
                column("id", None, None),
                column("new_a", None, None),
                column("new_b", None, None),
            ],
        );
        desired
            .renames
            .insert("new_a".to_string(), "old_name".to_string());
        desired
            .renames
            .insert("new_b".to_string(), "old_name".to_string());
        let changes = contract_diff(Some(&previous), Some(&desired));
        let removed = changes
            .iter()
            .find(|change| change.column == "old_name" && change.kind == "removed")
            .expect("old_name removal");
        assert_eq!(removed.safety, ContractSafety::Breaking);
        assert_eq!(kind(&changes, "new_a").unwrap().kind, "added");
        assert_eq!(kind(&changes, "new_b").unwrap().kind, "added");
        let ambiguous = changes
            .iter()
            .find(|change| change.kind == "rename_ambiguous")
            .expect("ambiguity must be reported");
        assert_eq!(ambiguous.safety, ContractSafety::Review);
    }

    #[test]
    fn numeric_widening_is_review_incompatible_is_breaking() {
        let previous = contract(
            true,
            vec![
                column("wide", Some(DataType::Integer), None),
                column("kind", Some(DataType::BigInt), None),
            ],
        );
        let desired = contract(
            true,
            vec![
                column("wide", Some(DataType::BigInt), None),
                column("kind", Some(DataType::Varchar), None),
            ],
        );
        let changes = contract_diff(Some(&previous), Some(&desired));
        assert_eq!(
            kind(&changes, "wide").unwrap().safety,
            ContractSafety::Review
        );
        assert_eq!(
            kind(&changes, "kind").unwrap().safety,
            ContractSafety::Breaking
        );
    }

    #[test]
    fn nullability_relax_breaks_tighten_reviews() {
        // Downstream direction: losing the NOT NULL guarantee breaks
        // consumers; gaining it can only reject producer data.
        let previous = contract(
            true,
            vec![
                column("tight", None, Some(true)),
                column("loose", None, Some(false)),
            ],
        );
        let desired = contract(
            true,
            vec![
                column("tight", None, Some(false)),
                column("loose", None, Some(true)),
            ],
        );
        let changes = contract_diff(Some(&previous), Some(&desired));
        assert_eq!(
            kind(&changes, "tight").unwrap().safety,
            ContractSafety::Review
        );
        assert_eq!(
            kind(&changes, "loose").unwrap().safety,
            ContractSafety::Breaking
        );
    }

    #[test]
    fn enforcement_off_is_breaking_on_is_review() {
        let enforced = contract(true, vec![column("id", None, None)]);
        let relaxed = contract(false, vec![column("id", None, None)]);
        let off = contract_diff(Some(&enforced), Some(&relaxed));
        assert_eq!(off[0].kind, "enforcement_changed");
        assert_eq!(off[0].safety, ContractSafety::Breaking);
        let on = contract_diff(Some(&relaxed), Some(&enforced));
        assert_eq!(on[0].kind, "enforcement_changed");
        assert_eq!(on[0].safety, ContractSafety::Review);
    }

    #[test]
    fn dropped_contract_is_breaking_new_contract_is_safe() {
        let existing = contract(true, vec![column("id", None, None)]);
        let dropped = contract_diff(Some(&existing), None);
        assert_eq!(dropped[0].kind, "contract_removed");
        assert_eq!(dropped[0].safety, ContractSafety::Breaking);
        let declared = contract_diff(None, Some(&existing));
        assert_eq!(declared[0].kind, "contract_added");
        assert_eq!(declared[0].safety, ContractSafety::Safe);
        assert!(contract_diff(None, None).is_empty());
    }

    #[test]
    fn stale_rename_is_ignored() {
        // A rename whose new name is not in the desired contract cannot
        // explain the old column's removal.
        let previous = contract(true, vec![column("old_name", None, None)]);
        let mut desired = contract(true, Vec::new());
        desired
            .renames
            .insert("not_present".to_string(), "old_name".to_string());
        let changes = contract_diff(Some(&previous), Some(&desired));
        assert_eq!(kind(&changes, "old_name").unwrap().kind, "removed");
    }

    #[test]
    fn rename_to_an_existing_column_does_not_hide_removal() {
        // `new_name` was already a contract column before — claiming
        // `old_name` became it is ambiguous, so the removal stays breaking.
        let previous = contract(
            true,
            vec![
                column("id", None, None),
                column("new_name", None, None),
                column("old_name", None, None),
            ],
        );
        let mut desired = contract(
            true,
            vec![column("id", None, None), column("new_name", None, None)],
        );
        desired
            .renames
            .insert("new_name".to_string(), "old_name".to_string());
        let changes = contract_diff(Some(&previous), Some(&desired));
        let change = kind(&changes, "old_name").unwrap();
        assert_eq!(change.kind, "removed");
        assert_eq!(change.safety, ContractSafety::Breaking);
    }

    #[test]
    fn key_changes_classify() {
        let key = |columns: &[&str]| -> RecordedKey {
            RecordedKey::Known(Some(vec![columns.iter().map(|c| c.to_string()).collect()]))
        };
        let was = key(&["id"]);
        let now = key(&["batch_id"]);
        let same = key(&["id"]);
        let none = RecordedKey::Known(None);

        assert!(key_change(&was, same.as_deref()).is_none());
        assert!(key_change(&none, None).is_none());

        let changed = key_change(&was, now.as_deref()).expect("key_changed");
        assert_eq!(changed.kind, "key_changed");
        assert_eq!(changed.safety, ContractSafety::Breaking);

        let removed = key_change(&was, None).expect("key_removed");
        assert_eq!(removed.kind, "key_removed");
        assert_eq!(removed.safety, ContractSafety::Breaking);

        let added = key_change(&none, now.as_deref()).expect("key_added");
        assert_eq!(added.kind, "key_added");
        assert_eq!(added.safety, ContractSafety::Review);

        // A composite key split into separate unique claims is a different
        // identity — it reports, not silently equal.
        let composite = key(&["a", "b"]);
        let separate = RecordedKey::Known(Some(vec![vec!["a".to_string()], vec!["b".to_string()]]));
        assert!(key_change(&composite, separate.as_deref()).is_some());

        // A record that cannot prove its historical key fails closed —
        // whether the workspace declares a key or not, an unverifiable
        // record may be hiding a change or a removal.
        for desired in [None, now.as_deref()] {
            let unverifiable =
                key_change(&RecordedKey::Unknown, desired).expect("key_unverifiable");
            assert_eq!(unverifiable.kind, "key_unverifiable");
            assert_eq!(unverifiable.safety, ContractSafety::Breaking);
        }
    }
}
