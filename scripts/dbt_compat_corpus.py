#!/usr/bin/env python3
"""Run `phlo-transform translate --from dbt` against a corpus of public dbt
projects and aggregate the migration reports.

Usage:
    scripts/dbt_compat_corpus.py [--binary PATH] [--clone] [--deps]
                                 [--analyse] [--verify] [--report PATH]

    --clone     clone the corpus repositories into corpus/repos/<name>
    --deps      resolve dbt package dependencies: install declared Hub/git
                packages into <project>/dbt_packages at a reproducible
                version/commit, recording resolutions in corpus/packages.lock
    --analyse   run `translate --from dbt --check --json` on each project and
                store the report at corpus/results/<name>.json
    --verify    additionally run `translate --from dbt --out <tmp> --overwrite
                --verify` on each project and record whether the generated
                workspace compiles (uses a temp dir; nothing is written to
                the repo checkout)
    --report    write an aggregate markdown report to the given path
                (default: print to stdout)

With no flags, all of clone/deps/analyse/verify/report are run.

Package resolution is reproducible: `package-lock.yml` entries shipped by a
project are honoured first; `corpus/packages.lock` (committed) pins every
fetched package to a commit; anything still unversioned resolves to the
latest tag matching the declared range and is recorded in the lock.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
CORPUS_DIR = REPO_ROOT / "corpus"
REPOS_DIR = CORPUS_DIR / "repos"
RESULTS_DIR = CORPUS_DIR / "results"
PACKAGE_CACHE = CORPUS_DIR / "packages-cache"
PACKAGE_LOCK = CORPUS_DIR / "packages.lock"

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


# --------------------------------------------------------------------------
# Package resolution (--deps)
# --------------------------------------------------------------------------


def check_report(binary: str, proj: Path) -> dict[str, Any] | None:
    """Run `translate --check --json` on a dbt project dir."""
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
        return None
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError:
        return None


def git_url(spec: str) -> str:
    """The clone URL for a git package spec."""
    spec = spec.strip()
    if spec.startswith("git@"):
        spec = "https://github.com/" + spec.split(":", 1)[1]
    spec = spec.removesuffix(".git")
    return spec + ".git"


def cache_clone(repo: str) -> Path | None:
    """A full clone of `repo` under corpus/packages-cache, reused across
    projects and runs."""
    safe = re.sub(r"[^A-Za-z0-9_.-]+", "_", repo).strip("_")
    dest = PACKAGE_CACHE / safe
    PACKAGE_CACHE.mkdir(parents=True, exist_ok=True)
    if dest.exists():
        run(["git", "-C", str(dest), "fetch", "--quiet", "origin"])
        return dest
    proc = run(["git", "clone", "--quiet", repo, str(dest)])
    if proc.returncode != 0:
        shutil.rmtree(dest, ignore_errors=True)
        print(f"  package clone failed: {repo}", file=sys.stderr)
        return None
    return dest


def remote_refs(repo: str) -> dict[str, str]:
    """All `ref -> sha` pairs advertised by the remote."""
    proc = run(["git", "ls-remote", repo])
    refs: dict[str, str] = {}
    for line in proc.stdout.splitlines():
        sha, _, ref = line.partition("\t")
        refs[ref] = sha
    return refs


def parse_version(text: str) -> tuple[int, ...] | None:
    match = re.match(r"^v?(\d+)\.(\d+)\.(\d+)$", text.strip())
    return tuple(int(p) for p in match.groups()) if match else None


def satisfies(version: tuple[int, ...], bound: str) -> bool:
    """Whether `version` satisfies one dbt range bound like `>=1.0.0`."""
    bound = bound.strip()
    for op in (">=", "<=", "==", ">", "<", "="):
        if bound.startswith(op):
            other = parse_version(bound[len(op) :])
            if other is None:
                return True
            if op == ">=":
                return version >= other
            if op == "<=":
                return version <= other
            if op in ("==", "="):
                return version == other
            if op == ">":
                return version > other
            return version < other
    exact = parse_version(bound)
    return exact is None or version == exact


def match_version(candidates: list[str], spec: str | None) -> str | None:
    """The highest candidate satisfying a dbt `version:` spec (an exact
    version or a comma-joined range); unconstrained → newest."""
    versions: dict[tuple[int, ...], str] = {}
    for name in candidates:
        parsed = parse_version(name)
        if parsed is not None:
            versions[parsed] = name
    bounds = [b for b in (spec or "").split(",") if b.strip()]
    matched = [v for v in versions if all(satisfies(v, b) for b in bounds)]
    return versions[max(matched)] if matched else None


# -- dbt Hub registry ----------------------------------------------------
HUB_API = "https://hub.getdbt.com/api/v1"


def hub_versions(spec: str) -> dict[str, Any] | None:
    """`{version: metadata}` for a Hub `org/name` spec."""
    proc = run(["curl", "-sf", "--max-time", "30", f"{HUB_API}/{spec}.json"])
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout).get("versions", {})
    except json.JSONDecodeError:
        return None


def hub_tarball(spec: str, version: str) -> str | None:
    """The codeload tarball URL dbt itself would download."""
    proc = run(["curl", "-sf", "--max-time", "30", f"{HUB_API}/{spec}/{version}.json"])
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout).get("downloads", {}).get("tarball")
    except json.JSONDecodeError:
        return None


def resolve_hub(
    spec: str, requested: str | None, locked: str | None, lock: dict[str, Any]
) -> tuple[str | None, str | None, str | None]:
    """Resolve a Hub package to (version, tarball URL, error).

    Priority: shipped `package-lock.yml` (`locked`) → corpus lock (`pinned`)
    → declared `version:` range → latest published version.
    """
    pinned = lock.get(f"hub:{spec}", {})
    if locked:
        version = locked
        url = hub_tarball(spec, version)
    elif pinned.get("version"):
        version = pinned["version"]
        url = pinned.get("tarball") or hub_tarball(spec, version)
    else:
        versions = hub_versions(spec)
        if versions is None:
            return None, None, f"hub registry has no package `{spec}`"
        version = match_version(list(versions), requested)
        if version is None:
            return None, None, f"no published `{spec}` satisfies `{requested}`"
        url = hub_tarball(spec, version)
    if url is None:
        return None, None, f"no tarball for `{spec}` {version}"
    return version, url, None


def resolve_git(
    repo: str, requested: str | None, locked: str | None, lock: dict[str, Any]
) -> tuple[str | None, str | None, str | None]:
    """Resolve a git package to (ref description, commit sha, error).

    Priority: shipped `package-lock.yml` sha (`locked`) → corpus lock
    (`pinned`) → declared `revision:` → HEAD.
    """
    pinned = lock.get(f"git:{repo}", {})
    if locked and re.fullmatch(r"[0-9a-f]{40}", locked):
        return locked, locked, None
    if pinned.get("commit"):
        return pinned.get("version"), pinned["commit"], None
    refs = remote_refs(repo)
    if requested:
        rev = requested.strip()
        if re.fullmatch(r"[0-9a-f]{7,40}", rev):
            return rev, rev, None
        for ref in (f"refs/tags/{rev}", f"refs/tags/v{rev}", f"refs/heads/{rev}"):
            if ref in refs:
                return rev, refs.get(ref + "^{}") or refs[ref], None
        return None, None, f"revision `{rev}` not found on {repo}"
    head = refs.get("HEAD")
    return ("HEAD", head, None) if head else (None, None, f"no refs on {repo}")


def install_git(clone: Path, sha: str, dest: Path) -> str | None:
    """Export the package tree at `sha` into `dest` (no .git)."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(
        ["git", "-C", str(clone), "archive", sha],
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        return proc.stderr.decode(errors="replace").strip()
    untar = subprocess.run(
        ["tar", "-xf", "-", "-C", str(dest)], input=proc.stdout, check=False
    )
    if untar.returncode != 0:
        return untar.stderr.decode(errors="replace").strip()
    return None


