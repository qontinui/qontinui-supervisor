use std::process::Stdio;
use tracing::{info, warn};

use super::{ScoreResponse, TestPrompt};
use crate::ai_debug::resolve_model_id;
use crate::state::SharedState;

/// System prompt for the scoring judge — keeps Claude focused on JSON output.
const JUDGE_SYSTEM_PROMPT: &str = "You are an automated workflow quality evaluator. \
Your ONLY job is to read the scoring rubric and respond with EXACTLY the JSON object requested. \
Do NOT describe the task, do NOT explain the rubric, do NOT add any text before or after the JSON. \
Respond with ONLY the JSON object.";

/// Build the scoring prompt for the LLM judge.
/// When ground truth is available, uses diff-based comparison.
/// Otherwise, uses generic quality rubric.
pub fn build_scoring_prompt(test_prompt: &TestPrompt, workflow_json: &str) -> String {
    if let Some(ref ground_truth) = test_prompt.ground_truth_json {
        build_ground_truth_prompt(test_prompt, workflow_json, ground_truth)
    } else {
        build_generic_prompt(test_prompt, workflow_json)
    }
}

/// Diff-based scoring: compare generated workflow against known-good reference.
fn build_ground_truth_prompt(
    test_prompt: &TestPrompt,
    workflow_json: &str,
    ground_truth: &str,
) -> String {
    format!(
        r#"You are an expert workflow quality evaluator. Compare the generated workflow against the known-correct reference workflow and score how closely it matches.

## Original Prompt
{prompt}

## Reference Workflow (Ground Truth)
```json
{ground_truth}
```

## Generated Workflow (To Score)
```json
{workflow_json}
```

## Scoring Rubric — Compare Against Reference
Score each dimension 1-5:

1. structural_correctness — Does the generated workflow have the same phase structure (setup/verification/agentic/completion), same number of steps per phase, and valid required fields matching the reference? IGNORE differences in runtime configuration fields like max_iterations, max_sweep_iterations, enable_sweep, health_check_enabled, log_watch_enabled, auto_include_contexts — these are user preferences, not structural correctness.
   5 = identical structure, 4 = minor field differences, 3 = same phases but different step count, 2 = missing a phase, 1 = completely wrong structure

2. command_accuracy — Do the step types match the reference? Are the parameters (URLs, commands, methods, assertions) the same or functionally equivalent?
   5 = identical step types and parameters, 4 = same types with minor param differences, 3 = mostly correct types but wrong params, 2 = wrong step types used, 1 = unrelated steps

3. phase_flow_logic — Are steps in the same phases as the reference? Is the execution order correct? Are verification steps properly gated?
   5 = identical ordering, 4 = same logic with minor reordering, 3 = right intent but steps in wrong phases, 2 = significant phase misplacement, 1 = illogical flow

4. step_completeness — Does the generated workflow include all steps from the reference without adding unnecessary extras?
   5 = exact step match, 4 = all reference steps present with 1 extra, 3 = missing 1 reference step or 2+ extras, 2 = missing multiple steps, 1 = most steps missing

5. prompt_quality — Are the step names, descriptions, and prompt contents as clear and specific as the reference?
   5 = equally clear, 4 = slightly less specific, 3 = vague but functional, 2 = unclear, 1 = misleading or missing

6. determinism — Are the generated steps as reproducible as the reference? No unnecessary AI-driven steps, no flaky waits, concrete parameters?
   5 = equally deterministic, 4 = one minor non-determinism added, 3 = some flaky steps, 2 = relies heavily on AI where reference is deterministic, 1 = non-reproducible

Respond with ONLY valid JSON (no markdown fences, no extra text):
{{"structural_correctness": {{"score": N, "rationale": "..."}}, "command_accuracy": {{"score": N, "rationale": "..."}}, "phase_flow_logic": {{"score": N, "rationale": "..."}}, "step_completeness": {{"score": N, "rationale": "..."}}, "prompt_quality": {{"score": N, "rationale": "..."}}, "determinism": {{"score": N, "rationale": "..."}}}}"#,
        prompt = test_prompt.prompt,
    )
}

