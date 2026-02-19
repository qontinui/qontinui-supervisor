"""
Seed UI Bridge ground truth workflows and eval prompts.

These GT workflows demonstrate deterministic UI test sequences using the
UI Bridge step type. Each workflow navigates to a page, interacts with
elements, and asserts expected state via first-class ui_bridge steps.

Prerequisites:
  - Web frontend running at http://localhost:3001
  - Browser tab open on the frontend (UI Bridge requires WebSocket connection)
  - Runner running at http://localhost:9876 (for unified workflow seeding)
  - Supervisor running at http://localhost:9875 (for eval prompt seeding)

Usage: python scripts/seed_ui_bridge_ground_truth.py
"""
import json
import urllib.request

RUNNER = "http://localhost:9876"
SUPERVISOR = "http://localhost:9875"


# ============================================================================
# HTTP helpers
# ============================================================================

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
        return {"ok": False, "message": str(e)}


def upsert_prompt(prompt_data):
    """Insert or update a test prompt."""
    pid = prompt_data["id"]
    result = api_post(f"{SUPERVISOR}/eval/test-suite", prompt_data)
    if result.get("ok"):
        return "NEW"
    result = api_put(f"{SUPERVISOR}/eval/test-suite/{pid}", prompt_data)
    if result.get("ok"):
        return "UPD"
    return f"FAIL: {result.get('message', '?')}"


# ============================================================================
# Step builder helpers
# ============================================================================

def navigate_step(step_id, name, url, phase="setup"):
    """Create a page navigation step using the ui_bridge step type."""
    return {
        "id": step_id,
        "type": "ui_bridge",
        "name": name,
        "phase": phase,
        "ui_bridge_action": "navigate",
        "ui_bridge_url": url,
    }


def ai_assert_step(step_id, name, target, assert_type="visible", expected=None,
                   phase="verification", retry_count=5, retry_delay_ms=3000):
    """Create an AI assertion step using the ui_bridge step type.

    target: natural language description of element (e.g., "Settings", "Runner Name input")
    assert_type: one of visible, hidden, enabled, disabled, checked, unchecked, focused,
                 containsText, hasText
    expected: expected value (for containsText/hasText assertions)

    retry_count/retry_delay_ms: after page navigation, the SSE connection
    reconnects after a few seconds. Retries allow the step to wait.
    """
    step = {
        "id": step_id,
        "type": "ui_bridge",
        "name": name,
        "phase": phase,
        "ui_bridge_action": "assert",
        "ui_bridge_target": target,
        "ui_bridge_assert_type": assert_type,
    }
    if expected is not None:
        step["ui_bridge_expected"] = expected
    if retry_count > 0:
        step["retry_count"] = retry_count
        step["retry_delay_ms"] = retry_delay_ms
    return step


def ai_execute_step(step_id, name, instruction, phase="verification",
                    retry_count=3, retry_delay_ms=2000):
    """Create an AI execute step using the ui_bridge step type.

    instruction: simple NL instruction like "click Settings" or "type 'hello' in search"
    Keep instructions short -- the NL parser works best with simple patterns.
    """
    step = {
        "id": step_id,
        "type": "ui_bridge",
        "name": name,
        "phase": phase,
        "ui_bridge_action": "execute",
        "ui_bridge_instruction": instruction,
    }
    if retry_count > 0:
        step["retry_count"] = retry_count
        step["retry_delay_ms"] = retry_delay_ms
    return step


def make_agentic():
    """Standard agentic step for fixing UI test failures."""
    return {
        "id": "step-fix",
        "type": "prompt",
        "name": "Fix UI test failures",
        "phase": "agentic",
        "content": (
            "Fix any UI Bridge test failures found in the verification phase. "
            "Check that the web frontend is running, a browser tab is connected, "
            "and that element IDs match the current UI. If navigation timing is "
            "an issue, the page may need more time to load after navigation."
        ),
    }


# ============================================================================
# Ground truth workflow definitions
# ============================================================================

GT_WORKFLOWS = []
EVAL_PROMPTS = []


def add_gt(workflow_id, name, description, prompt_text, setup_steps,
           verification_steps, complexity="simple"):
    """Add a ground truth workflow and its eval prompt."""
    workflow = {
        "name": name,
        "description": description,
        "category": "ground_truth",
        "tags": ["ground_truth", "ui_bridge", "test"],
        "setup_steps": setup_steps,
        "verification_steps": verification_steps,
        "agentic_steps": [make_agentic()],
        "completion_steps": [],
        "max_iterations": 3,
    }
    GT_WORKFLOWS.append(workflow)

    EVAL_PROMPTS.append({
        "id": workflow_id,
        "prompt": prompt_text,
        "category": "ui_bridge",
        "complexity": complexity,
        "expected_phases": None,
        "expected_step_types": ["ui_bridge"],
        "tags": ["ui_bridge", "ground_truth"],
        "ground_truth_json": json.dumps(workflow),
        "enabled": True,
        "created_at": "2026-02-19T00:00:00Z",
        "updated_at": "2026-02-19T00:00:00Z",
    })


