//! Environment provisioning: Nessie branch plus a branch-scoped catalog.
//!
//! For Nessie/Iceberg targets a candidate environment is a Nessie branch. Trino
//! cannot switch the Nessie reference of an existing catalog at query time, so
//! each environment is provisioned as a dynamic Trino catalog pointing at the
//! branch (verified against Trino 483). The engine stays adapter-agnostic: the
//! adapter owns catalog provisioning.

use std::path::Path;

use serde::{Deserialize, Serialize};

use phlo_transform_core::{
    compile, compile_with_options, load_project, Compilation, DataType, Nullability,
    RelationSchema, SchemaColumn, SemanticProject, StaticSchemaProvider,
};
use phlo_transform_nessie::{NessieClient, ReferenceInfo};

use crate::adapter::{Adapter, CatalogRequest, CatalogStatus};
use crate::audit::{read_environment_for, write_environment_artifacts};
use crate::error::EngineError;
use crate::source_state::{adapter_default_schema, collect_source_states, relation_for_source};

/// A request to ensure an environment exists.
#[derive(Clone, Debug)]
pub struct EnvironmentSpec {
    pub base_ref: String,
    pub candidate_ref: String,
    /// Nessie API base URI, e.g. `http://nessie:19120`.
    pub nessie_uri: Option<String>,
    /// Iceberg warehouse location for the candidate catalog.
    pub warehouse: Option<String>,
    /// Explicit physical catalog for the candidate (the `--catalog`
    /// override). `None` resolves the recorded binding when one exists,
    /// else the generated per-reference convention — see
    /// [`ensure_candidate`].
    pub catalog: Option<String>,
}

/// The provisioned environment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentSetup {
    /// The base reference as resolved at provisioning time — informational:
    /// it says which ref `base_ref` named, not what the candidate was cut
    /// from. Provenance lives in `created_from`.
    pub base: ReferenceInfo,
    pub candidate: ReferenceInfo,
    /// Immutable provenance: the reference (and commit) the candidate branch
    /// was actually created from. Recorded only when provable — when this
    /// call created the branch, or a prior artifact recorded it. `None` for
    /// a pre-existing branch with no recorded origin: promotion must never
    /// silently redefine an existing branch's base as today's target.
    #[serde(default)]
    pub created_from: Option<ReferenceInfo>,
    pub created_branch: bool,
    /// The physical catalog the candidate's writes target.
    pub catalog: String,
    /// How the catalog's existence was established — recorded so a later
    /// audit can tell a catalog phlo created from one it adopted.
    #[serde(default)]
    pub catalog_status: crate::adapter::CatalogStatus,
    /// Whether phlo owns this catalog and may drop it during candidate
    /// cleanup. Separate from `catalog_status`: ownership is recorded once
    /// at provisioning and stays sticky, while verification status reports
    /// what the *current* call could prove — a catalog phlo created keeps
    /// `owns_catalog()` true even when a later run can only observe it as
    /// `Unverified`.
    #[serde(default)]
    pub catalog_owned_by_phlo: Option<bool>,
}

impl EnvironmentSetup {
    /// Whether phlo may drop the candidate's catalog during cleanup.
    /// `catalog_owned_by_phlo` is authoritative for records this build
    /// wrote; artifacts written before the flag existed fall back to
    /// `catalog_status == Created` — the only ownership they could prove.
    pub fn owns_catalog(&self) -> bool {
        self.catalog_owned_by_phlo
            .unwrap_or(matches!(self.catalog_status, CatalogStatus::Created))
    }
}

