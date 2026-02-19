use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{EvalResult, EvalRunSummary, TestPrompt};

pub struct EvalDb {
    conn: Mutex<Connection>,
    #[allow(dead_code)]
    db_path: PathBuf,
}

impl EvalDb {
    pub fn new(dev_logs_dir: &Path) -> anyhow::Result<Self> {
        let db_path = dev_logs_dir.join("eval-benchmark.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self {
            conn: Mutex::new(conn),
            db_path,
        };
        db.init_schema()?;
        db.migrate()?;
        db.seed_defaults()?;
        db.cleanup_stale_runs()?;
        Ok(db)
    }

    fn init_schema(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS test_prompts (
                id TEXT PRIMARY KEY,
                prompt TEXT NOT NULL,
                category TEXT NOT NULL,
                complexity TEXT NOT NULL DEFAULT 'medium',
                expected_phases TEXT,
                expected_step_types TEXT,
                tags TEXT,
                ground_truth_json TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS eval_runs (
                id TEXT PRIMARY KEY,
                mode TEXT NOT NULL,
                status TEXT NOT NULL,
                prompts_total INTEGER NOT NULL DEFAULT 0,
                prompts_completed INTEGER NOT NULL DEFAULT 0,
                avg_overall_score REAL,
                avg_structural REAL,
                avg_command_accuracy REAL,
                avg_phase_flow REAL,
                avg_step_completeness REAL,
                avg_prompt_quality REAL,
                avg_determinism REAL,
                gt_avg_overall REAL,
                gt_avg_structural REAL,
                gt_avg_command_accuracy REAL,
                gt_avg_phase_flow REAL,
                gt_avg_step_completeness REAL,
                gt_avg_prompt_quality REAL,
                gt_avg_determinism REAL,
                gt_count INTEGER,
                gen_avg_overall REAL,
                gen_avg_structural REAL,
                gen_avg_command_accuracy REAL,
                gen_avg_phase_flow REAL,
                gen_avg_step_completeness REAL,
                gen_avg_prompt_quality REAL,
                gen_avg_determinism REAL,
                gen_count INTEGER,
                error TEXT,
                started_at TEXT NOT NULL,
                completed_at TEXT
            );

            CREATE TABLE IF NOT EXISTS eval_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES eval_runs(id) ON DELETE CASCADE,
                test_prompt_id TEXT NOT NULL,
                generated_workflow_json TEXT,
                task_run_id TEXT,
                workflow_id TEXT,

                structural_correctness INTEGER,
                command_accuracy INTEGER,
                phase_flow_logic INTEGER,
                step_completeness INTEGER,
                prompt_quality INTEGER,
                determinism INTEGER,
                overall_score REAL,

                score_rationales TEXT,

                generation_error TEXT,
                scoring_error TEXT,

                generation_duration_ms INTEGER,
                scoring_duration_ms INTEGER,

                started_at TEXT NOT NULL,
                completed_at TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_er_run_id ON eval_results(run_id);
            CREATE INDEX IF NOT EXISTS idx_er_prompt_id ON eval_results(test_prompt_id);
            CREATE INDEX IF NOT EXISTS idx_er_overall ON eval_results(overall_score);
            CREATE INDEX IF NOT EXISTS idx_runs_started ON eval_runs(started_at);
            CREATE INDEX IF NOT EXISTS idx_runs_status ON eval_runs(status);
        ",
        )?;
        Ok(())
    }

    /// Run schema migrations for existing databases.
    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();

        // Migration v2: Add ground_truth_json column to test_prompts
        if conn
            .prepare("SELECT ground_truth_json FROM test_prompts LIMIT 0")
            .is_err()
        {
            conn.execute_batch("ALTER TABLE test_prompts ADD COLUMN ground_truth_json TEXT;")?;
            tracing::info!("Migrated eval DB: added ground_truth_json column");
        }

        // Migration v3: Add separate gt/gen aggregate columns to eval_runs
        if conn
            .prepare("SELECT gt_avg_overall FROM eval_runs LIMIT 0")
            .is_err()
        {
            conn.execute_batch(
                "ALTER TABLE eval_runs ADD COLUMN gt_avg_overall REAL;
                 ALTER TABLE eval_runs ADD COLUMN gt_avg_structural REAL;
                 ALTER TABLE eval_runs ADD COLUMN gt_avg_command_accuracy REAL;
                 ALTER TABLE eval_runs ADD COLUMN gt_avg_phase_flow REAL;
                 ALTER TABLE eval_runs ADD COLUMN gt_avg_step_completeness REAL;
                 ALTER TABLE eval_runs ADD COLUMN gt_avg_prompt_quality REAL;
                 ALTER TABLE eval_runs ADD COLUMN gt_avg_determinism REAL;
                 ALTER TABLE eval_runs ADD COLUMN gt_count INTEGER;
                 ALTER TABLE eval_runs ADD COLUMN gen_avg_overall REAL;
                 ALTER TABLE eval_runs ADD COLUMN gen_avg_structural REAL;
                 ALTER TABLE eval_runs ADD COLUMN gen_avg_command_accuracy REAL;
                 ALTER TABLE eval_runs ADD COLUMN gen_avg_phase_flow REAL;
                 ALTER TABLE eval_runs ADD COLUMN gen_avg_step_completeness REAL;
                 ALTER TABLE eval_runs ADD COLUMN gen_avg_prompt_quality REAL;
                 ALTER TABLE eval_runs ADD COLUMN gen_avg_determinism REAL;
                 ALTER TABLE eval_runs ADD COLUMN gen_count INTEGER;",
            )?;
            tracing::info!("Migrated eval DB: added gt/gen aggregate columns");
        }

