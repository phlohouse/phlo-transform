//! OpenLineage export tests — the exported document is built from a real
//! compilation, never a hand-constructed graph. Every event is validated
//! against the vendored OpenLineage JSON schemas in `tests/spec/`.

use phlo_transform_core::{
    compile_with_provider, Compilation, DataType, ModelId, Nullability, RelationSchema,
    SchemaColumn, SemanticModel, SemanticProject, StaticSchemaProvider,
};
use phlo_transform_openlineage::{OpenLineageExporter, PRODUCER};

const EVENT_TIME: &str = "2026-01-01T00:00:00Z";

fn column(name: &str, data_type: DataType, nullability: Nullability) -> SchemaColumn {
    SchemaColumn {
        name: name.to_string(),
        data_type,
        nullability,
    }
}

fn provider() -> StaticSchemaProvider {
    let mut provider = StaticSchemaProvider::new();
    provider.insert(
        "external.raw",
        RelationSchema::new(vec![
            column("id", DataType::Varchar, Nullability::NotNull),
            column("titre", DataType::Double, Nullability::Nullable),
            column("sample_type", DataType::Varchar, Nullability::Nullable),
        ]),
    );
    provider.insert(
        "external.samples",
        RelationSchema::new(vec![
            column("sample_id", DataType::Varchar, Nullability::NotNull),
            column("batch", DataType::Varchar, Nullability::Nullable),
        ]),
    );
    provider
}

fn model(name: &str, sql: &str) -> SemanticModel {
    SemanticModel::in_memory(ModelId::parse(name).unwrap(), sql)
}

fn materialized(name: &str, sql: &str, materialization: &str) -> SemanticModel {
    let mut model = model(name, sql);
    model.config.materialization = match materialization {
        "view" => phlo_transform_sql::Materialization::View,
        "table" => phlo_transform_sql::Materialization::Table,
        "incremental" => phlo_transform_sql::Materialization::Incremental,
        "ephemeral" => phlo_transform_sql::Materialization::Ephemeral,
        other => panic!("unknown materialization {other}"),
    };
    model
}

fn compile() -> Compilation {
    let project = SemanticProject::in_memory(vec![
        model(
            "assay.raw",
            "select id, titre, sample_type from external.raw",
        ),
        model(
            "assay.clean",
            "select id as sample_id, ln(titre) as log_titre, sample_type \
             from assay.raw where sample_type = 'cell'",
        ),
        model(
            "assay.joined",
            "select c.sample_id, c.log_titre, s.batch \
             from assay.clean c join external.samples s \
             on c.sample_id = s.sample_id",
        ),
    ]);
    compile_with_provider(&project, &provider())
}

fn export(compilation: &Compilation) -> serde_json::Value {
    serde_json::to_value(
        OpenLineageExporter::new(&compilation.lineage)
            .with_event_time(EVENT_TIME)
            .export(),
    )
    .unwrap()
}

/// The job events in a document (the document is a bare event array).
fn jobs(document: &serde_json::Value) -> Vec<&serde_json::Value> {
    document
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event.get("job").is_some())
        .collect()
}

/// The dataset events in a document.
fn dataset_events(document: &serde_json::Value) -> Vec<&serde_json::Value> {
    document
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event.get("dataset").is_some())
        .collect()
}

/// A validator for every event and facet, resolving all schema references
/// offline from the vendored spec files.
struct Spec {
    registry: jsonschema::Registry<'static>,
}

impl Spec {
    fn new() -> Self {
        let mut builder = jsonschema::Registry::new();
        let files = [
            (
                "https://openlineage.io/spec/2-0-2/OpenLineage.json",
                include_str!("spec/OpenLineage.json"),
            ),
            // The 1-2-0 columnLineage facet still references this spec version.
            (
                "https://openlineage.io/spec/1-0-2/OpenLineage.json",
                include_str!("spec/OpenLineage-1-0-2.json"),
            ),
            (
                "https://openlineage.io/spec/facets/1-0-0/SchemaDatasetFacet.json",
                include_str!("spec/facets/1-0-0_SchemaDatasetFacet.json"),
            ),
            (
                "https://openlineage.io/spec/facets/1-0-1/DatasetTypeDatasetFacet.json",
                include_str!("spec/facets/1-0-1_DatasetTypeDatasetFacet.json"),
            ),
            (
                "https://openlineage.io/spec/facets/1-0-1/SymlinksDatasetFacet.json",
                include_str!("spec/facets/1-0-1_SymlinksDatasetFacet.json"),
            ),
            (
                "https://openlineage.io/spec/facets/1-2-0/ColumnLineageDatasetFacet.json",
                include_str!("spec/facets/1-2-0_ColumnLineageDatasetFacet.json"),
            ),
            (
                "https://openlineage.io/spec/facets/2-0-3/JobTypeJobFacet.json",
                include_str!("spec/facets/2-0-3_JobTypeJobFacet.json"),
            ),
            // The custom facet schemas published by this repository.
            (
                "https://raw.githubusercontent.com/phlohouse/phlo-transform/schemas-facets-1.0.0/schemas/facets/1-0-0/PhloJobFacet.json",
                include_str!("../../../schemas/facets/1-0-0/PhloJobFacet.json"),
            ),
            (
                "https://raw.githubusercontent.com/phlohouse/phlo-transform/schemas-facets-1.0.0/schemas/facets/1-0-0/PhloDatasetFacet.json",
                include_str!("../../../schemas/facets/1-0-0/PhloDatasetFacet.json"),
            ),
        ];
        for (uri, contents) in files {
            let schema: serde_json::Value =
                serde_json::from_str(contents).expect("vendored spec file parses");
            builder = builder
                .add(uri, schema)
                .expect("vendored spec file registers");
        }
        Self {
            registry: builder.prepare().expect("registry builds"),
        }
    }

