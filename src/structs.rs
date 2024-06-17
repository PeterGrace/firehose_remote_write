use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use strum::Display;
use tokio::sync::RwLock;

pub type SharedState = Arc<RwLock<AppState>>;
#[derive(Default, Debug, Deserialize, Clone)]
pub struct DimensionMap(HashMap<String, String>);
#[derive(Default)]
pub struct AppState {
    // leaving this unimplemented for now
    _string: Option<String>,
}

#[derive(Default, Deserialize)]
pub struct Firehose {
    pub(crate) message: String,
    pub(crate) request_id: String,
    pub(crate) source_arn: String,
    pub(crate) source_type: String,
    pub(crate) timestamp: String,
}

#[derive(Default, Deserialize, Debug, Clone)]
pub struct CloudWatchMetric {
    pub(crate) metric_stream_name: String,
    pub(crate) account_id: String,
    pub(crate) region: String,
    pub(crate) namespace: String,
    pub(crate) metric_name: String,
    pub(crate) dimensions: DimensionMap,
    pub(crate) timestamp: i64,
    pub(crate) value: MetricValue,
    pub(crate) unit: MetricUnit,
}
#[derive(Default, Deserialize, Debug, Clone)]
pub struct MetricValue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) min: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sum: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) count: Option<f32>,
}

#[derive(Default, Deserialize, Debug, Clone, Display)]
#[strum(serialize_all = "snake_case")]
pub enum MetricUnit {
    #[default]
    Unknown,
    Bytes,
    Count,
    Percent,
    Average,
}

impl DimensionMap {
    pub fn to_kv(&self) -> String {
        let mut dims: Vec<String> = vec![];
        for (k, v) in self.0.iter() {
            dims.push(format!("{k}={v}"));
        }
        dims.join(",")
    }
}