        Ok(())
    }

    fn seed_defaults(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM test_prompts", [], |row| row.get(0))?;
        if count > 0 {
            return Ok(());
        }

        let now = Utc::now().to_rfc3339();
        // (id, prompt, category, complexity)
        let seeds: &[(&str, &str, &str, &str)] = &Self::default_test_prompts();

        let mut stmt = conn.prepare(
            "INSERT INTO test_prompts (id, prompt, category, complexity, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
        )?;

        for (id, prompt, category, complexity) in seeds {
            stmt.execute(params![id, prompt, category, complexity, now])?;
        }

        tracing::info!("Seeded {} default test prompts", seeds.len());
        Ok(())
    }

    fn default_test_prompts() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
        vec![
            // ── Original 8 ──────────────────────────────────────────────
            ("lint-python-basic", "Run Python linting on the backend directory and fix any issues", "code_quality", "simple"),
            ("typecheck-ts", "Run TypeScript type checking on the frontend and fix type errors", "code_quality", "simple"),
            ("api-health-check", "Verify the backend API is running and the /health endpoint returns 200", "api_validation", "simple"),
            ("web-page-load", "Navigate to localhost:3001 and verify the main page loads with the correct title", "navigation", "medium"),
            ("full-stack-lint", "Run linting and formatting checks on both the Python backend and TypeScript frontend, fix all issues", "code_quality", "complex"),
            ("api-crud-test", "Test the CRUD operations for the workflow API endpoints", "api_validation", "medium"),
            ("ui-form-validation", "Test the login form validation - verify error messages appear for invalid inputs", "form_interaction", "medium"),
            ("e2e-workflow-create", "Create a new workflow through the web UI, verify it appears in the workflow list", "end_to_end", "complex"),

            // ── api_request ─────────────────────────────────────────────
            ("api-auth-token", "Send a POST to the auth endpoint with test credentials, extract the JWT token from the response, then use it to call a protected endpoint and verify a 200 response", "api_validation", "medium"),
            ("api-pagination", "Call the workflow list API with page=1&limit=5, verify the response contains at most 5 items and includes pagination metadata", "api_validation", "medium"),
            ("api-error-handling", "Send malformed JSON to the workflow create endpoint and verify it returns a 422 with a descriptive error message", "api_validation", "simple"),
            ("api-multi-step", "Create a workflow via POST, retrieve it by ID via GET, update its name via PUT, then delete it via DELETE and confirm a subsequent GET returns 404", "api_validation", "complex"),

            // ── shell_command ───────────────────────────────────────────
            ("shell-git-status", "Run git status on the project directory and verify there are no uncommitted changes", "devops", "simple"),
            ("shell-disk-space", "Check available disk space on the system drive and verify at least 1 GB is free", "devops", "simple"),
            ("shell-npm-audit", "Run npm audit in the frontend directory and report any high or critical vulnerabilities", "security", "medium"),
            ("shell-db-migration", "Run database migrations and verify the exit code is 0 and no errors appear in the output", "devops", "medium"),

            // ── check (subtypes) ────────────────────────────────────────
            ("check-format-python", "Check Python code formatting with black in the backend directory without making changes, report any files that would be reformatted", "code_quality", "simple"),
            ("check-security-scan", "Run a security scan on the backend Python code to detect common vulnerabilities like SQL injection or hardcoded secrets", "security", "medium"),
            ("check-ts-analyze", "Run static analysis on the TypeScript frontend code to detect unused variables, unreachable code, and potential null pointer issues", "code_quality", "medium"),
            ("check-http-status", "Verify that the frontend at localhost:3001, backend at localhost:8000, and runner at localhost:9876 all return HTTP 200 on their health endpoints", "monitoring", "medium"),
            ("check-ai-review", "Run an AI code review on the most recently changed files in the backend to identify code quality issues and suggest improvements", "code_quality", "complex"),
            ("check-ci-pipeline", "Run the full CI/CD check pipeline including lint, format, typecheck, and tests for the backend project", "devops", "complex"),

            // ── test (subtypes) ─────────────────────────────────────────
            ("test-playwright-nav", "Write and run a Playwright test that navigates to the login page, verifies the form fields are present, and takes a screenshot", "testing", "medium"),
            ("test-python-unit", "Run the Python unit test suite for the backend core module and verify all tests pass", "testing", "medium"),
            ("test-repo-integrity", "Run repository integrity checks to verify that all required files exist, no large binaries are committed, and the .gitignore is properly configured", "testing", "medium"),
            ("test-custom-cmd", "Run a custom test command that verifies the backend API responds within 500ms for the /health endpoint", "testing", "simple"),

            // ── script (Playwright) ─────────────────────────────────────
            ("script-login-flow", "Write a Playwright script that logs in with test credentials, verifies the dashboard loads, checks the user's name appears in the header, then logs out", "end_to_end", "complex"),
            ("script-dark-mode", "Write a Playwright script that toggles dark mode in the settings, verifies the background color changes, then toggles back and verifies it reverts", "ui_testing", "medium"),
            ("script-responsive", "Write a Playwright script that resizes the browser to mobile width (375px), verifies the navigation collapses to a hamburger menu, then restores desktop width", "ui_testing", "medium"),

            // ── gui_action ──────────────────────────────────────────────
            ("gui-click-nav", "Click on the Workflows nav link in the sidebar, verify the workflow list page loads with the correct heading", "navigation", "simple"),
            ("gui-type-search", "Click the search input field, type a workflow name, and verify the filtered results appear in the list", "form_interaction", "medium"),
            ("gui-keyboard-shortcut", "Use the Ctrl+K keyboard shortcut to open the command palette, type a search query, and verify matching results appear", "ui_testing", "medium"),

            // ── screenshot ──────────────────────────────────────────────
            ("screenshot-before-after", "Take a screenshot of the dashboard, trigger a data refresh, wait for the loading indicator to disappear, then take another screenshot to document the state change", "monitoring", "medium"),
            ("screenshot-error-state", "Navigate to an invalid URL path, take a screenshot of the error page to verify the 404 page renders correctly with the expected message", "ui_testing", "simple"),

            // ── log_watch ───────────────────────────────────────────────
            ("logwatch-backend-errors", "Watch the backend logs for any ERROR-level entries while sending 10 sequential API requests to the workflow endpoints, report any errors found", "monitoring", "medium"),
            ("logwatch-startup", "Start the backend service and watch its logs for 30 seconds, verify no errors or warnings appear during startup", "monitoring", "medium"),

            // ── gate ────────────────────────────────────────────────────
            ("gate-all-checks-pass", "Run lint, format, and typecheck on the frontend, then use a gate step to aggregate all results and only proceed to completion if all three pass", "code_quality", "complex"),
            ("gate-api-health-gate", "Verify the backend, frontend, and database are all reachable via their health endpoints, then gate on all three being healthy before proceeding", "monitoring", "complex"),

            // ── spec (UI Bridge assertions) ─────────────────────────────
            ("spec-dashboard-elements", "Use UI Bridge specs to verify the dashboard page contains the expected cards: status, recent workflows, and activity feed, with correct headings", "ui_testing", "medium"),
            ("spec-form-fields", "Use UI Bridge specs to verify the workflow create form has all required fields: name input, description textarea, category dropdown, and a submit button", "ui_testing", "medium"),

            // ── mcp_call ────────────────────────────────────────────────
            ("mcp-file-read", "Use an MCP server tool to read the contents of the project README file and verify it contains the expected project name", "integration", "simple"),
            ("mcp-multi-tool", "Use MCP tools to first list files in the project root, then read the package.json, and verify the project name and version are correct", "integration", "medium"),

            // ── prompt (AI instructions) ─────────────────────────────────
            ("prompt-code-review", "In the agentic phase, instruct the AI to review the most recent git commit for potential issues, then in verification check that the AI produced a structured review", "ai_assisted", "medium"),
            ("prompt-generate-test", "In the agentic phase, instruct the AI to generate a unit test for a specified utility function, then verify the generated test file exists and has valid syntax", "ai_assisted", "complex"),

            // ── state (vision navigation) ────────────────────────────────
            ("state-navigate-settings", "Navigate to the saved settings page state using Qontinui vision, verify the page matches by checking for the settings heading element", "navigation", "medium"),
            ("state-return-home", "Navigate to the saved home page state using Qontinui vision from any starting page, verify the home dashboard elements are visible", "navigation", "simple"),

            // ── workflow_ref ─────────────────────────────────────────────
            ("wfref-reuse-setup", "Reference a saved setup workflow that initializes test data, then run verification steps to confirm the test data was created correctly", "testing", "medium"),
            ("wfref-chain-workflows", "Chain two saved workflows: first run a data preparation workflow, then run a validation workflow that checks the prepared data meets quality criteria", "testing", "complex"),

            // ── macro ───────────────────────────────────────────────────
            ("macro-form-fill", "Execute a saved macro that fills in a multi-field form with predefined values, then verify all fields contain the expected values", "form_interaction", "simple"),
            ("macro-nav-sequence", "Execute a saved navigation macro that visits three pages in sequence, then verify the final page is correct by checking its heading", "navigation", "simple"),

            // ── check_group ─────────────────────────────────────────────
            ("checkgroup-frontend-all", "Run a saved check group that includes lint, format, and typecheck for the frontend project, verify all checks in the group pass", "code_quality", "medium"),
            ("checkgroup-pre-deploy", "Run the pre-deployment check group covering security scan, type checking, and test suite across both frontend and backend", "devops", "complex"),

            // ── error_resolved ───────────────────────────────────────────
            ("errresolved-import", "After fixing an import error in the backend, run the error_resolved step to confirm the import error no longer appears in the application logs", "debugging", "medium"),
            ("errresolved-runtime", "After applying a fix for a runtime TypeError, verify the error is resolved by checking it no longer appears in the last 100 lines of the backend error log", "debugging", "medium"),

            // ── awas (web automation specification) ──────────────────────
            ("awas-discover", "Discover the AWAS manifest from the frontend at localhost:3001 and verify it exposes automation actions for the main navigation", "integration", "simple"),
            ("awas-execute-action", "Discover AWAS actions from the frontend, then execute a navigation action to go to the workflows page and verify the page loaded", "integration", "medium"),
            ("awas-full-flow", "Discover AWAS support on the frontend, list available actions, execute a form submission action, then extract elements to verify the result", "integration", "complex"),

            // ── save_workflow_artifact ────────────────────────────────────
            ("artifact-save-generated", "Generate a simple lint workflow and save it as a named artifact in the workflow library, then verify it appears in the workflow list", "workflow_management", "medium"),

            // ── ai_session ──────────────────────────────────────────────
            ("ai-session-debug", "Run an AI session that investigates why the backend /health endpoint returns slowly, analyzes the route handler code, and suggests a fix in the agentic phase, then verify the suggestion is actionable", "ai_assisted", "complex"),

            // ── playwright (visual regression) ──────────────────────────
            ("playwright-visual-regression", "Run a Playwright test that captures screenshots of the dashboard, workflow list, and settings pages, then compares each against baseline images and reports any visual differences exceeding a 1 percent threshold", "testing", "complex"),

            // ── UI Bridge: Snapshot & Discovery (4) ─────────────────────
            ("uib-snapshot-dashboard", "Take a UI Bridge snapshot of the dashboard page and verify it contains at least 5 registered elements including navigation links and status indicators", "ui_bridge", "simple"),
            ("uib-discover-elements", "Force element discovery on the workflow list page via the UI Bridge, then verify all expected elements are registered: table rows, action buttons, and search input", "ui_bridge", "medium"),
            ("uib-snapshot-compare", "Take a UI Bridge snapshot before and after clicking a refresh button, compare the two snapshots to verify the data timestamp element updated", "ui_bridge", "medium"),
            ("uib-element-detail", "Use the UI Bridge to get detailed properties of the main navigation sidebar element including its rect dimensions, visibility state, and child element count", "ui_bridge", "simple"),

            // ── UI Bridge: Click Actions (4) ────────────────────────────
            ("uib-click-tab", "Use UI Bridge to click a tab element in the workflow detail view, verify the active tab changes by checking the aria-selected attribute on the clicked tab", "ui_bridge", "medium"),
            ("uib-click-dropdown", "Use UI Bridge to click a dropdown trigger button, verify the dropdown menu becomes visible by checking the aria-expanded attribute changes to true", "ui_bridge", "medium"),
            ("uib-doubleclick-edit", "Use UI Bridge to double-click a workflow name cell to enter inline edit mode, verify an input element appears with the current name as its value", "ui_bridge", "medium"),
            ("uib-rightclick-context", "Use UI Bridge to right-click on a workflow row, verify a context menu appears with options for edit, duplicate, and delete", "ui_bridge", "medium"),

            // ── UI Bridge: Type & Input (4) ─────────────────────────────
            ("uib-type-search", "Use UI Bridge to type a search query into the search input, verify the element value updates and the workflow list filters to show matching results", "ui_bridge", "medium"),
            ("uib-type-clear", "Use UI Bridge to type text into an input field, then use the clear action to empty it, verify the element value is empty and any validation message appears", "ui_bridge", "simple"),
            ("uib-type-multifield", "Use UI Bridge to fill in a multi-field form by typing into the name input, description textarea, and tags input, then verify all three fields contain the expected values", "ui_bridge", "medium"),
            ("uib-type-with-delay", "Use UI Bridge to type text character-by-character with a delay into a search field that has autocomplete, verify suggestions appear after partial input", "ui_bridge", "medium"),

            // ── UI Bridge: Select & Dropdown (3) ────────────────────────
            ("uib-select-category", "Use UI Bridge to select a category from a dropdown by label, verify the selected option changes and the list filters accordingly", "ui_bridge", "medium"),
            ("uib-select-multi", "Use UI Bridge to select multiple tags from a multi-select dropdown using additive selection, verify all selected options appear as badges", "ui_bridge", "complex"),
            ("uib-select-verify", "Use UI Bridge to read the current selected_options of a dropdown element, change the selection, then verify selected_options updated correctly", "ui_bridge", "medium"),

            // ── UI Bridge: Checkbox & Toggle (3) ────────────────────────
            ("uib-check-enable", "Use UI Bridge to check an enable checkbox, verify the checked state becomes true and related form fields become enabled", "ui_bridge", "simple"),
            ("uib-toggle-setting", "Use UI Bridge to toggle a settings switch three times, verify the checked state alternates correctly between true and false after each toggle", "ui_bridge", "medium"),
            ("uib-uncheck-all", "Use UI Bridge to uncheck all checked items in a checklist by reading element states and unchecking each one, verify all items show checked=false", "ui_bridge", "complex"),

            // ── UI Bridge: Hover & Focus (3) ────────────────────────────
            ("uib-hover-tooltip", "Use UI Bridge to hover over an info icon element, verify a tooltip element becomes visible with the expected help text content", "ui_bridge", "medium"),
            ("uib-focus-validation", "Use UI Bridge to focus an email input, type an invalid email, then blur the field, verify a validation error element becomes visible with an error message", "ui_bridge", "medium"),
            ("uib-hover-nav", "Use UI Bridge to hover over each navigation link in the sidebar, verify each one has a distinct label and the hovered state is reflected in element properties", "ui_bridge", "medium"),

            // ── UI Bridge: Scroll (2) ───────────────────────────────────
            ("uib-scroll-load-more", "Use UI Bridge to scroll down in a long workflow list until a load-more button or additional items appear, verify new elements are registered after scrolling", "ui_bridge", "medium"),
            ("uib-scroll-to-element", "Use UI Bridge to scroll a container to bring a specific element into view using the toElement scroll option, verify the target element becomes visible", "ui_bridge", "medium"),

            // ── UI Bridge: Drag & Drop (2) ──────────────────────────────
            ("uib-drag-reorder", "Use UI Bridge to drag a workflow step to a new position in an ordered list, verify the element order changed by reading the updated element positions", "ui_bridge", "complex"),
            ("uib-drag-to-zone", "Use UI Bridge to drag a file element onto a drop zone, verify the drop zone updates to show the accepted file name", "ui_bridge", "complex"),

            // ── UI Bridge: Form Submit & Reset (2) ──────────────────────
            ("uib-submit-form", "Use UI Bridge to fill in a workflow create form and submit it, verify a success indicator appears and the form fields are cleared", "ui_bridge", "medium"),
            ("uib-reset-form", "Use UI Bridge to fill in several form fields, then use the reset action, verify all fields return to their default empty values", "ui_bridge", "simple"),

            // ── UI Bridge: Console Error Capture (3) ────────────────────
            ("uib-console-errors-clean", "Use UI Bridge to clear console errors, perform a page navigation, then check console errors to verify no errors were generated during the transition", "ui_bridge", "simple"),
            ("uib-console-errors-detect", "Navigate to a page with a known issue, use UI Bridge console error capture to detect and report any JavaScript errors including their stack traces", "ui_bridge", "medium"),
            ("uib-fail-on-console", "Enable fail_on_console_errors in a spec step, navigate through three pages, verify the step fails only on pages that produce console errors", "ui_bridge", "complex"),

            // ── UI Bridge: Spec Assertions (5) ──────────────────────────
            ("uib-spec-text-content", "Create a spec that asserts the page heading element contains the expected text, the subtitle contains a date string, and the footer contains the version number", "ui_bridge", "medium"),
            ("uib-spec-element-states", "Create a spec that asserts the submit button is visible and enabled, the delete button is visible but disabled, and the cancel link is visible and focusable", "ui_bridge", "medium"),
            ("uib-spec-form-values", "Create a spec that asserts a pre-filled form has the correct default values: name field contains the workflow name, category dropdown has the correct selection, and enabled checkbox is checked", "ui_bridge", "medium"),
            ("uib-spec-layout", "Create a spec that asserts the sidebar navigation has exactly 6 links, the main content area is wider than the sidebar, and the header is at the top of the page", "ui_bridge", "complex"),
            ("uib-spec-accessibility", "Create a spec that verifies accessibility attributes: all form inputs have associated labels, buttons have aria-labels, and the navigation has the correct ARIA role", "ui_bridge", "complex"),

            // ── UI Bridge: Exploration (2) ──────────────────────────────
            ("uib-explore-page", "Use UI Bridge automated exploration to discover all interactive elements on the settings page, verify the exploration completes and reports found elements with their available actions", "ui_bridge", "medium"),
            ("uib-explore-form-flow", "Use UI Bridge exploration to map the complete form submission flow: discover form fields, identify required fields, find the submit button, and report the full interaction sequence", "ui_bridge", "complex"),

            // ── UI Bridge: E2E Flows (3) ────────────────────────────────
            ("uib-e2e-crud", "Using only UI Bridge actions, create a new workflow by filling in the form and submitting, verify it appears in the list, edit its name, then delete it and verify removal", "ui_bridge", "complex"),
            ("uib-e2e-search-filter", "Using only UI Bridge actions, type a search query, select a category filter, verify the filtered results match both criteria, then clear all filters and verify the full list returns", "ui_bridge", "complex"),
            ("uib-e2e-navigation", "Using only UI Bridge actions, navigate through every page in the application by clicking sidebar links, take a snapshot on each page, and verify each page has a unique heading element", "ui_bridge", "complex"),
        ]
    }

    /// Mark any runs left in "running" status as "interrupted".
    /// Called on startup to clean up stale state from previous supervisor instances.
    pub fn cleanup_stale_runs(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let count = conn.execute(
            "UPDATE eval_runs SET status='interrupted', completed_at=?1
             WHERE status='running'",
            params![now],
        )?;
        if count > 0 {
            tracing::info!("Marked {} stale eval runs as interrupted", count);
        }
        Ok(count)
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    // ========================================================================
    // Test prompt CRUD
    // ========================================================================

    pub fn list_test_prompts(&self) -> anyhow::Result<Vec<TestPrompt>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, prompt, category, complexity, expected_phases, expected_step_types, tags, ground_truth_json, enabled, created_at, updated_at
             FROM test_prompts ORDER BY category, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TestPrompt {
                id: row.get(0)?,
                prompt: row.get(1)?,
                category: row.get(2)?,
                complexity: row.get(3)?,
                expected_phases: row
                    .get::<_, Option<String>>(4)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                expected_step_types: row
                    .get::<_, Option<String>>(5)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                tags: row
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                ground_truth_json: row.get(7)?,
                enabled: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_enabled_test_prompts(&self) -> anyhow::Result<Vec<TestPrompt>> {
        let all = self.list_test_prompts()?;
        Ok(all.into_iter().filter(|p| p.enabled).collect())
    }

    pub fn insert_test_prompt(&self, prompt: &TestPrompt) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO test_prompts (id, prompt, category, complexity, expected_phases, expected_step_types, tags, ground_truth_json, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                prompt.id,
                prompt.prompt,
                prompt.category,
                prompt.complexity,
                prompt.expected_phases.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.expected_step_types.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.tags.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.ground_truth_json,
                prompt.enabled as i64,
                prompt.created_at,
                prompt.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_test_prompt(&self, id: &str, prompt: &TestPrompt) -> anyhow::Result<bool> {
        let conn = self.conn();
        let updated = conn.execute(
            "UPDATE test_prompts SET prompt=?2, category=?3, complexity=?4, expected_phases=?5, expected_step_types=?6, tags=?7, ground_truth_json=?8, enabled=?9, updated_at=?10
             WHERE id=?1",
            params![
                id,
                prompt.prompt,
                prompt.category,
                prompt.complexity,
                prompt.expected_phases.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.expected_step_types.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.tags.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.ground_truth_json,
                prompt.enabled as i64,
                prompt.updated_at,
            ],
        )?;
        Ok(updated > 0)
    }

    pub fn set_ground_truth(&self, id: &str, workflow_json: &str) -> anyhow::Result<bool> {
        let conn = self.conn();
        let gt = if workflow_json.is_empty() {
            None
        } else {
            Some(workflow_json)
        };
        let now = Utc::now().to_rfc3339();
        let updated = conn.execute(
            "UPDATE test_prompts SET ground_truth_json=?2, updated_at=?3 WHERE id=?1",
            params![id, gt, now],
        )?;
        Ok(updated > 0)
    }

    pub fn delete_test_prompt(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn();
        let deleted = conn.execute("DELETE FROM test_prompts WHERE id=?1", params![id])?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Eval run CRUD
    // ========================================================================

    pub fn insert_eval_run(&self, run: &EvalRunSummary) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO eval_runs (id, mode, status, prompts_total, prompts_completed, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run.id,
                run.mode,
                run.status,
                run.prompts_total,
                run.prompts_completed,
                run.started_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_eval_run_progress(
        &self,
        run_id: &str,
        prompts_completed: i64,
    ) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE eval_runs SET prompts_completed=?2 WHERE id=?1",
            params![run_id, prompts_completed],
        )?;
        Ok(())
    }

    pub fn complete_eval_run(
        &self,
        run_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.conn();
        let now = Utc::now().to_rfc3339();

        // Compute aggregates from results — combined, ground-truth, and generic
        // Ground-truth prompts have IDs starting with "gt-"
        conn.execute(
            "UPDATE eval_runs SET
                status=?2,
                error=?3,
                completed_at=?4,
                -- Combined averages
                avg_overall_score = (SELECT AVG(overall_score) FROM eval_results WHERE run_id=?1 AND overall_score IS NOT NULL),
                avg_structural = (SELECT AVG(structural_correctness) FROM eval_results WHERE run_id=?1 AND structural_correctness IS NOT NULL),
                avg_command_accuracy = (SELECT AVG(command_accuracy) FROM eval_results WHERE run_id=?1 AND command_accuracy IS NOT NULL),
                avg_phase_flow = (SELECT AVG(phase_flow_logic) FROM eval_results WHERE run_id=?1 AND phase_flow_logic IS NOT NULL),
                avg_step_completeness = (SELECT AVG(step_completeness) FROM eval_results WHERE run_id=?1 AND step_completeness IS NOT NULL),
                avg_prompt_quality = (SELECT AVG(prompt_quality) FROM eval_results WHERE run_id=?1 AND prompt_quality IS NOT NULL),
                avg_determinism = (SELECT AVG(determinism) FROM eval_results WHERE run_id=?1 AND determinism IS NOT NULL),
                -- Ground-truth averages (gt-* prompts)
                gt_avg_overall = (SELECT AVG(overall_score) FROM eval_results WHERE run_id=?1 AND overall_score IS NOT NULL AND test_prompt_id LIKE 'gt-%'),
                gt_avg_structural = (SELECT AVG(structural_correctness) FROM eval_results WHERE run_id=?1 AND structural_correctness IS NOT NULL AND test_prompt_id LIKE 'gt-%'),
                gt_avg_command_accuracy = (SELECT AVG(command_accuracy) FROM eval_results WHERE run_id=?1 AND command_accuracy IS NOT NULL AND test_prompt_id LIKE 'gt-%'),
                gt_avg_phase_flow = (SELECT AVG(phase_flow_logic) FROM eval_results WHERE run_id=?1 AND phase_flow_logic IS NOT NULL AND test_prompt_id LIKE 'gt-%'),
                gt_avg_step_completeness = (SELECT AVG(step_completeness) FROM eval_results WHERE run_id=?1 AND step_completeness IS NOT NULL AND test_prompt_id LIKE 'gt-%'),
                gt_avg_prompt_quality = (SELECT AVG(prompt_quality) FROM eval_results WHERE run_id=?1 AND prompt_quality IS NOT NULL AND test_prompt_id LIKE 'gt-%'),
                gt_avg_determinism = (SELECT AVG(determinism) FROM eval_results WHERE run_id=?1 AND determinism IS NOT NULL AND test_prompt_id LIKE 'gt-%'),
                gt_count = (SELECT COUNT(*) FROM eval_results WHERE run_id=?1 AND overall_score IS NOT NULL AND test_prompt_id LIKE 'gt-%'),
                -- Generic averages (non-gt prompts)
                gen_avg_overall = (SELECT AVG(overall_score) FROM eval_results WHERE run_id=?1 AND overall_score IS NOT NULL AND test_prompt_id NOT LIKE 'gt-%'),
                gen_avg_structural = (SELECT AVG(structural_correctness) FROM eval_results WHERE run_id=?1 AND structural_correctness IS NOT NULL AND test_prompt_id NOT LIKE 'gt-%'),
                gen_avg_command_accuracy = (SELECT AVG(command_accuracy) FROM eval_results WHERE run_id=?1 AND command_accuracy IS NOT NULL AND test_prompt_id NOT LIKE 'gt-%'),
                gen_avg_phase_flow = (SELECT AVG(phase_flow_logic) FROM eval_results WHERE run_id=?1 AND phase_flow_logic IS NOT NULL AND test_prompt_id NOT LIKE 'gt-%'),
                gen_avg_step_completeness = (SELECT AVG(step_completeness) FROM eval_results WHERE run_id=?1 AND step_completeness IS NOT NULL AND test_prompt_id NOT LIKE 'gt-%'),
                gen_avg_prompt_quality = (SELECT AVG(prompt_quality) FROM eval_results WHERE run_id=?1 AND prompt_quality IS NOT NULL AND test_prompt_id NOT LIKE 'gt-%'),
                gen_avg_determinism = (SELECT AVG(determinism) FROM eval_results WHERE run_id=?1 AND determinism IS NOT NULL AND test_prompt_id NOT LIKE 'gt-%'),
                gen_count = (SELECT COUNT(*) FROM eval_results WHERE run_id=?1 AND overall_score IS NOT NULL AND test_prompt_id NOT LIKE 'gt-%')
             WHERE id=?1",
            params![run_id, status, error, now],
        )?;
        Ok(())
    }

    // ========================================================================
    // Eval result CRUD
    // ========================================================================

    pub fn insert_eval_result(&self, result: &EvalResult) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO eval_results (run_id, test_prompt_id, generated_workflow_json, task_run_id, workflow_id,
                structural_correctness, command_accuracy, phase_flow_logic, step_completeness, prompt_quality, determinism, overall_score,
                score_rationales, generation_error, scoring_error, generation_duration_ms, scoring_duration_ms, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                result.run_id,
                result.test_prompt_id,
                result.generated_workflow_json,
                result.task_run_id,
                result.workflow_id,
                result.structural_correctness,
                result.command_accuracy,
                result.phase_flow_logic,
                result.step_completeness,
                result.prompt_quality,
                result.determinism,
                result.overall_score,
                result.score_rationales,
                result.generation_error,
                result.scoring_error,
                result.generation_duration_ms,
                result.scoring_duration_ms,
                result.started_at,
                result.completed_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    // ========================================================================
    // Query helpers
    // ========================================================================

    pub fn get_eval_run(&self, run_id: &str) -> anyhow::Result<Option<EvalRunSummary>> {
        let conn = self.conn();
        let result = conn
            .query_row(
                "SELECT id, mode, status, prompts_total, prompts_completed,
                    avg_overall_score, avg_structural, avg_command_accuracy, avg_phase_flow,
                    avg_step_completeness, avg_prompt_quality, avg_determinism,
                    gt_avg_overall, gt_avg_structural, gt_avg_command_accuracy, gt_avg_phase_flow,
                    gt_avg_step_completeness, gt_avg_prompt_quality, gt_avg_determinism, gt_count,
                    gen_avg_overall, gen_avg_structural, gen_avg_command_accuracy, gen_avg_phase_flow,
                    gen_avg_step_completeness, gen_avg_prompt_quality, gen_avg_determinism, gen_count,
                    error, started_at, completed_at
                 FROM eval_runs WHERE id=?1",
                params![run_id],
                |row| {
                    Ok(EvalRunSummary {
                        id: row.get(0)?,
                        mode: row.get(1)?,
                        status: row.get(2)?,
                        prompts_total: row.get(3)?,
                        prompts_completed: row.get(4)?,
                        avg_overall_score: row.get(5)?,
                        avg_structural: row.get(6)?,
                        avg_command_accuracy: row.get(7)?,
                        avg_phase_flow: row.get(8)?,
                        avg_step_completeness: row.get(9)?,
                        avg_prompt_quality: row.get(10)?,
                        avg_determinism: row.get(11)?,
                        gt_avg_overall: row.get(12)?,
                        gt_avg_structural: row.get(13)?,
                        gt_avg_command_accuracy: row.get(14)?,
                        gt_avg_phase_flow: row.get(15)?,
                        gt_avg_step_completeness: row.get(16)?,
                        gt_avg_prompt_quality: row.get(17)?,
                        gt_avg_determinism: row.get(18)?,
                        gt_count: row.get(19)?,
                        gen_avg_overall: row.get(20)?,
                        gen_avg_structural: row.get(21)?,
                        gen_avg_command_accuracy: row.get(22)?,
                        gen_avg_phase_flow: row.get(23)?,
                        gen_avg_step_completeness: row.get(24)?,
                        gen_avg_prompt_quality: row.get(25)?,
                        gen_avg_determinism: row.get(26)?,
                        gen_count: row.get(27)?,
                        error: row.get(28)?,
                        started_at: row.get(29)?,
                        completed_at: row.get(30)?,
                    })
                },
            )
            .optional()?;
        Ok(result)
    }

    pub fn get_results_for_run(&self, run_id: &str) -> anyhow::Result<Vec<EvalResult>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, test_prompt_id, generated_workflow_json, task_run_id, workflow_id,
                    structural_correctness, command_accuracy, phase_flow_logic, step_completeness,
                    prompt_quality, determinism, overall_score, score_rationales,
                    generation_error, scoring_error, generation_duration_ms, scoring_duration_ms,
                    started_at, completed_at
             FROM eval_results WHERE run_id=?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![run_id], |row| {
            Ok(EvalResult {
                id: row.get(0)?,
                run_id: row.get(1)?,
                test_prompt_id: row.get(2)?,
                generated_workflow_json: row.get(3)?,
                task_run_id: row.get(4)?,
                workflow_id: row.get(5)?,
                structural_correctness: row.get(6)?,
                command_accuracy: row.get(7)?,
                phase_flow_logic: row.get(8)?,
                step_completeness: row.get(9)?,
                prompt_quality: row.get(10)?,
                determinism: row.get(11)?,
                overall_score: row.get(12)?,
                score_rationales: row.get(13)?,
                generation_error: row.get(14)?,
                scoring_error: row.get(15)?,
                generation_duration_ms: row.get(16)?,
                scoring_duration_ms: row.get(17)?,
                started_at: row.get(18)?,
                completed_at: row.get(19)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
