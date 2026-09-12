//! OpenLineage export for the Phlo lineage graph.
//!
//! This crate is a one-way boundary: it reads the canonical
//! [`LineageGraph`] and emits OpenLineage-compatible structures. OpenLineage
//! is an export format — none of its types appear in the compiler.
//!
//! ```text
//!   Compilation ──▶ LineageGraph ──▶ OpenLineageExporter ──▶ OpenMetadata / DataHub / …
//! ```
//!
//! Phlo exports *design-time* lineage: what the workspace declares, not a
//! run that happened. The document contains spec-valid `JobEvent`s (one per
//! transform model, with input and output datasets and column lineage) and
//! `DatasetEvent`s (one per dataset, carrying schema and Phlo identity).
//! No `run` is fabricated — the spec's `JobEvent` explicitly forbids one.
//!
//! Each event carries the required `eventTime`, `producer` and `schemaURL`
//! fields, so every element of [`OpenLineageDocument::events`] is a payload
//! that can be POSTed to an OpenLineage `/lineage` endpoint as-is.
//!
//! ## Mapping
//!
//! * Model → `Job` (`namespace: phlo`, `name: assay.results`) with the
//!   `jobType` facet and a custom `phlo_job` facet carrying the model's
//!   path, materialization, workflow and content-addressed version.
//! * Dataset (model output, source, seed) → `Dataset` with `schema`,
//!   `datasetType`, `symlinks` (the physical target relation — present only
//!   for materializations that create one) and a custom `phlo_dataset`
//!   facet carrying the `dataset://` URI and dataset kind.
//! * Column `derives` edges → the `columnLineage` facet on each output
//!   dataset inside its job. [`Directness`] maps to `DIRECT`/`INDIRECT`
//!   and [`Transformation`] to the OpenLineage subtype vocabulary
//!   (`IDENTITY`, `TRANSFORMATION`, `AGGREGATION`, `JOIN`, `FILTER`,
//!   `GROUP_BY`, `SORT`, `WINDOW`, `CONDITIONAL`). Lineage confidence has
//!   no standard equivalent; it is reported per field in `phlo_dataset`.
//! * Materialization → `datasetType`: tables and incrementals are `TABLE`,
//!   views are `VIEW`, ephemeral models are `JOB_OUTPUT` with subType
//!   `TEMPORARY`, sources are `TABLE` with subType `EXTERNAL`.
//! * Test → a `phlo` facet is not needed: tests stay internal. Quality
//!   assertions become relevant to OpenLineage consumers through dataset
//!   facets like `dataQualityAssertions` later, when assertions carry
//!   enough metadata to map cleanly.

use std::collections::BTreeMap;

use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use phlo_transform_core::{
    DatasetId, DatasetKind, Directness, LineageGraph, LineageNode, Transformation,
};

/// The producer URI recorded on every event and facet.
pub const PRODUCER: &str = "https://github.com/phlohouse/phlo-transform";

/// Event-level `schemaURL`s — the spec definition each event validates as.
pub const JOB_EVENT_SCHEMA_URL: &str =
    "https://openlineage.io/spec/2-0-2/OpenLineage.json#/$defs/JobEvent";
pub const DATASET_EVENT_SCHEMA_URL: &str =
    "https://openlineage.io/spec/2-0-2/OpenLineage.json#/$defs/DatasetEvent";

/// `_schemaURL`s for the standard facets this exporter emits. Each facet
/// validates against its own published schema, not the event schema.
pub mod facet_schema_url {
    pub const SCHEMA: &str =
        "https://openlineage.io/spec/facets/1-0-0/SchemaDatasetFacet.json#/$defs/SchemaDatasetFacet";
    pub const DATASET_TYPE: &str =
        "https://openlineage.io/spec/facets/1-0-1/DatasetTypeDatasetFacet.json#/$defs/DatasetTypeDatasetFacet";
    pub const SYMLINKS: &str =
        "https://openlineage.io/spec/facets/1-0-1/SymlinksDatasetFacet.json#/$defs/SymlinksDatasetFacet";
    pub const COLUMN_LINEAGE: &str =
        "https://openlineage.io/spec/facets/1-2-0/ColumnLineageDatasetFacet.json#/$defs/ColumnLineageDatasetFacet";
    pub const JOB_TYPE: &str =
        "https://openlineage.io/spec/facets/2-0-3/JobTypeJobFacet.json#/$defs/JobTypeJobFacet";
    /// Custom Phlo facets — immutable, versioned-by-path schemas published
    /// in this repository under `schemas/facets/`.
    pub const PHLO_JOB: &str =
        "https://raw.githubusercontent.com/phlohouse/phlo-transform/main/schemas/facets/1-0-0/PhloJobFacet.json#/$defs/PhloJobFacet";
    pub const PHLO_DATASET: &str =
        "https://raw.githubusercontent.com/phlohouse/phlo-transform/main/schemas/facets/1-0-0/PhloDatasetFacet.json#/$defs/PhloDatasetFacet";
}

