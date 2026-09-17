#!/usr/bin/env bash
# Dogfood phlo-transform against a generated ~120-model workspace.
#
# Runs the workflows a real project exercises: check, plan, run, test,
# lineage, impact, git-aware --since, deliberate failures, --resume and
# --retry-failed — twice, to catch surprising state behaviour on repeat.
#
# With PHLO_NESSIE_ENDPOINT set, also exercises the environment workflow:
# ref create, run --ref, branch diff, promote.
#
# Usage: scripts/dogfood.sh [WORKDIR]
#   WORKDIR defaults to a fresh temp dir; the generated workspace lands in
#   $WORKDIR/dogfood. Set PHLO to a phlo-transform binary (default:
#   target/debug/phlo-transform, built if absent).

set -uo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
PHLO="${PHLO:-$REPO/target/debug/phlo-transform}"
WORKDIR="${1:-$(mktemp -d /tmp/phlo-dogfood.XXXXXX)}"
WS="$WORKDIR/dogfood"
PASS=0
FAIL=0
FAILED_STEPS=()

step() { printf '\n=== %s ===\n' "$*"; }
record() {
    if [ "$1" -eq 0 ]; then PASS=$((PASS + 1)); else
        FAIL=$((FAIL + 1)); FAILED_STEPS+=("$2");
    fi
}

[ -x "$PHLO" ] || cargo build -p phlo-transform-cli --manifest-path "$REPO/Cargo.toml" || {
    echo "could not build phlo-transform"; exit 1;
}

step "generate workspace"
python3 "$REPO/scripts/dogfood_workspace.py" "$WS" --models 120
record $? "generate"

cd "$WS" || exit 1
git init -q && git add -A && git -c user.email=dogfood@local -c user.name=dogfood commit -qm init
DB="--adapter duckdb --duckdb-path ./local.duckdb"

step "check"
$PHLO check >/dev/null 2>&1
record $? "check"

step "list"
$PHLO list >/dev/null 2>&1
record $? "list"

step "run 1 (cold)"
$PHLO run $DB >/dev/null 2>&1
record $? "run-cold"

step "run 2 (warm — everything should skip)"
WARM="$($PHLO run $DB 2>&1)"
echo "$WARM" | grep -q "0 failed" && ! echo "$WARM" | grep -qE "[1-9][0-9]* build"
record $? "run-warm-all-skip"

step "test"
$PHLO test $DB >/dev/null 2>&1
record $? "test"

step "lineage + impact"
$PHLO lineage assay.marts.daily_analyte_summary >/dev/null 2>&1
record $? "lineage"
$PHLO impact assay.staging.measurements.concentration >/dev/null 2>&1
record $? "impact"

step "edit + --since"
python3 - <<'EOF'
p = 'workflows/assay/transforms/intermediate/lead_screened.sql'
s = open(p).read()
assert "concentration >= 0" not in s
open(p, 'w').write(s.replace("where analyte = 'lead'", "where analyte = 'lead' and concentration >= 0"))
EOF
git add -A && git -c user.email=dogfood@local -c user.name=dogfood commit -qm "narrow lead_screened"
SINCE="$($PHLO plan --since HEAD~1 $DB 2>&1)"
echo "$SINCE" | grep -q "1 build"
record $? "since-plans-one-model"
$PHLO run --since HEAD~1 $DB >/dev/null 2>&1
record $? "run-since"

step "deliberate failure -> resume -> retry-failed"
printf 'flag\nboom\n' > seeds/fail_flag.csv
OUT="$($PHLO run $DB 2>&1)"
[ $? -ne 0 ]
record $? "run-fails-on-boom"
RUN_ID="$(echo "$OUT" | grep -oE 'retry-failed [0-9a-f]+' | awk '{print $2}' | head -1)"
# Still broken: retry must fail again.
$PHLO run $DB --retry-failed "$RUN_ID" >/dev/null 2>&1
[ $? -ne 0 ]
record $? "retry-failed-still-fails"
# Resume a finished run is an error with guidance (capture rather than
# pipe — pipefail would mask grep's verdict behind phlo's exit code).
RESUME_OUT="$($PHLO run $DB --resume "$RUN_ID" 2>&1)"
echo "$RESUME_OUT" | grep -q "retry-failed"
record $? "resume-finished-run-guides"
# Fix the flag; retry completes the run.
printf 'flag\nok\n' > seeds/fail_flag.csv
$PHLO run $DB --retry-failed "$RUN_ID" >/dev/null 2>&1
record $? "retry-failed-passes"

step "state inspection"
$PHLO state runs >/dev/null 2>&1
record $? "state-runs"
$PHLO state evidence >/dev/null 2>&1
record $? "state-evidence"

