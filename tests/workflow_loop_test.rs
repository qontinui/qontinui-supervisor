use qontinui_supervisor::workflow_loop::*;
use serde_json::json;

// --- Config deserialization tests ---

#[test]
fn test_simple_mode_config_deserializes() {
    let json = json!({
        "workflow_id": "abc-123",
        "max_iterations": 5,
        "exit_strategy": {"type": "reflection", "reflection_workflow_id": null},
        "between_iterations": {"type": "restart_runner", "rebuild": true}
    });

    let config: WorkflowLoopConfig = serde_json::from_value(json).unwrap();
    assert_eq!(config.workflow_id.as_deref(), Some("abc-123"));
    assert_eq!(config.max_iterations, 5);
    assert!(config.exit_strategy.is_some());
    assert!(config.phases.is_none());
}

#[test]
fn test_pipeline_mode_config_deserializes() {
    let json = json!({
        "max_iterations": 3,
        "between_iterations": {"type": "restart_runner", "rebuild": true},
        "phases": {
            "build": {
                "description": "Create a login test workflow",
                "context": "App uses email/password auth"
            },
            "reflect": {},
            "implement_fixes": {
                "timeout_secs": 300,
                "additional_context": "Focus on runner code"
            }
        }
    });

    let config: WorkflowLoopConfig = serde_json::from_value(json).unwrap();
    assert!(config.workflow_id.is_none());
    assert!(config.exit_strategy.is_none());
    assert_eq!(config.max_iterations, 3);

    let phases = config.phases.unwrap();
    let build = phases.build.unwrap();
    assert_eq!(build.description, "Create a login test workflow");
    assert_eq!(
        build.context.as_deref(),
        Some("App uses email/password auth")
    );
    assert!(phases.execute_workflow_id.is_none());

    let fix_config = phases.implement_fixes.unwrap();
    assert_eq!(fix_config.timeout_secs, 300);
    assert_eq!(
        fix_config.additional_context.as_deref(),
        Some("Focus on runner code")
    );
}

#[test]
fn test_pipeline_with_execute_workflow_id() {
    let json = json!({
        "max_iterations": 5,
        "between_iterations": {"type": "none"},
        "phases": {
            "execute_workflow_id": "existing-wf-id",
            "reflect": {}
        }
    });

    let config: WorkflowLoopConfig = serde_json::from_value(json).unwrap();
    let phases = config.phases.unwrap();
    assert!(phases.build.is_none());
    assert_eq!(
        phases.execute_workflow_id.as_deref(),
        Some("existing-wf-id")
    );
}

#[test]
fn test_default_max_iterations() {
    let json = json!({
        "workflow_id": "abc",
        "exit_strategy": {"type": "fixed_iterations"},
        "between_iterations": {"type": "none"}
    });

    let config: WorkflowLoopConfig = serde_json::from_value(json).unwrap();
    assert_eq!(config.max_iterations, 5); // default
}

#[test]
fn test_default_fix_timeout() {
    let json = json!({
        "max_iterations": 3,
        "between_iterations": {"type": "none"},
        "phases": {
            "execute_workflow_id": "wf-1",
            "reflect": {},
            "implement_fixes": {}
        }
    });

    let config: WorkflowLoopConfig = serde_json::from_value(json).unwrap();
    let fix_config = config.phases.unwrap().implement_fixes.unwrap();
    assert_eq!(fix_config.timeout_secs, 600); // default
}

// --- should_rebuild tests ---

#[test]
fn test_should_rebuild_with_workflow_step_rewrite() {
    let fixes = vec![json!({
        "fix_type": "workflow_step_rewrite",
        "fix_description": "Rewrote step 3"
    })];
    assert!(should_rebuild(&fixes));
}

#[test]
fn test_should_rebuild_with_instruction_clarification() {
    let fixes = vec![json!({
        "fix_type": "instruction_clarification",
        "fix_description": "Clarified step instructions"
    })];
    assert!(should_rebuild(&fixes));
}

#[test]
fn test_should_rebuild_with_context_addition() {
    let fixes = vec![json!({
        "fix_type": "context_addition",
        "fix_description": "Added missing context"
    })];
    assert!(should_rebuild(&fixes));
}

#[test]
fn test_should_not_rebuild_with_selector_fix() {
    let fixes = vec![json!({
        "fix_type": "selector_fix",
        "fix_description": "Fixed CSS selector"
    })];
    assert!(!should_rebuild(&fixes));
}

