#!/usr/bin/env bash
# Insert 40 UI Bridge-focused eval test prompts
BASE="http://localhost:9875"
NOW=$(date -u +"%Y-%m-%dT%H:%M:%S+00:00")
COUNT=0
FAIL=0

add_prompt() {
  local id="$1" category="$3" complexity="$4"
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

echo "=== Inserting 40 UI Bridge test prompts ==="

# ── Snapshot & Discovery (4) ──────────────────────────────────────
add_prompt "uib-snapshot-dashboard" "Take a UI Bridge snapshot of the dashboard page and verify it contains at least 5 registered elements including navigation links and status indicators" "ui_bridge" "simple"
add_prompt "uib-discover-elements" "Force element discovery on the workflow list page via the UI Bridge, then verify all expected elements are registered: table rows, action buttons, and search input" "ui_bridge" "medium"
add_prompt "uib-snapshot-compare" "Take a UI Bridge snapshot before and after clicking a refresh button, compare the two snapshots to verify the data timestamp element updated" "ui_bridge" "medium"
add_prompt "uib-element-detail" "Use the UI Bridge to get detailed properties of the main navigation sidebar element including its rect dimensions, visibility state, and child element count" "ui_bridge" "simple"

# ── Click Actions (4) ─────────────────────────────────────────────
add_prompt "uib-click-tab" "Use UI Bridge to click a tab element in the workflow detail view, verify the active tab changes by checking the aria-selected attribute on the clicked tab" "ui_bridge" "medium"
add_prompt "uib-click-dropdown" "Use UI Bridge to click a dropdown trigger button, verify the dropdown menu becomes visible by checking the aria-expanded attribute changes to true" "ui_bridge" "medium"
add_prompt "uib-doubleclick-edit" "Use UI Bridge to double-click a workflow name cell to enter inline edit mode, verify an input element appears with the current name as its value" "ui_bridge" "medium"
add_prompt "uib-rightclick-context" "Use UI Bridge to right-click on a workflow row, verify a context menu appears with options for edit, duplicate, and delete" "ui_bridge" "medium"

# ── Type & Input (4) ──────────────────────────────────────────────
add_prompt "uib-type-search" "Use UI Bridge to type a search query into the search input, verify the element value updates and the workflow list filters to show matching results" "ui_bridge" "medium"
add_prompt "uib-type-clear" "Use UI Bridge to type text into an input field, then use the clear action to empty it, verify the element value is empty and any validation message appears" "ui_bridge" "simple"
add_prompt "uib-type-multifield" "Use UI Bridge to fill in a multi-field form by typing into the name input, description textarea, and tags input, then verify all three fields contain the expected values" "ui_bridge" "medium"
add_prompt "uib-type-with-delay" "Use UI Bridge to type text character-by-character with a delay into a search field that has autocomplete, verify suggestions appear after partial input" "ui_bridge" "medium"

# ── Select & Dropdown (3) ─────────────────────────────────────────
add_prompt "uib-select-category" "Use UI Bridge to select a category from a dropdown by label, verify the selected option changes and the list filters accordingly" "ui_bridge" "medium"
add_prompt "uib-select-multi" "Use UI Bridge to select multiple tags from a multi-select dropdown using additive selection, verify all selected options appear as badges" "ui_bridge" "complex"
add_prompt "uib-select-verify" "Use UI Bridge to read the current selected_options of a dropdown element, change the selection, then verify selected_options updated correctly" "ui_bridge" "medium"

# ── Checkbox & Toggle (3) ─────────────────────────────────────────
add_prompt "uib-check-enable" "Use UI Bridge to check an enable checkbox, verify the checked state becomes true and related form fields become enabled" "ui_bridge" "simple"
add_prompt "uib-toggle-setting" "Use UI Bridge to toggle a settings switch three times, verify the checked state alternates correctly between true and false after each toggle" "ui_bridge" "medium"
add_prompt "uib-uncheck-all" "Use UI Bridge to uncheck all checked items in a checklist by reading element states and unchecking each one, verify all items show checked=false" "ui_bridge" "complex"

# ── Hover & Focus (3) ─────────────────────────────────────────────
add_prompt "uib-hover-tooltip" "Use UI Bridge to hover over an info icon element, verify a tooltip element becomes visible with the expected help text content" "ui_bridge" "medium"
add_prompt "uib-focus-validation" "Use UI Bridge to focus an email input, type an invalid email, then blur the field, verify a validation error element becomes visible with an error message" "ui_bridge" "medium"
add_prompt "uib-hover-nav" "Use UI Bridge to hover over each navigation link in the sidebar, verify each one has a distinct label and the hovered state is reflected in element properties" "ui_bridge" "medium"

# ── Scroll (2) ────────────────────────────────────────────────────
add_prompt "uib-scroll-load-more" "Use UI Bridge to scroll down in a long workflow list until a load-more button or additional items appear, verify new elements are registered after scrolling" "ui_bridge" "medium"
add_prompt "uib-scroll-to-element" "Use UI Bridge to scroll a container to bring a specific element into view using the toElement scroll option, verify the target element becomes visible" "ui_bridge" "medium"

# ── Drag & Drop (2) ───────────────────────────────────────────────
add_prompt "uib-drag-reorder" "Use UI Bridge to drag a workflow step to a new position in an ordered list, verify the element order changed by reading the updated element positions" "ui_bridge" "complex"
add_prompt "uib-drag-to-zone" "Use UI Bridge to drag a file element onto a drop zone, verify the drop zone updates to show the accepted file name" "ui_bridge" "complex"

# ── Form Submit & Reset (2) ───────────────────────────────────────
add_prompt "uib-submit-form" "Use UI Bridge to fill in a workflow create form and submit it, verify a success indicator appears and the form fields are cleared" "ui_bridge" "medium"
add_prompt "uib-reset-form" "Use UI Bridge to fill in several form fields, then use the reset action, verify all fields return to their default empty values" "ui_bridge" "simple"

# ── Console Error Capture (3) ─────────────────────────────────────
add_prompt "uib-console-errors-clean" "Use UI Bridge to clear console errors, perform a page navigation, then check console errors to verify no errors were generated during the transition" "ui_bridge" "simple"
add_prompt "uib-console-errors-detect" "Navigate to a page with a known issue, use UI Bridge console error capture to detect and report any JavaScript errors including their stack traces" "ui_bridge" "medium"
add_prompt "uib-fail-on-console" "Enable fail_on_console_errors in a spec step, navigate through three pages, verify the step fails only on pages that produce console errors" "ui_bridge" "complex"

# ── Spec Assertions (5) ───────────────────────────────────────────
add_prompt "uib-spec-text-content" "Create a spec that asserts the page heading element contains the expected text, the subtitle contains a date string, and the footer contains the version number" "ui_bridge" "medium"
add_prompt "uib-spec-element-states" "Create a spec that asserts the submit button is visible and enabled, the delete button is visible but disabled, and the cancel link is visible and focusable" "ui_bridge" "medium"
add_prompt "uib-spec-form-values" "Create a spec that asserts a pre-filled form has the correct default values: name field contains the workflow name, category dropdown has the correct selection, and enabled checkbox is checked" "ui_bridge" "medium"
add_prompt "uib-spec-layout" "Create a spec that asserts the sidebar navigation has exactly 6 links, the main content area is wider than the sidebar, and the header is at the top of the page" "ui_bridge" "complex"
add_prompt "uib-spec-accessibility" "Create a spec that verifies accessibility attributes: all form inputs have associated labels, buttons have aria-labels, and the navigation has the correct ARIA role" "ui_bridge" "complex"

# ── Exploration (2) ───────────────────────────────────────────────
add_prompt "uib-explore-page" "Use UI Bridge automated exploration to discover all interactive elements on the settings page, verify the exploration completes and reports found elements with their available actions" "ui_bridge" "medium"
add_prompt "uib-explore-form-flow" "Use UI Bridge exploration to map the complete form submission flow: discover form fields, identify required fields, find the submit button, and report the full interaction sequence" "ui_bridge" "complex"

# ── End-to-End UI Bridge Flows (3) ────────────────────────────────
add_prompt "uib-e2e-crud" "Using only UI Bridge actions, create a new workflow by filling in the form and submitting, verify it appears in the list, edit its name, then delete it and verify removal" "ui_bridge" "complex"
add_prompt "uib-e2e-search-filter" "Using only UI Bridge actions, type a search query, select a category filter, verify the filtered results match both criteria, then clear all filters and verify the full list returns" "ui_bridge" "complex"
add_prompt "uib-e2e-navigation" "Using only UI Bridge actions, navigate through every page in the application by clicking sidebar links, take a snapshot on each page, and verify each page has a unique heading element" "ui_bridge" "complex"

echo ""
echo "=== Done: $COUNT added, $FAIL failed ==="
