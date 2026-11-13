#!/usr/bin/env python3
"""Check that ci.yml spells the self-hosted pool's label set the same everywhere.

The pool label set lives in several places in `.github/workflows/ci.yml`: the
`route` job's `--self-hosted-labels`, the forced-lane literal in each routed
job's `runs-on`, and each routed job's `# coord-pools:` line. The router counts
a pool job toward its STALLED / SATURATED signals only when the job carries
EXACTLY that set (`route-ci-lane.py` `is_self_hosted`). So when the spellings
drift apart, the router sees no pool jobs, a dead pool reads healthy forever,
and the hosted floor never takes over. Nothing reports that. This check fails
CI on the drift instead.

It also checks that each routed job falls back to the label `route` names as
its hosted floor, and that every job that `needs: route` is routed.

And it checks the lane-variable declaration, `.qontinui/ci-pool-variables.toml`,
which coord's pool-variable controller reads to decide each lane (plan
2026-11-11-coord-owns-ci-lane-variables, Phase 2c): every lane variable a
routed `runs-on` forces the pool under must be declared there with that same
pool (compared as coord does, ignoring order and case), and every declared
variable must be read by a routed job. Otherwise coord judges the wrong pool's
health for the lane, or reads it `declaration_mismatch` and decides nothing.

Usage: check-ci-lane-labels.py [path/to/ci.yml] [path/to/ci-pool-variables.toml].
The declaration defaults to `.qontinui/ci-pool-variables.toml` beside the
`.github` directory holding ci.yml. Exit 0 = consistent,
1 = drift (each problem printed), 2 = a file could not be read. Stdlib only,
and plain regex over the text: PyYAML is not on every runner, and the
`# coord-pools:` lines are comments that a YAML parser would throw away.
Tests: .github/scripts/test-ci-lane-labels.sh.
"""

from __future__ import annotations

import json
import os
import re
import sys

JOB_RE = re.compile(r"^  ([A-Za-z0-9_-]+):\s*$")
LANE_RE = re.compile(r"vars\.([A-Za-z0-9_]+)\s*==\s*'self-hosted'")


def split_jobs(text: str) -> dict[str, list[str]]:
    """The lines of each job under `jobs:`, keyed by job id."""
    jobs: dict[str, list[str]] = {}
    in_jobs = False
    current: str | None = None
    for line in text.splitlines():
        if re.match(r"^jobs:\s*$", line):
            in_jobs = True
            continue
        if not in_jobs:
            continue
        if line and not line.startswith(" ") and not line.startswith("#"):
            break  # another top-level key ends `jobs:`
        m = JOB_RE.match(line)
        if m:
            current = m.group(1)
            jobs[current] = []
        elif current is not None:
            jobs[current].append(line)
    return jobs


def labels_of(raw: str) -> list[str]:
    return [s.strip() for s in raw.split(",") if s.strip()]


def needs_route(lines: list[str]) -> bool:
    """True when the job's `needs` names `route`: scalar, flow list or block list."""
    for i, line in enumerate(lines):
        m = re.match(r"^    needs:\s*(.*?)\s*$", line)
        if not m:
            continue
        value = m.group(1)
        if value:
            return "route" in [v.strip(" '\"") for v in value.strip("[]").split(",")]
        for item in lines[i + 1 :]:
            mi = re.match(r"^      -\s*(\S+)\s*$", item)
            if not mi:
                break
            if mi.group(1).strip("'\"") == "route":
                return True
        return False
    return False


def pool_key(labels: list[str]) -> list[str]:
    """coord's pool key: case-insensitive, order-insensitive, de-duplicated."""
    return sorted({label.lower() for label in labels})


def parse_declaration(text: str) -> dict[str, list[str]]:
    """{VARIABLE: [labels]} from a ci-pool-variables.toml body; ValueError if malformed.

    Mirrors the shape rules coord's parser enforces (qontinui-coord
    ci_pool_alerts/variables.rs `parse_declaration`): only a top-level
    `variables` array of tables, each with exactly `name` and `pool`, and no
    name declared twice (compared case-insensitively). coord refuses the WHOLE
    file on any of these, which leaves every lane in it undecided.
    """
    try:
        import tomllib
    except ImportError as exc:
        raise ValueError("cannot be checked: this needs Python 3.11+ (tomllib)") from exc
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        raise ValueError(f"does not parse: {exc}") from exc
    extra = sorted(set(data) - {"variables"})
    if extra:
        raise ValueError(f"unknown top-level key(s) {extra}; coord refuses the file")
    entries = data.get("variables", [])
    if not isinstance(entries, list):
        raise ValueError("`variables` must be an array of tables ([[variables]])")
    declared: dict[str, list[str]] = {}
    for i, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise ValueError(f"variables[{i}] is not a table")
        extra = sorted(set(entry) - {"name", "pool"})
        if extra:
            raise ValueError(f"variables[{i}] has unknown key(s) {extra}; coord refuses the file")
        name, pool = entry.get("name"), entry.get("pool")
        if not isinstance(name, str) or not isinstance(pool, list):
            raise ValueError(f"variables[{i}] needs a string `name` and a list `pool`")
        name = name.strip().upper()
        if name in declared:
            raise ValueError(f"variables[{i}]: {name} is declared more than once; coord refuses the file")
        declared[name] = [str(label).strip() for label in pool]
    return declared