def install_tarball(url: str, dest: Path) -> str | None:
    """Download and extract a codeload tarball into `dest`."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.mkdir(parents=True, exist_ok=True)
    try:
        data = urllib.request.urlopen(url, timeout=60).read()
    except (urllib.error.URLError, OSError) as error:
        return f"download failed: {error}"
    untar = subprocess.run(
        ["tar", "-xzf", "-", "-C", str(dest), "--strip-components", "1"],
        input=data,
        check=False,
        capture_output=True,
    )
    if untar.returncode != 0:
        return untar.stderr.decode(errors="replace").strip()
    return None


def write_package_lock(proj: Path, entries: list[dict[str, Any]]) -> None:
    """Write a `package-lock.yml` recording what was installed — the same
    record `dbt deps` produces."""
    if not entries or (proj / "package-lock.yml").exists():
        return
    lines = ["packages:"]
    for entry in entries:
        key = entry["kind"]
        lines.append(f"  - {key}: {entry['spec']}")
        lines.append(f"    name: {entry['name']}")
        if key == "package":
            lines.append(f"    version: {entry['resolved']}")
        elif key == "git":
            lines.append(f"    revision: {entry['resolved']}")
    (proj / "package-lock.yml").write_text("\n".join(lines) + "\n")


def deps(binary: str) -> None:
    """Install declared package dependencies under `dbt_packages/` so the
    translator can analyse their source. Reproducible via
    `corpus/packages.lock` and any shipped `package-lock.yml`."""
    lock: dict[str, Any] = {}
    if PACKAGE_LOCK.exists():
        lock = json.loads(PACKAGE_LOCK.read_text())
    lock_changed = False

    for name, _, subdir in PROJECTS:
        checkout = REPOS_DIR / name
        if not checkout.exists():
            continue
        proj = project_dir(checkout, subdir)
        if proj is None:
            continue
        installed: dict[str, dict[str, Any]] = {}
        failures: list[str] = []
        seen: set[Path] = set()
        queue = [proj]
        while queue:
            base = queue.pop(0)
            if base in seen:
                continue
            seen.add(base)
            report = check_report(binary, base)
            if report is None:
                continue
            for pkg in report.get("packages", []):
                if pkg["resolved"]:
                    # Already resolved (vendored, local, or installed by an
                    # earlier pass) — record its resolution for the report.
                    installed.setdefault(
                        pkg["name"],
                        {
                            "kind": "local"
                            if pkg["kind"] == "vendored"
                            else pkg["kind"],
                            "spec": pkg["spec"],
                            "name": pkg["name"],
                            "resolved": pkg.get("locked")
                            or pkg.get("requested")
                            or "vendored",
                        },
                    )
                    continue
                kind = pkg["kind"]
                if kind == "local":
                    failures.append(f"local package `{pkg['spec']}` is missing")
                    continue
                if kind not in ("hub", "git"):
                    failures.append(
                        f"{kind} package `{pkg['spec']}` cannot be fetched automatically"
                    )
                    continue
                if pkg["name"] in installed:
                    continue
                dest = proj / "dbt_packages" / pkg["name"]
                if kind == "hub":
                    version, url, error = resolve_hub(
                        pkg["spec"], pkg.get("requested"), pkg.get("locked"), lock
                    )
                    if url is None:
                        failures.append(f"`{pkg['spec']}`: {error}")
                        continue
                    if not dest.exists():
                        problem = install_tarball(url, dest)
                        if problem is not None:
                            failures.append(f"`{pkg['spec']}`: {problem}")
                            continue
                    # Only unpinned resolutions are written to the corpus
                    # lock; a shipped package-lock.yml pins itself.
                    lock_key = f"hub:{pkg['spec']}"
                    if pkg.get("locked") is None and (
                        lock.get(lock_key, {}).get("version") != version
                    ):
                        lock[lock_key] = {"version": version, "tarball": url}
                        lock_changed = True
                    resolved_desc = version
                else:
                    repo = git_url(pkg["spec"])
                    ref, sha, error = resolve_git(
                        repo, pkg.get("requested"), pkg.get("locked"), lock
                    )
                    if sha is None:
                        failures.append(f"`{pkg['spec']}`: {error}")
                        continue
                    clone = cache_clone(repo)
                    if clone is None:
                        failures.append(f"`{pkg['spec']}`: clone of {repo} failed")
                        continue
                    if not dest.exists():
                        problem = install_git(clone, sha, dest)
                        if problem is not None:
                            failures.append(f"`{pkg['spec']}`: {problem}")
                            continue
                    lock_key = f"git:{repo}"
                    if lock.get(lock_key, {}).get("commit") != sha:
                        lock[lock_key] = {"commit": sha, "version": ref}
                        lock_changed = True
                    resolved_desc = ref or sha
                installed[pkg["name"]] = {
                    "kind": "package" if kind == "hub" else "git",
                    "spec": pkg["spec"],
                    "name": pkg["name"],
                    "resolved": resolved_desc,
                }
                # Scan the installed package for its own dependencies
                # (dbt flattens transitives into the root's dbt_packages).
                queue.append(dest)
        if failures:
            for failure in failures:
                print(f"  {name}: {failure}", file=sys.stderr)
        write_package_lock(
            proj, [entry for entry in installed.values() if entry["kind"] != "local"]
        )
        if installed:
            summary = ", ".join(
                f"{pkg['name']}@{pkg['resolved']}" for pkg in installed.values()
            )
            print(f"{name}: {summary}")
    if lock_changed or not PACKAGE_LOCK.exists():
        PACKAGE_LOCK.write_text(json.dumps(lock, indent=2, sort_keys=True) + "\n")


# --------------------------------------------------------------------------
# Analysis + reporting
# --------------------------------------------------------------------------


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
                report = check_report(binary, proj)
                if report is None:
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
                    entry["error"] = (proc.stderr or proc.stdout).strip()[:2000]
                else:
                    entry["report"] = report
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
        report = entry.get("report")
        if report:
            stats = summarise(entry)
            status = (
                f"{stats['clean']}/{stats['active']} active CLEAN" if stats else "?"
            )
        else:
            status = entry.get("error", "?")
        print(f"{name}: {status}")


def load_results(results_dir: Path = RESULTS_DIR) -> list[dict[str, Any]]:
    entries = []
    for path in sorted(results_dir.glob("*.json")):
        entry = json.loads(path.read_text())
        entry["name"] = path.stem
        entries.append(entry)
    return entries


def is_model(resource: dict[str, Any]) -> bool:
    return resource.get("kind") == "model"


def is_disabled(resource: dict[str, Any]) -> bool:
    return any(issue["code"] == "DBT011" for issue in resource.get("issues", []))


def summarise(entry: dict[str, Any]) -> dict[str, Any] | None:
    report = entry.get("report")
    if not report:
        return None
    summary = report["summary"]
    resources = report["resources"]

    models = [r for r in resources if is_model(r)]
    disabled = [r for r in models if is_disabled(r)]
    active = [r for r in models if not is_disabled(r)]

    def cls(resources: list[dict[str, Any]], cls_name: str) -> int:
        return sum(1 for r in resources if r["classification"] == cls_name)

    reasons = Counter()
    reason_models: dict[str, set[str]] = {}
    for resource in resources:
        for issue in resource.get("issues", []):
            reason = f"{issue['code']}: {issue['message']}"
            reasons[reason] += 1
            # Affected *active* models — the ranking unit for blockers.
            if is_model(resource) and not is_disabled(resource):
                reason_models.setdefault(reason, set()).add(resource["name"])
    return {
        "models": len(models),
        "active": len(active),
        "disabled": len(disabled),
        "clean": cls(active, "CLEAN"),
        "review": cls(active, "REVIEW"),
        "unsupported": cls(active, "UNSUPPORTED"),
        "clean_total": cls(models, "CLEAN"),
        "packages": summary.get("packages", {}),
        "package_details": report.get("packages", []),
        "reasons": reasons,
        "reason_models": reason_models,
    }


def blocker_category(code: str, message: str) -> str:
    """Bucket a diagnostic into the report's blocker taxonomy."""
    if code in ("DBT011", "DBT017"):
        return "inactive/disabled resource"
    if code == "DBT012" or (
        code == "DBT004"
        and (
            "package macro" in message
            or "package not declared" in message
            or "(from `" in message
            or "not installed" in message
            or "no such macro" in message
        )
    ):
        return "missing/static package translation"
    if code in ("DBT001", "DBT002") and "package" in message:
        return "missing/static package translation"
    if code == "DBT009":
        return "backend ambiguity"
    if code in ("DBT003", "DBT004", "DBT005"):
        return "dynamic Jinja/runtime behaviour"
    if code == "DBT008" or code == "DBT013":
        return "native Phlo feature gap"
    if code in ("DBT006", "DBT007", "DBT010", "DBT014", "DBT015"):
        return "unsupported dbt concept"
    return "other"


