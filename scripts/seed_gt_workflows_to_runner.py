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
    """Create a command step with check_type set (unified command type)."""
    return {
        "id": check_id,
        "type": "command",
        "name": name,
        "phase": "verification",
        "check_type": check_type,
        "tool": tool,
        "command": command,
        "working_directory": working_dir,
    }


def make_command(step_id, name, phase, command, working_dir=None, **kwargs):
    """Create a plain command step."""
    step = {
        "id": step_id,
        "type": "command",
        "name": name,
        "phase": phase,
        "command": command,
    }
    if working_dir:
        step["working_directory"] = working_dir
    step.update(kwargs)
    return step


def make_test(step_id, name, test_type, command, working_dir=None, **kwargs):
    """Create a command step with test_type set (unified command type)."""
    step = {
        "id": step_id,
        "type": "command",
        "name": name,
        "phase": "verification",
        "test_type": test_type,
        "command": command,
    }
    if working_dir:
        step["working_directory"] = working_dir
    step.update(kwargs)
    return step


def make_prompt(step_id, name, phase, content):
    """Create a prompt step."""
    return {
        "id": step_id,
        "type": "prompt",
        "name": name,
        "phase": phase,
        "content": content,
    }


def make_ui_bridge(step_id, name, phase, action, **kwargs):
    """Create a ui_bridge step."""
    step = {
        "id": step_id,
        "type": "ui_bridge",
        "name": name,
        "phase": phase,
        "action": action,
    }
    step.update(kwargs)
    return step


def make_agentic(content="Fix any check failures found in the verification phase."):
    """Standard agentic step for fixing check failures."""
    return {
        "id": "step-fix",
        "type": "prompt",
        "name": "Fix issues",
        "phase": "agentic",
        "content": content,
    }


# ============================================================================
# Ground truth workflow definitions
# ============================================================================

GT_WORKFLOWS = []


