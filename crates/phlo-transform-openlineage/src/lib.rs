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
//! run that happened. The document therefore contains `JobEvent`s (one per
//! transform model, with input and output datasets and column lineage) and
//! `DatasetEvent`s (one per dataset, carrying schema and Phlo identity).
//! No `RunEvent` is fabricated.
//!
//! ## Mapping
//!
//! * Model → `Job` (`namespace: phlo`, `name: assay.results`) with
//!   `sourceCodeLocation` (file path + content hash) and a custom `phlo`
//!   job facet carrying materialization, workflow and version.
//! * Dataset (model output, source, seed) → `Dataset` with `schema`,
//!   `datasetType`, `symlinks` (the physical target relation when known)
//!   and a custom `phlo` dataset facet carrying the `dataset://` URI and
//!   dataset kind.
//! * Column `derives` edges → the `columnLineage` facet on each output
//!   dataset inside its job. [`Directness`] maps to `DIRECT`/`INDIRECT`
//!   and [`Transformation`] to the OpenLineage subtype vocabulary
//!   (`IDENTITY`, `TRANSFORMATION`, `AGGREGATION`, `JOIN`, `FILTER`,
//!   `GROUP_BY`, `SORT`, `WINDOW`, `CONDITIONAL`). Lineage confidence has
//!   no standard equivalent; it is reported per field in the `phlo`
//!   dataset facet.
//! * Test → a `phlo` facet is not needed: tests stay internal. Quality
//!   assertions become relevant to OpenLineage consumers through dataset
//!   facets like `dataQualityAssertions` later, when assertions carry
//!   enough metadata to map cleanly.

use std::collections::BTreeMap;

use serde::Serialize;

use phlo_transform_core::{
    DatasetId, DatasetKind, Directness, LineageGraph, LineageNode, Transformation,
};

/// The producer URI recorded on every facet and the document.
pub const PRODUCER: &str = "https://github.com/phlohouse/phlo-transform";

/// OpenLineage schema version this exporter targets.
pub const SCHEMA_URL: &str = "https://openlineage.io/spec/2-0-2/OpenLineage.json#/$defs/JobEvent";

/// The design-time lineage document: jobs plus standalone datasets.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenLineageDocument {
    pub producer: String,
    pub schema_url: String,
    /// One event per transform model.
    pub jobs: Vec<JobEvent>,
    /// One event per dataset (model outputs, sources and seeds).
    pub datasets: Vec<DatasetEvent>,
}

/// A design-time `JobEvent`: a model, its input and output datasets.
#[derive(Clone, Debug, Serialize)]
pub struct JobEvent {
    pub job: Job,
    pub inputs: Vec<Dataset>,
    pub outputs: Vec<Dataset>,
}

/// A design-time `DatasetEvent`: standalone dataset metadata.
#[derive(Clone, Debug, Serialize)]
pub struct DatasetEvent {
    pub dataset: Dataset,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_code_location: Option<SourceCodeLocationFacet>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phlo: Option<PhloJobFacet>,
}

impl JobFacets {
    fn is_empty(&self) -> bool {
        self.job_type.is_none() && self.source_code_location.is_none() && self.phlo.is_none()
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

/// The `sourceCodeLocation` job facet: where the model's SQL lives.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceCodeLocationFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    #[serde(rename = "type")]
    pub location_type: String,
    pub path: String,
    /// The content-addressed model version — not a VCS revision, but the
    /// stable identity of what was compiled.
    pub version: String,
}

/// Phlo-specific job metadata with no standard OpenLineage equivalent.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhloJobFacet {
    #[serde(rename = "_producer")]
    pub producer: String,
    #[serde(rename = "_schemaURL")]
    pub schema_url: String,
    /// `model://` URI — the logical identity.
    pub uri: String,
    pub materialization: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Content-addressed desired version.
    pub version: String,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phlo: Option<PhloDatasetFacet>,
}