/// Generic quality scoring without a reference workflow.
fn build_generic_prompt(test_prompt: &TestPrompt, workflow_json: &str) -> String {
    let expected_phases = test_prompt
        .expected_phases
        .as_ref()
        .map(|v| format!("{:?}", v))
        .unwrap_or_else(|| "not specified".to_string());

    let expected_step_types = test_prompt
        .expected_step_types
        .as_ref()
        .map(|v| format!("{:?}", v))
        .unwrap_or_else(|| "not specified".to_string());

    format!(
        r#"You are an expert workflow quality evaluator. Score the following generated workflow.

## Original Prompt
{prompt}

## Expected Characteristics
Category: {category}, Complexity: {complexity}
Expected phases: {expected_phases}
Expected step types: {expected_step_types}

## Generated Workflow
```json
{workflow_json}
```

## Scoring Rubric
Score each dimension 1-5:
1. structural_correctness — Valid structure, required fields, unique IDs
2. command_accuracy — Correct step types, valid parameters, real commands
3. phase_flow_logic — Logical phase ordering, sensible transitions
4. step_completeness — All necessary steps, nothing extraneous
5. prompt_quality — Clear, specific, actionable instructions
6. determinism — Reproducible outcomes, no flaky steps

Respond with ONLY valid JSON (no markdown fences, no extra text):
{{"structural_correctness": {{"score": N, "rationale": "..."}}, "command_accuracy": {{"score": N, "rationale": "..."}}, "phase_flow_logic": {{"score": N, "rationale": "..."}}, "step_completeness": {{"score": N, "rationale": "..."}}, "prompt_quality": {{"score": N, "rationale": "..."}}, "determinism": {{"score": N, "rationale": "..."}}}}"#,
        prompt = test_prompt.prompt,
        category = test_prompt.category,
        complexity = test_prompt.complexity,
    )
}

