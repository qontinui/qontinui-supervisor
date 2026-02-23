use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::Json;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;

use crate::ai_debug;
use crate::config::AI_MODELS;
use crate::settings::{self, PersistentSettings};
use crate::state::{AiOutputEntry, SharedState};

// --- Request/Response types ---

#[derive(Deserialize)]
pub struct DebugRequest {
    pub prompt: Option<String>,
}

#[derive(Deserialize)]
pub struct AutoDebugRequest {
    pub enabled: bool,
}

#[derive(Deserialize)]
pub struct ProviderRequest {
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Serialize)]
pub struct AiStatusResponse {
    pub running: bool,
    pub provider: String,
    pub model: String,
    pub auto_debug_enabled: bool,
    pub session_started_at: Option<String>,
    pub last_debug_at: Option<String>,
    pub output_tail: Vec<AiOutputEntry>,
    pub code_being_edited: bool,
    pub external_claude_session: bool,
    pub pending_debug: bool,
    pub pending_debug_reason: Option<String>,
}

#[derive(Serialize)]
pub struct ProviderResponse {
    pub provider: String,
    pub model: String,
    pub model_id: String,
    pub display_name: String,
}

#[derive(Serialize)]
pub struct ModelsResponse {
    pub models: Vec<ModelInfo>,
}

#[derive(Serialize)]
pub struct ModelInfo {
    pub provider: String,
    pub key: String,
    pub model_id: String,
    pub display_name: String,
}

#[derive(Serialize)]
pub struct GenericResponse {
    pub status: String,
    pub message: String,
}

// --- Persistence helper ---

async fn persist_ai_settings(state: &SharedState) {
    let ai = state.ai.read().await;
    let path = settings::settings_path(&state.config);
    let s = PersistentSettings {
        ai_provider: Some(ai.provider.clone()),
        ai_model: Some(ai.model.clone()),
        auto_debug_enabled: Some(ai.auto_debug_enabled),
    };
    drop(ai);
    settings::save_settings(&path, &s);
}

// --- Route handlers ---

/// POST /ai/debug — Manually trigger a debug session.
pub async fn debug(
    State(state): State<SharedState>,
    Json(body): Json<DebugRequest>,
) -> Result<Json<GenericResponse>, Json<GenericResponse>> {
    // Validate prompt length if provided
    if let Some(ref prompt) = body.prompt {
        if prompt.trim().is_empty() {
            return Err(Json(GenericResponse {
                status: "error".to_string(),
                message: "prompt must not be empty or whitespace-only".to_string(),
            }));
        }
        if prompt.len() > 50_000 {
            return Err(Json(GenericResponse {
                status: "error".to_string(),
                message: format!("prompt too long ({} chars, max 50000)", prompt.len()),
            }));
        }
    }

    let reason = body.prompt.as_deref().unwrap_or("Manual trigger");
    match ai_debug::spawn_ai_debug(&state, Some(reason)).await {
        Ok(()) => Ok(Json(GenericResponse {
            status: "started".to_string(),
            message: "AI debug session started".to_string(),
        })),
        Err(e) => Err(Json(GenericResponse {
            status: "error".to_string(),
            message: e.to_string(),
        })),
    }
}

/// POST /ai/auto-debug — Enable/disable auto-debug.
pub async fn auto_debug(
    State(state): State<SharedState>,
    Json(body): Json<AutoDebugRequest>,
) -> Json<GenericResponse> {
    {
        let mut ai = state.ai.write().await;
        ai.auto_debug_enabled = body.enabled;
    }
    persist_ai_settings(&state).await;
    state.notify_health_change();
    Json(GenericResponse {
        status: "ok".to_string(),
        message: format!(
            "Auto-debug {}",
            if body.enabled { "enabled" } else { "disabled" }
        ),
    })
}

/// GET /ai/status — Current AI session status.
pub async fn status(State(state): State<SharedState>) -> Json<AiStatusResponse> {
    let ai = state.ai.read().await;
    let ca = state.code_activity.read().await;

    // Get last 50 output entries for the tail
    let tail_start = if ai.output_buffer.len() > 50 {
        ai.output_buffer.len() - 50
    } else {
        0
    };
    let output_tail: Vec<AiOutputEntry> =
        ai.output_buffer.iter().skip(tail_start).cloned().collect();

    let model_id = ai_debug::resolve_model_id(&ai.provider, &ai.model).unwrap_or_default();
    let _ = model_id; // Used for reference, not in response

    Json(AiStatusResponse {
        running: ai.running,
        provider: ai.provider.clone(),
        model: ai.model.clone(),
        auto_debug_enabled: ai.auto_debug_enabled,
        session_started_at: ai.session_started_at.map(|t| t.to_rfc3339()),
        last_debug_at: ai.last_debug_at.map(|t| t.to_rfc3339()),
        output_tail,
        code_being_edited: ca.code_being_edited,
        external_claude_session: ca.external_claude_session,
        pending_debug: ca.pending_debug,
        pending_debug_reason: ca.pending_debug_reason.clone(),
    })
}