# -- 1. Navigate to Settings page and verify ------------------------------------
add_gt(
    "gt-ui-bridge-nav-settings",
    "GT: UI Bridge - Navigate to Settings",
    "Navigate to the Settings page and verify it loaded with the Account "
    "section showing connection status, device info, and runner name input.",
    "Navigate to the Settings page and verify it loaded correctly",
    setup_steps=[
        navigate_step(
            "step-nav-settings",
            "Navigate to Settings page",
            "http://localhost:3001/settings",
        ),
    ],
    verification_steps=[
        ai_assert_step(
            "step-verify-settings-heading",
            "Verify Settings page heading is visible",
            "Settings",
        ),
        ai_assert_step(
            "step-verify-account-section",
            "Verify Account section heading is visible",
            "Account",
        ),
        ai_assert_step(
            "step-verify-runner-name-input",
            "Verify Runner Name input field exists",
            "Runner Name",
        ),
    ],
)

# -- 2. Navigate to Discoveries page and verify tabs ----------------------------
add_gt(
    "gt-ui-bridge-nav-discoveries",
    "GT: UI Bridge - Navigate to Discoveries",
    "Navigate to the Discoveries page and verify it loaded with the "
    "Pending/Accepted/Rejected tabs visible.",
    "Navigate to the Discoveries page and verify the tabs are present",
    setup_steps=[
        navigate_step(
            "step-nav-discoveries",
            "Navigate to Discoveries page",
            "http://localhost:3001/discoveries",
        ),
    ],
    verification_steps=[
        ai_assert_step(
            "step-verify-discoveries-heading",
            "Verify Discoveries page heading is visible",
            "Discoveries",
        ),
        ai_assert_step(
            "step-verify-pending-tab",
            "Verify Pending tab is visible",
            "Pending",
        ),
        ai_assert_step(
            "step-verify-accepted-tab",
            "Verify Accepted tab is visible",
            "Accepted",
        ),
        ai_assert_step(
            "step-verify-rejected-tab",
            "Verify Rejected tab is visible",
            "Rejected",
        ),
    ],
)

# -- 3. Navigate to Agentic Settings and verify --------------------------------
add_gt(
    "gt-ui-bridge-nav-agentic-settings",
    "GT: UI Bridge - Navigate to Agentic Settings",
    "Navigate to the Agentic Settings page and verify it loaded with the "
    "Advanced AI configuration form including memory compression settings.",
    "Navigate to the AI agentic settings page and verify the configuration form is visible",
    setup_steps=[
        navigate_step(
            "step-nav-agentic-settings",
            "Navigate to Agentic Settings page",
            "http://localhost:3001/settings/agentic",
        ),
    ],
    verification_steps=[
        ai_assert_step(
            "step-verify-agentic-heading",
            "Verify Advanced AI heading is visible",
            "Advanced AI",
        ),
        ai_assert_step(
            "step-verify-threshold-tokens",
            "Verify Threshold Tokens field is visible",
            "Threshold Tokens",
        ),
        ai_assert_step(
            "step-verify-target-tokens",
            "Verify Target Tokens field is visible",
            "Target Tokens",
        ),
    ],
)

# -- 4. Navigate to Automation Builder (no project) ----------------------------
add_gt(
    "gt-ui-bridge-nav-builder",
    "GT: UI Bridge - Navigate to Automation Builder",
    "Navigate to the Automation Builder page and verify it loads. Without "
    "a project selected, it shows a 'No project selected' message with a "
    "link back to the dashboard.",
    "Navigate to the Automation Builder page and verify the page loads",
    setup_steps=[
        navigate_step(
            "step-nav-builder",
            "Navigate to Automation Builder",
            "http://localhost:3001/automation-builder",
        ),
    ],
    verification_steps=[
        ai_assert_step(
            "step-verify-no-project",
            "Verify 'No project selected' message is visible",
            "No project selected",
        ),
        ai_assert_step(
            "step-verify-dashboard-link",
            "Verify 'Go to Dashboard' link is visible",
            "Go to Dashboard",
        ),
    ],
)

# -- 5. Discoveries tab switching -----------------------------------------------
add_gt(
    "gt-ui-bridge-discoveries-tabs",
    "GT: UI Bridge - Switch Discoveries tabs",
    "Navigate to Discoveries, verify tabs are present, then click the "
    "Accepted tab to test tab switching.",
    "Navigate to the Discoveries page and switch between the discovery tabs",
    setup_steps=[
        navigate_step(
            "step-nav-discoveries",
            "Navigate to Discoveries page",
            "http://localhost:3001/discoveries",
        ),
    ],
    verification_steps=[
        ai_assert_step(
            "step-verify-tabs-present",
            "Verify tabs are present before switching",
            "Pending",
        ),
        ai_execute_step(
            "step-click-accepted-tab",
            "Click the Accepted tab",
            "click Accepted",
        ),
        ai_assert_step(
            "step-verify-accepted-active",
            "Verify Discoveries page still visible after tab click",
            "Discoveries",
        ),
    ],
    complexity="medium",
)

