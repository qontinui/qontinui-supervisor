use std::process::Stdio;
use tracing::{info, warn};

use super::{ScoreResponse, TestPrompt};
use crate::ai_debug::resolve_model_id;
use crate::state::SharedState;

/// Build the scoring prompt for the LLM judge.
pub fn build_scoring_prompt(test_prompt: &TestPrompt, workflow_json: &str) -> String {
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

/// Score a workflow by spawning `claude --print` with the scoring prompt.
pub async fn score_workflow(
    state: &SharedState,
    test_prompt: &TestPrompt,
    workflow_json: &str,
) -> anyhow::Result<ScoreResponse> {
    let prompt = build_scoring_prompt(test_prompt, workflow_json);

    // Resolve model from supervisor's AI state
    let (provider, model_key) = {
        let ai = state.ai.read().await;
        (ai.provider.clone(), ai.model.clone())
    };
    let model_id =
        resolve_model_id(&provider, &model_key).unwrap_or_else(|| "claude-opus-4-6".to_string());

    info!(
        "Scoring workflow for prompt '{}' with {}/{}",
        test_prompt.id, provider, model_key
    );

    // Write prompt to temp file
    let temp_dir = std::env::temp_dir();
    let prompt_file = temp_dir.join("qontinui-eval-scoring-prompt.md");
    tokio::fs::write(&prompt_file, &prompt).await?;

    // Spawn claude --print
    let output = match provider.as_str() {
        "claude" => {
            let child = tokio::process::Command::new("claude")
                .args([
                    "--print",
                    &prompt_file.display().to_string(),
                    "--permission-mode",
                    "bypassPermissions",
                    "--output-format",
                    "text",
                    "--model",
                    &model_id,
                ])
                .env_remove("CLAUDECODE")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await?;

            String::from_utf8_lossy(&child.stdout).to_string()
        }
        "gemini" => {
            let script_path = temp_dir.join("qontinui-eval-scoring.ps1");
            let script = format!(
                "Get-Content -Raw '{}' | gemini --yolo -o text -m '{}'",
                prompt_file.display(),
                model_id,
            );
            tokio::fs::write(&script_path, &script).await?;

            let child = tokio::process::Command::new("powershell.exe")
                .args([
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    &script_path.display().to_string(),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await?;

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
}
