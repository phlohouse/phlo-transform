//! Canonical lineage graph tests built from real SQL fixtures.
//!
//! The graph is compiled from SQL, not constructed by hand, so these tests
//! exercise the full `Compilation -> LineageGraph` path: node kinds, edge
//! kinds, column-level direct/indirect/transformation metadata, confidence
//! degradation, traversal indexes, scoping and serialisation.

use std::collections::BTreeSet;
use std::path::PathBuf;

use phlo_transform_core::{
    compile_with_provider, Compilation, DataType, DatasetColumn, DatasetId, DatasetKind,
    Directness, LineageConfidence, LineageEdgeKind, LineageNode, Materialization, ModelId,
    ModelOrigin, Nullability, RelationSchema, SchemaColumn, SemanticModel, SemanticProject,
    SemanticSeed, SemanticTest, StaticSchemaProvider, TestId, Transformation,
};

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
            column("captured", DataType::Timestamp, Nullability::Nullable),
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

fn ephemeral(name: &str, sql: &str) -> SemanticModel {
    let mut model = model(name, sql);
    model.config.materialization = Materialization::Ephemeral;
    model
}

/// The fixture workspace: a source-read model, a transforming model with a
/// filter, an aggregate, a join, an ephemeral hop, a seed consumer, and a
/// model over an unknown source.
fn compile() -> Compilation {
    let project = SemanticProject {
        models: vec![
            model(
                "assay.raw",
                "select id, titre, sample_type, captured from external.raw",
            ),
            model(
                "assay.clean",
                "select id as sample_id, ln(titre) as log_titre, sample_type \
                 from assay.raw where sample_type = 'cell'",
            ),
            model(
                "assay.daily",
                "select sample_type, count(*) as n, avg(log_titre) as mean_titre \
                 from assay.clean group by sample_type order by sample_type",
            ),
            model(
                "assay.joined",
                "select c.sample_id, c.log_titre, s.batch \
                 from assay.clean c join external.samples s \
                 on c.sample_id = s.sample_id",
            ),
            ephemeral(
                "assay.scaled",
                "select id, titre * 2 as scaled_titre from assay.raw",
            ),
            model("assay.marts", "select id, scaled_titre from assay.scaled"),
            model("assay.seed_read", "select id, level from controls"),
            model("assay.mystery", "select value from external.mystery"),
        ],
        tests: vec![SemanticTest {
            id: TestId::new("check_clean"),
            sql: "select * from assay.clean where log_titre is null".to_string(),
            origin: ModelOrigin::in_memory(),
        }],
        seeds: vec![SemanticSeed {
            name: "controls".to_string(),
            path: PathBuf::from("seeds/controls.csv"),
            schema: None,
            content_hash: "abc123".to_string(),
            columns: vec!["id".to_string(), "level".to_string()],
        }],
        ..SemanticProject::in_memory(vec![])
    };
    compile_with_provider(&project, &provider())
}

fn id(name: &str) -> ModelId {
    ModelId::parse(name).unwrap()
}

fn dataset(name: &str) -> DatasetId {
    DatasetId::model(&id(name))
}

fn source_dataset(name: &str) -> DatasetId {
    DatasetId::source(
        &phlo_transform_core::SourceId::new(name.split('.').map(str::to_string).collect()).unwrap(),
    )
}

fn col(dataset: DatasetId, name: &str) -> DatasetColumn {
    DatasetColumn {
        dataset,
        name: name.to_string(),
    }
}

fn has_edge(
    compilation: &Compilation,
    from: LineageNode,
    to: LineageNode,
    kind: LineageEdgeKind,
) -> bool {
    compilation
        .lineage
        .edges()
        .iter()
        .any(|(left, right, edge)| *left == &from && *right == &to && edge.kind == kind)
}

