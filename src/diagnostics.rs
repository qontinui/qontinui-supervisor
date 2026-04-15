use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::VecDeque;

const DIAGNOSTICS_BUFFER_SIZE: usize = 200;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartSource {
    Manual,
    Watchdog,
}

impl RestartSource {
    /// Returns true if this is a manual (user-initiated) restart.
    pub fn is_manual(&self) -> bool {
        matches!(self, Self::Manual)
    }
}

impl std::fmt::Display for RestartSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manual => write!(f, "manual request"),
            Self::Watchdog => write!(f, "watchdog"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum DiagnosticEventKind {
    // Runner restarts
    RestartStarted {
        source: RestartSource,
        rebuild: bool,
    },
    RestartCompleted {
        source: RestartSource,
        rebuild: bool,
        duration_secs: f64,
        build_duration_secs: Option<f64>,
    },
    RestartFailed {
        source: RestartSource,
        error: String,
    },

    // Build outcomes
    BuildStarted,
    BuildCompleted {
        duration_secs: f64,
        success: bool,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticEvent {
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: DiagnosticEventKind,
}

pub struct DiagnosticsState {
    events: VecDeque<DiagnosticEvent>,
}

impl DiagnosticsState {
    pub fn new() -> Self {
        Self {
            events: VecDeque::with_capacity(DIAGNOSTICS_BUFFER_SIZE),
        }
    }

    pub fn emit(&mut self, kind: DiagnosticEventKind) {
        if self.events.len() >= DIAGNOSTICS_BUFFER_SIZE {
            self.events.pop_front();
        }
        self.events.push_back(DiagnosticEvent {
            timestamp: Utc::now(),
            kind,
        });
    }

    pub fn events(&self, limit: usize, filter: Option<&[String]>) -> Vec<DiagnosticEvent> {
        let iter = self.events.iter().rev();
        if let Some(filters) = filter {
            iter.filter(|e| {
                let category = e.filter_category();
                filters.iter().any(|f| category == f)
            })
            .take(limit)
            .cloned()
            .collect()
        } else {
            iter.take(limit).cloned().collect()
        }
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }
}

impl Default for DiagnosticsState {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticEvent {
    fn filter_category(&self) -> &'static str {
        match &self.kind {
            DiagnosticEventKind::RestartStarted { .. }
            | DiagnosticEventKind::RestartCompleted { .. }
            | DiagnosticEventKind::RestartFailed { .. } => "restart",

            DiagnosticEventKind::BuildStarted | DiagnosticEventKind::BuildCompleted { .. } => {
                "build"
            }
        }
    }
}