/// Ensure the candidate branch and its catalog exist.
///
/// This is the primitive: it trusts the adapter's report. An
/// [`CatalogStatus::Unverified`] catalog — one that already existed, whose
/// bound Nessie ref cannot be read back — is *not* vetted here. Callers
/// with workspace access must go through [`ensure_candidate`], which
/// accepts an unverified catalog only on a recorded binding and refuses
/// catalogs claimed by other candidates.
pub async fn ensure_environment(
    nessie: &dyn NessieClient,
    adapter: &dyn Adapter,
    spec: &EnvironmentSpec,
) -> Result<EnvironmentSetup, EngineError> {
    let base = nessie
        .get_reference(&spec.base_ref)
        .await
        .map_err(|error| EngineError::Environment(error.to_string()))?
        .ok_or_else(|| {
            EngineError::NotFound(format!("base reference `{}` was not found", spec.base_ref))
        })?;

    let (candidate, created_branch) = if spec.candidate_ref == spec.base_ref {
        (base.clone(), false)
    } else {
        match nessie
            .get_reference(&spec.candidate_ref)
            .await
            .map_err(|error| EngineError::Environment(error.to_string()))?
        {
            Some(existing) => (existing, false),
            None => (
                nessie
                    .create_branch(&spec.candidate_ref, &base)
                    .await
                    .map_err(|error| EngineError::Environment(error.to_string()))?,
                true,
            ),
        }
    };
    // Provenance is recorded only when it is provable: a branch we just
    // created is by construction cut from `base`; a pre-existing branch's
    // origin is unknown here — the caller may preserve a prior artifact's
    // `created_from`, but this function must not guess.
    let created_from = if created_branch || candidate.name == base.name {
        Some(base.clone())
    } else {
        None
    };

    // `None` resolves the generated per-reference convention; callers with
    // workspace access should prefer `ensure_candidate`, which also honours
    // the recorded binding.
    let catalog = spec
        .catalog
        .clone()
        .unwrap_or_else(|| catalog_name(&spec.candidate_ref));
    let catalog_status = match adapter
        .ensure_catalog(&CatalogRequest {
            catalog: catalog.clone(),
            reference: Some(spec.candidate_ref.clone()),
            nessie_uri: spec.nessie_uri.clone(),
            warehouse: spec.warehouse.clone(),
        })
        .await
    {
        Ok(status) => status,
        // A failed provisioning attempt must not leak the branch it just
        // created — a stray pre-existing branch has unprovable provenance
        // on the next attempt and would block promotion later.
        Err(error) => {
            if created_branch {
                let _ = nessie.delete_branch(&spec.candidate_ref).await;
            }
            return Err(EngineError::Adapter(error));
        }
    };

    // A Nessie-bound environment whose adapter cannot provision the
    // generated catalog would run on the default target while evidence
    // claims the candidate — fail closed. The branch just created is rolled
    // back best-effort. An explicit `catalog` pin is the escape hatch: the
    // caller named the physical binding and takes responsibility for it.
    if catalog_status == CatalogStatus::Unmanaged
        && spec.nessie_uri.is_some()
        && spec.catalog.is_none()
    {
        if created_branch {
            let _ = nessie.delete_branch(&spec.candidate_ref).await;
        }
        return Err(EngineError::NotConfigured(format!(
            "adapter `{}` cannot provision a catalog bound to Nessie ref `{}` — \
             the environment claims branch isolation it cannot honour",
            adapter.name(),
            spec.candidate_ref
        )));
    }

    Ok(EnvironmentSetup {
        base,
        candidate,
        created_from,
        created_branch,
        catalog,
        catalog_status,
        catalog_owned_by_phlo: None,
    })
}

/// The one place a candidate's physical catalog is decided: an explicit
/// override wins; otherwise the catalog this ref was provisioned with
/// before (reruns must land on the same object); otherwise the generated
/// ref-keyed convention. `ensure_candidate` (provisioning) and
/// `EnvironmentContext::resolve` (previews) share this — plan and run can
/// never disagree about where an environment lands.
fn resolve_catalog(spec: &EnvironmentSpec, prior: Option<&EnvironmentSetup>) -> String {
    spec.catalog
        .clone()
        .or_else(|| prior.map(|recorded| recorded.catalog.clone()))
        .unwrap_or_else(|| catalog_name(&spec.candidate_ref))
}

/// Whether a recorded artifact attests a *binding* between the candidate
/// and a catalog — not merely intent. An artifact that observed the
/// catalog (`Created`, or `Unverified` already vetted by an earlier
/// ensure) attests it; so does a non-conventional recorded name, which is
/// a `ref create --catalog` pin the workspace stands behind. A plain
/// `ref create` artifact's conventional name is intent only: it was
/// written before the catalog existed and cannot vouch for a catalog that
/// appeared later.
fn prior_records_binding(prior: &EnvironmentSetup, candidate_ref: &str) -> bool {
    prior.catalog_status != CatalogStatus::Unmanaged || prior.catalog != catalog_name(candidate_ref)
}

/// Whether the resolved catalog is a pin — an explicit override or a
/// recorded binding the workspace stands behind — rather than the
/// generated convention, which requires the adapter to provision it.
/// `ensure_candidate` (`Ensure`) and `EnvironmentContext::resolve`
/// (`ReadOnly`) share this so both modes apply the same provisioning
/// requirement.
fn catalog_is_pinned(spec: &EnvironmentSpec, prior: Option<&EnvironmentSetup>) -> bool {
    spec.catalog.is_some()
        || prior.is_some_and(|prior| prior_records_binding(prior, &spec.candidate_ref))
}

