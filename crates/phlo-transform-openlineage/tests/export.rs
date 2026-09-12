//! OpenLineage export tests — the exported document is built from a real
//! compilation, never a hand-constructed graph.

use phlo_transform_core::{
    compile_with_provider, Compilation, DataType, ModelId, Nullability, RelationSchema,
    SchemaColumn, SemanticModel, SemanticProject, StaticSchemaProvider,
};
use phlo_transform_openlineage::{OpenLineageExporter, PRODUCER};

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

#[test]
fn dataset_level_export() {
    let compilation = compile();
    let document = OpenLineageExporter::new(&compilation.lineage).export();
    let json = serde_json::to_value(&document).unwrap();

    assert_eq!(json["producer"], PRODUCER);
    let jobs = json["jobs"].as_array().unwrap();
    let datasets = json["datasets"].as_array().unwrap();

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
    assert_eq!(clean["job"]["facets"]["phlo"]["uri"], "model://assay/clean");

    // Schemas and dataset kinds are exported.
    let raw = datasets
        .iter()
        .find(|event| event["dataset"]["name"] == "external.raw")
        .unwrap();
    assert_eq!(raw["dataset"]["facets"]["phlo"]["kind"], "source");
    let fields: Vec<&str> = raw["dataset"]["facets"]["schema"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["name"].as_str().unwrap())
        .collect();
    assert_eq!(fields, vec!["id", "sample_type", "titre"]);
}

#[test]
fn column_level_export() {
    let compilation = compile();
    let document = OpenLineageExporter::new(&compilation.lineage).export();
    let json = serde_json::to_value(&document).unwrap();

    let clean_job = json["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["job"]["name"] == "assay.clean")
        .unwrap();
    let fields = &clean_job["outputs"][0]["facets"]["columnLineage"]["fields"];

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
    let joined_job = json["jobs"]
        .as_array()
        .unwrap()
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
    let first_json =
        serde_json::to_value(OpenLineageExporter::new(&first.lineage).export()).unwrap();
    let second_json =
        serde_json::to_value(OpenLineageExporter::new(&second.lineage).export()).unwrap();
    assert_eq!(first_json, second_json);
}