/// POST /ai/stop — Kill running AI session.
pub async fn stop(
    State(state): State<SharedState>,
) -> Result<Json<GenericResponse>, Json<GenericResponse>> {
    match ai_debug::stop_ai_debug(&state).await {
        Ok(()) => Ok(Json(GenericResponse {
            status: "stopped".to_string(),
            message: "AI debug session stopped".to_string(),
        })),
        Err(e) => Err(Json(GenericResponse {
            status: "error".to_string(),
            message: e.to_string(),
        })),
    }
}

/// GET /ai/provider — Current provider + model.
pub async fn get_provider(State(state): State<SharedState>) -> Json<ProviderResponse> {
    let ai = state.ai.read().await;
    let model_id = ai_debug::resolve_model_id(&ai.provider, &ai.model).unwrap_or_default();
    let display_name = AI_MODELS
        .iter()
        .find(|(p, k, _, _)| *p == ai.provider && *k == ai.model)
        .map(|(_, _, _, d)| d.to_string())
        .unwrap_or_default();

    Json(ProviderResponse {
        provider: ai.provider.clone(),
        model: ai.model.clone(),
        model_id,
        display_name,
    })
}

/// POST /ai/provider — Set provider/model.
pub async fn set_provider(
    State(state): State<SharedState>,
    Json(body): Json<ProviderRequest>,
) -> Result<Json<ProviderResponse>, Json<GenericResponse>> {
    // Validate provider and model strings if provided
    if let Some(ref provider) = body.provider {
        if provider.trim().is_empty() {
            return Err(Json(GenericResponse {
                status: "error".to_string(),
                message: "provider must not be empty".to_string(),
            }));
        }
        if provider.len() > 100 {
            return Err(Json(GenericResponse {
                status: "error".to_string(),
                message: "provider name too long (max 100 chars)".to_string(),
            }));
        }
    }
    if let Some(ref model) = body.model {
        if model.trim().is_empty() {
            return Err(Json(GenericResponse {
                status: "error".to_string(),
                message: "model must not be empty".to_string(),
            }));
        }
        if model.len() > 100 {
            return Err(Json(GenericResponse {
                status: "error".to_string(),
                message: "model name too long (max 100 chars)".to_string(),
            }));
        }
    }

    let response = {
        let mut ai = state.ai.write().await;

        if let Some(ref provider) = body.provider {
            ai.provider = provider.clone();
        }
        if let Some(ref model) = body.model {
            ai.model = model.clone();
        }

        // Validate the combination exists
        let valid = AI_MODELS
            .iter()
            .any(|(p, k, _, _)| *p == ai.provider && *k == ai.model);

        if !valid {
            return Err(Json(GenericResponse {
                status: "error".to_string(),
                message: format!(
                    "Invalid provider/model combination: {}/{}",
                    ai.provider, ai.model
                ),
            }));
        }

        let model_id = ai_debug::resolve_model_id(&ai.provider, &ai.model).unwrap_or_default();
        let display_name = AI_MODELS
            .iter()
            .find(|(p, k, _, _)| *p == ai.provider && *k == ai.model)
            .map(|(_, _, _, d)| d.to_string())
            .unwrap_or_default();

        ProviderResponse {
            provider: ai.provider.clone(),
            model: ai.model.clone(),
            model_id,
            display_name,
        }
    };

    persist_ai_settings(&state).await;
    state.notify_health_change();
    Ok(Json(response))
}

/// GET /ai/models — Available providers and models.
pub async fn models() -> Json<ModelsResponse> {
    let models: Vec<ModelInfo> = AI_MODELS
        .iter()
        .map(|(provider, key, model_id, display_name)| ModelInfo {
            provider: provider.to_string(),
            key: key.to_string(),
            model_id: model_id.to_string(),
            display_name: display_name.to_string(),
        })
        .collect();

    Json(ModelsResponse { models })
}

/// GET /ai/output/stream — SSE stream of AI output.
pub async fn output_stream(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Poll every 500ms for new output entries
    let interval = IntervalStream::new(tokio::time::interval(Duration::from_millis(500)));
    let mut last_seen = 0usize;

    let stream = interval.map(move |_| {
        let state = state.clone();
        // We need to block on the async read, but since we're in a sync map,
        // we'll return what we can
        let entries: Vec<AiOutputEntry> = {
            // Use try_read to avoid blocking
            match state.ai.try_read() {
                Ok(ai) => {
                    let total = ai.output_buffer.len();
                    if total > last_seen {
                        let new_entries: Vec<AiOutputEntry> =
                            ai.output_buffer.iter().skip(last_seen).cloned().collect();
                        last_seen = total;
                        new_entries
                    } else {
                        Vec::new()
                    }
                }
                Err(_) => Vec::new(),
            }
        };

        if entries.is_empty() {
            Ok(Event::default().comment("keepalive"))
        } else {
            let data = serde_json::to_string(&entries).unwrap_or_default();
            Ok(Event::default().event("ai_output").data(data))
        }
    });

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}
