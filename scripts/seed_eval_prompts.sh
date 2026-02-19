#!/usr/bin/env bash
# Insert 50 new eval test prompts into the running supervisor
BASE="http://localhost:9875"
NOW=$(date -u +"%Y-%m-%dT%H:%M:%S+00:00")
COUNT=0
FAIL=0

add_prompt() {
  local id="$1" category="$3" complexity="$4"
  # prompt is $2 but may contain special chars, pass via variable
  local body
  body=$(printf '{"id":"%s","prompt":"%s","category":"%s","complexity":"%s","expected_phases":null,"expected_step_types":null,"tags":null,"enabled":true,"created_at":"%s","updated_at":"%s"}' \
    "$id" "$2" "$category" "$complexity" "$NOW" "$NOW")

  local code
  code=$(curl -s -X POST "$BASE/eval/test-suite" \
    -H "Content-Type: application/json" \
    -d "$body" \
    -o /dev/null -w "%{http_code}")

  if [ "$code" = "200" ]; then
    COUNT=$((COUNT + 1))
    echo "  OK  $id"
  else
    FAIL=$((FAIL + 1))
    echo "  FAIL($code) $id"
  fi
}

echo "=== Inserting eval test prompts ==="

# api_request (4)
add_prompt "api-auth-token" "Send a POST to the auth endpoint with test credentials, extract the JWT token from the response, then use it to call a protected endpoint and verify a 200 response" "api_validation" "medium"
add_prompt "api-pagination" "Call the workflow list API with page=1 and limit=5, verify the response contains at most 5 items and includes pagination metadata" "api_validation" "medium"
add_prompt "api-error-handling" "Send malformed JSON to the workflow create endpoint and verify it returns a 422 with a descriptive error message" "api_validation" "simple"
add_prompt "api-multi-step" "Create a workflow via POST, retrieve it by ID via GET, update its name via PUT, then delete it via DELETE and confirm a subsequent GET returns 404" "api_validation" "complex"

# shell_command (4)
add_prompt "shell-git-status" "Run git status on the project directory and verify there are no uncommitted changes" "devops" "simple"
add_prompt "shell-disk-space" "Check available disk space on the system drive and verify at least 1 GB is free" "devops" "simple"
add_prompt "shell-npm-audit" "Run npm audit in the frontend directory and report any high or critical vulnerabilities" "security" "medium"
add_prompt "shell-db-migration" "Run database migrations and verify the exit code is 0 and no errors appear in the output" "devops" "medium"

# check subtypes (6)
add_prompt "check-format-python" "Check Python code formatting with black in the backend directory without making changes, report any files that would be reformatted" "code_quality" "simple"
add_prompt "check-security-scan" "Run a security scan on the backend Python code to detect common vulnerabilities like SQL injection or hardcoded secrets" "security" "medium"
add_prompt "check-ts-analyze" "Run static analysis on the TypeScript frontend code to detect unused variables, unreachable code, and potential null pointer issues" "code_quality" "medium"
add_prompt "check-http-status" "Verify that the frontend at localhost:3001, backend at localhost:8000, and runner at localhost:9876 all return HTTP 200 on their health endpoints" "monitoring" "medium"
add_prompt "check-ai-review" "Run an AI code review on the most recently changed files in the backend to identify code quality issues and suggest improvements" "code_quality" "complex"
add_prompt "check-ci-pipeline" "Run the full CI/CD check pipeline including lint, format, typecheck, and tests for the backend project" "devops" "complex"

# test subtypes (4)
add_prompt "test-playwright-nav" "Write and run a Playwright test that navigates to the login page, verifies the form fields are present, and takes a screenshot" "testing" "medium"
add_prompt "test-python-unit" "Run the Python unit test suite for the backend core module and verify all tests pass" "testing" "medium"
add_prompt "test-repo-integrity" "Run repository integrity checks to verify that all required files exist, no large binaries are committed, and the .gitignore is properly configured" "testing" "medium"
add_prompt "test-custom-cmd" "Run a custom test command that verifies the backend API responds within 500ms for the /health endpoint" "testing" "simple"

# script / Playwright (3)
add_prompt "script-login-flow" "Write a Playwright script that logs in with test credentials, verifies the dashboard loads, checks the user name appears in the header, then logs out" "end_to_end" "complex"
add_prompt "script-dark-mode" "Write a Playwright script that toggles dark mode in the settings, verifies the background color changes, then toggles back and verifies it reverts" "ui_testing" "medium"
add_prompt "script-responsive" "Write a Playwright script that resizes the browser to mobile width 375px, verifies the navigation collapses to a hamburger menu, then restores desktop width" "ui_testing" "medium"

# gui_action (3)
add_prompt "gui-click-nav" "Click on the Workflows nav link in the sidebar, verify the workflow list page loads with the correct heading" "navigation" "simple"
add_prompt "gui-type-search" "Click the search input field, type a workflow name, and verify the filtered results appear in the list" "form_interaction" "medium"
add_prompt "gui-keyboard-shortcut" "Use the Ctrl+K keyboard shortcut to open the command palette, type a search query, and verify matching results appear" "ui_testing" "medium"