# -- 6. Settings runner name input field ----------------------------------------
add_gt(
    "gt-ui-bridge-settings-runner-name",
    "GT: UI Bridge - Verify Settings form fields",
    "Navigate to Settings and verify the runner name input field is present "
    "and accessible.",
    "Navigate to Settings and verify the runner name input field is present and editable",
    setup_steps=[
        navigate_step(
            "step-nav-settings",
            "Navigate to Settings page",
            "http://localhost:3001/settings",
        ),
    ],
    verification_steps=[
        ai_assert_step(
            "step-verify-settings-loaded",
            "Verify Settings page heading is visible",
            "Settings",
        ),
        ai_assert_step(
            "step-verify-runner-name",
            "Verify Runner Name input field exists",
            "Runner Name",
        ),
        ai_assert_step(
            "step-verify-runner-name-enabled",
            "Verify Runner Name input is enabled",
            "Runner Name",
            assert_type="enabled",
        ),
    ],
)

# -- 7. Agentic Settings config validation -------------------------------------
add_gt(
    "gt-ui-bridge-agentic-config-fields",
    "GT: UI Bridge - Verify Agentic Settings configuration",
    "Navigate to the Agentic Settings page and verify all memory compression "
    "configuration fields are present: Threshold Tokens, Target Tokens, "
    "Keep Recent Items, and Summarize Batch Size.",
    "Navigate to the Agentic Settings page and verify all configuration fields are present",
    setup_steps=[
        navigate_step(
            "step-nav-agentic",
            "Navigate to Agentic Settings",
            "http://localhost:3001/settings/agentic",
        ),
    ],
    verification_steps=[
        ai_assert_step(
            "step-verify-heading",
            "Verify Advanced AI heading is visible",
            "Advanced AI",
        ),
        ai_assert_step(
            "step-verify-threshold-tokens",
            "Verify Threshold Tokens field is visible",
            "Threshold Tokens",
        ),
        ai_assert_step(
            "step-verify-target-tokens",
            "Verify Target Tokens field is visible",
            "Target Tokens",
        ),
        ai_assert_step(
            "step-verify-keep-recent",
            "Verify Keep Recent Items field is visible",
            "Keep Recent Items",
        ),
        ai_assert_step(
            "step-verify-batch-size",
            "Verify Summarize Batch Size field is visible",
            "Summarize Batch Size",
        ),
    ],
    complexity="medium",
)


# ============================================================================
# Seed: workflows to runner + eval prompts to supervisor
# ============================================================================

if __name__ == "__main__":
    print("=" * 60)
    print("Seeding UI Bridge ground truth workflows")
    print("=" * 60)

    # -- Step 1: Seed GT workflows to runner ------------------------------------
    print("\n--- Seeding workflows to runner (port 9876) ---")

    # Find and delete existing UI Bridge GT workflows
    existing = api_get(f"{RUNNER}/unified-workflows")
    if isinstance(existing, dict) and "data" in existing:
        workflows = existing["data"]
        ui_gt = [
            w for w in workflows
            if w.get("category") == "ground_truth"
            and "ui_bridge" in (w.get("tags") or [])
        ]
        if ui_gt:
            print(f"Deleting {len(ui_gt)} existing UI Bridge GT workflows...")
            for w in ui_gt:
                result = api_delete(f"{RUNNER}/unified-workflows/{w['id']}")
                status = "OK" if result.get("success") else "FAIL"
                print(f"  {status} delete {w['name']}")
    elif "error" in (existing or {}):
        print(f"  WARNING: Could not fetch existing workflows: {existing['error']}")
        print("  (Runner may not be running -- skipping workflow seeding)")

    # Insert new GT workflows
    print(f"\nInserting {len(GT_WORKFLOWS)} UI Bridge GT workflows...")
    wf_success = 0
    for wf in GT_WORKFLOWS:
        result = api_post(f"{RUNNER}/unified-workflows", wf)
        if result.get("success"):
            saved = result.get("data", {})
            print(f"  OK   {wf['name']} -> {saved.get('id', '?')}")
            wf_success += 1
        else:
            print(f"  FAIL {wf['name']}: {result.get('error', '?')}")

    print(f"\nWorkflows: {wf_success}/{len(GT_WORKFLOWS)} inserted")

    # -- Step 2: Seed eval prompts to supervisor --------------------------------
    print("\n--- Seeding eval prompts to supervisor (port 9875) ---")

    prompt_success = 0
    prompt_fail = 0
    for p in EVAL_PROMPTS:
        result = upsert_prompt(p)
        status = "OK" if result in ("NEW", "UPD") else "FAIL"
        print(f"  {result:4s} {p['id']}")
        if status == "OK":
            prompt_success += 1
        else:
            prompt_fail += 1

    print(f"\nEval prompts: {prompt_success}/{len(EVAL_PROMPTS)} added/updated"
          f"{f', {prompt_fail} failed' if prompt_fail else ''}")

    # -- Summary ----------------------------------------------------------------
    print("\n" + "=" * 60)
    print(f"Done: {wf_success} workflows, {prompt_success} eval prompts")
    print("=" * 60)
    print("\nNote: Embeddings will be computed automatically by the background job.")
    print("Wait ~30s for embeddings to be ready before running eval.")