/// Ensure a candidate environment exists — Nessie branch plus its catalog —
/// preserving any previously recorded cut-from provenance, and persist the
/// provisioning artifacts. Shared by the CLI's `--ref` provisioning and the
/// daemon's environment-targeted operations.
///
/// After this call `setup.created_from.is_none()` means the branch
/// pre-existed with unrecorded provenance — promotion must refuse it; the
/// caller decides how to surface the warning.
pub async fn ensure_candidate(
    workspace_root: &Path,
    nessie: &dyn NessieClient,
    adapter: &dyn Adapter,
    spec: &EnvironmentSpec,
) -> Result<EnvironmentSetup, EngineError> {
    let prior = read_environment_for(workspace_root, &spec.candidate_ref);
    let catalog = resolve_catalog(spec, prior.as_ref());
    // A catalog another candidate's evidence claims is refused outright —
    // whatever the adapter would report, sharing one physical catalog
    // across refs breaks the isolation the environment claims.
    if crate::audit::environment_artifacts(workspace_root)
        .iter()
        .any(|other| other.catalog == catalog && other.candidate.name != spec.candidate_ref)
    {
        return Err(EngineError::Environment(format!(
            "catalog `{catalog}` is already claimed by another candidate — \
             choose a different `--catalog` or remove the other candidate's artifacts",
        )));
    }
    // Keep `spec.catalog == None` honest into `ensure_environment`: a
    // generated name means phlo must provision the catalog, so an
    // `Unmanaged` answer fails closed there. The pin is an explicit
    // override or a recorded binding the workspace stands behind — a bare
    // `ref create` artifact records only the conventional name with
    // `Unmanaged` status, which is an intent, not a binding: pinning it
    // would silence the fail-closed check on a non-provisioning adapter.
    let resolved = EnvironmentSpec {
        catalog: catalog_is_pinned(spec, prior.as_ref()).then_some(catalog),
        ..spec.clone()
    };
    let mut setup = ensure_environment(nessie, adapter, &resolved).await?;
    if setup.catalog_status == CatalogStatus::Unverified {
        // The catalog already existed and its configured Nessie ref cannot
        // be introspected — "exists" is not "correct", and the generated
        // name is no proof either: `phlo_<ref>_<hash>` is a public
        // convention anyone can mint pointed at another ref. Accept the
        // catalog only when this workspace recorded the binding for this
        // exact ref and catalog — an artifact that observed it before, or
        // an explicit `ref create --catalog` pin.
        let recorded = prior.as_ref().is_some_and(|prior| {
            prior.catalog == setup.catalog && prior_records_binding(prior, &spec.candidate_ref)
        });
        if !recorded {
            if setup.created_branch {
                let _ = nessie.delete_branch(&spec.candidate_ref).await;
            }
            return Err(EngineError::Environment(format!(
                "catalog `{}` already exists but cannot be verified as bound to `{}` — \
                 drop it or let phlo provision it once so the binding is recorded",
                setup.catalog, spec.candidate_ref
            )));
        }
    }
    // Ownership is decided once, here, so a later `Unverified` status cannot
    // make a phlo-owned catalog look abandoned. A catalog phlo just created
    // is ours; an `Unverified` one was accepted only on this workspace's
    // recorded binding, so it keeps whatever ownership that record
    // established; an adopted or unmanaged catalog belongs to someone else.
    setup.catalog_owned_by_phlo = Some(match setup.catalog_status {
        CatalogStatus::Created => true,
        CatalogStatus::Unverified => prior
            .as_ref()
            .filter(|p| p.catalog == setup.catalog)
            .is_some_and(|p| p.owns_catalog()),
        CatalogStatus::Unmanaged => false,
    });
    if setup.created_from.is_none() {
        // The branch pre-existed: never redefine its base as today's `base`.
        // Preserve whatever an earlier artifact recorded — if nothing did,
        // provenance is unknown and `promote` will refuse this candidate.
        setup.created_from = prior.and_then(|recorded| recorded.created_from);
    }
    write_environment_artifacts(workspace_root, &setup)
        .map_err(|error| EngineError::Artifact(error.to_string()))?;
    Ok(setup)
}

