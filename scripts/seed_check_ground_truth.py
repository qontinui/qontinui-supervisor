"""
Seed eval test prompts with ground truth check groups derived from actual
pre-commit hook configurations in each repo.

Usage: python3 scripts/seed_check_ground_truth.py
"""
import json
import urllib.request

BASE = "http://localhost:9875"


def api_post(path, data):
    body = json.dumps(data).encode()
    req = urllib.request.Request(
        f"{BASE}{path}",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"ok": False, "message": str(e)}


def api_put(path, data):
    body = json.dumps(data).encode()
    req = urllib.request.Request(
        f"{BASE}{path}",
        data=body,
        headers={"Content-Type": "application/json"},
        method="PUT",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"ok": False, "message": str(e)}


def api_delete(path):
    req = urllib.request.Request(f"{BASE}{path}", method="DELETE")
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except Exception:
        return {"ok": False}


def upsert_prompt(prompt_data):
    """Insert or update a test prompt."""
    pid = prompt_data["id"]
    # Try POST first
    result = api_post("/eval/test-suite", prompt_data)
    if result.get("ok"):
        return "NEW"
    # Already exists — update
    result = api_put(f"/eval/test-suite/{pid}", prompt_data)
    if result.get("ok"):
        return "UPD"
    return f"FAIL: {result.get('message', '?')}"


# ============================================================================
# Ground truth definitions — derived from actual .pre-commit-config.yaml files
# ============================================================================

PROMPTS = []

# Base path for all repos
WS = r"C:\Users\jspin\Documents\qontinui_parent"


def add(prompt_id, prompt_text, ground_truth_checks):
    """Add a prompt with ground truth check group."""
    gt_workflow = {
        "name": f"Check Group: {prompt_id}",
        "description": f"Ground truth checks for: {prompt_text}",
        "category": "code_quality",
        "tags": ["check", "ground_truth"],
        "setup_steps": [],
        "verification_steps": [
            *ground_truth_checks,
            {
                "id": "step-gate",
                "type": "gate",
                "name": "All checks pass",
                "phase": "verification",
                "required_steps": [c["id"] for c in ground_truth_checks],
            },
        ],
        "agentic_steps": [
            {
                "id": "step-fix",
                "type": "prompt",
                "name": "Fix issues",
                "phase": "agentic",
                "content": "Fix any check failures found in the verification phase.",
            }
        ],
        "completion_steps": [],
        "max_iterations": 5,
    }

    PROMPTS.append(
        {
            "id": prompt_id,
            "prompt": prompt_text,
            "category": "curated",
            "complexity": "medium",
            "expected_phases": None,
            "expected_step_types": None,
            "tags": ["check_group", "ground_truth"],
            "ground_truth_json": json.dumps(gt_workflow),
            "enabled": True,
            "created_at": "2026-02-18T00:00:00Z",
            "updated_at": "2026-02-18T00:00:00Z",
        }
    )