impl DatasetFacets {
    fn is_empty(&self) -> bool {
        self.schema.is_none()
            && self.dataset_type.is_none()
            && self.symlinks.is_none()
            && self.column_lineage.is_none()
            && self.phlo.is_none()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transformation_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transformation_type: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InputField {
    pub namespace: String,
    /// The input dataset name.
    pub name: String,
    /// The input column.
    pub field: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
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

/// Phlo-specific dataset metadata.
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
}

impl<'a> OpenLineageExporter<'a> {
    /// Export with the default `phlo` namespace.
    pub fn new(graph: &'a LineageGraph) -> Self {
        Self {
            graph,
            namespace: "phlo".to_string(),
        }
    }

    /// Export with a caller-chosen OpenLineage namespace.
    pub fn with_namespace(graph: &'a LineageGraph, namespace: impl Into<String>) -> Self {
        Self {
            graph,
            namespace: namespace.into(),
        }
    }

    /// Build the design-time lineage document.
    pub fn export(&self) -> OpenLineageDocument {
        let mut jobs = Vec::new();
        let mut datasets = Vec::new();

        for (node, meta) in self.graph.nodes() {
            match node {
                LineageNode::Model(id) => jobs.push(self.job_event(id, meta)),
                LineageNode::Dataset(id) => {
                    datasets.push(DatasetEvent {
                        dataset: self.dataset(id, meta, None),
                    });
                }
                LineageNode::Column(_) | LineageNode::Test(_) => {}
            }
        }

        jobs.sort_by(|left, right| left.job.name.cmp(&right.job.name));
        datasets.sort_by(|left, right| left.dataset.name.cmp(&right.dataset.name));
        OpenLineageDocument {
            producer: PRODUCER.to_string(),
            schema_url: SCHEMA_URL.to_string(),
            jobs,
            datasets,
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
                self.dataset(&dataset, meta.unwrap_or(&EMPTY_META), None)
            })
            .collect();

        let output_id = self.graph.output_dataset(id);
        let output_meta = self
            .graph
            .node(&LineageNode::Dataset(output_id.clone()))
            .map(|(_, meta)| meta)
            .unwrap_or(&EMPTY_META);
        let column_lineage = self.column_lineage(&output_id);
        let outputs = vec![self.dataset(&output_id, output_meta, column_lineage)];

        let mut facets = JobFacets {
            job_type: Some(JobTypeFacet {
                producer: PRODUCER.to_string(),
                schema_url: SCHEMA_URL.to_string(),
                processing_type: "BATCH".to_string(),
                integration: "PHLO".to_string(),
                job_type: "MODEL".to_string(),
            }),
            ..JobFacets::default()
        };
        if let Some(path) = &meta.path {
            facets.source_code_location = Some(SourceCodeLocationFacet {
                producer: PRODUCER.to_string(),
                schema_url: SCHEMA_URL.to_string(),
                location_type: "file".to_string(),
                path: path.clone(),
                version: meta.version.clone().unwrap_or_default(),
            });
        }
        facets.phlo = Some(PhloJobFacet {
            producer: PRODUCER.to_string(),
            schema_url: SCHEMA_URL.to_string(),
            uri: id.uri(),
            materialization: meta.materialization.clone().unwrap_or_default(),
            workflow: meta.workflow.clone(),
            version: meta.version.clone().unwrap_or_default(),
        });

        JobEvent {
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
                schema_url: SCHEMA_URL.to_string(),
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

        facets.dataset_type = Some(DatasetTypeFacet {
            producer: PRODUCER.to_string(),
            schema_url: SCHEMA_URL.to_string(),
            dataset_type: "TABLE".to_string(),
            sub_type: None,
        });

        if let Some(target) = &meta.target {
            facets.symlinks = Some(SymlinksFacet {
                producer: PRODUCER.to_string(),
                schema_url: SCHEMA_URL.to_string(),
                identifiers: vec![SymlinkIdentifier {
                    namespace: self.namespace.clone(),
                    name: target.clone(),
                    identifier_type: "TABLE".to_string(),
                }],
            });
        }

        facets.phlo = Some(PhloDatasetFacet {
            producer: PRODUCER.to_string(),
            schema_url: SCHEMA_URL.to_string(),
            uri: id.uri(),
            kind: match meta.dataset_kind {
                Some(DatasetKind::Model) => "model",
                Some(DatasetKind::Source) => "source",
                Some(DatasetKind::Seed) => "seed",
                Some(DatasetKind::External) | None => "external",
            }
            .to_string(),
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
                let namespace = self.namespace.clone();
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
                        namespace,
                        name,
                        field: input.name.clone(),
                        transformations: vec![record],
                    }),
                }
            }
            input_fields
                .sort_by(|left, right| (&left.name, &left.field).cmp(&(&right.name, &right.field)));
            fields.insert(
                column.name.clone(),
                ColumnLineageField {
                    input_fields,
                    transformation_description: None,
                    transformation_type: None,
                },
            );
        }
        if fields.is_empty() {
            None
        } else {
            Some(ColumnLineageFacet {
                producer: PRODUCER.to_string(),
                schema_url: SCHEMA_URL.to_string(),
                fields,
            })
        }
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