#[test]
fn model_to_model_lineage() {
    let compilation = compile();
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    let graph = &compilation.lineage;

    // Model-level and dataset-level derives rollup edges both exist.
    assert!(has_edge(
        &compilation,
        LineageNode::Model(id("assay.raw")),
        LineageNode::Model(id("assay.clean")),
        LineageEdgeKind::Derives,
    ));
    assert!(has_edge(
        &compilation,
        LineageNode::Dataset(dataset("assay.raw")),
        LineageNode::Dataset(dataset("assay.clean")),
        LineageEdgeKind::Derives,
    ));
    // The canonical input edge: the upstream model's output dataset is an
    // input of the downstream model — identical to how a source connects.
    assert!(has_edge(
        &compilation,
        LineageNode::Dataset(dataset("assay.raw")),
        LineageNode::Model(id("assay.clean")),
        LineageEdgeKind::Input,
    ));
    // The model produces its output dataset.
    assert!(has_edge(
        &compilation,
        LineageNode::Model(id("assay.clean")),
        LineageNode::Dataset(dataset("assay.clean")),
        LineageEdgeKind::Output,
    ));
    assert_eq!(
        graph.dataset_producer(&dataset("assay.clean")),
        Some(id("assay.clean"))
    );
}

#[test]
fn source_to_model_lineage() {
    let compilation = compile();
    let graph = &compilation.lineage;
    let raw_source = source_dataset("external.raw");

    assert!(has_edge(
        &compilation,
        LineageNode::Dataset(raw_source.clone()),
        LineageNode::Model(id("assay.raw")),
        LineageEdgeKind::Input,
    ));
    assert!(has_edge(
        &compilation,
        LineageNode::Dataset(raw_source.clone()),
        LineageNode::Dataset(dataset("assay.raw")),
        LineageEdgeKind::Derives,
    ));
    let (_, meta) = graph
        .node(&LineageNode::Dataset(raw_source.clone()))
        .unwrap();
    assert_eq!(meta.dataset_kind, Some(DatasetKind::Source));

    // The model reads both the upstream model's output and the source chain.
    assert_eq!(
        graph.input_datasets(&id("assay.clean")),
        vec![dataset("assay.raw")]
    );
    assert_eq!(
        graph.input_datasets(&id("assay.joined")),
        vec![dataset("assay.clean"), source_dataset("external.samples")]
    );
}

#[test]
fn seed_datasets_carry_seed_kind_and_schema() {
    let compilation = compile();
    let graph = &compilation.lineage;
    let controls = source_dataset("controls");

    let (_, meta) = graph.node(&LineageNode::Dataset(controls.clone())).unwrap();
    assert_eq!(meta.dataset_kind, Some(DatasetKind::Seed));
    assert_eq!(meta.path.as_deref(), Some("seeds/controls.csv"));

    // Seed header columns feed the consumer's column lineage.
    let upstream = graph.column_upstream(&col(controls.clone(), "id"), false);
    assert!(upstream.is_empty());
    let downstream = graph.column_downstream(&col(controls, "level"), false);
    assert_eq!(
        downstream
            .iter()
            .map(|(column, _)| column.uri())
            .collect::<Vec<_>>(),
        vec!["dataset://assay/seed_read#level".to_string()]
    );
}

#[test]
fn nested_ephemeral_lineage() {
    let compilation = compile();
    let graph = &compilation.lineage;

    // The ephemeral model is a model node with its own output dataset.
    let (_, meta) = graph.node(&LineageNode::Model(id("assay.scaled"))).unwrap();
    assert_eq!(meta.materialization.as_deref(), Some("ephemeral"));

    // `marts.scaled_titre` flows through the ephemeral hop to `external.raw`.
    let upstream =
        graph.column_upstream_transitive(&col(dataset("assay.marts"), "scaled_titre"), false);
    let uris: Vec<String> = upstream.iter().map(|(column, _)| column.uri()).collect();
    // The value flows through the ephemeral hop and the staging model.
    assert_eq!(
        uris,
        vec![
            "dataset://assay/raw#titre".to_string(),
            "dataset://assay/scaled#scaled_titre".to_string(),
            "dataset://external/raw#titre".to_string(),
        ]
    );
    // The ephemeral hop carries the transformation, not the consumer.
    let (_, edge) = upstream
        .iter()
        .find(|(column, _)| column.uri() == "dataset://assay/scaled#scaled_titre")
        .unwrap();
    assert_eq!(edge.transformation, Some(Transformation::Identity));
}