    /// Assert `instance` validates against `$defs/$name` of the spec file
    /// registered at `base_uri`.
    fn assert_valid(&self, base_uri: &str, def: &str, instance: &serde_json::Value) {
        let schema = serde_json::json!({ "$ref": format!("{base_uri}#/$defs/{def}") });
        let validator = jsonschema::options()
            .with_registry(&self.registry)
            .build(&schema)
            .expect("validator builds");
        if let Err(error) = validator.validate(instance) {
            panic!("{def} validation failed: {error}\ninstance: {instance}");
        }
    }
}

/// Validate every event against its declared event schema and every facet
/// against the facet schema its `_schemaURL` names.
fn assert_document_valid(document: &serde_json::Value) {
    let spec = Spec::new();
    for event in document.as_array().unwrap() {
        // The event's schemaURL names the spec definition it must satisfy.
        let schema_url = event["schemaURL"].as_str().unwrap();
        let (base, def) = schema_url.split_once("#/$defs/").unwrap();
        spec.assert_valid(base, def, event);

        // Every facet validates against the schema its _schemaURL names.
        let mut facet_bags: Vec<&serde_json::Value> =
            vec![&event["job"]["facets"], &event["dataset"]["facets"]];
        for outputs in event["outputs"].as_array().into_iter().flatten() {
            facet_bags.push(&outputs["facets"]);
        }
        for inputs in event["inputs"].as_array().into_iter().flatten() {
            facet_bags.push(&inputs["facets"]);
        }
        for bag in facet_bags {
            for (name, facet) in bag.as_object().into_iter().flatten() {
                let url = facet["_schemaURL"]
                    .as_str()
                    .unwrap_or_else(|| panic!("facet {name} has no _schemaURL: {facet}"));
                let (base, def) = url.split_once("#/$defs/").unwrap();
                spec.assert_valid(base, def, facet);
            }
        }
    }
}

#[test]
fn every_event_is_schema_valid() {
    let compilation = compile();
    let document = export(&compilation);
    assert_document_valid(&document);
}