/// The result of provisioning a candidate environment for a run: the
/// environment record plus the workspace compiled against its catalog.
pub struct CandidateWorkspace {
    pub setup: EnvironmentSetup,
    pub compilation: Compilation,
}

/// Provision the candidate environment and compile `root` retargeted at its
/// catalog — the `plan/apply/run --ref <candidate>` orchestration, shared by
/// the CLI and the daemon so an environment-targeted operation can never
/// drift onto the default catalog.
///
/// `enrich` mirrors the CLI's catalogue enrichment (adapter-observed source
/// schemas and source states) for run/apply/plan/test.
pub async fn provision_candidate(
    workspace_root: &Path,
    nessie: &dyn NessieClient,
    adapter: &dyn Adapter,
    spec: &EnvironmentSpec,
    enrich: bool,
) -> Result<CandidateWorkspace, EngineError> {
    let setup = ensure_candidate(workspace_root, nessie, adapter, spec).await?;
    let compilation =
        compile_for_catalog(workspace_root, Some(&setup.catalog), Some(adapter), enrich).await?;
    Ok(CandidateWorkspace { setup, compilation })
}

/// Whether environment resolution may provision (`Ensure` — run paths) or
/// must only compute the would-be target (`ReadOnly` — plan paths; the
/// candidate branch and catalog are never created).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentMode {
    /// Provision the candidate branch + catalog, then compile retargeted.
    Ensure,
    /// Resolve the same physical target without touching Nessie or the
    /// warehouse: the catalog name comes from the same precedence
    /// (override → recorded binding → generated convention) so the plan an
    /// API or CLI caller previews is exactly what a later run executes.
    ReadOnly,
}

/// The handles environment resolution needs, shared by the CLI's `--ref`
/// orchestration and the daemon's environment-labelled operations and
/// reads — one code path so plan and run can never disagree about where an
/// environment's work physically lands.
pub struct EnvironmentContext<'a> {
    /// The workspace root — the live checkout and where provisioning
    /// artifacts are recorded.
    pub root: &'a Path,
    /// Nessie client. `None` means environment labels are state records
    /// only — honest local mode, never an error.
    pub nessie: Option<&'a dyn NessieClient>,
    pub adapter: Option<&'a dyn Adapter>,
    /// The catalog-facing Nessie URI — what `ensure_catalog` needs to bind
    /// a branch-scoped catalog. Required whenever a candidate environment
    /// is resolved against a Nessie client.
    pub nessie_uri: Option<&'a str>,
    pub warehouse: Option<&'a str>,
    /// The `--catalog` override, when configured.
    pub catalog: Option<&'a str>,
}

/// What an environment resolves to for planning/execution.
#[derive(Default)]
pub struct EnvironmentTarget {
    /// The workspace is compiled for the environment's physical catalog —
    /// `None` means the caller's existing workspace compilation stands
    /// (no candidate dimension: no environment, the base ref itself, or
    /// label-only mode without Nessie).
    pub compilation: Option<Compilation>,
    /// The physical catalog resolved, when a candidate was resolved.
    pub catalog: Option<String>,
    /// Provisioning evidence — recorded in `Ensure` mode; in `ReadOnly`
    /// mode only when a prior run already wrote it.
    pub setup: Option<EnvironmentSetup>,
}