#[test]
fn aliased_identity_column() {
    let compilation = compile();
    let graph = &compilation.lineage;

    let upstream = graph.column_upstream(&col(dataset("assay.clean"), "sample_id"), false);
    let inputs: Vec<(String, Option<Transformation>, Option<Directness>)> = upstream
        .iter()
        .map(|(column, edge)| (column.uri(), edge.transformation, edge.directness))
        .collect();
    assert_eq!(
        inputs,
        vec![(
            "dataset://assay/raw#id".to_string(),
            Some(Transformation::Identity),
            Some(Directness::Direct),
        )]
    );
}

#[test]
fn expression_transformation_column() {
    let compilation = compile();
    let graph = &compilation.lineage;

    let upstream = graph.column_upstream(&col(dataset("assay.clean"), "log_titre"), false);
    let direct: Vec<(String, Option<Transformation>)> = upstream
        .iter()
        .filter(|(_, edge)| edge.directness == Some(Directness::Direct))
        .map(|(column, edge)| (column.uri(), edge.transformation))
        .collect();
    assert_eq!(
        direct,
        vec![(
            "dataset://assay/raw#titre".to_string(),
            Some(Transformation::Transformation),
        )]
    );
}

#[test]
fn filter_columns_are_indirect_inputs() {
    let compilation = compile();
    let graph = &compilation.lineage;

    // `sample_type` feeds the WHERE clause, so it is an indirect input to
    // every output column — including outputs that project other columns.
    let upstream = graph.column_upstream(&col(dataset("assay.clean"), "sample_id"), true);
    let filter = upstream
        .iter()
        .find(|(column, _)| column.uri() == "dataset://assay/raw#sample_type")
        .map(|(_, edge)| edge);
    assert_eq!(
        filter.map(|edge| (edge.directness, edge.transformation)),
        Some((Some(Directness::Indirect), Some(Transformation::Filter)))
    );

    // Direct-only traversal hides the filter input.
    let direct_only = graph.column_upstream(&col(dataset("assay.clean"), "sample_id"), false);
    assert!(!direct_only
        .iter()
        .any(|(column, _)| column.name == "sample_type"));
}

#[test]
fn aggregation_and_grouping() {
    let compilation = compile();
    let graph = &compilation.lineage;

    let upstream = graph.column_upstream(&col(dataset("assay.daily"), "mean_titre"), true);
    let by_uri: std::collections::BTreeMap<String, &phlo_transform_core::LineageEdge> = upstream
        .iter()
        .map(|(column, edge)| (column.uri(), edge))
        .collect();

    // avg() is a direct aggregation over log_titre.
    let avg = by_uri.get("dataset://assay/clean#log_titre").unwrap();
    assert_eq!(
        (avg.directness, avg.transformation),
        (Some(Directness::Direct), Some(Transformation::Aggregation))
    );
    // The group-by key only influences which rows land in each group.
    let grouping = by_uri.get("dataset://assay/clean#sample_type").unwrap();
    assert_eq!(grouping.directness, Some(Directness::Indirect));
}

#[test]
fn join_keys_are_indirect_inputs() {
    let compilation = compile();
    let graph = &compilation.lineage;

    let upstream = graph.column_upstream(&col(dataset("assay.joined"), "log_titre"), true);
    let by_uri: std::collections::BTreeMap<String, &phlo_transform_core::LineageEdge> = upstream
        .iter()
        .map(|(column, edge)| (column.uri(), edge))
        .collect();

    let value = by_uri.get("dataset://assay/clean#log_titre").unwrap();
    assert_eq!(
        (value.directness, value.transformation),
        (Some(Directness::Direct), Some(Transformation::Identity))
    );
    // Both sides of the join predicate are indirect join inputs.
    for uri in [
        "dataset://assay/clean#sample_id",
        "dataset://external/samples#sample_id",
    ] {
        let edge = by_uri.get(uri).unwrap_or_else(|| panic!("missing {uri}"));
        assert_eq!(
            (edge.directness, edge.transformation),
            (Some(Directness::Indirect), Some(Transformation::Join)),
            "{uri}"
        );
    }
}

