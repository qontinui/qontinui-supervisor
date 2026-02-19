"""
Seed ground truth workflows into the runner's unified_workflows table.

These GT workflows serve as RAG examples for the workflow generator.
When the generator encounters a similar prompt, it retrieves the GT workflow
as a reference example with full JSON, helping it produce correct tool choices.

Usage: python scripts/seed_gt_workflows_to_runner.py
"""
import json
import urllib.request

RUNNER = "http://localhost:9876"

WS = r"C:\Users\jspin\Documents\qontinui_parent"


def api_post(url, data):
    body = json.dumps(data).encode()
    req = urllib.request.Request(
        url,
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"error": str(e)}


def api_get(url):
    req = urllib.request.Request(url)
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"error": str(e)}


def api_delete(url):
    req = urllib.request.Request(url, method="DELETE")
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"error": str(e)}


def make_check(check_id, name, check_type, tool, command, working_dir):
    """Create a check step dict."""
    return {
        "id": check_id,
        "type": "check",
        "name": name,
        "phase": "verification",
        "check_type": check_type,
        "tool": tool,
        "command": command,
        "working_directory": working_dir,
    }


def make_gate(check_ids):
    """Create a gate step that requires all checks."""
    return {
        "id": "step-gate",
        "type": "gate",
        "name": "All checks pass",
        "phase": "verification",
        "required_steps": check_ids,
    }


def make_agentic():
    """Standard agentic step for fixing check failures."""
    return {
        "id": "step-fix",
        "type": "prompt",
        "name": "Fix issues",
        "phase": "agentic",
        "content": "Fix any check failures found in the verification phase.",
    }


# ============================================================================
# Ground truth workflow definitions
# ============================================================================

GT_WORKFLOWS = []


def add_gt(name, description, checks):
    """Add a ground truth workflow."""
    GT_WORKFLOWS.append({
        "name": name,
        "description": description,
        "category": "ground_truth",
        "tags": ["ground_truth", "check_group", "reference"],
        "setup_steps": [],
        "verification_steps": [*checks, make_gate([c["id"] for c in checks])],
        "agentic_steps": [make_agentic()],
        "completion_steps": [],
        "max_iterations": 5,
    })


# ── qontinui-web (full: backend + frontend) ──────────────────────────────────
add_gt(
    "GT: qontinui-web code quality checks",
    f"Run all code quality checks on the qontinui-web repository at {WS}\\qontinui-web. "
    "This is a full-stack project with a Python (FastAPI) backend and TypeScript (Next.js) frontend.",
    [
        make_check("check-ruff-lint", "Python lint (ruff)", "lint", "ruff",
                    "ruff check .", f"{WS}\\qontinui-web\\backend"),
        make_check("check-ruff-format", "Python format (ruff)", "format", "ruff",
                    "ruff format --check .", f"{WS}\\qontinui-web\\backend"),
        make_check("check-mypy", "Python type check (mypy)", "typecheck", "mypy",
                    "mypy .", f"{WS}\\qontinui-web\\backend"),
        make_check("check-eslint", "TypeScript lint (ESLint)", "lint", "eslint",
                    "npx eslint .", f"{WS}\\qontinui-web\\frontend"),
        make_check("check-tsc", "TypeScript type check", "typecheck", "tsc",
                    "npx tsc --noEmit", f"{WS}\\qontinui-web\\frontend"),
        make_check("check-prettier", "Prettier format check", "format", "prettier",
                    "npx prettier --check .", f"{WS}\\qontinui-web\\frontend"),
    ],
)

# ── qontinui-web backend only ─────────────────────────────────────────────────
add_gt(
    "GT: qontinui-web backend Python checks",
    f"Run code quality checks on the qontinui-web backend (Python) at {WS}\\qontinui-web\\backend. "
    "This is a Python FastAPI project using ruff for linting/formatting and mypy for type checking.",
    [
        make_check("check-ruff-lint", "Python lint (ruff)", "lint", "ruff",
                    "ruff check .", f"{WS}\\qontinui-web\\backend"),
        make_check("check-ruff-format", "Python format (ruff)", "format", "ruff",
                    "ruff format --check .", f"{WS}\\qontinui-web\\backend"),
        make_check("check-mypy", "Python type check (mypy)", "typecheck", "mypy",
                    "mypy .", f"{WS}\\qontinui-web\\backend"),
    ],
)