def add_gt(name, description, checks, tags=None, setup=None,
           agentic=None, completion=None, max_iterations=5):
    """Add a ground truth workflow."""
    GT_WORKFLOWS.append({
        "name": name,
        "description": description,
        "category": "ground_truth",
        "tags": tags or ["ground_truth", "command", "reference"],
        "setup_steps": setup or [],
        "verification_steps": checks,
        "agentic_steps": agentic or [make_agentic()],
        "completion_steps": completion or [],
        "max_iterations": max_iterations,
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


# ── Web frontend UI Bridge verification ────────────────────────────────────
add_gt(
    "GT: qontinui-web frontend UI verification",
    "Verify the qontinui-web frontend at http://localhost:3001 is working correctly. "
    "Connect to the UI Bridge SDK, navigate to key pages, and verify elements render properly. "
    "Fix any frontend issues found.",
    tags=["ground_truth", "ui_bridge", "command", "web", "reference"],
    setup=[
        make_command("setup-sdk-connect", "Connect UI Bridge SDK", "setup",
                     'curl -sf -X POST http://localhost:9876/ui-bridge/sdk/connect '
                     '-H "Content-Type: application/json" '
                     '-d "{\\"url\\": \\"http://localhost:3001\\"}"',
                     fail_on_error=True),
        make_command("setup-navigate", "Navigate to workflows page", "setup",
                     'curl -sf -X POST http://localhost:9876/ui-bridge/sdk/page/navigate '
                     '-H "Content-Type: application/json" '
                     '-d "{\\"url\\": \\"http://localhost:3001/build/workflows\\"}"'),
    ],
    checks=[
        make_command("verify-navigate", "Navigate to target page", "verification",
                     'curl -sf -X POST http://localhost:9876/ui-bridge/sdk/page/navigate '
                     '-H "Content-Type: application/json" '
                     '-d "{\\"url\\": \\"http://localhost:3001/build/workflows\\"}"',
                     fail_on_error=True,
                     retry={"count": 3, "delay_ms": 3000}),
        make_command("verify-snapshot", "Verify page elements loaded", "verification",
                     'curl -sf http://localhost:9876/ui-bridge/sdk/snapshot | grep "elements"',
                     fail_on_error=True,
                     retry={"count": 3, "delay_ms": 3000}),
        make_check("verify-eslint", "TypeScript lint (ESLint)", "lint", "eslint",
                    "npx eslint .", f"{WS}\\qontinui-web\\frontend"),
        make_check("verify-tsc", "TypeScript type check", "typecheck", "tsc",
                    "npx tsc --noEmit", f"{WS}\\qontinui-web\\frontend"),
    ],
    agentic=[make_agentic(
        "Fix any issues found in the verification phase. For UI rendering issues, "
        "inspect the component code and fix JSX/CSS problems. For lint/type errors, "
        "fix the TypeScript source. Use sdk_snapshot and sdk_elements tools to inspect "
        "the current page state and identify what's wrong."
    )],
)

# ── Backend API + test workflow ───────────────────────────────────────────
add_gt(
    "GT: qontinui-web backend tests and checks",
    f"Run the qontinui-web backend test suite and code quality checks at {WS}\\qontinui-web\\backend. "
    "Runs pytest for unit/integration tests along with ruff and mypy. Fixes any failures.",
    tags=["ground_truth", "command", "test", "python", "reference"],
    checks=[
        make_test("verify-pytest", "Run pytest", "repository",
                  "poetry run pytest -x --tb=short",
                  f"{WS}\\qontinui-web\\backend"),
        make_check("verify-ruff-lint", "Python lint (ruff)", "lint", "ruff",
                    "ruff check .", f"{WS}\\qontinui-web\\backend"),
        make_check("verify-mypy", "Python type check (mypy)", "typecheck", "mypy",
                    "mypy .", f"{WS}\\qontinui-web\\backend"),
    ],
    agentic=[make_agentic(
        "Fix any test failures or code quality issues found in the verification phase. "
        "For test failures, read the test output carefully and fix the implementation code "
        "(not the tests) unless the tests are clearly wrong. For lint/type errors, fix the "
        "source code to comply with the project's coding standards."
    )],
)

# ── Multi-phase feature development workflow ──────────────────────────────
add_gt(
    "GT: Web frontend feature development",
    "Develop a feature on the qontinui-web frontend. Sets up the environment, "
    "runs code quality checks and UI verification, iterates with AI to fix issues, "
    "and cleans up on completion. Uses all 3 step types across all 4 phases.",
    tags=["ground_truth", "command", "ui_bridge", "prompt", "multi-phase", "reference"],
    setup=[
        make_command("setup-sdk-connect", "Connect UI Bridge SDK", "setup",
                     'curl -sf -X POST http://localhost:9876/ui-bridge/sdk/connect '
                     '-H "Content-Type: application/json" '
                     '-d "{\\"url\\": \\"http://localhost:3001\\"}"',
                     fail_on_error=True),
        make_prompt("setup-plan", "Plan the implementation", "setup",
                    "Read the task description and plan the implementation approach. "
                    "Identify which files need to be modified, what components are involved, "
                    "and what the acceptance criteria are. Read the relevant source files."),
    ],
    checks=[
        make_check("verify-eslint", "TypeScript lint (ESLint)", "lint", "eslint",
                    "npx eslint .", f"{WS}\\qontinui-web\\frontend"),
        make_check("verify-tsc", "TypeScript type check", "typecheck", "tsc",
                    "npx tsc --noEmit", f"{WS}\\qontinui-web\\frontend"),
        make_check("verify-prettier", "Prettier format check", "format", "prettier",
                    "npx prettier --check .", f"{WS}\\qontinui-web\\frontend"),
        make_command("verify-ui-elements", "Verify UI renders correctly", "verification",
                     'curl -sf http://localhost:9876/ui-bridge/sdk/snapshot | grep "elements"',
                     fail_on_error=True,
                     retry={"count": 3, "delay_ms": 3000}),
        make_prompt("verify-ai-review", "AI review of implementation", "verification",
                    "Review the verification results. Check if the feature is correctly implemented "
                    "by examining the UI state via SDK tools and the code quality check results. "
                    "If there are issues, describe exactly what needs to be fixed."),
    ],
    agentic=[make_agentic(
        "Based on the verification results, implement or fix the feature. Use the Edit tool "
        "to modify source files. Use sdk_snapshot and sdk_elements to inspect the current UI state. "
        "Fix any lint, type, or formatting errors. Ensure the feature matches the requirements."
    )],
    completion=[
        make_command("completion-format", "Auto-format code", "completion",
                     "npx prettier --write .", f"{WS}\\qontinui-web\\frontend",
                     fail_on_error=False),
        make_prompt("completion-summary", "Summarize changes", "completion",
                    "Write a concise summary of all changes made during this workflow. "
                    "Include: files modified, features implemented, issues fixed, "
                    "and any remaining considerations."),
    ],
    max_iterations=8,
)

# ── Runner Rust test workflow ─────────────────────────────────────────────
add_gt(
    "GT: qontinui-runner Rust tests and checks",
    f"Run the qontinui-runner Rust test suite and code quality checks at {WS}\\qontinui-runner\\src-tauri. "
    "Runs cargo test for unit tests along with cargo fmt and clippy. Fixes any failures.",
    tags=["ground_truth", "command", "test", "rust", "reference"],
    checks=[
        make_test("verify-cargo-test", "Run cargo test", "repository",
                  "cargo test -- --nocapture",
                  f"{WS}\\qontinui-runner\\src-tauri"),
        make_check("verify-cargo-fmt", "Rust format (cargo fmt)", "format", "cargo",
                    "cargo fmt -- --check", f"{WS}\\qontinui-runner\\src-tauri"),
        make_check("verify-cargo-clippy", "Rust lint (cargo clippy)", "lint", "cargo",
                    "cargo clippy -- -D warnings", f"{WS}\\qontinui-runner\\src-tauri"),
    ],
    agentic=[make_agentic(
        "Fix any test failures or code quality issues in the Rust codebase. "
        "For test failures, read the test output and fix the implementation. "
        "For clippy warnings, apply the suggested fixes. "
        "For formatting issues, the agentic phase should fix the source formatting."
    )],
)

# ── Service health check workflow ─────────────────────────────────────────
add_gt(
    "GT: Dev environment health check",
    "Verify that all qontinui development services are running and healthy. "
    "Checks the web backend API, web frontend, runner API, and supervisor.",
    tags=["ground_truth", "command", "health", "reference"],
    checks=[
        make_command("verify-backend", "Check backend health", "verification",
                     "curl -sf http://localhost:8000/health",
                     fail_on_error=True),
        make_command("verify-frontend", "Check frontend health", "verification",
                     "curl -sf http://localhost:3001",
                     fail_on_error=True),
        make_command("verify-runner", "Check runner API health", "verification",
                     "curl -sf http://localhost:9876/health",
                     fail_on_error=True),
        make_command("verify-supervisor", "Check supervisor health", "verification",
                     "curl -sf http://localhost:9875/health",
                     fail_on_error=True),
    ],
    agentic=[make_agentic(
        "Some services failed their health check. Diagnose which services are down "
        "by reading the dev logs in .dev-logs/. Try restarting failed services using "
        "the dev-start.ps1 script (e.g., dev-start.ps1 -Backend for backend). "
        "Wait for the service to start before the next verification cycle."
    )],
    max_iterations=3,
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
