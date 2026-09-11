#!/usr/bin/env python3
"""Run `phlo-transform translate --from dbt` against a corpus of public dbt
projects and aggregate the migration reports.

Usage:
    scripts/dbt_compat_corpus.py [--binary PATH] [--clone] [--analyse]
                                 [--verify] [--report PATH]

    --clone     clone the corpus repositories into corpus/repos/<name>
    --analyse   run `translate --from dbt --check --json` on each project and
                store the report at corpus/results/<name>.json
    --verify    additionally run `translate --from dbt --out <tmp> --overwrite
                --verify` on each project and record whether the generated
                workspace compiles (uses a temp dir; nothing is written to
                the repo checkout)
    --report    write an aggregate markdown report to the given path
                (default: print to stdout)

With no flags, all of clone/analyse/verify/report are run.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
CORPUS_DIR = REPO_ROOT / "corpus"
REPOS_DIR = CORPUS_DIR / "repos"
RESULTS_DIR = CORPUS_DIR / "results"

# Public dbt projects, roughly ordered small → large. `subdir` is used when the
# dbt project is not at the repository root (package integration tests).
PROJECTS = [
    # -- Realistic standalone projects --------------------------------------
    ("jaffle_shop_duckdb", "dbt-labs/jaffle_shop_duckdb", None),
    ("jaffle-shop-classic", "dbt-labs/jaffle-shop-classic", None),
    ("jaffle-shop", "dbt-labs/jaffle-shop", None),
    ("mrr-playbook", "dbt-labs/mrr-playbook", None),
    ("attribution-playbook", "dbt-labs/attribution-playbook", None),
    ("dbt-starter-project", "dbt-labs/dbt-starter-project", None),
    ("canvas-exemplar", "dbt-labs/canvas-exemplar", None),
    ("elementary-tutorial", "elementary-data/elementary-tutorial", None),
    ("dbt-duckdb-tutorial", "mehd-io/dbt-duckdb-tutorial", "dbt_demo"),
    ("the_tuva_project", "tuva-health/the_tuva_project", None),
    # -- Package integration-test projects (heavy package/macro usage) -------
    ("dbt-utils", "dbt-labs/dbt-utils", "integration_tests"),
    ("dbt-project-evaluator", "dbt-labs/dbt-project-evaluator", "integration_tests_2"),
    ("dbt-codegen", "dbt-labs/dbt-codegen", "integration_tests"),
    ("dbt-external-tables", "dbt-labs/dbt-external-tables", "integration_tests"),
    ("spark-utils", "dbt-labs/spark-utils", "integration_tests"),
    ("dbt-date", "calogica/dbt-date", "integration_tests"),
    ("dbt-expectations", "calogica/dbt-expectations", "integration_tests"),
    (
        "dbt-data-reliability",
        "elementary-data/dbt-data-reliability",
        "integration_tests/dbt_project",
    ),
    ("dbt_artifacts", "brooklyn-data/dbt_artifacts", "integration_test_project"),
    # Fivetran packages: the real project (models/) is the repo root; the
    # integration_tests/ dir is a thin consumer with no models of its own.
    ("dbt_hubspot", "fivetran/dbt_hubspot", None),
    ("dbt_github", "fivetran/dbt_github", None),
    ("dbt_zendesk", "fivetran/dbt_zendesk", None),
    ("dbt_stripe", "fivetran/dbt_stripe", None),
    ("dbt_ad_reporting", "fivetran/dbt_ad_reporting", None),
    ("dbt_shopify", "fivetran/dbt_shopify", None),
    ("dbt_salesforce", "fivetran/dbt_salesforce", None),
    ("dbt_netsuite", "fivetran/dbt_netsuite", None),
    ("dbt_jira", "fivetran/dbt_jira", None),
    ("dbt_marketo", "fivetran/dbt_marketo", None),
    ("dbt_linkedin", "fivetran/dbt_linkedin", None),
    (
        "dbt-snowplow-media-player",
        "snowplow/dbt-snowplow-media-player",
        "integration_tests",
    ),
]


def run(cmd: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(cmd, capture_output=True, text=True, check=False)


def clone_all() -> None:
    REPOS_DIR.mkdir(parents=True, exist_ok=True)
    for name, slug, _ in PROJECTS:
        dest = REPOS_DIR / name
        if dest.exists():
            continue
        url = f"https://github.com/{slug}.git"
        proc = run(["git", "clone", "--depth", "1", url, str(dest)])
        if proc.returncode != 0:
            print(f"clone failed: {slug}\n{proc.stderr.strip()}", file=sys.stderr)


def project_dir(checkout: Path, subdir: str | None) -> Path | None:
    """Locate the dbt project inside a checkout."""
    candidates = [checkout / subdir] if subdir else [checkout]
    for base in candidates:
        if (base / "dbt_project.yml").exists() or (base / "dbt_project.yaml").exists():
            return base
    # Fallback: the dbt_project.yml with the most sibling models.
    best: tuple[int, Path] | None = None
    for marker in checkout.rglob("dbt_project.y*ml"):
        if "dbt_packages" in marker.parts or "deps" in marker.parts:
            continue
        base = marker.parent
        models = sum(1 for _ in base.rglob("*.sql") if "dbt_packages" not in _.parts)
        if best is None or models > best[0]:
            best = (models, base)
    return best[1] if best else None


def analyse(binary: str, verify: bool) -> None:
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    for name, slug, subdir in PROJECTS:
        checkout = REPOS_DIR / name
        result_path = RESULTS_DIR / f"{name}.json"
        entry: dict[str, Any] = {"repo": slug}
        if not checkout.exists():
            entry["error"] = "not cloned"
        else:
            entry["commit"] = run(
                ["git", "-C", str(checkout), "rev-parse", "HEAD"]
            ).stdout.strip()
            proj = project_dir(checkout, subdir)
            if proj is None:
                entry["error"] = "no dbt_project.yml found"
            else:
                entry["project_dir"] = str(proj.relative_to(checkout)) or "."
                proc = run(
                    [
                        binary,
                        "-r",
                        str(proj),
                        "translate",
                        "--from",
                        "dbt",
                        "--check",
                        "--json",
                    ]
                )
                if proc.returncode != 0 or not proc.stdout.strip():
                    entry["error"] = (proc.stderr or proc.stdout).strip()[:2000]
                else:
                    entry["report"] = json.loads(proc.stdout)
                if verify:
                    with tempfile.TemporaryDirectory() as tmp:
                        vproc = run(
                            [
                                binary,
                                "-r",
                                str(proj),
                                "translate",
                                "--from",
                                "dbt",
                                "--out",
                                tmp,
                                "--overwrite",
                                "--verify",
                                "--json",
                            ]
                        )
                        entry["verify_ok"] = vproc.returncode == 0
        result_path.write_text(json.dumps(entry, indent=2) + "\n")
        status = entry.get("error") or (
            f"{entry['report']['model_coverage'] * 100:.0f}% CLEAN"
            if "report" in entry
            else "?"
        )
        print(f"{name}: {status}")


def load_results(results_dir: Path = RESULTS_DIR) -> list[dict[str, Any]]:
    entries = []
    for path in sorted(results_dir.glob("*.json")):
        entry = json.loads(path.read_text())
        entry["name"] = path.stem
        entries.append(entry)
    return entries


def summarise(entry: dict[str, Any]) -> dict[str, Any] | None:
    report = entry.get("report")
    if not report:
        return None
    summary = report["summary"]

    def count(kind: str, cls: str) -> int:
        return summary.get(kind, {}).get(cls, 0)

    models = sum(summary.get("models", {}).values())
    reasons = Counter()
    for resource in report["resources"]:
        for issue in resource.get("issues", []):
            reasons[f"{issue['code']}: {issue['message']}"] += 1
    return {
        "models": models,
        "clean": count("models", "CLEAN"),
        "review": count("models", "REVIEW"),
        "unsupported": count("models", "UNSUPPORTED"),
        "packages": summary.get("packages", {}),
        "reasons": reasons,
    }


def render_report(entries: list[dict[str, Any]]) -> str:
    lines = [
        "# dbt compatibility corpus",
        "",
        "Generated by `scripts/dbt_compat_corpus.py`. Each entry is a public dbt",
        "project analysed with `phlo-transform translate --from dbt --check`.",
        "",
        "## Reading the results",
        "",
        "- `model_coverage` counts dbt **models** only; seeds, sources, tests,",
        "  macros and exposures are classified but excluded from the percentage.",
        "- Projects without a `profiles.yml` have no `target.*` values, so",
        "  target-dependent expressions legitimately stay REVIEW.",
        "- Projects gated on vars that default false (e.g. the_tuva_project's",
        "  `data_quality_enabled`, Fivetran's connector switches) classify the",
        "  disabled models UNSUPPORTED with DBT011 — this is correct dbt",
        "  semantics under the project's default configuration, not a gap.",
        "- Package macros are resolved only when the package's source is",
        "  vendored in `dbt_packages/`; corpus checkouts are not vendored, so",
        "  `fivetran_utils.*`, `dbt_expectations.*` etc. stay REVIEW by design.",
        "- `ephemeral` models emit as views with a DBT008 note; dbt unit tests,",
        "  metrics and semantic-layer resources have no Phlo equivalent.",
        "- When `corpus/results-baseline/` holds an earlier run, the CLEAN %",
        "  column shows the point change against it.",
        "",
        "| Project | Repo | Commit | Models | CLEAN | REVIEW | UNSUP. | CLEAN % | verify |",
        "|---|---|---|---|---|---|---|---|---|",
    ]
    baseline_stats = {
        entry["name"]: stats
        for entry in load_results(RESULTS_DIR / ".." / "results-baseline")
        if (stats := summarise(entry)) is not None
    }
    agg_reasons = Counter()
    agg_code_projects: dict[str, set[str]] = {}
    agg_packages = Counter()
    totals = Counter()
    for entry in entries:
        stats = summarise(entry)
        baseline = baseline_stats.get(entry["name"])
        commit = entry.get("commit", "?")[:8]
        if stats is None:
            lines.append(
                f"| {entry['name']} | {entry['repo']} | {commit} | — | — | — | — | {entry.get('error', '?')} | — |"
            )
            continue
        pct = stats["clean"] / stats["models"] * 100 if stats["models"] else 0.0
        delta = ""
        if baseline and baseline["models"]:
            base_pct = baseline["clean"] / baseline["models"] * 100
            delta = f" ({pct - base_pct:+.0f}pt)"
        verify = (
            "pass"
            if entry.get("verify_ok") is True
            else "fail"
            if entry.get("verify_ok") is False
            else "—"
        )
        lines.append(
            f"| {entry['name']} | {entry['repo']} | {commit} | {stats['models']} "
            f"| {stats['clean']} | {stats['review']} | {stats['unsupported']} "
            f"| {pct:.0f}%{delta} | {verify} |"
        )
        totals["models"] += stats["models"]
        totals["clean"] += stats["clean"]
        agg_reasons.update(stats["reasons"])
        for reason in stats["reasons"]:
            code = reason.split(":", 1)[0]
            agg_code_projects.setdefault(code, set()).add(entry["name"])
        # `summary.packages` maps classification → count.
        for cls, n in stats["packages"].items():
            agg_packages[cls] += n

    if totals["models"]:
        lines += [
            "",
            (
                f"**Aggregate model coverage: {totals['clean']}/{totals['models']} "
                f"CLEAN ({totals['clean'] / totals['models'] * 100:.0f}%)**"
            ),
            "",
            "## Most common review/unsupported reasons",
            "",
            "| Count | Projects | Reason |",
            "|---|---|---|",
        ]
        for reason, count in agg_reasons.most_common(40):
            code = reason.split(":", 1)[0]
            nproj = len(agg_code_projects.get(code, ()))
            lines.append(f"| {count} | {nproj} | {reason} |")
    if agg_packages:
        lines += [
            "",
            "## Declared package dependencies by classification",
            "",
            "| Count | Classification |",
            "|---|---|",
        ]
        for cls, count in agg_packages.most_common(30):
            lines.append(f"| {count} | {cls} |")
    lines.append("")
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary", default=str(REPO_ROOT / "target/release/phlo-transform")
    )
    parser.add_argument("--clone", action="store_true")
    parser.add_argument("--analyse", action="store_true")
    parser.add_argument("--verify", action="store_true")
    parser.add_argument("--report", nargs="?", const="-", default=None)
    args = parser.parse_args()
    if not (args.clone or args.analyse or args.verify or args.report is not None):
        args.clone = args.analyse = args.verify = True
        args.report = "-"

    if args.clone:
        clone_all()
    if args.analyse:
        analyse(args.binary, args.verify)
    if args.report is not None:
        report = render_report(load_results())
        if args.report == "-":
            print(report)
        else:
            Path(args.report).write_text(report)
            print(f"wrote {args.report}")


if __name__ == "__main__":
    main()
