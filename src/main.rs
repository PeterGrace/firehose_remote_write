mod consts;
mod prometheus;
pub(crate) mod structs;

#[macro_use]
extern crate tracing;
#[macro_use]
extern crate anyhow;

use crate::prometheus::{push_firehose_metrics, record_metric};
use crate::structs::AppState;
use crate::structs::SharedState;
use crate::structs::{CloudWatchMetric, Firehose, MetricUnit, MetricValue};
use ::prometheus::core::Metric;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{debug_handler, extract, Json, Router};
use serde::Deserialize;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;
use base64::prelude::*;

#[tokio::main]
async fn main() {
    if let Err(_) = env::var("RUST_LOG") {
        env::set_var("RUST_LOG", "info");
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_file(true)
        .with_line_number(true)
        .init();

    let shared_state = SharedState::default();
    {
        let mut state = shared_state.write().await;
        *state = AppState::default();
    }

    let app = Router::new()
        .route("/", post(get_firehose).put(get_firehose))
        .with_state(Arc::clone(&shared_state));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    info!("Spawning axum listener.");
    axum::serve(listener, app).await.unwrap();
    loop {}
}

#[debug_handler]
async fn get_firehose(
    State(state): State<SharedState>,
    headers: HeaderMap,
    extract::Json(payload): extract::Json<Firehose>,
) -> Result<String, StatusCode> {
    let mut payload_message: String = String::from("");
    info!("Entering get-firehose");
    if let Some(records) = payload.records {
        // although it's not beyond belief that amazon would send us malformed b64, it's unlikely,
        // so I'm skipping error processing here for now
        payload_message = records.iter().map(|s| {
            String::from_utf8(BASE64_STANDARD.decode(&s.data).unwrap()).unwrap()
        }).collect::<Vec<String>>().join("");
        info!("We received a records payload");
    };
    if let Some(message) = payload.message {
        payload_message = message;
        info!("We received a plain message payload");
    }
    for line in payload_message.lines() {
        trace!("Processing {line}");
        let metric: CloudWatchMetric = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                error!("Can't process metric: {e}");
                return Err(StatusCode::BAD_REQUEST);
            }
        };
        record_metric(metric).await;
    }
    match push_firehose_metrics().await {
        Ok(_) => {
            info!("succeeded on push")
        }
        Err(e) => {
            error!("Failed to push metrics: {e}");
        }
    }
    Ok("".to_string())
}