/// The design-time lineage document: a flat list of valid OpenLineage
/// events — one `JobEvent` per model followed by one `DatasetEvent` per
/// dataset, in deterministic order.
#[derive(Clone, Debug, Serialize)]
pub struct OpenLineageDocument {
    /// Every element is a complete OpenLineage event that can be sent to a
    /// `/lineage` endpoint individually.
    pub events: Vec<OpenLineageEvent>,
}

/// One event in the document — a `JobEvent` or a `DatasetEvent`.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum OpenLineageEvent {
    Job(JobEvent),
    Dataset(DatasetEvent),
}

/// A design-time `JobEvent`: a model, its input and output datasets.
/// Validates against `OpenLineage.json#/$defs/JobEvent`.
#[derive(Clone, Debug, Serialize)]
pub struct JobEvent {
    #[serde(rename = "eventTime")]
    pub event_time: String,
    pub producer: String,
    #[serde(rename = "schemaURL")]
    pub schema_url: String,
    pub job: Job,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputDataset>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputDataset>,
}

/// A design-time `DatasetEvent`: standalone dataset metadata.
/// Validates against `OpenLineage.json#/$defs/DatasetEvent`.
#[derive(Clone, Debug, Serialize)]
pub struct DatasetEvent {
    #[serde(rename = "eventTime")]
    pub event_time: String,
    pub producer: String,
    #[serde(rename = "schemaURL")]
    pub schema_url: String,
    pub dataset: StaticDataset,
}

#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub namespace: String,
    pub name: String,
    #[serde(skip_serializing_if = "JobFacets::is_empty")]
    pub facets: JobFacets,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobFacets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_type: Option<JobTypeFacet>,
    /// Namespaced custom facet key — `phlo_job`, not a camelCase `phloJob`.
    #[serde(rename = "phlo_job", skip_serializing_if = "Option::is_none")]
    pub phlo_job: Option<PhloJobFacet>,
}

impl JobFacets {
    fn is_empty(&self) -> bool {
        self.job_type.is_none() && self.phlo_job.is_none()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct JobTypeFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    #[serde(rename = "processingType")]
    pub processing_type: String,
    pub integration: String,
    #[serde(rename = "jobType")]
    pub job_type: String,
}

/// Phlo-specific job metadata with no standard OpenLineage equivalent.
/// The schema lives at `schemas/facets/1-0-0/PhloJobFacet.json`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhloJobFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    /// `model://` URI — the logical identity.
    pub uri: String,
    /// Workspace-relative path of the model's SQL file, when file-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub materialization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Content-addressed desired version.
    pub version: String,
}

/// A dataset as it appears inside a job's `inputs`.
#[derive(Clone, Debug, Serialize)]
pub struct InputDataset {
    #[serde(flatten)]
    pub dataset: Dataset,
}

/// A dataset as it appears inside a job's `outputs`.
#[derive(Clone, Debug, Serialize)]
pub struct OutputDataset {
    #[serde(flatten)]
    pub dataset: Dataset,
}

/// A dataset as it appears in a standalone `DatasetEvent`.
#[derive(Clone, Debug, Serialize)]
pub struct StaticDataset {
    #[serde(flatten)]
    pub dataset: Dataset,
}

/// The dataset identity plus its facets — the shared shape of
/// [`InputDataset`], [`OutputDataset`] and [`StaticDataset`]. None of the
/// position-specific facet bags (`inputFacets`, `outputFacets`) are emitted
/// today.
#[derive(Clone, Debug, Serialize)]
pub struct Dataset {
    pub namespace: String,
    pub name: String,
    #[serde(skip_serializing_if = "DatasetFacets::is_empty")]
    pub facets: DatasetFacets,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetFacets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaFacet>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset_type: Option<DatasetTypeFacet>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlinks: Option<SymlinksFacet>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column_lineage: Option<ColumnLineageFacet>,
    /// Namespaced custom facet key — `phlo_dataset`.
    #[serde(rename = "phlo_dataset", skip_serializing_if = "Option::is_none")]
    pub phlo_dataset: Option<PhloDatasetFacet>,
}