# ── qontinui-web frontend only ────────────────────────────────────────────────
add_gt(
    "GT: qontinui-web frontend TypeScript checks",
    f"Run code quality checks on the qontinui-web frontend (TypeScript/React) at {WS}\\qontinui-web\\frontend. "
    "This is a Next.js TypeScript project using ESLint, TypeScript compiler, and Prettier.",
    [
        make_check("check-eslint", "TypeScript lint (ESLint)", "lint", "eslint",
                    "npx eslint .", f"{WS}\\qontinui-web\\frontend"),
        make_check("check-tsc", "TypeScript type check", "typecheck", "tsc",
                    "npx tsc --noEmit", f"{WS}\\qontinui-web\\frontend"),
        make_check("check-prettier", "Prettier format check", "format", "prettier",
                    "npx prettier --check .", f"{WS}\\qontinui-web\\frontend"),
    ],
)

# ── qontinui (core Python library) ────────────────────────────────────────────
add_gt(
    "GT: qontinui core library Python checks",
    f"Run code quality checks on the qontinui core Python library at {WS}\\qontinui. "
    "This is a Python library using black for formatting, ruff for linting, and mypy for type checking.",
    [
        make_check("check-black", "Python format (black)", "format", "black",
                    "black --check --line-length=100 .", f"{WS}\\qontinui"),
        make_check("check-ruff", "Python lint (ruff)", "lint", "ruff",
                    "ruff check .", f"{WS}\\qontinui"),
        make_check("check-mypy", "Python type check (mypy)", "typecheck", "mypy",
                    "mypy -p qontinui", f"{WS}\\qontinui"),
    ],
)

# ── qontinui-runner (Rust + TypeScript) ───────────────────────────────────────
add_gt(
    "GT: qontinui-runner code quality checks",
    f"Run all code quality checks on the qontinui-runner repository at {WS}\\qontinui-runner. "
    "This is a Tauri desktop app with a Rust backend and TypeScript frontend. "
    "Uses cargo fmt/clippy for Rust and ESLint/tsc/Prettier for TypeScript.",
    [
        make_check("check-cargo-fmt", "Rust format (cargo fmt)", "format", "cargo",
                    "cargo fmt -- --check", f"{WS}\\qontinui-runner\\src-tauri"),
        make_check("check-cargo-clippy", "Rust lint (cargo clippy)", "lint", "cargo",
                    "cargo clippy -- -D warnings", f"{WS}\\qontinui-runner\\src-tauri"),
        make_check("check-eslint", "TypeScript lint (ESLint)", "lint", "eslint",
                    "npx eslint .", f"{WS}\\qontinui-runner"),
        make_check("check-tsc", "TypeScript type check", "typecheck", "tsc",
                    "npx tsc --noEmit", f"{WS}\\qontinui-runner"),
        make_check("check-prettier", "Prettier format check", "format", "prettier",
                    "npx prettier --check 'src/**/*.{ts,tsx}'", f"{WS}\\qontinui-runner"),
    ],
)

# ── qontinui-supervisor (Rust only) ──────────────────────────────────────────
add_gt(
    "GT: qontinui-supervisor Rust checks",
    f"Run code quality checks on the qontinui-supervisor (Rust) at {WS}\\qontinui-supervisor. "
    "This is a pure Rust project using cargo fmt for formatting and cargo clippy for linting.",
    [
        make_check("check-cargo-fmt", "Rust format (cargo fmt)", "format", "cargo",
                    "cargo fmt -- --check", f"{WS}\\qontinui-supervisor"),
        make_check("check-cargo-clippy", "Rust lint (cargo clippy)", "lint", "cargo",
                    "cargo clippy -- -D warnings", f"{WS}\\qontinui-supervisor"),
    ],
)

# ── multistate (Python library) ──────────────────────────────────────────────
add_gt(
    "GT: multistate Python library checks",
    f"Run code quality checks on the multistate Python library at {WS}\\multistate. "
    "This is a Python library using black for formatting, ruff for linting, and mypy for type checking.",
    [
        make_check("check-black", "Python format (black)", "format", "black",
                    "black --check --line-length=100 .", f"{WS}\\multistate"),
        make_check("check-ruff", "Python lint (ruff)", "lint", "ruff",
                    "ruff check .", f"{WS}\\multistate"),
        make_check("check-mypy", "Python type check (mypy)", "typecheck", "mypy",
                    "mypy .", f"{WS}\\multistate"),
    ],
)