# ── 1. qontinui-web: Full code checks ──────────────────────────────────────
# Pre-commit hooks: ruff (lint+format), mypy, eslint, prettier, tsc
add(
    "gt-checks-qontinui-web",
    f"Run all code quality checks on the qontinui-web repository at {WS}\\qontinui-web",
    [
        {
            "id": "check-ruff-lint",
            "type": "check",
            "name": "Python lint (ruff)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "ruff",
            "command": "ruff check .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
        {
            "id": "check-ruff-format",
            "type": "check",
            "name": "Python format (ruff)",
            "phase": "verification",
            "check_type": "format",
            "tool": "ruff",
            "command": "ruff format --check .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
        {
            "id": "check-mypy",
            "type": "check",
            "name": "Python type check (mypy)",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "mypy",
            "command": "mypy .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
        {
            "id": "check-eslint",
            "type": "check",
            "name": "TypeScript lint (ESLint)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "eslint",
            "command": "npx eslint .",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
        {
            "id": "check-tsc",
            "type": "check",
            "name": "TypeScript type check",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "tsc",
            "command": "npx tsc --noEmit",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
        {
            "id": "check-prettier",
            "type": "check",
            "name": "Prettier format check",
            "phase": "verification",
            "check_type": "format",
            "tool": "prettier",
            "command": "npx prettier --check .",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
    ],
)

# ── 2. qontinui-web backend: Python checks ─────────────────────────────────
# Pre-commit hooks: ruff (lint+format), mypy
add(
    "gt-checks-web-backend",
    f"Run code quality checks on the qontinui-web backend (Python) at {WS}\\qontinui-web\\backend",
    [
        {
            "id": "check-ruff-lint",
            "type": "check",
            "name": "Python lint (ruff)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "ruff",
            "command": "ruff check .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
        {
            "id": "check-ruff-format",
            "type": "check",
            "name": "Python format (ruff)",
            "phase": "verification",
            "check_type": "format",
            "tool": "ruff",
            "command": "ruff format --check .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
        {
            "id": "check-mypy",
            "type": "check",
            "name": "Python type check (mypy)",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "mypy",
            "command": "mypy .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
    ],
)

# ── 3. qontinui-web frontend: TS/JS checks ─────────────────────────────────
# Pre-commit hooks: eslint, prettier, tsc
add(
    "gt-checks-web-frontend",
    f"Run code quality checks on the qontinui-web frontend (TypeScript/React) at {WS}\\qontinui-web\\frontend",
    [
        {
            "id": "check-eslint",
            "type": "check",
            "name": "TypeScript lint (ESLint)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "eslint",
            "command": "npx eslint .",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
        {
            "id": "check-tsc",
            "type": "check",
            "name": "TypeScript type check",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "tsc",
            "command": "npx tsc --noEmit",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
        {
            "id": "check-prettier",
            "type": "check",
            "name": "Prettier format check",
            "phase": "verification",
            "check_type": "format",
            "tool": "prettier",
            "command": "npx prettier --check .",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
    ],
)

# ── 4. qontinui (core lib): Python checks ──────────────────────────────────
# Pre-commit hooks: black (line-length=100), ruff, mypy
add(
    "gt-checks-qontinui-core",
    f"Run code quality checks on the qontinui core Python library at {WS}\\qontinui",
    [
        {
            "id": "check-black",
            "type": "check",
            "name": "Python format (black)",
            "phase": "verification",
            "check_type": "format",
            "tool": "black",
            "command": "black --check --line-length=100 .",
            "working_directory": f"{WS}\\qontinui",
        },
        {
            "id": "check-ruff",
            "type": "check",
            "name": "Python lint (ruff)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "ruff",
            "command": "ruff check .",
            "working_directory": f"{WS}\\qontinui",
        },
        {
            "id": "check-mypy",
            "type": "check",
            "name": "Python type check (mypy)",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "mypy",
            "command": "mypy -p qontinui",
            "working_directory": f"{WS}\\qontinui",
        },
    ],
)

# ── 5. qontinui-runner: Rust + TS checks ───────────────────────────────────
# Pre-commit hooks: cargo fmt, cargo clippy, prettier, eslint, tsc
add(
    "gt-checks-runner",
    f"Run all code quality checks on the qontinui-runner repository at {WS}\\qontinui-runner",
    [
        {
            "id": "check-cargo-fmt",
            "type": "check",
            "name": "Rust format (cargo fmt)",
            "phase": "verification",
            "check_type": "format",
            "tool": "cargo",
            "command": "cargo fmt -- --check",
            "working_directory": f"{WS}\\qontinui-runner\\src-tauri",
        },
        {
            "id": "check-cargo-clippy",
            "type": "check",
            "name": "Rust lint (cargo clippy)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "cargo",
            "command": "cargo clippy -- -D warnings",
            "working_directory": f"{WS}\\qontinui-runner\\src-tauri",
        },
        {
            "id": "check-eslint",
            "type": "check",
            "name": "TypeScript lint (ESLint)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "eslint",
            "command": "npx eslint .",
            "working_directory": f"{WS}\\qontinui-runner",
        },
        {
            "id": "check-tsc",
            "type": "check",
            "name": "TypeScript type check",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "tsc",
            "command": "npx tsc --noEmit",
            "working_directory": f"{WS}\\qontinui-runner",
        },
        {
            "id": "check-prettier",
            "type": "check",
            "name": "Prettier format check",
            "phase": "verification",
            "check_type": "format",
            "tool": "prettier",
            "command": "npx prettier --check 'src/**/*.{ts,tsx}'",
            "working_directory": f"{WS}\\qontinui-runner",
        },
    ],
)

# ── 6. qontinui-supervisor: Rust checks ────────────────────────────────────
# Pre-commit hooks: cargo fmt, cargo clippy
add(
    "gt-checks-supervisor",
    f"Run code quality checks on the qontinui-supervisor (Rust) at {WS}\\qontinui-supervisor",
    [
        {
            "id": "check-cargo-fmt",
            "type": "check",
            "name": "Rust format (cargo fmt)",
            "phase": "verification",
            "check_type": "format",
            "tool": "cargo",
            "command": "cargo fmt -- --check",
            "working_directory": f"{WS}\\qontinui-supervisor",
        },
        {
            "id": "check-cargo-clippy",
            "type": "check",
            "name": "Rust lint (cargo clippy)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "cargo",
            "command": "cargo clippy -- -D warnings",
            "working_directory": f"{WS}\\qontinui-supervisor",
        },
    ],
)

# ── 7. multistate: Python checks ───────────────────────────────────────────
# Pre-commit hooks: black (line-length=100), ruff, mypy
add(
    "gt-checks-multistate",
    f"Run code quality checks on the multistate Python library at {WS}\\multistate",
    [
        {
            "id": "check-black",
            "type": "check",
            "name": "Python format (black)",
            "phase": "verification",
            "check_type": "format",
            "tool": "black",
            "command": "black --check --line-length=100 .",
            "working_directory": f"{WS}\\multistate",
        },
        {
            "id": "check-ruff",
            "type": "check",
            "name": "Python lint (ruff)",
            "phase": "verification",
            "check_type": "lint",
            "tool": "ruff",
            "command": "ruff check .",
            "working_directory": f"{WS}\\multistate",
        },
        {
            "id": "check-mypy",
            "type": "check",
            "name": "Python type check (mypy)",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "mypy",
            "command": "mypy .",
            "working_directory": f"{WS}\\multistate",
        },
    ],
)

# ── 8. Full stack: All repos ────────────────────────────────────────────────
add(
    "gt-checks-full-stack",
    f"Run code quality checks across all qontinui repositories (web backend at {WS}\\qontinui-web\\backend, web frontend at {WS}\\qontinui-web\\frontend, runner at {WS}\\qontinui-runner, supervisor at {WS}\\qontinui-supervisor)",
    [
        {
            "id": "check-web-backend-ruff-lint",
            "type": "check",
            "name": "Web backend: ruff lint",
            "phase": "verification",
            "check_type": "lint",
            "tool": "ruff",
            "command": "ruff check .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
        {
            "id": "check-web-backend-ruff-format",
            "type": "check",
            "name": "Web backend: ruff format",
            "phase": "verification",
            "check_type": "format",
            "tool": "ruff",
            "command": "ruff format --check .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
        {
            "id": "check-web-backend-mypy",
            "type": "check",
            "name": "Web backend: mypy",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "mypy",
            "command": "mypy .",
            "working_directory": f"{WS}\\qontinui-web\\backend",
        },
        {
            "id": "check-web-frontend-eslint",
            "type": "check",
            "name": "Web frontend: ESLint",
            "phase": "verification",
            "check_type": "lint",
            "tool": "eslint",
            "command": "npx eslint .",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
        {
            "id": "check-web-frontend-tsc",
            "type": "check",
            "name": "Web frontend: tsc",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "tsc",
            "command": "npx tsc --noEmit",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
        {
            "id": "check-web-frontend-prettier",
            "type": "check",
            "name": "Web frontend: Prettier",
            "phase": "verification",
            "check_type": "format",
            "tool": "prettier",
            "command": "npx prettier --check .",
            "working_directory": f"{WS}\\qontinui-web\\frontend",
        },
        {
            "id": "check-runner-cargo-fmt",
            "type": "check",
            "name": "Runner: cargo fmt",
            "phase": "verification",
            "check_type": "format",
            "tool": "cargo",
            "command": "cargo fmt -- --check",
            "working_directory": f"{WS}\\qontinui-runner\\src-tauri",
        },
        {
            "id": "check-runner-clippy",
            "type": "check",
            "name": "Runner: cargo clippy",
            "phase": "verification",
            "check_type": "lint",
            "tool": "cargo",
            "command": "cargo clippy -- -D warnings",
            "working_directory": f"{WS}\\qontinui-runner\\src-tauri",
        },
        {
            "id": "check-runner-eslint",
            "type": "check",
            "name": "Runner: ESLint",
            "phase": "verification",
            "check_type": "lint",
            "tool": "eslint",
            "command": "npx eslint .",
            "working_directory": f"{WS}\\qontinui-runner",
        },
        {
            "id": "check-runner-tsc",
            "type": "check",
            "name": "Runner: tsc",
            "phase": "verification",
            "check_type": "typecheck",
            "tool": "tsc",
            "command": "npx tsc --noEmit",
            "working_directory": f"{WS}\\qontinui-runner",
        },
        {
            "id": "check-runner-prettier",
            "type": "check",
            "name": "Runner: Prettier",
            "phase": "verification",
            "check_type": "format",
            "tool": "prettier",
            "command": "npx prettier --check 'src/**/*.{ts,tsx}'",
            "working_directory": f"{WS}\\qontinui-runner",
        },
        {
            "id": "check-supervisor-cargo-fmt",
            "type": "check",
            "name": "Supervisor: cargo fmt",
            "phase": "verification",
            "check_type": "format",
            "tool": "cargo",
            "command": "cargo fmt -- --check",
            "working_directory": f"{WS}\\qontinui-supervisor",
        },
        {
            "id": "check-supervisor-clippy",
            "type": "check",
            "name": "Supervisor: cargo clippy",
            "phase": "verification",
            "check_type": "lint",
            "tool": "cargo",
            "command": "cargo clippy -- -D warnings",
            "working_directory": f"{WS}\\qontinui-supervisor",
        },
    ],
)


# ============================================================================
# Insert all prompts
# ============================================================================

if __name__ == "__main__":
    print(f"=== Inserting {len(PROMPTS)} ground truth check prompts ===")
    count = 0
    fail = 0
    for p in PROMPTS:
        result = upsert_prompt(p)
        status = "OK" if result in ("NEW", "UPD") else "FAIL"
        print(f"  {result:4s} {p['id']}")
        if status == "OK":
            count += 1
        else:
            fail += 1
    print(f"\n=== Done: {count} added/updated, {fail} failed ===")