#[test]
fn unprovable_lineage_has_unknown_confidence() {
    let compilation = compile();
    let graph = &compilation.lineage;

    // `external.mystery` has no schema, so the output column's lineage is
    // recorded but degraded rather than presented as exact.
    let (_, meta) = graph
        .node(&LineageNode::Column(col(dataset("assay.mystery"), "value")))
        .unwrap();
    assert_eq!(meta.confidence, Some(LineageConfidence::Unknown));

    // Fully analysed columns stay exact.
    let (_, meta) = graph
        .node(&LineageNode::Column(col(
            dataset("assay.clean"),
            "log_titre",
        )))
        .unwrap();
    assert_eq!(meta.confidence, Some(LineageConfidence::Exact));
}

#[test]
fn tests_link_to_target_datasets() {
    let compilation = compile();
    let graph = &compilation.lineage;

    assert_eq!(
        graph.tests_for_dataset(&dataset("assay.clean")),
        vec![TestId::new("check_clean")]
    );
    // Tests consume the dataset: dataset → test, so impact traversal
    // reaches them without special-casing.
    assert!(has_edge(
        &compilation,
        LineageNode::Dataset(dataset("assay.clean")),
        LineageNode::Test(TestId::new("check_clean")),
        LineageEdgeKind::Tests,
    ));
    let impact = graph.impact(&LineageNode::Dataset(dataset("assay.clean")));
    assert!(impact.contains(&LineageNode::Test(TestId::new("check_clean"))));
    // Tests do not attach to datasets they do not read.
    assert!(graph.tests_for_dataset(&dataset("assay.raw")).is_empty());
}

#[test]
fn ephemeral_outputs_have_no_physical_target() {
    let compilation = compile();
    let graph = &compilation.lineage;

    // The ephemeral model's dataset carries its materialization but no
    // physical target — it must never be published as a relation.
    let (_, ephemeral_meta) = graph
        .node(&LineageNode::Dataset(dataset("assay.scaled")))
        .unwrap();
    assert_eq!(ephemeral_meta.materialization.as_deref(), Some("ephemeral"));
    assert_eq!(ephemeral_meta.target, None);

    // A default (view) model keeps its physical target.
    let (_, view_meta) = graph
        .node(&LineageNode::Dataset(dataset("assay.clean")))
        .unwrap();
    assert_eq!(view_meta.materialization.as_deref(), Some("view"));
    assert!(view_meta.target.is_some());
}

#[test]
fn traversal_indexes() {
    let compilation = compile();
    let graph = &compilation.lineage;

    // upstream(model) — one hop: the model reads assay.clean's output
    // dataset (canonical input edge) plus the model-level rollup edge.
    let upstream = graph.upstream(&LineageNode::Model(id("assay.daily")));
    assert_eq!(
        upstream,
        vec![
            LineageNode::Model(id("assay.clean")),
            LineageNode::Dataset(dataset("assay.clean")),
        ]
    );

    // upstream_transitive reaches the external source.
    let transitive = graph.upstream_transitive(&LineageNode::Dataset(dataset("assay.daily")));
    let uris: Vec<String> = transitive.iter().map(LineageNode::uri).collect();
    assert!(uris.contains(&"dataset://external/raw".to_string()));
    assert!(uris.contains(&"dataset://assay/raw".to_string()));

    // downstream(source) reaches every consuming model dataset.
    let downstream =
        graph.downstream_transitive(&LineageNode::Dataset(source_dataset("external.raw")));
    let uris: Vec<String> = downstream.iter().map(LineageNode::uri).collect();
    for expected in [
        "dataset://assay/raw",
        "dataset://assay/clean",
        "dataset://assay/daily",
        "dataset://assay/joined",
        "dataset://assay/scaled",
        "dataset://assay/marts",
    ] {
        assert!(uris.contains(&expected.to_string()), "missing {expected}");
    }

    // impact(model) returns the same downstream set.
    let impact = graph.impact(&LineageNode::Model(id("assay.clean")));
    assert_eq!(
        impact,
        graph.downstream_transitive(&LineageNode::Model(id("assay.clean")))
    );
    assert!(!impact.is_empty());
}

