//! Test-only: a stand-in for coord's two dev-action routes, so a test can plant
//! the expectations a mint reads and see what the client sent. A config reaches
//! it through [`crate::config::AmbientCoord::At`].

use std::sync::{Arc, Mutex};

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Value};

/// One request the stand-in received, whatever its route.
#[derive(Debug, Clone)]
pub(crate) struct SeenRequest {
    pub method: Method,
    pub path: String,
    pub query: String,
    pub body: String,
}

pub(crate) struct PlantedCoord {
    /// `http://127.0.0.1:<port>`, for `AmbientCoord::At`.
    pub base: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl PlantedCoord {
    /// Every request so far, in arrival order.
    pub(crate) fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen lock").clone()
    }
}

/// Serve `rows` as `GET /coord/dev-actions/expectations`' `expectations` array
/// and accept `POST /coord/dev-actions/snapshot`, for the life of the test's
/// runtime. Anything else is recorded too, and answered 404.
///
/// It binds `127.0.0.1`, not `localhost`, which costs a doomed IPv6 connect on
/// Windows.
pub(crate) async fn serve_planted_expectations(rows: Value) -> PlantedCoord {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the planted coord");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let app = Router::new().fallback(move |req: Request| {
        let (recorder, rows) = (recorder.clone(), rows.clone());
        async move { answer(req, &recorder, rows).await }
    });
    tokio::spawn(async move { axum::serve(listener, app).await });
    PlantedCoord { base, seen }
}

async fn answer(req: Request, recorder: &Mutex<Vec<SeenRequest>>, rows: Value) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("collect the request body");
    let path = parts.uri.path().to_string();
    recorder.lock().expect("seen lock").push(SeenRequest {
        method: parts.method.clone(),
        path: path.clone(),
        query: parts.uri.query().unwrap_or_default().to_string(),
        body: String::from_utf8_lossy(&bytes).into_owned(),
    });
    match (parts.method, path.as_str()) {
        (Method::GET, "/coord/dev-actions/expectations") => {
            Json(json!({ "expectations": rows })).into_response()
        }
        (Method::POST, "/coord/dev-actions/snapshot") => StatusCode::OK.into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Five rows that are not boot failures, then the boot-failure row coord's pooled
/// mean ranks sixth, 4 of 4 from this exact state set: (4 + 0.4) / (4 + 4) = 0.55,
/// over the exact-cell arm's 0.5 although its served mean is 0.2.
pub(crate) fn five_rows_and_a_sixth_boot_row() -> Value {
    let row = |signature: &str| {
        json!({
            "signature": signature,
            "posterior_mean": 0.29,
            "occurrences": 1,
            "trials": 10,
            "evidence_basis": "marginal",
        })
    };
    json!([
        row("DEV-A"),
        row("DEV-B"),
        row("DEV-C"),
        row("DEV-D"),
        row("DEV-E"),
        {
            "signature": "DEV-TAURI-ASSET-MISSING",
            "posterior_mean": 0.2,
            "occurrences": 1,
            "trials": 10,
            "evidence_basis": "marginal",
            "exact_occurrences": 4,
            "exact_trials": 4,
        },
    ])
}

/// A mint served [`five_rows_and_a_sixth_boot_row`] asked for them once, assessed
/// all six, and kept the top five in its ACK.
pub(crate) fn assert_the_ack_read_every_planted_row(
    coord: &PlantedCoord,
    action_kind: &str,
    ack: &Value,
) {
    assert_eq!(
        ack["risk"]["route_lkg"], true,
        "the sixth-ranked boot row must trip the exact-cell arm: {ack}"
    );
    assert_eq!(
        ack["risk"]["top_signature"]["signature"], "DEV-TAURI-ASSET-MISSING",
        "{ack}"
    );
    let predicted: Vec<&str> = ack["predicted"]
        .as_array()
        .unwrap_or_else(|| panic!("the ACK carries `predicted`: {ack}"))
        .iter()
        .map(|row| row["signature"].as_str().expect("a row's signature"))
        .collect();
    assert_eq!(predicted, ["DEV-A", "DEV-B", "DEV-C", "DEV-D", "DEV-E"]);

    let seen = coord.seen();
    assert_eq!(seen.len(), 1, "exactly one request at the mint: {seen:?}");
    assert_eq!(seen[0].method, Method::GET);
    assert_eq!(seen[0].path, "/coord/dev-actions/expectations");
    let params: Vec<&str> = seen[0].query.split('&').collect();
    let kind = format!("action_kind={action_kind}");
    assert!(params.contains(&kind.as_str()), "{}", seen[0].query);
    assert!(params.contains(&"top_k=100"), "{}", seen[0].query);
}