# ── Full stack (all repos) ───────────────────────────────────────────────────
add_gt(
    "GT: Full stack code quality checks",
    f"Run code quality checks across all qontinui repositories "
    f"(web backend at {WS}\\qontinui-web\\backend, web frontend at {WS}\\qontinui-web\\frontend, "
    f"runner at {WS}\\qontinui-runner, supervisor at {WS}\\qontinui-supervisor). "
    "Covers Python (ruff, mypy), TypeScript (ESLint, tsc, Prettier), and Rust (cargo fmt, clippy).",
    [
        make_check("check-web-backend-ruff-lint", "Web backend: ruff lint", "lint", "ruff",
                    "ruff check .", f"{WS}\\qontinui-web\\backend"),
        make_check("check-web-backend-ruff-format", "Web backend: ruff format", "format", "ruff",
                    "ruff format --check .", f"{WS}\\qontinui-web\\backend"),
        make_check("check-web-backend-mypy", "Web backend: mypy", "typecheck", "mypy",
                    "mypy .", f"{WS}\\qontinui-web\\backend"),
        make_check("check-web-frontend-eslint", "Web frontend: ESLint", "lint", "eslint",
                    "npx eslint .", f"{WS}\\qontinui-web\\frontend"),
        make_check("check-web-frontend-tsc", "Web frontend: tsc", "typecheck", "tsc",
                    "npx tsc --noEmit", f"{WS}\\qontinui-web\\frontend"),
        make_check("check-web-frontend-prettier", "Web frontend: Prettier", "format", "prettier",
                    "npx prettier --check .", f"{WS}\\qontinui-web\\frontend"),
        make_check("check-runner-cargo-fmt", "Runner: cargo fmt", "format", "cargo",
                    "cargo fmt -- --check", f"{WS}\\qontinui-runner\\src-tauri"),
        make_check("check-runner-clippy", "Runner: cargo clippy", "lint", "cargo",
                    "cargo clippy -- -D warnings", f"{WS}\\qontinui-runner\\src-tauri"),
        make_check("check-runner-eslint", "Runner: ESLint", "lint", "eslint",
                    "npx eslint .", f"{WS}\\qontinui-runner"),
        make_check("check-runner-tsc", "Runner: tsc", "typecheck", "tsc",
                    "npx tsc --noEmit", f"{WS}\\qontinui-runner"),
        make_check("check-runner-prettier", "Runner: Prettier", "format", "prettier",
                    "npx prettier --check 'src/**/*.{ts,tsx}'", f"{WS}\\qontinui-runner"),
        make_check("check-supervisor-cargo-fmt", "Supervisor: cargo fmt", "format", "cargo",
                    "cargo fmt -- --check", f"{WS}\\qontinui-supervisor"),
        make_check("check-supervisor-clippy", "Supervisor: cargo clippy", "lint", "cargo",
                    "cargo clippy -- -D warnings", f"{WS}\\qontinui-supervisor"),
    ],
)


# ============================================================================
# Seed: delete old GT workflows, insert new ones
# ============================================================================

if __name__ == "__main__":
    print("=== Seeding ground truth workflows to runner DB ===")

    # First, find and delete existing GT workflows
    existing = api_get(f"{RUNNER}/unified-workflows")
    if isinstance(existing, dict) and "data" in existing:
        workflows = existing["data"]
        gt_existing = [w for w in workflows if w.get("category") == "ground_truth"]
        if gt_existing:
            print(f"Deleting {len(gt_existing)} existing GT workflows...")
            for w in gt_existing:
                result = api_delete(f"{RUNNER}/unified-workflows/{w['id']}")
                status = "OK" if result.get("success") else "FAIL"
                print(f"  {status} delete {w['name']}")

    # Insert new GT workflows
    print(f"\nInserting {len(GT_WORKFLOWS)} GT workflows...")
    success = 0
    for wf in GT_WORKFLOWS:
        result = api_post(f"{RUNNER}/unified-workflows", wf)
        if result.get("success"):
            saved = result.get("data", {})
            print(f"  OK  {wf['name']} -> {saved.get('id', '?')}")
            success += 1
        else:
            print(f"  FAIL {wf['name']}: {result.get('error', '?')}")

    print(f"\n=== Done: {success}/{len(GT_WORKFLOWS)} inserted ===")
    print("\nNote: Embeddings will be computed automatically by the background job.")
    print("Wait ~30s for embeddings to be ready before running eval.")
