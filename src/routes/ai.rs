use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::config::{resolve_model_id, AI_MODELS};
use crate::settings::{self, PersistentSettings};
use crate::state::SharedState;

// --- Request/Response types ---

#[derive(Deserialize)]
pub struct ProviderRequest {
    pub provider: Option<String>,
    pub model: Option<String>,
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
    // Load existing settings to preserve runner configs, then update AI fields
    let existing = settings::load_settings(&path);
    let s = PersistentSettings {
        ai_provider: Some(ai.provider.clone()),
        ai_model: Some(ai.model.clone()),
        auto_debug_enabled: Some(ai.auto_debug_enabled),
        runners: existing.runners,
    };
    drop(ai);
    settings::save_settings(&path, &s);
}

// --- Route handlers ---

/// GET /ai/provider — Current provider + model.
pub async fn get_provider(State(state): State<SharedState>) -> Json<ProviderResponse> {
    let ai = state.ai.read().await;
    let model_id = resolve_model_id(&ai.provider, &ai.model).unwrap_or_default();
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

        let model_id = resolve_model_id(&ai.provider, &ai.model).unwrap_or_default();
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
