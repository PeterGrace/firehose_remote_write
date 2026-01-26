pub(crate) mod aws;
mod consts;
mod prometheus;
pub(crate) mod structs;

#[macro_use]
extern crate tracing;
#[macro_use]
extern crate anyhow;

use crate::aws::get_freshness;
use crate::prometheus::{push_firehose_metrics, record_metric, FRESHNESS_INFO, STREAMS_RECEIVED};
use crate::structs::SharedState;
use crate::structs::{AppState, FirehoseData, FirehoseResponse};
use crate::structs::{CloudWatchMetric, Firehose, MetricUnit, MetricValue};
use ::prometheus::core::Metric;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{debug_handler, extract, Json, Router};
use base64::decode;
use base64::prelude::*;
use serde::Deserialize;
use std::env;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::{interval, Instant};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();
    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
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

    tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(15));
        loop {
            println!("Running scheduled task");
            interval.tick().await;
            // Check discovered firehose arns and check their freshness
            let firehose_arns = shared_state.read().await.firehose_arns.clone();
            for firehose_arn in firehose_arns.iter() {
                let freshness = get_freshness(firehose_arn.clone()).await.unwrap();
                info!("Freshness for {firehose_arn}: {freshness}");
                FRESHNESS_INFO
                    .with_label_values(&[firehose_arn])
                    .set(freshness);
            }

            // push to prometheus remote write
            match push_firehose_metrics().await {
                Ok(_) => info!("Pushed metrics to prometheus"),
                Err(e) => error!("Couldn't push metrics to prometheus: {e}"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    info!("Spawning axum listener.");
    axum::serve(listener, app).await.unwrap();
    loop {}
}

async fn decode_payloads(records: Vec<FirehoseData>) -> Result<String, Box<dyn Error>> {
    let payload_message = records
        .iter()
        .map(|s| String::from_utf8(BASE64_STANDARD.decode(&s.data).unwrap()).unwrap())
        .collect::<Vec<String>>()
        .join("");
    Ok(payload_message)
}
pub async fn convert_to_cloudmetric(input: &str) -> Result<CloudWatchMetric, StatusCode> {
    let metric: CloudWatchMetric = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(e) => {
            println!("Can't process metric: {e}");
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    Ok(metric)
}

#[debug_handler]
async fn get_firehose(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(payload): Json<Firehose>,
) -> Result<Json<FirehoseResponse>, (StatusCode, Json<FirehoseResponse>)> {
    let mut payload_message: String = String::from("");

    if let Some(source_arn) = headers.get("X-Amz-Firehose-Source-Arn") {
        state.write().await.firehose_arns.insert(source_arn.to_str().unwrap().to_string());

    } else if let Some(firehose) = payload.source_arn {
        state.write().await.firehose_arns.insert(firehose);
    } else {
        warn!("Could not find source arn in headers or payload for this request.")
    }
    
    if let Some(records) = payload.records {
        // although it's not beyond belief that amazon would send us malformed b64, it's unlikely,
        // so I'm skipping error processing here for now
        payload_message = decode_payloads(records).await.unwrap_or_default();
    };

    if let Some(message) = payload.message {
        payload_message = message;
    }

    for line in payload_message.lines() {
        trace!("Processing {line}");
        if let Ok(metric) = convert_to_cloudmetric(line).await {
            if let Err(e) = record_metric(metric).await {
                error!("Couldn't record_metric: {e}");
                continue;
            }
        } else {
            debug!("unable to decode cloudmetric");
        }
    }
    STREAMS_RECEIVED.with_label_values(&[]).inc();
    let response = FirehoseResponse {
        request_id: payload.request_id.unwrap(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u64,
        error_message: None,
    };
    info!("{response:#?}");
    Ok(Json(response))
}
use crate::aws::AWSState;
#[cfg(test)]
use std::fs::File;
use std::io::Read;
use tokio::time;

#[tokio::test]
async fn test_convert_to_labels_values() {
    let mut data: String = Default::default();
    let mut fd: File = File::open("testdata/just-post-payload.json").unwrap();
    let buffer = fd.read_to_string(&mut data).unwrap();
    let payload = serde_json::from_str::<Firehose>(&data).unwrap();
    // begin: Json(payload): Json<Firehose>
    if let Some(records) = payload.records {
        let payload_str = decode_payloads(records).await.unwrap();
        for line in payload_str.lines() {
            let cm = convert_to_cloudmetric(&line).await.unwrap();
            println!("{:#?}", cm.dimensions.to_labels_values());
        }
    }
}