def render_report(entries: list[dict[str, Any]]) -> str:
    lines = [
        "# dbt compatibility corpus",
        "",
        "Generated by `scripts/dbt_compat_corpus.py`. Each entry is a public dbt",
        "project analysed with `phlo-transform translate --from dbt --check`,",
        "with package dependencies installed under `dbt_packages/` beforehand",
        "(`--deps`; exact versions/commits in `corpus/packages.lock` and each",
        "project's `package-lock.yml`).",
        "",
        "## Reading the results",
        "",
        "- Coverage counts **active** models only — models dbt disables under",
        "  the project's default configuration (`enabled: false`, vars that",
        "  default off, unsupported `ref()` targets) are reported separately",
        "  as Disabled and do not distort the percentage.",
        "- CLEAN % is `CLEAN / active models`. UNSUPPORTED covers active",
        "  models with no native path (custom materialisations, snapshots",
        "  emitted as models, `ref()` to disabled models).",
        "- Projects without a `profiles.yml` have no `target.*` values, so",
        "  target-dependent expressions legitimately stay REVIEW.",
        "- Package source is inspected statically only — never executed.",
        "  Dynamic macros (`run_query`, adapter introspection, runtime",
        "  `execute`-gated behaviour) stay REVIEW by design.",
        "- `ephemeral` models emit `-- @ephemeral` and are inlined into",
        "  dependents as subqueries; dbt unit tests, metrics and",
        "  semantic-layer resources have no Phlo equivalent.",
        "- `verify` runs `translate --verify`: the generated workspace must",
        "  compile. Any REVIEW model keeps residual Jinja and fails the check",
        "  loudly, so `fail` is expected whenever REVIEW > 0.",
        "- When `corpus/results-baseline/` holds an earlier run, the CLEAN %",
        "  column shows the point change against it (same active-model basis).",
        "",
        "| Project | Repo | Commit | Models | Disabled | Active | CLEAN | REVIEW | UNSUP. | CLEAN % | verify |",
        "|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    baseline_stats = {
        entry["name"]: stats
        for entry in load_results(RESULTS_DIR / ".." / "results-baseline")
        if (stats := summarise(entry)) is not None
    }
    agg_reasons = Counter()
    reason_active_models: dict[str, set[str]] = {}
    reason_projects: dict[str, set[str]] = {}
    agg_packages = Counter()
    package_versions: dict[str, dict[str, Any]] = {}
    totals = Counter()
    for entry in entries:
        stats = summarise(entry)
        baseline = baseline_stats.get(entry["name"])
        commit = entry.get("commit", "?")[:8]
        if stats is None:
            lines.append(
                f"| {entry['name']} | {entry['repo']} | {commit} | — | — | — | — | — | — | {entry.get('error', '?')} | — |"
            )
            continue
        pct = stats["clean"] / stats["active"] * 100 if stats["active"] else 0.0
        delta = ""
        if baseline and baseline["active"]:
            base_pct = baseline["clean"] / baseline["active"] * 100
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
            f"| {stats['disabled']} | {stats['active']} | {stats['clean']} "
            f"| {stats['review']} | {stats['unsupported']} | {pct:.0f}%{delta} "
            f"| {verify} |"
        )
        totals["models"] += stats["models"]
        totals["disabled"] += stats["disabled"]
        totals["active"] += stats["active"]
        totals["clean"] += stats["clean"]
        totals["review"] += stats["review"]
        totals["unsupported"] += stats["unsupported"]
        agg_reasons.update(stats["reasons"])
        for reason, models in stats["reason_models"].items():
            reason_active_models.setdefault(reason, set()).update(models)
            reason_projects.setdefault(reason, set()).add(entry["name"])
        for cls, n in stats["packages"].items():
            agg_packages[cls] += n
        for pkg in stats["package_details"]:
            key = pkg["spec"]
            record = package_versions.setdefault(
                key,
                {
                    "name": pkg["name"],
                    "resolved": pkg.get("locked") or pkg.get("requested"),
                    "resolved_on_disk": False,
                    "projects": set(),
                },
            )
            record["projects"].add(entry["name"])
            if pkg.get("locked"):
                record["resolved"] = pkg["locked"]
            record["resolved_on_disk"] = record["resolved_on_disk"] or pkg["resolved"]

    if totals["models"]:
        lines += [
            "",
            (
                f"**Aggregate: {totals['models']} declared models, "
                f"{totals['disabled']} disabled, {totals['active']} active — "
                f"{totals['clean']}/{totals['active']} CLEAN "
                f"({totals['clean'] / totals['active'] * 100:.0f}% of active)**"
            ),
            "",
            "## Blockers by category",
            "",
            "Ranked by the number of affected *active* models.",
            "",
            "| Category | Active models | Issues |",
            "|---|---|---|",
        ]
        categories: dict[str, dict[str, Any]] = {}
        for reason, count in agg_reasons.items():
            code, _, message = reason.partition(": ")
            category = blocker_category(code, message)
            bucket = categories.setdefault(category, {"models": set(), "issues": 0})
            bucket["models"].update(reason_active_models.get(reason, set()))
            bucket["issues"] += count
        for category, bucket in sorted(
            categories.items(), key=lambda item: -len(item[1]["models"])
        ):
            lines.append(
                f"| {category} | {len(bucket['models'])} | {bucket['issues']} |"
            )
        lines += [
            "",
            "## Most common review/unsupported reasons",
            "",
            "Ranked by the number of affected *active* models, then issue count.",
            "",
            "| Active models | Issues | Projects | Reason |",
            "|---|---|---|---|",
        ]
        ranked = sorted(
            agg_reasons.items(),
            key=lambda item: (
                -len(reason_active_models.get(item[0], set())),
                -item[1],
            ),
        )
        for reason, count in ranked[:40]:
            models = len(reason_active_models.get(reason, set()))
            nproj = len(reason_projects.get(reason, set()))
            lines.append(f"| {models} | {count} | {nproj} | {reason} |")
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
    if package_versions:
        lines += [
            "",
            "## Resolved package versions",
            "",
            "Exact versions/commits analysed (from `package-lock.yml` or the",
            "corpus lock; `—` means no source was resolved).",
            "",
            "| Package | Resolved | On disk | Projects |",
            "|---|---|---|---|",
        ]
        for spec, record in sorted(package_versions.items()):
            lines.append(
                f"| {spec} | {record['resolved'] or '—'} "
                f"| {'yes' if record['resolved_on_disk'] else 'no'} "
                f"| {len(record['projects'])} |"
            )
    lines.append("")
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary", default=str(REPO_ROOT / "target/release/phlo-transform")
    )
    parser.add_argument("--clone", action="store_true")
    parser.add_argument("--deps", action="store_true")
    parser.add_argument("--analyse", action="store_true")
    parser.add_argument("--verify", action="store_true")
    parser.add_argument("--report", nargs="?", const="-", default=None)
    args = parser.parse_args()
    if not (
        args.clone
        or args.deps
        or args.analyse
        or args.verify
        or args.report is not None
    ):
        args.clone = args.deps = args.analyse = args.verify = True
        args.report = "-"

    if args.clone:
        clone_all()
    if args.deps:
        deps(args.binary)
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