#[test]
fn scoped_documents_only_contain_selected_models() {
    let compilation = compile();
    let graph = &compilation.lineage;

    let scope: BTreeSet<ModelId> = [id("assay.clean")].into_iter().collect();
    let document = graph.document_for(&scope);
    let uris: BTreeSet<String> = document.nodes.iter().map(|node| node.uri.clone()).collect();

    assert!(uris.contains("model://assay/clean"));
    assert!(uris.contains("dataset://assay/clean"));
    assert!(uris.contains("dataset://assay/clean#log_titre"));
    // The input dataset and the columns feeding the selection are included.
    assert!(uris.contains("dataset://assay/raw"));
    assert!(uris.contains("dataset://assay/raw#titre"));
    assert!(uris.contains("test://check_clean"));
    // Downstream and unrelated models are out of scope.
    assert!(!uris.contains("model://assay/daily"));
    assert!(!uris.contains("dataset://assay/daily"));
    assert!(!uris.contains("dataset://external/samples"));
}

#[test]
fn graph_document_is_deterministic_and_serialisable() {
    let first = compile();
    let second = compile();

    let first_json = serde_json::to_value(first.lineage.document()).unwrap();
    let second_json = serde_json::to_value(second.lineage.document()).unwrap();
    assert_eq!(first_json, second_json);

    // Nodes are ordered deterministically by kind then URI; each kind's
    // block is URI-sorted.
    let nodes = first_json["nodes"].as_array().unwrap();
    let mut by_kind: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for node in nodes {
        by_kind
            .entry(node["kind"].as_str().unwrap())
            .or_default()
            .push(node["uri"].as_str().unwrap());
    }
    for (kind, uris) in &by_kind {
        let mut sorted = uris.clone();
        sorted.sort();
        assert_eq!(uris, &sorted, "{kind} nodes are not URI-sorted");
    }

    let edges = first_json["edges"].as_array().unwrap();
    let pairs: Vec<(&str, &str)> = edges
        .iter()
        .map(|edge| (edge["from"].as_str().unwrap(), edge["to"].as_str().unwrap()))
        .collect();
    let mut sorted_pairs = pairs.clone();
    sorted_pairs.sort();
    assert_eq!(pairs, sorted_pairs);

    // Column derives edges carry their semantic metadata.
    let derives = edges
        .iter()
        .find(|edge| {
            edge["from"] == "dataset://assay/raw#titre"
                && edge["to"] == "dataset://assay/clean#log_titre"
        })
        .unwrap();
    assert_eq!(derives["kind"], "derives");
    assert_eq!(derives["directness"], "direct");
    assert_eq!(derives["transformation"], "transformation");
    assert_eq!(derives["confidence"], "exact");
}

#[test]
fn source_column_impact_reaches_consumers() {
    let compilation = compile();
    let graph = &compilation.lineage;

    let downstream =
        graph.column_downstream_transitive(&col(source_dataset("external.raw"), "titre"), false);
    let uris: Vec<String> = downstream.iter().map(|(column, _)| column.uri()).collect();
    // titre -> raw.titre -> clean.log_titre -> daily.mean_titre (+ joined).
    for expected in [
        "dataset://assay/raw#titre",
        "dataset://assay/clean#log_titre",
        "dataset://assay/joined#log_titre",
        "dataset://assay/daily#mean_titre",
        "dataset://assay/scaled#scaled_titre",
        "dataset://assay/marts#scaled_titre",
    ] {
        assert!(uris.contains(&expected.to_string()), "missing {expected}");
    }
}