impl EnvironmentContext<'_> {
    /// Resolve the physical target an environment label implies.
    ///
    /// - `None` or the base ref → `EnvironmentTarget::default` (the caller's
    ///   own compilation; the base is not a candidate).
    /// - Nessie absent → `default` (the label is recorded in state but no
    ///   branch semantics exist to provision).
    /// - Nessie present but isolation unprovisionable (no adapter or no
    ///   catalog-facing `nessie_uri`) → [`EngineError::NotConfigured`] —
    ///   failing closed, since running on the default target and binding
    ///   evidence to the candidate's head would fabricate isolation.
    /// - `ReadOnly` applies the same requirement `Ensure` ends in: a
    ///   resolution that lands on the generated catalog fails when the
    ///   adapter cannot provision catalogs at all — an explicit or
    ///   recorded pin is the escape hatch — so a preview never names a
    ///   target no run could execute against.
    pub async fn resolve(
        &self,
        environment: Option<&str>,
        base_ref: &str,
        mode: EnvironmentMode,
    ) -> Result<EnvironmentTarget, EngineError> {
        let Some(environment) = environment else {
            return Ok(EnvironmentTarget::default());
        };
        if environment == base_ref {
            return Ok(EnvironmentTarget::default());
        }
        let Some(nessie) = self.nessie else {
            return Ok(EnvironmentTarget::default());
        };
        let (Some(adapter), Some(nessie_uri)) = (self.adapter, self.nessie_uri) else {
            return Err(EngineError::NotConfigured(format!(
                "environment `{environment}` claims a Nessie branch but its catalog cannot be \
                 provisioned — a catalog-facing Nessie URI (and an adapter) is required"
            )));
        };
        let spec = EnvironmentSpec {
            base_ref: base_ref.to_string(),
            candidate_ref: environment.to_string(),
            nessie_uri: Some(nessie_uri.to_string()),
            warehouse: self.warehouse.map(str::to_string),
            catalog: self.catalog.map(str::to_string),
        };
        match mode {
            EnvironmentMode::Ensure => {
                let target = provision_candidate(self.root, nessie, adapter, &spec, true).await?;
                Ok(EnvironmentTarget {
                    catalog: Some(target.setup.catalog.clone()),
                    setup: Some(target.setup),
                    compilation: Some(target.compilation),
                })
            }
            EnvironmentMode::ReadOnly => {
                // The same precedence `ensure_candidate` applies — an
                // explicit override, then the recorded binding, then the
                // generated convention — so the previewed target is the
                // one a later `Ensure` resolves to.
                let prior = read_environment_for(self.root, environment);
                // And the same provisioning requirement: when resolution
                // lands on the generated convention, a later `Ensure` must
                // provision that catalog — an adapter that cannot provision
                // it fails the preview too, rather than previewing a target
                // no run can reach. An explicit or recorded user-managed
                // pin stays the escape hatch.
                if !catalog_is_pinned(&spec, prior.as_ref())
                    && !adapter.supports_catalog_provisioning()
                {
                    return Err(EngineError::NotConfigured(format!(
                        "environment `{environment}` resolves to generated catalog `{}`, which \
                         adapter `{}` cannot provision — a run would fail closed the same way; \
                         pin an existing catalog with `--catalog`",
                        catalog_name(environment),
                        adapter.name()
                    )));
                }
                let catalog = resolve_catalog(&spec, prior.as_ref());
                let compilation =
                    compile_for_catalog(self.root, Some(&catalog), Some(adapter), true).await?;
                Ok(EnvironmentTarget {
                    compilation: Some(compilation),
                    catalog: Some(catalog),
                    setup: prior,
                })
            }
        }
    }
}

/// Compile the workspace at `root`, retargeted at `catalog` when given, and
/// — when `enrich` and an adapter are both present — recompiled with
/// adapter-observed source schemas and source states. Sources resolve in the
/// project's own catalog (`defaults.catalog` before the override): the
/// candidate catalog is for outputs, not inputs.
pub async fn compile_for_catalog(
    workspace_root: &Path,
    catalog: Option<&str>,
    adapter: Option<&dyn Adapter>,
    enrich: bool,
) -> Result<Compilation, EngineError> {
    let label = workspace_root.display().to_string();
    let mut project =
        load_project(workspace_root).map_err(|diagnostics| EngineError::FailedDiagnostics {
            label: label.clone(),
            problem: "cannot load",
            diagnostics,
        })?;
    let source_catalog = project.defaults.catalog.clone();
    let source_schema = project.defaults.schema.clone();
    if let Some(catalog) = catalog {
        project.defaults.catalog = Some(catalog.to_string());
    }
    let base = compile(&project);
    let compiled = match (enrich, adapter) {
        (true, Some(adapter)) => {
            enrich_sources(
                adapter,
                &project,
                source_catalog.as_deref(),
                source_schema.as_deref(),
                &base,
            )
            .await
        }
        _ => base,
    };
    if !compiled.is_ok() {
        return Err(EngineError::FailedDiagnostics {
            label,
            problem: "does not compile cleanly",
            diagnostics: compiled.diagnostics.clone(),
        });
    }
    Ok(compiled)
}

