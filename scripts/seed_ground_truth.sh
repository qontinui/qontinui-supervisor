#!/usr/bin/env bash
# Insert 15 curated test prompts with ground truth workflows
BASE="http://localhost:9875"
NOW=$(date -u +"%Y-%m-%dT%H:%M:%S+00:00")
COUNT=0
FAIL=0

add_gt() {
  local id="$1" prompt="$2" category="$3" complexity="$4" gt_file="$5"
  local gt_json
  gt_json=$(cat "$gt_file")

  # Build JSON payload using python to handle escaping
  local code
  code=$(python3 -c "
import json, sys
payload = {
    'id': sys.argv[1],
    'prompt': sys.argv[2],
    'category': sys.argv[3],
    'complexity': sys.argv[4],
    'expected_phases': None,
    'expected_step_types': None,
    'tags': None,
    'ground_truth_json': sys.argv[5],
    'enabled': True,
    'created_at': sys.argv[6],
    'updated_at': sys.argv[6]
}
print(json.dumps(payload))
" "$id" "$prompt" "$category" "$complexity" "$gt_json" "$NOW")

  # Try POST (create), fall back to PUT (update) if already exists
  local http_code
  http_code=$(curl -s -X POST "$BASE/eval/test-suite" \
    -H "Content-Type: application/json" \
    -d "$code" \
    -o /dev/null -w "%{http_code}")

  if [ "$http_code" = "200" ]; then
    COUNT=$((COUNT + 1))
    echo "  NEW $id"
  else
    # Already exists — update via PUT to attach ground truth
    http_code=$(curl -s -X PUT "$BASE/eval/test-suite/$id" \
      -H "Content-Type: application/json" \
      -d "$code" \
      -o /dev/null -w "%{http_code}")
    if [ "$http_code" = "200" ]; then
      COUNT=$((COUNT + 1))
      echo "  UPD $id"
    else
      FAIL=$((FAIL + 1))
      echo "  FAIL($http_code) $id"
    fi
  fi
}

# Create temp dir for ground truth files
GT_DIR=$(mktemp -d)
echo "=== Inserting 15 ground truth prompts ==="

# ── 1. gt-api-health-get: Simple GET health check ────────────────
cat > "$GT_DIR/1.json" << 'GTEOF'
{
  "name": "API Health Check",
  "description": "Verify the backend API health endpoint returns 200",
  "category": "api_validation",
  "tags": ["api", "health"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-health",
      "type": "api_request",
      "name": "Check /health returns 200",
      "phase": "verification",
      "method": "GET",
      "url": "http://localhost:8000/health",
      "timeout_ms": 5000,
      "assertions": [
        { "type": "status_code", "expected": 200 }
      ]
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Health check gate",
      "phase": "verification",
      "required_steps": ["step-health"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 3
}
GTEOF
add_gt "gt-api-health-get" "Send a GET request to the backend /health endpoint and verify it returns HTTP 200" "curated" "simple" "$GT_DIR/1.json"

# ── 2. gt-check-lint-python: Python linting ──────────────────────
cat > "$GT_DIR/2.json" << 'GTEOF'
{
  "name": "Python Lint Check",
  "description": "Run Python linting on the backend",
  "category": "code_quality",
  "tags": ["lint", "python"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-lint",
      "type": "check",
      "name": "Run ruff lint",
      "phase": "verification",
      "check_type": "lint",
      "tool": "ruff",
      "command": "ruff check .",
      "working_directory": "qontinui-web/backend"
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Lint gate",
      "phase": "verification",
      "required_steps": ["step-lint"]
    }
  ],
  "agentic_steps": [
    {
      "id": "step-fix",
      "type": "prompt",
      "name": "Fix lint issues",
      "phase": "agentic",
      "content": "Fix any lint errors found by ruff in the backend code."
    }
  ],
  "completion_steps": [],
  "max_iterations": 5
}
GTEOF
add_gt "gt-check-lint-python" "Run Python linting with ruff on the backend directory and fix any issues" "curated" "simple" "$GT_DIR/2.json"

# ── 3. gt-check-typecheck-ts: TypeScript type checking ───────────
cat > "$GT_DIR/3.json" << 'GTEOF'
{
  "name": "TypeScript Type Check",
  "description": "Run TypeScript type checking on the frontend",
  "category": "code_quality",
  "tags": ["typecheck", "typescript"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-tsc",
      "type": "check",
      "name": "Run tsc --noEmit",
      "phase": "verification",
      "check_type": "typecheck",
      "tool": "tsc",
      "command": "npx tsc --noEmit",
      "working_directory": "qontinui-web/frontend"
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Typecheck gate",
      "phase": "verification",
      "required_steps": ["step-tsc"]
    }
  ],
  "agentic_steps": [
    {
      "id": "step-fix",
      "type": "prompt",
      "name": "Fix type errors",
      "phase": "agentic",
      "content": "Fix any TypeScript type errors found by tsc."
    }
  ],
  "completion_steps": [],
  "max_iterations": 5
}
GTEOF
add_gt "gt-check-typecheck-ts" "Run TypeScript type checking on the frontend with tsc --noEmit and fix any errors" "curated" "simple" "$GT_DIR/3.json"

# ── 4. gt-shell-git-status: Shell command ────────────────────────
cat > "$GT_DIR/4.json" << 'GTEOF'
{
  "name": "Git Status Clean Check",
  "description": "Verify no uncommitted changes in the repo",
  "category": "devops",
  "tags": ["git", "shell"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-status",
      "type": "shell_command",
      "name": "Run git status",
      "phase": "verification",
      "command": "git status --porcelain",
      "fail_on_error": false,
      "timeout_seconds": 30
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Clean repo gate",
      "phase": "verification",
      "required_steps": ["step-status"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 1
}
GTEOF
add_gt "gt-shell-git-status" "Run git status and verify there are no uncommitted changes" "curated" "simple" "$GT_DIR/4.json"

# ── 5. gt-api-post-create: POST with assertions ─────────────────
cat > "$GT_DIR/5.json" << 'GTEOF'
{
  "name": "API Create Workflow",
  "description": "Create a workflow via POST and verify the response",
  "category": "api_validation",
  "tags": ["api", "post", "create"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-create",
      "type": "api_request",
      "name": "POST create workflow",
      "phase": "verification",
      "method": "POST",
      "url": "http://localhost:8000/api/v1/workflows",
      "content_type": "application/json",
      "body": "{\"name\": \"Test Workflow\", \"description\": \"Created by eval\"}",
      "assertions": [
        { "type": "status_code", "expected": 201 },
        { "type": "json_path", "json_path": "$.name", "expected": "Test Workflow", "operator": "equals" }
      ],
      "extractions": [
        { "variable_name": "workflow_id", "json_path": "$.id" }
      ]
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Create gate",
      "phase": "verification",
      "required_steps": ["step-create"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 1
}
GTEOF
add_gt "gt-api-post-create" "Send a POST request to create a new workflow and verify the response contains the workflow name and returns 201" "curated" "simple" "$GT_DIR/5.json"

# ── 6. gt-check-http-status: HTTP status check ──────────────────
cat > "$GT_DIR/6.json" << 'GTEOF'
{
  "name": "HTTP Status Check",
  "description": "Verify backend returns 200",
  "category": "monitoring",
  "tags": ["http", "check"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-check",
      "type": "check",
      "name": "Backend HTTP 200",
      "phase": "verification",
      "check_type": "http_status",
      "check_url": "http://localhost:8000/health",
      "expected_status": 200
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "HTTP gate",
      "phase": "verification",
      "required_steps": ["step-check"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 3
}
GTEOF
add_gt "gt-check-http-status" "Use an HTTP status check to verify the backend at localhost:8000/health returns 200" "curated" "simple" "$GT_DIR/6.json"

# ── 7. gt-screenshot-page: Simple screenshot ─────────────────────
cat > "$GT_DIR/7.json" << 'GTEOF'
{
  "name": "Screenshot Dashboard",
  "description": "Take a screenshot of the current screen",
  "category": "monitoring",
  "tags": ["screenshot"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-screenshot",
      "type": "screenshot",
      "name": "Capture screen state",
      "phase": "verification"
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 1
}
GTEOF
add_gt "gt-screenshot-page" "Take a screenshot of the current screen to document the application state" "curated" "simple" "$GT_DIR/7.json"

# ── 8. gt-test-python-unit: Python unit tests ────────────────────
cat > "$GT_DIR/8.json" << 'GTEOF'
{
  "name": "Python Unit Tests",
  "description": "Run Python unit tests for the backend",
  "category": "testing",
  "tags": ["test", "python", "unit"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-test",
      "type": "test",
      "name": "Run pytest",
      "phase": "verification",
      "test_type": "python",
      "command": "poetry run pytest tests/ -v",
      "working_directory": "qontinui-web/backend",
      "timeout_seconds": 120
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Test gate",
      "phase": "verification",
      "required_steps": ["step-test"]
    }
  ],
  "agentic_steps": [
    {
      "id": "step-fix",
      "type": "prompt",
      "name": "Fix test failures",
      "phase": "agentic",
      "content": "Fix any failing unit tests in the backend."
    }
  ],
  "completion_steps": [],
  "max_iterations": 5
}
GTEOF
add_gt "gt-test-python-unit" "Run the Python unit test suite with pytest on the backend and fix any failures" "curated" "simple" "$GT_DIR/8.json"

# ── 9. gt-check-format-black: Format check ──────────────────────
cat > "$GT_DIR/9.json" << 'GTEOF'
{
  "name": "Python Format Check",
  "description": "Check Python formatting with black",
  "category": "code_quality",
  "tags": ["format", "python", "black"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-format",
      "type": "check",
      "name": "Check black formatting",
      "phase": "verification",
      "check_type": "format",
      "tool": "black",
      "command": "black --check .",
      "working_directory": "qontinui-web/backend"
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Format gate",
      "phase": "verification",
      "required_steps": ["step-format"]
    }
  ],
  "agentic_steps": [
    {
      "id": "step-fix",
      "type": "prompt",
      "name": "Fix formatting",
      "phase": "agentic",
      "content": "Run black to fix any formatting issues in the backend code."
    }
  ],
  "completion_steps": [],
  "max_iterations": 3
}
GTEOF
add_gt "gt-check-format-black" "Check Python code formatting with black in the backend directory and fix any issues" "curated" "simple" "$GT_DIR/9.json"

# ── 10. gt-uib-snapshot: UI Bridge snapshot ──────────────────────
cat > "$GT_DIR/10.json" << 'GTEOF'
{
  "name": "UI Bridge Snapshot Verify",
  "description": "Take a UI Bridge snapshot and verify elements exist",
  "category": "ui_bridge",
  "tags": ["ui_bridge", "snapshot", "spec"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-spec",
      "type": "spec",
      "name": "Verify dashboard elements",
      "phase": "verification",
      "element_source": "control",
      "spec_group": {},
      "description": "Verify the dashboard has expected navigation and status elements"
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Spec gate",
      "phase": "verification",
      "required_steps": ["step-spec"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 1
}
GTEOF
add_gt "gt-uib-snapshot" "Use UI Bridge to take a snapshot and verify the dashboard has navigation and status elements" "curated" "simple" "$GT_DIR/10.json"

# ── 11. gt-api-get-with-extract: GET + extraction ────────────────
cat > "$GT_DIR/11.json" << 'GTEOF'
{
  "name": "API List and Extract",
  "description": "GET the workflow list and extract the first ID",
  "category": "api_validation",
  "tags": ["api", "get", "extraction"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-list",
      "type": "api_request",
      "name": "GET workflow list",
      "phase": "verification",
      "method": "GET",
      "url": "http://localhost:8000/api/v1/workflows",
      "assertions": [
        { "type": "status_code", "expected": 200 }
      ],
      "extractions": [
        { "variable_name": "first_id", "json_path": "$.data[0].id" }
      ]
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "List gate",
      "phase": "verification",
      "required_steps": ["step-list"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 1
}
GTEOF
add_gt "gt-api-get-with-extract" "Send a GET request to the workflow list endpoint, verify 200, and extract the first workflow ID" "curated" "simple" "$GT_DIR/11.json"

# ── 12. gt-shell-npm-install: Shell with working dir ─────────────
cat > "$GT_DIR/12.json" << 'GTEOF'
{
  "name": "NPM Install Check",
  "description": "Run npm install and verify success",
  "category": "devops",
  "tags": ["shell", "npm"],
  "setup_steps": [
    {
      "id": "step-install",
      "type": "shell_command",
      "name": "Run npm install",
      "phase": "setup",
      "command": "npm install",
      "working_directory": "qontinui-web/frontend",
      "timeout_seconds": 120,
      "fail_on_error": true
    }
  ],
  "verification_steps": [
    {
      "id": "step-verify",
      "type": "shell_command",
      "name": "Verify node_modules exists",
      "phase": "verification",
      "command": "test -d node_modules",
      "working_directory": "qontinui-web/frontend",
      "fail_on_error": true
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Install gate",
      "phase": "verification",
      "required_steps": ["step-verify"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 1
}
GTEOF
add_gt "gt-shell-npm-install" "Run npm install in the frontend directory and verify node_modules was created" "curated" "simple" "$GT_DIR/12.json"

# ── 13. gt-check-security: Security scan ────────────────────────
cat > "$GT_DIR/13.json" << 'GTEOF'
{
  "name": "Security Scan",
  "description": "Run security scan on backend",
  "category": "security",
  "tags": ["security", "check"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-scan",
      "type": "check",
      "name": "Run bandit security scan",
      "phase": "verification",
      "check_type": "security",
      "tool": "bandit",
      "command": "bandit -r . -ll",
      "working_directory": "qontinui-web/backend"
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Security gate",
      "phase": "verification",
      "required_steps": ["step-scan"]
    }
  ],
  "agentic_steps": [
    {
      "id": "step-fix",
      "type": "prompt",
      "name": "Fix security issues",
      "phase": "agentic",
      "content": "Fix any security vulnerabilities found by bandit."
    }
  ],
  "completion_steps": [],
  "max_iterations": 5
}
GTEOF
add_gt "gt-check-security" "Run a bandit security scan on the backend Python code and fix any vulnerabilities" "curated" "simple" "$GT_DIR/13.json"

# ── 14. gt-logwatch-errors: Log watching ─────────────────────────
cat > "$GT_DIR/14.json" << 'GTEOF'
{
  "name": "Backend Error Log Watch",
  "description": "Watch backend logs for errors",
  "category": "monitoring",
  "tags": ["logs", "monitoring"],
  "setup_steps": [],
  "verification_steps": [
    {
      "id": "step-logwatch",
      "type": "log_watch",
      "name": "Watch for ERROR in backend logs",
      "phase": "verification"
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "No errors gate",
      "phase": "verification",
      "required_steps": ["step-logwatch"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 1
}
GTEOF
add_gt "gt-logwatch-errors" "Watch the backend logs and verify no ERROR-level entries appear" "curated" "simple" "$GT_DIR/14.json"

# ── 15. gt-api-delete: DELETE with status check ──────────────────
cat > "$GT_DIR/15.json" << 'GTEOF'
{
  "name": "API Delete and Verify",
  "description": "Delete a resource and verify 404 on re-fetch",
  "category": "api_validation",
  "tags": ["api", "delete"],
  "setup_steps": [
    {
      "id": "step-create",
      "type": "api_request",
      "name": "Create test workflow",
      "phase": "setup",
      "method": "POST",
      "url": "http://localhost:8000/api/v1/workflows",
      "content_type": "application/json",
      "body": "{\"name\": \"To Delete\", \"description\": \"Will be deleted\"}",
      "assertions": [
        { "type": "status_code", "expected": 201 }
      ],
      "extractions": [
        { "variable_name": "delete_id", "json_path": "$.id" }
      ]
    }
  ],
  "verification_steps": [
    {
      "id": "step-delete",
      "type": "api_request",
      "name": "DELETE the workflow",
      "phase": "verification",
      "method": "DELETE",
      "url": "http://localhost:8000/api/v1/workflows/{{delete_id}}",
      "assertions": [
        { "type": "status_code", "expected": 200 }
      ]
    },
    {
      "id": "step-verify-gone",
      "type": "api_request",
      "name": "Verify 404 on re-fetch",
      "phase": "verification",
      "method": "GET",
      "url": "http://localhost:8000/api/v1/workflows/{{delete_id}}",
      "assertions": [
        { "type": "status_code", "expected": 404 }
      ]
    },
    {
      "id": "step-gate",
      "type": "gate",
      "name": "Delete verified gate",
      "phase": "verification",
      "required_steps": ["step-delete", "step-verify-gone"]
    }
  ],
  "agentic_steps": [],
  "completion_steps": [],
  "max_iterations": 1
}
GTEOF
add_gt "gt-api-delete" "Create a workflow, delete it via DELETE, then verify a subsequent GET returns 404" "curated" "medium" "$GT_DIR/15.json"

# Cleanup
rm -rf "$GT_DIR"

echo ""
echo "=== Done: $COUNT added, $FAIL failed ==="
