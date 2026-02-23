"""
Update generation_rules in runner DB to match the 3-type step system.

The old rules reference `api_request`, `check`, `test`, `gate`, `spec`, `shell_command`
as separate step types. The new system has only 3 types: command, ui_bridge, prompt.

Usage: python scripts/update_generation_rules_3types.py
"""
import json
import urllib.request


RUNNER = "http://localhost:9876"


def api_put(url, data):
    body = json.dumps(data).encode()
    req = urllib.request.Request(
        url,
        data=body,
        headers={"Content-Type": "application/json"},
        method="PUT",
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


# ============================================================================
# Rule updates: (rule_id, new_title, new_content)
# ============================================================================

UPDATES = [
    # --- schema_context / verification_quality ---
    (
        "seed-schema_context-verification_quality-6",
        "Deterministic verification step required",
        'verification_steps MUST include at least one deterministic, automated step — '
        'a `command` step (with check_type, test_type, or a shell command) or a `ui_bridge` step '
        '(with assert action). Do NOT use only `prompt` type steps for verification. '
        'Prompts provide AI judgment, not deterministic pass/fail results. '
        'A verification phase with ONLY prompt steps is INVALID.',
    ),
    (
        "seed-schema_context-verification_quality-7",
        "Code modification requires typecheck",
        'When the workflow creates or modifies source code files (TypeScript, Python, Rust, etc.), '
        'verification MUST include a `command` step with `check_type` set to the appropriate type checker:\n'
        '   - TypeScript/TSX/JSX: `{"type": "command", "check_type": "typecheck", "command": "npx tsc --noEmit", "working_directory": "..."}`\n'
        '   - Python: `{"type": "command", "check_type": "typecheck", "command": "mypy .", "working_directory": "..."}`\n'
        '   - Rust: `{"type": "command", "check_type": "typecheck", "command": "cargo check", "working_directory": "..."}`',
    ),
    (
        "seed-schema_context-verification_quality-8",
        "Web app verification requires SDK or Playwright",
        'When the workflow targets a web application (localhost:3001, localhost:1420), verification MUST include at least one of:\n'
        '   - A `command` step using curl to query UI Bridge SDK endpoints (preferred) to verify UI state\n'
        '   - A `command` step with `test_type: "playwright"` for browser-based verification\n'
        '   - A `ui_bridge` step with an `assert` action for direct element assertions',
    ),
    (
        "seed-schema_context-verification_quality-9",
        "Verification must be deterministic",
        'Every workflow with 2+ verification steps should ensure ALL non-prompt verification steps '
        'are meaningful and required. If a step is worth including in verification, its failure should '
        'be visible to the verification loop. Do NOT include verification steps whose failures would '
        'be silently ignored.',
    ),
    (
        "seed-schema_context-verification_quality-11",
        "Test steps with inline commands use repository",
        'When a `command` step with `test_type` runs a shell command (e.g., `npx playwright test ...`, `cargo test ...`), '
        'set `test_type: "repository"`. The `test_type: "playwright"` value is ONLY for steps that provide `code` with '
        'Playwright assertions to be executed via CDP. Using `"playwright"` for shell commands causes a "No test_id specified" error.',
    ),
    (
        "seed-schema_context-verification_quality-12",
        "Next.js App Router path conventions",
        'For Next.js projects using the App Router (`src/app/`), components are organized under route groups '
        'like `src/app/(app)/`. When creating `command` steps with `check_type`, use the correct working directory paths '
        '— e.g., the frontend directory, not `src/components/`. Always verify path patterns match the actual project structure.',
    ),
    (
        "seed-schema_context-verification_quality-15",
        "SDK verification must verify content",
        'When a `command` step calls a UI Bridge SDK endpoint via curl (`/ui-bridge/sdk/...`), '
        'checking only the exit code is INSUFFICIENT. SDK endpoints return 200 even for empty results '
        '(e.g., `ai/search` returns `{"results": [], "total": 0}`). Every SDK verification command '
        'MUST pipe to `grep` with expected text/element content to verify meaningful results.',
    ),
    (
        "seed-schema_context-verification_quality-16",
        "Agentic-verification correspondence",
        'Each `prompt` step in `agentic_steps` describes a specific piece of work (e.g., "implement drag-and-drop", '
        '"add thumbnails"). For EACH agentic step, `verification_steps` MUST contain at least one deterministic '
        '`command` or `ui_bridge` step that verifies the output of that work.',
    ),
    (
        "seed-schema_context-verification_quality-17",
        "Only 3 step types exist",
        'The only valid step types are `command`, `ui_bridge`, and `prompt`. Do NOT use `shell_command`, '
        '`api_request`, `mcp_call`, `check`, `check_group`, `test`, `gate`, or `spec` — these are not valid types. '
        'Tests are run via `command` with `test_type` set. Checks are run via `command` with `check_type` set.',
    ),

    # --- hardener / conversion_rules ---
    (
        "seed-hardener-conversion_rules-1",
        "Convert prompt steps to deterministic",
        'Convert `prompt` steps to deterministic equivalents. Only 3 step types are valid: `command`, `ui_bridge`, `prompt`.\n'
        '| Prompt check type | Convert to | Method |\n'
        '|---|---|---|\n'
        '| UI element presence/structure | `command` | curl to UI Bridge SDK endpoint, pipe to grep for content check |\n'
        '| Content/text on page | `command` | curl to UI Bridge SDK `/ai/search`, pipe to grep for expected text |\n'
        '| File existence | `command` | `check_type: "custom_command"` with `test -f <path>` |\n'
        '| File content | `command` | `check_type: "custom_command"` with `grep -q <pattern> <file>` |\n'
        '| Code quality (lint) | `command` | `check_type: "lint"` with appropriate command |\n'
        '| Code quality (typecheck) | `command` | `check_type: "typecheck"` with appropriate command |\n'
        '| API health/response | `command` | curl to endpoint, check exit code |\n'
        '| UI assertion | `ui_bridge` | Use assert action with target and expected value |\n'
        '| Subjective/qualitative | Keep as `prompt` | Cannot be made deterministic |',
    ),
    (
        "seed-hardener-conversion_rules-2",
        "Replace Playwright with SDK checks",
        'When the UI Bridge SDK is connected, Playwright-based UI verification tests should be converted to '
        '`command` steps (using curl to SDK endpoints piped to grep) or `ui_bridge` steps. The SDK provides direct '
        'programmatic access to registered UI elements without requiring a Playwright browser instance. '
        'If a single Playwright test checks multiple things, split it into multiple `command` or `ui_bridge` steps '
        '— one per distinct verification concern. Tests that require keyboard shortcuts, file uploads, '
        'or screenshot comparisons MUST remain as `command` steps with `test_type: "playwright"`.',
    ),
    (
        "seed-hardener-conversion_rules-3",
        "Strengthen weak SDK verification commands",
        'If an existing `command` step calls a UI Bridge SDK endpoint via curl but only checks exit code (no grep), '
        'add a pipe to `grep` to verify meaningful content. A successful curl to the SDK just means the endpoint is '
        'reachable — it doesn\'t verify the UI state. SDK endpoints return 200 even for EMPTY results.',
    ),
    (
        "seed-hardener-conversion_rules-4",
        "Inject page navigation before SDK checks",
        'If the workflow\'s setup_steps include a page navigation step (curl POST to `/ui-bridge/sdk/page/navigate` '
        'or a `ui_bridge` step with `action: "navigate"`), the verification phase MUST also navigate to that same '
        'URL before any SDK element checks. Use a `command` step with curl or a `ui_bridge` navigate step.',
    ),
    (
        "seed-hardener-conversion_rules-5",
        "Agentic-verification correspondence",
        'Examine EACH prompt step in `agentic_steps` and identify the distinct goals/features it describes. '
        'Then check whether `verification_steps` has at least one deterministic `command` or `ui_bridge` step '
        'that would FAIL if that specific goal was NOT implemented. For each uncovered agentic goal, ADD a new '
        '`command` verification step (e.g., curl to SDK endpoint piped to grep for expected content).',
    ),

    # --- hardener / critical_rules ---
    (
        "seed-hardener-critical_rules-4",
        "Adding steps is allowed",
        'If a Playwright test step checks multiple things, you MAY replace it with multiple `command` or `ui_bridge` '
        'steps. You MAY also add NEW verification steps to cover uncovered agentic goals. Keep original `id`s on '
        'existing steps and generate new UUIDs for additions.',
    ),
    (
        "seed-hardener-critical_rules-7",
        "Only 3 step types",
        'All steps must use `command`, `ui_bridge`, or `prompt`. Do NOT output `api_request`, `check`, `test`, '
        '`gate`, or `spec` types.',
    ),
    (
        "seed-hardener-critical_rules-8",
        "Command with check_type fields",
        'For check conversions, use `command` type with `check_type`, `command`, and `working_directory` fields.',
    ),
    (
        "seed-hardener-critical_rules-9",
        "Do not convert existing command+check_type steps",
        'Do NOT convert `command` steps that already have `check_type` set (lint, typecheck, etc.) — they are already deterministic.',
    ),
    (
        "seed-hardener-critical_rules-10",
        "SDK verification uses command+curl",
        'Use `command` steps with curl piped to grep for SDK-based verification, not `api_request`.',
    ),

    # --- verification / check_rules ---
    (
        "seed-verification-check_rules-1",
        "command step validation (plain shell mode)",
        '`command` is a real, syntactically valid shell command (not a placeholder like "echo TODO" or "/path/to/script"). '
        '`working_directory`, if present, looks like a real path. `timeout_seconds` is reasonable. `fail_on_error` is appropriate. '
        'Step type MUST be `command` (not `shell_command`).',
    ),
    (
        "seed-verification-check_rules-2",
        "command step validation (check mode — check_type set)",
        '`check_type` and `command` are consistent: "lint" → linter, "typecheck" → type checker, "format" → formatter check, '
        '"analyze" → static analysis, "security" → security scanner, "custom_command" → any command. '
        '`command` is non-empty and syntactically valid. Step type MUST be `command` (not `check`).',
    ),
    (
        "seed-verification-check_rules-3",
        "command step validation (test mode — test_type set)",
        'Has either `command` (for repository/custom_command) or `code` (for playwright/python). '
        '`test_type` is one of: playwright, qontinui_vision, python, repository, custom_command. '
        'The command/code looks substantive (not a placeholder). Step type MUST be `command` (not `test`).',
    ),
    (
        "seed-verification-check_rules-4",
        "ui_bridge step validation",
        '`action` is one of: navigate, execute, assert, snapshot. Required fields vary by action: '
        'navigate needs `url`, execute needs `instruction`, assert needs `target` and `assert_type`. '
        '`timeout_ms` is reasonable if set.',
    ),
    (
        "seed-verification-check_rules-6",
        "Invalid step type detection",
        'If any step uses a type other than `command`, `ui_bridge`, or `prompt`, flag it immediately. '
        'Common mistakes: using `check` (should be `command` with `check_type`), `test` (should be `command` with `test_type`), '
        '`api_request` (should be `command` with curl), `shell_command` (should be `command`), `gate` or `spec` (removed).',
    ),
    (
        "seed-verification-check_rules-7",
        "Step type consistency",
        'All step types must be one of: `command`, `ui_bridge`, `prompt`. No other types are valid. '
        'Verify that the `type` field of every step matches this constraint.',
    ),
]


# ============================================================================
# Apply updates
# ============================================================================

if __name__ == "__main__":
    print("=== Updating generation rules for 3-type step system ===\n")

    success = 0
    fail = 0

    for rule_id, new_title, new_content in UPDATES:
        result = api_put(
            f"{RUNNER}/generation-rules/{rule_id}",
            {"title": new_title, "content": new_content},
        )
        if result.get("success") or result.get("data"):
            print(f"  OK  {rule_id}")
            success += 1
        else:
            error = result.get("error", "?")
            print(f"  FAIL {rule_id}: {error}")
            fail += 1

    print(f"\n=== Done: {success} updated, {fail} failed ===")