if [ -n "${PHLO_NESSIE_ENDPOINT:-}" ] && [ -n "${PHLO_TRINO_ENDPOINT:-}" ]; then
    step "nessie environment workflow"
    NESSIE="--nessie-endpoint $PHLO_NESSIE_ENDPOINT --trino-endpoint $PHLO_TRINO_ENDPOINT"
    NESSIE="$NESSIE --warehouse ${PHLO_WAREHOUSE:-local:///tmp/phlo-dogfood-wh}"
    # When Trino reaches Nessie by a different address than this process —
    # e.g. both in containers on a shared network — the catalog must be
    # provisioned with the container-facing URI.
    [ -n "${PHLO_NESSIE_CATALOG_URI:-}" ] && NESSIE="$NESSIE --nessie-catalog-uri $PHLO_NESSIE_CATALOG_URI"
    # Iceberg catalogs backed by Nessie cannot store views, so the
    # environment leg materialises everything as tables. The DuckDB legs
    # above already exercised the view path.
    find . \( -name transform.toml -o -name phlo.toml \) -print | while read -r config; do
        sed -i.bak 's/materialized = "view"/materialized = "table"/g; s/default_materialization = "view"/default_materialization = "table"/g' "$config" && rm -f "$config.bak"
    done
    NESSIE_CAT_URI="${PHLO_NESSIE_CATALOG_URI:-$PHLO_NESSIE_ENDPOINT}"
    WH="${PHLO_WAREHOUSE:-local:///tmp/phlo-dogfood-wh}"
    FS_LOCAL=""
    case "$WH" in local://*|/*) FS_LOCAL=', "fs.local.enabled"='"'"'true'"'"'' ;; esac
    # The base catalog is production: materialise the workspace on `main`
    # first so a candidate merge has real ancestors — Nessie cannot merge
    # into a branch still at the repository's no-ancestor sentinel.
    NEXT_URI="$(curl -sf -X POST "$PHLO_TRINO_ENDPOINT/v1/statement" \
        -H 'X-Trino-User: dogfood' \
        -d "CREATE CATALOG IF NOT EXISTS phlo_main USING iceberg WITH (\"iceberg.catalog.type\"='nessie', \"iceberg.nessie-catalog.uri\"='$NESSIE_CAT_URI/api/v2', \"iceberg.nessie-catalog.ref\"='main', \"iceberg.nessie-catalog.default-warehouse-dir\"='$WH'$FS_LOCAL)" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin).get("nextUri") or "")')"
    while [ -n "$NEXT_URI" ]; do
        NEXT_URI="$(curl -sf "$NEXT_URI" -H 'X-Trino-User: dogfood' | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("nextUri") or ("ERROR:"+d["error"]["message"] if "error" in d else ""))')"
        case "$NEXT_URI" in ERROR:*) echo "$NEXT_URI"; break ;; esac
    done
    $PHLO run --adapter trino --catalog phlo_main --retries 2 $NESSIE >/dev/null 2>&1
    record $? "run-base-main"
    # A unique candidate ref per run — the CI pattern — so a leftover
    # catalog from an interrupted run can never collide. phlo refuses an
    # unverified pre-existing catalog by design; unique refs sidestep it.
    REF="ci/dogfood-$(date +%s)"
    $PHLO ref list $NESSIE >/dev/null 2>&1
    record $? "ref-list"
    $PHLO ref create "$REF" --from main $NESSIE >/dev/null 2>&1
    record $? "ref-create"
    REFRUN="$($PHLO run --ref "$REF" --adapter trino --retries 2 --json $NESSIE 2>/dev/null)"
    record $? "run-ref"
    # The candidate inherits main's Iceberg tables untouched: identical
    # content must adopt the recorded outputs, not rebuild them.
    echo "$REFRUN" | python3 -c 'import json,sys
r = json.load(sys.stdin)
models = r.get("models", [])
cached = sum(1 for m in models if m.get("status") == "cached")
built = sum(1 for m in models if m.get("status") == "passed")
sys.exit(0 if models and cached == len(models) and built == 0 else 1)'
    record $? "run-ref-cache-reuse"
    $PHLO diff --from "$REF" --to main $NESSIE >/dev/null 2>&1
    record $? "branch-diff"
    $PHLO promote "$REF" --to main --check $NESSIE >/dev/null 2>&1
    record $? "promote-check"
    $PHLO ref delete "$REF" $NESSIE >/dev/null 2>&1
    record $? "ref-delete"
fi

printf '\n=== summary ===\n%d passed, %d failed\n' "$PASS" "$FAIL"
for step in ${FAILED_STEPS[@]+"${FAILED_STEPS[@]}"}; do echo "  FAILED: $step"; done
echo "workspace: $WS"
[ "$FAIL" -eq 0 ]