def check(text: str, declared: dict[str, list[str]] | None = None) -> list[str]:
    """Drift in ci.yml; with `declared`, also against the lane-variable declaration."""
    problems: list[str] = []
    lanes_read: set[str] = set()
    jobs = split_jobs(text)
    route = jobs.get("route")
    if route is None:
        return ["no `route` job under jobs:"]
    route_text = "\n".join(route)

    m = re.search(r"--self-hosted-labels\s+(\S+)", route_text)
    if not m:
        return ["`route` passes no --self-hosted-labels"]
    pool = labels_of(m.group(1))
    m = re.search(r"_HOSTED_LABEL:\s*(\S+)", route_text)
    if not m:
        return ["`route` sets no *_HOSTED_LABEL"]
    floor = m.group(1)

    routed = [name for name, lines in jobs.items() if needs_route(lines)]
    if not routed:
        problems.append("no job `needs: route`: nothing is routed")

    for name in routed:
        body = jobs[name]
        pools = [l for l in body if re.match(r"^\s+# coord-pools:", l)]
        if len(pools) != 1:
            problems.append(f"{name}: {len(pools)} `# coord-pools:` lines, want 1")
        else:
            m = re.match(r"^\s+# coord-pools:\s*\[([^\]]*)\]\s*\|\s*\[([^\]]*)\]\s*$", pools[0])
            if not m:
                problems.append(f"{name}: unparseable coord-pools line: {pools[0].strip()}")
            else:
                if labels_of(m.group(1)) != pool:
                    problems.append(f"{name}: coord-pools pool {labels_of(m.group(1))} != route's {pool}")
                if labels_of(m.group(2)) != [floor]:
                    problems.append(f"{name}: coord-pools floor {labels_of(m.group(2))} != route's [{floor!r}]")

        runs_on = [l for l in body if re.match(r"^    runs-on:", l)]
        if len(runs_on) != 1:
            problems.append(f"{name}: {len(runs_on)} job-level runs-on lines, want 1")
            continue
        expr = runs_on[0]
        if "needs.route.outputs.labels" not in expr:
            problems.append(f"{name}: runs-on does not read needs.route.outputs.labels")
        literals = []
        for lit in re.findall(r"'(\[[^']*\])'", expr):
            try:
                literals.append(json.loads(lit))
            except ValueError:
                problems.append(f"{name}: runs-on literal {lit} is not JSON")
        forced = [lit for lit in literals if "self-hosted" in lit]
        floors = [lit for lit in literals if "self-hosted" not in lit]
        if len(forced) != 1 or forced[0] != pool:
            problems.append(f"{name}: runs-on forced-lane literal {forced} != route's {pool}")
        if len(floors) != 1 or floors[0] != [floor]:
            problems.append(f"{name}: runs-on floor literal {floors} != route's [{floor!r}]")
        if declared is not None:
            lane = LANE_RE.search(expr)
            if not lane:
                problems.append(f"{name}: runs-on forces the pool under no `vars.<LANE> == 'self-hosted'`")
            else:
                var = lane.group(1).upper()
                lanes_read.add(var)
                if var not in declared:
                    problems.append(f"{name}: lane variable {var} is not declared in ci-pool-variables.toml")
                elif len(forced) == 1 and pool_key(declared[var]) != pool_key(forced[0]):
                    problems.append(
                        f"{name}: forced-lane pool {forced[0]} != ci-pool-variables.toml's "
                        f"declared pool {declared[var]} for {var}"
                    )

    # A coord-pools line or a pool literal in a job that is NOT routed would be a
    # job that thinks it is on the pool without `route` deciding it.
    for name, body in jobs.items():
        if name in routed:
            continue
        if any(re.match(r"^\s+# coord-pools:", l) for l in body):
            problems.append(f"{name}: has a coord-pools line but does not `needs: route`")
        pool_set = {p.lower() for p in pool}
        for l in body:
            m = re.match(r"^    runs-on:\s*(.*)$", l)
            if m and pool_set <= {t.strip(" '\"[]").lower() for t in re.split(r"[,\s]+", m.group(1)) if t.strip(" '\"[]")}:
                problems.append(f"{name}: runs-on names the pool's labels but the job does not `needs: route`")
    if declared is not None:
        for var in sorted(set(declared) - lanes_read):
            problems.append(f"ci-pool-variables.toml declares {var} but no routed runs-on reads it")
    return problems


def main(argv: list[str]) -> int:
    path = argv[0] if argv else ".github/workflows/ci.yml"
    decl_path = argv[1] if len(argv) > 1 else os.path.join(
        os.path.dirname(os.path.abspath(path)), "..", "..", ".qontinui", "ci-pool-variables.toml"
    )
    try:
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
    except OSError as exc:
        print(f"cannot read {path}: {exc}", file=sys.stderr)
        return 2
    try:
        with open(decl_path, encoding="utf-8") as fh:
            declared = parse_declaration(fh.read())
    except OSError as exc:
        print(f"cannot read the lane-variable declaration {decl_path}: {exc}", file=sys.stderr)
        return 2
    except ValueError as exc:
        print(f"DRIFT: {decl_path}: {exc}")
        print(f"{path}: the lane-variable declaration is malformed")
        return 1
    problems = check(text, declared)
    for p in problems:
        print(f"DRIFT: {p}")
    if problems:
        print(f"{path}: pool label set is not spelled the same everywhere ({len(problems)} problem(s))")
        return 1
    print(f"{path}: pool label set consistent across route and every routed job")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