/// Score a workflow by spawning `claude --print` with a system prompt override.
pub async fn score_workflow(
    state: &SharedState,
    test_prompt: &TestPrompt,
    workflow_json: &str,
) -> anyhow::Result<ScoreResponse> {
    let prompt = build_scoring_prompt(test_prompt, workflow_json);
    let has_ground_truth = test_prompt.ground_truth_json.is_some();

    // Resolve model from supervisor's AI state
    let (provider, model_key) = {
        let ai = state.ai.read().await;
        (ai.provider.clone(), ai.model.clone())
    };
    let model_id =
        resolve_model_id(&provider, &model_key).unwrap_or_else(|| "claude-opus-4-6".to_string());

    info!(
        "Scoring workflow for prompt '{}' with {}/{} (ground_truth={})",
        test_prompt.id, provider, model_key, has_ground_truth
    );

    let temp_dir = std::env::temp_dir();

    let output = match provider.as_str() {
        "claude" => {
            // Pipe prompt via stdin with --system-prompt and --tools "" for
            // reliable JSON output. Disabling tools prevents Claude Code from
            // trying to read files or use other tools instead of responding directly.
            let mut cmd = tokio::process::Command::new("claude");
            cmd.args([
                "--print",
                "--output-format",
                "text",
                "--model",
                &model_id,
                "--system-prompt",
                JUDGE_SYSTEM_PROMPT,
                "--tools",
                "",
            ])
            .env_remove("CLAUDECODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            let mut child = cmd.spawn()?;

            // Write prompt to stdin, then drop to signal EOF
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(prompt.as_bytes()).await?;
            }

            let result = child.wait_with_output().await?;
            String::from_utf8_lossy(&result.stdout).to_string()
        }
        "gemini" => {
            let prompt_file = temp_dir.join("qontinui-eval-scoring-prompt.md");
            tokio::fs::write(&prompt_file, &prompt).await?;

            let script_path = temp_dir.join("qontinui-eval-scoring.ps1");
            let script = format!(
                "Get-Content -Raw '{}' | gemini --yolo -o text -m '{}'",
                prompt_file.display(),
                model_id,
            );
            tokio::fs::write(&script_path, &script).await?;

            let mut cmd = tokio::process::Command::new("powershell.exe");
            cmd.args([
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &script_path.display().to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            #[cfg(windows)]
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            let child = cmd.output().await?;

            String::from_utf8_lossy(&child.stdout).to_string()
        }
        _ => {
            anyhow::bail!("Unsupported provider: {}", provider);
        }
    };

    parse_score_response(&output)
}

/// Parse the LLM's JSON response into a ScoreResponse.
pub fn parse_score_response(raw: &str) -> anyhow::Result<ScoreResponse> {
    let trimmed = raw.trim();

    // Try direct parse first
    if let Ok(score) = serde_json::from_str::<ScoreResponse>(trimmed) {
        return Ok(score);
    }

    // Try extracting JSON from markdown code fences
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            let json_str = &trimmed[start..=end];
            if let Ok(score) = serde_json::from_str::<ScoreResponse>(json_str) {
                return Ok(score);
            }
        }
    }

    warn!(
        "Failed to parse score response: {}",
        &trimmed[..trimmed.len().min(200)]
    );
    anyhow::bail!(
        "Failed to parse LLM judge response as valid JSON. Response starts with: {}",
        &trimmed[..trimmed.len().min(100)]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_json() {
        let json = r#"{"structural_correctness": {"score": 4, "rationale": "Good structure"}, "command_accuracy": {"score": 3, "rationale": "Mostly correct"}, "phase_flow_logic": {"score": 5, "rationale": "Excellent flow"}, "step_completeness": {"score": 4, "rationale": "Complete"}, "prompt_quality": {"score": 3, "rationale": "Clear enough"}, "determinism": {"score": 4, "rationale": "Reproducible"}}"#;
        let result = parse_score_response(json).unwrap();
        assert_eq!(result.structural_correctness.score, 4);
        assert_eq!(result.command_accuracy.score, 3);
        assert!((result.overall() - 3.833).abs() < 0.01);
    }

    #[test]
    fn test_parse_json_in_fences() {
        let raw = r#"Here is the evaluation:
```json
{"structural_correctness": {"score": 4, "rationale": "ok"}, "command_accuracy": {"score": 4, "rationale": "ok"}, "phase_flow_logic": {"score": 4, "rationale": "ok"}, "step_completeness": {"score": 4, "rationale": "ok"}, "prompt_quality": {"score": 4, "rationale": "ok"}, "determinism": {"score": 4, "rationale": "ok"}}
```"#;
        let result = parse_score_response(raw).unwrap();
        assert_eq!(result.structural_correctness.score, 4);
    }

    #[test]
    fn test_ground_truth_prompt_used_when_available() {
        let prompt = TestPrompt {
            id: "test".to_string(),
            prompt: "Check health".to_string(),
            category: "api_validation".to_string(),
            complexity: "simple".to_string(),
            expected_phases: None,
            expected_step_types: None,
            tags: None,
            ground_truth_json: Some(r#"{"name":"test"}"#.to_string()),
            enabled: true,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        };
        let result = build_scoring_prompt(&prompt, r#"{"name":"generated"}"#);
        assert!(result.contains("Reference Workflow (Ground Truth)"));
        assert!(result.contains("Compare Against Reference"));
    }

    #[test]
    fn test_generic_prompt_used_without_ground_truth() {
        let prompt = TestPrompt {
            id: "test".to_string(),
            prompt: "Check health".to_string(),
            category: "api_validation".to_string(),
            complexity: "simple".to_string(),
            expected_phases: None,
            expected_step_types: None,
            tags: None,
            ground_truth_json: None,
            enabled: true,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        };
        let result = build_scoring_prompt(&prompt, r#"{"name":"generated"}"#);
        assert!(!result.contains("Ground Truth"));
        assert!(result.contains("Expected Characteristics"));
    }
}