#[test]
fn dataset_level_export() {
    let compilation = compile();
    let document = export(&compilation);
    let jobs = jobs(&document);
    let datasets = dataset_events(&document);

    // One job per model, one dataset event per dataset.
    assert_eq!(jobs.len(), 3);
    let names: Vec<&str> = datasets
        .iter()
        .map(|event| event["dataset"]["name"].as_str().unwrap())
        .collect();
    for expected in [
        "assay.clean",
        "assay.joined",
        "assay.raw",
        "external.raw",
        "external.samples",
    ] {
        assert!(names.contains(&expected), "missing dataset {expected}");
    }

    // Every event carries the required BaseEvent fields.
    for event in document.as_array().unwrap() {
        assert_eq!(event["eventTime"], EVENT_TIME);
        assert_eq!(event["producer"], PRODUCER);
        assert!(
            event["schemaURL"]
                .as_str()
                .unwrap()
                .starts_with("https://openlineage.io/spec/"),
            "{}",
            event["schemaURL"]
        );
    }

    // Job inputs/outputs wire the model to its datasets.
    let clean = jobs
        .iter()
        .find(|job| job["job"]["name"] == "assay.clean")
        .unwrap();
    let inputs: Vec<&str> = clean["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dataset| dataset["name"].as_str().unwrap())
        .collect();
    assert_eq!(inputs, vec!["assay.raw"]);
    let outputs: Vec<&str> = clean["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dataset| dataset["name"].as_str().unwrap())
        .collect();
    assert_eq!(outputs, vec!["assay.clean"]);
    assert_eq!(clean["job"]["facets"]["jobType"]["jobType"], "MODEL");
    assert_eq!(
        clean["job"]["facets"]["jobType"]["_schemaURL"],
        "https://openlineage.io/spec/facets/2-0-3/JobTypeJobFacet.json#/$defs/JobTypeJobFacet"
    );
    assert_eq!(
        clean["job"]["facets"]["phlo_job"]["uri"],
        "model://assay/clean"
    );
    assert_eq!(
        clean["job"]["facets"]["phlo_job"]["materialization"],
        "view"
    );

    // Schemas and dataset kinds are exported; a source is an external table.
    let raw = datasets
        .iter()
        .find(|event| event["dataset"]["name"] == "external.raw")
        .unwrap();
    assert_eq!(raw["dataset"]["facets"]["phlo_dataset"]["kind"], "source");
    assert_eq!(
        raw["dataset"]["facets"]["datasetType"]["datasetType"],
        "TABLE"
    );
    assert_eq!(
        raw["dataset"]["facets"]["datasetType"]["subType"],
        "EXTERNAL"
    );
    let fields: Vec<&str> = raw["dataset"]["facets"]["schema"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["name"].as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["id", "sample_type", "titre"]);
}

#[test]
fn materialization_controls_dataset_type_and_symlinks() {
    let project = SemanticProject::in_memory(vec![
        materialized("assay.physical", "select id from external.raw", "table"),
        materialized("assay.staged", "select id from external.raw", "incremental"),
        materialized("assay.soft", "select id from external.raw", "view"),
        materialized("assay.inline", "select id from external.raw", "ephemeral"),
    ]);
    let compilation = compile_with_provider(&project, &provider());
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    let document = export(&compilation);

    let dataset_type = |name: &str| -> (String, Option<String>) {
        let event = dataset_events(&document)
            .into_iter()
            .find(|event| event["dataset"]["name"] == name)
            .unwrap_or_else(|| panic!("no dataset event for {name}"));
        let facet = &event["dataset"]["facets"]["datasetType"];
        (
            facet["datasetType"].as_str().unwrap().to_string(),
            facet["subType"].as_str().map(str::to_string),
        )
    };
    let has_symlink = |name: &str| -> bool {
        dataset_events(&document)
            .into_iter()
            .find(|event| event["dataset"]["name"] == name)
            .unwrap()["dataset"]["facets"]["symlinks"]
            .is_object()
    };

    assert_eq!(dataset_type("assay.physical"), ("TABLE".into(), None));
    assert_eq!(dataset_type("assay.staged"), ("TABLE".into(), None));
    assert_eq!(dataset_type("assay.soft"), ("VIEW".into(), None));
    assert_eq!(
        dataset_type("assay.inline"),
        ("JOB_OUTPUT".into(), Some("TEMPORARY".into()))
    );

    // Physical relations get a symlink; an ephemeral never does.
    assert!(has_symlink("assay.physical"));
    assert!(has_symlink("assay.staged"));
    assert!(has_symlink("assay.soft"));
    assert!(!has_symlink("assay.inline"));
}

#[test]
fn column_level_export() {
    let compilation = compile();
    let document = export(&compilation);
    let jobs = jobs(&document);

    let clean_job = jobs
        .iter()
        .find(|job| job["job"]["name"] == "assay.clean")
        .unwrap();
    let fields = &clean_job["outputs"][0]["facets"]["columnLineage"]["fields"];
    assert_eq!(
        clean_job["outputs"][0]["facets"]["columnLineage"]["_schemaURL"],
        "https://openlineage.io/spec/facets/1-2-0/ColumnLineageDatasetFacet.json#/$defs/ColumnLineageDatasetFacet"
    );

    // log_titre: direct transformation on raw.titre, indirect filter on
    // raw.sample_type.
    let log_titre_inputs = fields["log_titre"]["inputFields"].as_array().unwrap();
    let titre = log_titre_inputs
        .iter()
        .find(|input| input["field"] == "titre")
        .unwrap();
    assert_eq!(titre["name"], "assay.raw");
    assert_eq!(titre["transformations"][0]["type"], "DIRECT");
    assert_eq!(titre["transformations"][0]["subtype"], "TRANSFORMATION");
    let sample_type = log_titre_inputs
        .iter()
        .find(|input| input["field"] == "sample_type")
        .unwrap();
    assert_eq!(sample_type["transformations"][0]["type"], "INDIRECT");
    assert_eq!(sample_type["transformations"][0]["subtype"], "FILTER");

    // Aliased identity: sample_id derives directly from raw.id.
    let sample_id_inputs = fields["sample_id"]["inputFields"].as_array().unwrap();
    let id = sample_id_inputs
        .iter()
        .find(|input| input["field"] == "id")
        .unwrap();
    assert_eq!(id["transformations"][0]["subtype"], "IDENTITY");

    // Join keys show up as INDIRECT/JOIN inputs on joined.log_titre.
    let joined_job = jobs
        .iter()
        .find(|job| job["job"]["name"] == "assay.joined")
        .unwrap();
    let joined_fields = &joined_job["outputs"][0]["facets"]["columnLineage"]["fields"];
    let inputs = joined_fields["log_titre"]["inputFields"]
        .as_array()
        .unwrap();
    let join_keys: Vec<(&str, &str)> = inputs
        .iter()
        .filter(|input| {
            input["transformations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["subtype"] == "JOIN")
        })
        .map(|input| {
            (
                input["name"].as_str().unwrap(),
                input["field"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        join_keys,
        vec![
            ("assay.clean", "sample_id"),
            ("external.samples", "sample_id")
        ]
    );
}

#[test]
fn export_is_deterministic() {
    let first = compile();
    let second = compile();
    let first_json = export(&first);
    let second_json = export(&second);
    assert_eq!(first_json, second_json);
}