# screenshot (2)
add_prompt "screenshot-before-after" "Take a screenshot of the dashboard, trigger a data refresh, wait for the loading indicator to disappear, then take another screenshot to document the state change" "monitoring" "medium"
add_prompt "screenshot-error-state" "Navigate to an invalid URL path, take a screenshot of the error page to verify the 404 page renders correctly with the expected message" "ui_testing" "simple"

# log_watch (2)
add_prompt "logwatch-backend-errors" "Watch the backend logs for any ERROR-level entries while sending 10 sequential API requests to the workflow endpoints, report any errors found" "monitoring" "medium"
add_prompt "logwatch-startup" "Start the backend service and watch its logs for 30 seconds, verify no errors or warnings appear during startup" "monitoring" "medium"

# gate (2)
add_prompt "gate-all-checks-pass" "Run lint, format, and typecheck on the frontend, then use a gate step to aggregate all results and only proceed to completion if all three pass" "code_quality" "complex"
add_prompt "gate-api-health-gate" "Verify the backend, frontend, and database are all reachable via their health endpoints, then gate on all three being healthy before proceeding" "monitoring" "complex"

# spec / UI Bridge (2)
add_prompt "spec-dashboard-elements" "Use UI Bridge specs to verify the dashboard page contains the expected cards: status, recent workflows, and activity feed, with correct headings" "ui_testing" "medium"
add_prompt "spec-form-fields" "Use UI Bridge specs to verify the workflow create form has all required fields: name input, description textarea, category dropdown, and a submit button" "ui_testing" "medium"

# mcp_call (2)
add_prompt "mcp-file-read" "Use an MCP server tool to read the contents of the project README file and verify it contains the expected project name" "integration" "simple"
add_prompt "mcp-multi-tool" "Use MCP tools to first list files in the project root, then read the package.json, and verify the project name and version are correct" "integration" "medium"

# prompt / AI (2)
add_prompt "prompt-code-review" "In the agentic phase, instruct the AI to review the most recent git commit for potential issues, then in verification check that the AI produced a structured review" "ai_assisted" "medium"
add_prompt "prompt-generate-test" "In the agentic phase, instruct the AI to generate a unit test for a specified utility function, then verify the generated test file exists and has valid syntax" "ai_assisted" "complex"

# state / vision (2)
add_prompt "state-navigate-settings" "Navigate to the saved settings page state using Qontinui vision, verify the page matches by checking for the settings heading element" "navigation" "medium"
add_prompt "state-return-home" "Navigate to the saved home page state using Qontinui vision from any starting page, verify the home dashboard elements are visible" "navigation" "simple"

# workflow_ref (2)
add_prompt "wfref-reuse-setup" "Reference a saved setup workflow that initializes test data, then run verification steps to confirm the test data was created correctly" "testing" "medium"
add_prompt "wfref-chain-workflows" "Chain two saved workflows: first run a data preparation workflow, then run a validation workflow that checks the prepared data meets quality criteria" "testing" "complex"

# macro (2)
add_prompt "macro-form-fill" "Execute a saved macro that fills in a multi-field form with predefined values, then verify all fields contain the expected values" "form_interaction" "simple"
add_prompt "macro-nav-sequence" "Execute a saved navigation macro that visits three pages in sequence, then verify the final page is correct by checking its heading" "navigation" "simple"

# check_group (2)
add_prompt "checkgroup-frontend-all" "Run a saved check group that includes lint, format, and typecheck for the frontend project, verify all checks in the group pass" "code_quality" "medium"
add_prompt "checkgroup-pre-deploy" "Run the pre-deployment check group covering security scan, type checking, and test suite across both frontend and backend" "devops" "complex"

# error_resolved (2)
add_prompt "errresolved-import" "After fixing an import error in the backend, run the error_resolved step to confirm the import error no longer appears in the application logs" "debugging" "medium"
add_prompt "errresolved-runtime" "After applying a fix for a runtime TypeError, verify the error is resolved by checking it no longer appears in the last 100 lines of the backend error log" "debugging" "medium"

# awas (3)
add_prompt "awas-discover" "Discover the AWAS manifest from the frontend at localhost:3001 and verify it exposes automation actions for the main navigation" "integration" "simple"
add_prompt "awas-execute-action" "Discover AWAS actions from the frontend, then execute a navigation action to go to the workflows page and verify the page loaded" "integration" "medium"
add_prompt "awas-full-flow" "Discover AWAS support on the frontend, list available actions, execute a form submission action, then extract elements to verify the result" "integration" "complex"

# save_workflow_artifact (1)
add_prompt "artifact-save-generated" "Generate a simple lint workflow and save it as a named artifact in the workflow library, then verify it appears in the workflow list" "workflow_management" "medium"

echo ""
echo "=== Done: $COUNT added, $FAIL failed ==="