#[test]
fn test_should_not_rebuild_with_knowledge_base_update() {
    let fixes = vec![json!({
        "fix_type": "knowledge_base_update",
        "fix_description": "Updated KB entry"
    })];
    assert!(!should_rebuild(&fixes));
}

#[test]
fn test_should_not_rebuild_with_tool_config_update() {
    let fixes = vec![json!({
        "fix_type": "tool_config_update",
        "fix_description": "Updated tool config"
    })];
    assert!(!should_rebuild(&fixes));
}

#[test]
fn test_should_rebuild_mixed_fixes() {
    let fixes = vec![
        json!({"fix_type": "selector_fix", "fix_description": "Fixed selector"}),
        json!({"fix_type": "context_addition", "fix_description": "Added context"}),
    ];
    assert!(should_rebuild(&fixes));
}

#[test]
fn test_should_not_rebuild_empty_fixes() {
    let fixes: Vec<serde_json::Value> = vec![];
    assert!(!should_rebuild(&fixes));
}

#[test]
fn test_should_not_rebuild_missing_fix_type() {
    let fixes = vec![json!({
        "fix_description": "No type field"
    })];
    assert!(!should_rebuild(&fixes));
}

// --- build_fix_prompt tests ---

#[test]
fn test_build_fix_prompt_basic() {
    let fixes = vec![json!({
        "fix_type": "selector_fix",
        "fix_description": "Fix login button selector"
    })];

    let prompt = build_fix_prompt(&fixes, None);
    assert!(prompt.contains("selector_fix"));
    assert!(prompt.contains("Fix login button selector"));
    assert!(prompt.contains("## Reflection Fixes"));
    assert!(prompt.contains("## Instructions"));
    assert!(!prompt.contains("## Additional Context"));
}

#[test]
fn test_build_fix_prompt_with_context() {
    let fixes = vec![json!({"fix_type": "selector_fix"})];
    let prompt = build_fix_prompt(&fixes, Some("Focus on qontinui-runner src-tauri code"));

    assert!(prompt.contains("## Additional Context"));
    assert!(prompt.contains("Focus on qontinui-runner src-tauri code"));
}

#[test]
fn test_build_fix_prompt_multiple_fixes() {
    let fixes = vec![
        json!({"fix_type": "selector_fix", "fix_description": "Fix 1"}),
        json!({"fix_type": "context_addition", "fix_description": "Fix 2"}),
    ];

    let prompt = build_fix_prompt(&fixes, None);
    assert!(prompt.contains("Fix 1"));
    assert!(prompt.contains("Fix 2"));
}

// --- LoopPhase serialization tests ---

#[test]
fn test_loop_phase_serialization() {
    assert_eq!(
        serde_json::to_string(&LoopPhase::BuildingWorkflow).unwrap(),
        "\"building_workflow\""
    );
    assert_eq!(
        serde_json::to_string(&LoopPhase::Reflecting).unwrap(),
        "\"reflecting\""
    );
    assert_eq!(
        serde_json::to_string(&LoopPhase::ImplementingFixes).unwrap(),
        "\"implementing_fixes\""
    );
}

// --- IterationResult serialization tests ---

#[test]
fn test_iteration_result_skips_none_pipeline_fields() {
    let result = IterationResult {
        iteration: 1,
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
        task_run_id: "tr-1".to_string(),
        exit_check: ExitCheckResult {
            should_exit: false,
            reason: "continuing".to_string(),
        },
        generated_workflow_id: None,
        reflection_task_run_id: None,
        fix_count: None,
        fixes_implemented: None,
        rebuild_triggered: None,
    };

    let json = serde_json::to_value(&result).unwrap();
    assert!(!json
        .as_object()
        .unwrap()
        .contains_key("generated_workflow_id"));
    assert!(!json.as_object().unwrap().contains_key("fix_count"));
}

#[test]
fn test_iteration_result_includes_pipeline_fields_when_set() {
    let result = IterationResult {
        iteration: 1,
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
        task_run_id: "tr-1".to_string(),
        exit_check: ExitCheckResult {
            should_exit: false,
            reason: "continuing".to_string(),
        },
        generated_workflow_id: Some("gen-wf-1".to_string()),
        reflection_task_run_id: Some("refl-1".to_string()),
        fix_count: Some(3),
        fixes_implemented: Some(true),
        rebuild_triggered: Some(true),
    };

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["generated_workflow_id"], "gen-wf-1");
    assert_eq!(json["fix_count"], 3);
    assert_eq!(json["fixes_implemented"], true);
    assert_eq!(json["rebuild_triggered"], true);
}