/// Recompile with external source schemas and source states observed through
/// the adapter — the catalogue enrichment the CLI applies to compile,
/// materialised-tree, and retargeted compiles alike. Best-effort per source:
/// one that does not respond keeps its declared schema; the recompile itself
/// always runs.
pub async fn enrich_sources(
    adapter: &dyn Adapter,
    project: &SemanticProject,
    default_catalog: Option<&str>,
    default_schema: Option<&str>,
    base: &Compilation,
) -> Compilation {
    let sources = base.sources();
    // Unqualified sources resolve through the engine's search path, so a
    // bare `raw_orders` lands in the adapter's own default schema — `main`
    // on DuckDB. Match that here or state/schema lookups miss entirely.
    let default_schema = default_schema.or(adapter_default_schema(adapter.name()));
    let mut provider = StaticSchemaProvider::new();
    for source in &sources {
        let relation = relation_for_source(source, default_catalog, default_schema);
        let Ok(columns) = adapter.relation_columns(&relation).await else {
            continue;
        };
        if columns.is_empty() {
            continue;
        }
        let schema = RelationSchema::new(
            columns
                .into_iter()
                .map(|column| SchemaColumn {
                    name: column.name,
                    data_type: DataType::parse_trino(&column.data_type),
                    nullability: if column.nullable {
                        Nullability::Unknown
                    } else {
                        Nullability::NotNull
                    },
                })
                .collect(),
        );
        provider.insert(&source.logical_name(), schema);
    }
    // A source whose state cannot be observed must not discard the schema
    // enrichment already gathered for the others.
    let source_states = collect_source_states(
        adapter,
        &sources,
        &base.seeds,
        default_catalog,
        default_schema,
    )
    .await
    .unwrap_or_default();
    compile_with_options(project, &provider, &source_states)
}

/// The conventional catalog name for a candidate environment:
/// `phlo_<ref>_<hash>` — the readable ref (unsafe characters folded to
/// `_`, truncated) plus the first 8 hex of the ref's SHA-256. The hash is
/// the collision guard: refs that fold identically (`ci/pr-1`, `ci_pr_1`,
/// `ci-pr-1`) must not share one physical catalog. The name is *not*
/// proof of binding — anyone (an admin, a stale install, a failed old
/// run) can create `phlo_<ref>_<hash>` pointed at another Nessie ref, and
/// a catalog's configured ref cannot be read back over SQL. An existing
/// catalog with this name is trusted only when this workspace recorded
/// the binding — see [`ensure_candidate`]. Provisioning and
/// diff/promotion evidence resolution share this convention.
pub fn catalog_name(reference: &str) -> String {
    let mut name = String::from("phlo_");
    let mut previous_underscore = false;
    for character in reference.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
            previous_underscore = false;
        } else if !previous_underscore {
            name.push('_');
            previous_underscore = true;
        }
    }
    let mut readable = name.trim_end_matches('_').to_string();
    // Keep the whole name inside engine identifier limits.
    readable.truncate(48);
    let readable = readable.trim_end_matches('_');
    let hash = phlo_transform_core::version::sha256_hex(reference);
    format!("{readable}_{}", &hash[..8])
}

/// The base ref an environment resolves against: an explicit `--from`/`base`
/// wins; else the base this workspace's provisioning already recorded for
/// the environment (resume and continuation must resolve the same base the
/// original run used); else `main`. Shared so the CLI, the daemon, and
/// diff/promotion never disagree about a candidate's base.
pub fn base_ref_for(
    workspace_root: &Path,
    environment: Option<&str>,
    explicit: Option<&str>,
) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| {
            environment
                .and_then(|env| read_environment_for(workspace_root, env))
                .map(|setup| setup.base.name)
        })
        .unwrap_or_else(|| "main".to_string())
}

/// The physical catalog a reference's environment resolves through: the
/// recorded binding when this workspace provisioned it, else the generated
/// `phlo_<ref>` convention — the same precedence provisioning applies, so
/// evidence readers and provisioners never disagree about where a ref's
/// data physically lives.
pub fn environment_catalog(workspace_root: &Path, reference: &str) -> String {
    read_environment_for(workspace_root, reference)
        .map(|setup| setup.catalog)
        .unwrap_or_else(|| catalog_name(reference))
}

/// The catalog the *base* side of a branch diff resolves through: `main`
/// is the deployment catalog (the configured `--catalog` override, else
/// the workspace's own compiled target); any other ref resolves its
/// recorded or conventional environment catalog. Shared by the CLI's
/// `diff` and the daemon's branch-diff operation.
pub fn base_catalog(
    workspace_root: &Path,
    base_ref: &str,
    configured: Option<&str>,
    compiled: Option<&str>,
) -> Option<String> {
    if base_ref == "main" {
        configured.or(compiled).map(str::to_string)
    } else {
        Some(environment_catalog(workspace_root, base_ref))
    }
}