impl DatasetFacets {
    fn is_empty(&self) -> bool {
        self.schema.is_none()
            && self.dataset_type.is_none()
            && self.symlinks.is_none()
            && self.column_lineage.is_none()
            && self.phlo_dataset.is_none()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SchemaFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    pub fields: Vec<SchemaField>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SchemaField {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub data_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetTypeFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    pub dataset_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_type: Option<String>,
}

/// Alternate identifiers for a dataset — the physical target relation.
/// Only emitted for materializations that create one.
#[derive(Clone, Debug, Serialize)]
pub struct SymlinksFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    pub identifiers: Vec<SymlinkIdentifier>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SymlinkIdentifier {
    pub namespace: String,
    pub name: String,
    /// Identifier type — `table` or `view`. Required by the facet schema.
    #[serde(rename = "type")]
    pub identifier_type: String,
}

/// The `columnLineage` dataset facet on an output dataset.
#[derive(Clone, Debug, Serialize)]
pub struct ColumnLineageFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    pub fields: BTreeMap<String, ColumnLineageField>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnLineageField {
    pub input_fields: Vec<InputField>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InputField {
    pub namespace: String,
    /// The input dataset name.
    pub name: String,
    /// The input column.
    pub field: String,
    pub transformations: Vec<TransformationRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TransformationRecord {
    /// `DIRECT` or `INDIRECT`.
    #[serde(rename = "type")]
    pub record_type: String,
    /// `IDENTITY`, `TRANSFORMATION`, `AGGREGATION`, `JOIN`, `FILTER`,
    /// `GROUP_BY`, `SORT`, `WINDOW` or `CONDITIONAL`.
    pub subtype: String,
    /// The SQL expression responsible, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub masking: bool,
}

/// Phlo-specific dataset metadata. The schema lives at
/// `schemas/facets/1-0-0/PhloDatasetFacet.json`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhloDatasetFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    /// `dataset://` URI — the canonical Phlo identity.
    pub uri: String,
    /// `model`, `source`, `seed` or `external`.
    pub kind: String,
    /// The model's materialization, for model-output datasets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialization: Option<String>,
    /// Workspace-relative seed CSV path, for seeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Lineage confidence per output field, when recorded.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub field_confidence: BTreeMap<String, String>,
}

/// Exports a [`LineageGraph`] as an OpenLineage document.
pub struct OpenLineageExporter<'a> {
    graph: &'a LineageGraph,
    /// The OpenLineage namespace for every job and dataset.
    namespace: String,
    /// The `eventTime` stamped on every event — the export moment by
    /// default; overridable for deterministic output.
    event_time: String,
}

impl<'a> OpenLineageExporter<'a> {
    /// Export with the default `phlo` namespace; `eventTime` is now.
    pub fn new(graph: &'a LineageGraph) -> Self {
        Self {
            graph,
            namespace: "phlo".to_string(),
            event_time: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default(),
        }
    }

    /// Export with a caller-chosen OpenLineage namespace.
    pub fn with_namespace(graph: &'a LineageGraph, namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            ..Self::new(graph)
        }
    }

    /// Pin the `eventTime` stamped on every event — for deterministic
    /// exports and tests.
    pub fn with_event_time(mut self, event_time: impl Into<String>) -> Self {
        self.event_time = event_time.into();
        self
    }

    /// Build the design-time lineage document.
    pub fn export(&self) -> OpenLineageDocument {
        let mut jobs = Vec::new();
        let mut datasets = Vec::new();

        for (node, meta) in self.graph.nodes() {
            match node {
                LineageNode::Model(id) => jobs.push(self.job_event(id, meta)),
                LineageNode::Dataset(id) => datasets.push(DatasetEvent {
                    event_time: self.event_time.clone(),
                    producer: PRODUCER.to_string(),
                    schema_url: DATASET_EVENT_SCHEMA_URL.to_string(),
                    dataset: StaticDataset {
                        dataset: self.dataset(id, meta, None),
                    },
                }),
                LineageNode::Column(_) | LineageNode::Test(_) => {}
            }
        }

        jobs.sort_by(|left, right| left.job.name.cmp(&right.job.name));
        datasets.sort_by(|left, right| left.dataset.dataset.name.cmp(&right.dataset.dataset.name));
        OpenLineageDocument {
            events: jobs
                .into_iter()
                .map(OpenLineageEvent::Job)
                .chain(datasets.into_iter().map(OpenLineageEvent::Dataset))
                .collect(),
        }
    }

    fn job_event(
        &self,
        id: &phlo_transform_core::ModelId,
        meta: &phlo_transform_core::NodeMeta,
    ) -> JobEvent {
        let inputs = self
            .graph
            .input_datasets(id)
            .into_iter()
            .map(|dataset| {
                let meta = self
                    .graph
                    .node(&LineageNode::Dataset(dataset.clone()))
                    .map(|(_, meta)| meta);
                InputDataset {
                    dataset: self.dataset(&dataset, meta.unwrap_or(&EMPTY_META), None),
                }
            })
            .collect();

        let output_id = self.graph.output_dataset(id);
        let output_meta = self
            .graph
            .node(&LineageNode::Dataset(output_id.clone()))
            .map(|(_, meta)| meta)
            .unwrap_or(&EMPTY_META);
        let column_lineage = self.column_lineage(&output_id);
        let outputs = vec![OutputDataset {
            dataset: self.dataset(&output_id, output_meta, column_lineage),
        }];

        let facets = JobFacets {
            job_type: Some(JobTypeFacet {
                producer: PRODUCER.to_string(),
                schema_url: facet_schema_url::JOB_TYPE.to_string(),
                processing_type: "BATCH".to_string(),
                integration: "PHLO".to_string(),
                job_type: "MODEL".to_string(),
            }),
            phlo_job: Some(PhloJobFacet {
                producer: PRODUCER.to_string(),
                schema_url: facet_schema_url::PHLO_JOB.to_string(),
                uri: id.uri(),
                path: meta.path.clone(),
                materialization: meta.materialization.clone().unwrap_or_default(),
                workflow: meta.workflow.clone(),
                version: meta.version.clone().unwrap_or_default(),
            }),
        };

        JobEvent {
            event_time: self.event_time.clone(),
            producer: PRODUCER.to_string(),
            schema_url: JOB_EVENT_SCHEMA_URL.to_string(),
            job: Job {
                namespace: self.namespace.clone(),
                name: id.logical_name(),
                facets,
            },
            inputs,
            outputs,
        }
    }

    fn dataset(
        &self,
        id: &DatasetId,
        meta: &phlo_transform_core::NodeMeta,
        column_lineage: Option<ColumnLineageFacet>,
    ) -> Dataset {
        let mut facets = DatasetFacets::default();

        let columns = self.graph.dataset_columns(id);
        if !columns.is_empty() {
            facets.schema = Some(SchemaFacet {
                producer: PRODUCER.to_string(),
                schema_url: facet_schema_url::SCHEMA.to_string(),
                fields: columns
                    .iter()
                    .map(|column| {
                        let column_meta = self
                            .graph
                            .node(&LineageNode::Column(column.clone()))
                            .map(|(_, meta)| meta);
                        SchemaField {
                            name: column.name.clone(),
                            data_type: column_meta.and_then(|meta| meta.data_type.clone()),
                            description: None,
                        }
                    })
                    .collect(),
            });
        }

        let (dataset_type, sub_type) = dataset_type(meta);
        facets.dataset_type = Some(DatasetTypeFacet {
            producer: PRODUCER.to_string(),
            schema_url: facet_schema_url::DATASET_TYPE.to_string(),
            dataset_type: dataset_type.to_string(),
            sub_type: sub_type.map(str::to_string),
        });

        // Only materializations that create a physical relation get a
        // symlink; ephemeral outputs never point at a warehouse relation.
        if let Some(target) = &meta.target {
            facets.symlinks = Some(SymlinksFacet {
                producer: PRODUCER.to_string(),
                schema_url: facet_schema_url::SYMLINKS.to_string(),
                identifiers: vec![SymlinkIdentifier {
                    namespace: self.namespace.clone(),
                    name: target.clone(),
                    identifier_type: match meta.materialization.as_deref() {
                        Some("view") => "view",
                        _ => "table",
                    }
                    .to_string(),
                }],
            });
        }

        facets.phlo_dataset = Some(PhloDatasetFacet {
            producer: PRODUCER.to_string(),
            schema_url: facet_schema_url::PHLO_DATASET.to_string(),
            uri: id.uri(),
            kind: match meta.dataset_kind {
                Some(DatasetKind::Model) => "model",
                Some(DatasetKind::Source) => "source",
                Some(DatasetKind::Seed) => "seed",
                Some(DatasetKind::External) | None => "external",
            }
            .to_string(),
            materialization: meta.materialization.clone(),
            path: meta.path.clone(),
            field_confidence: columns
                .iter()
                .filter_map(|column| {
                    self.graph
                        .node(&LineageNode::Column(column.clone()))
                        .and_then(|(_, meta)| meta.confidence)
                        .map(|confidence| (column.name.clone(), confidence_name(confidence)))
                })
                .collect(),
        });

        facets.column_lineage = column_lineage;

        Dataset {
            namespace: self.namespace.clone(),
            name: id.name(),
            facets,
        }
    }

    /// The `columnLineage` facet for a model's output dataset.
    fn column_lineage(&self, output: &DatasetId) -> Option<ColumnLineageFacet> {
        let mut fields: BTreeMap<String, ColumnLineageField> = BTreeMap::new();
        for column in self.graph.dataset_columns(output) {
            let upstream = self.graph.column_upstream(&column, true);
            if upstream.is_empty() {
                continue;
            }
            // Merge repeated edges to the same input field — a column can
            // be both a direct input and an indirect one.
            let mut input_fields: Vec<InputField> = Vec::new();
            for (input, edge) in upstream {
                let name = input.dataset.name();
                let record = TransformationRecord {
                    record_type: match edge.directness {
                        Some(Directness::Direct) => "DIRECT",
                        _ => "INDIRECT",
                    }
                    .to_string(),
                    subtype: edge
                        .transformation
                        .map(transformation_name)
                        .unwrap_or("TRANSFORMATION")
                        .to_string(),
                    description: edge.expression.clone(),
                    masking: false,
                };
                match input_fields
                    .iter()
                    .position(|field| field.name == name && field.field == input.name)
                {
                    Some(position) => input_fields[position].transformations.push(record),
                    None => input_fields.push(InputField {
                        namespace: self.namespace.clone(),
                        name,
                        field: input.name.clone(),
                        transformations: vec![record],
                    }),
                }
            }
            input_fields
                .sort_by(|left, right| (&left.name, &left.field).cmp(&(&right.name, &right.field)));
            fields.insert(column.name.clone(), ColumnLineageField { input_fields });
        }
        if fields.is_empty() {
            None
        } else {
            Some(ColumnLineageFacet {
                producer: PRODUCER.to_string(),
                schema_url: facet_schema_url::COLUMN_LINEAGE.to_string(),
                fields,
            })
        }
    }
}

/// The OpenLineage `datasetType`/`subType` for a dataset, from its kind and
/// the producing model's materialization.
fn dataset_type(meta: &phlo_transform_core::NodeMeta) -> (&'static str, Option<&'static str>) {
    match (meta.dataset_kind, meta.materialization.as_deref()) {
        // Ephemeral models compile into dependents; their dataset is a
        // job-scoped intermediate, never a physical relation.
        (Some(DatasetKind::Model), Some("ephemeral")) => ("JOB_OUTPUT", Some("TEMPORARY")),
        (Some(DatasetKind::Model), Some("view")) => ("VIEW", None),
        (Some(DatasetKind::Model), _) => ("TABLE", None),
        (Some(DatasetKind::Seed), _) => ("TABLE", None),
        // Declared sources and future ingestion-contributed datasets are
        // external to this workspace.
        _ => ("TABLE", Some("EXTERNAL")),
    }
}

fn transformation_name(transformation: Transformation) -> &'static str {
    match transformation {
        Transformation::Identity => "IDENTITY",
        Transformation::Transformation => "TRANSFORMATION",
        Transformation::Aggregation => "AGGREGATION",
        Transformation::Join => "JOIN",
        Transformation::Filter => "FILTER",
        Transformation::GroupBy => "GROUP_BY",
        Transformation::Sort => "SORT",
        Transformation::Window => "WINDOW",
        Transformation::Conditional => "CONDITIONAL",
    }
}

fn confidence_name(confidence: phlo_transform_core::LineageConfidence) -> String {
    use phlo_transform_core::LineageConfidence::*;
    match confidence {
        Exact => "exact",
        Inferred => "inferred",
        Declared => "declared",
        Runtime => "runtime",
        Unknown => "unknown",
    }
    .to_string()
}

static EMPTY_META: phlo_transform_core::NodeMeta = phlo_transform_core::NodeMeta {
    path: None,
    version: None,
    dataset_kind: None,
    target: None,
    data_type: None,
    nullability: None,
    confidence: None,
    generated: false,
    materialization: None,
    workflow: None,
};
