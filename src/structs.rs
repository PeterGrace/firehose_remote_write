use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use strum::Display;
use tokio::sync::RwLock;
use tracing_subscriber::registry::Data;
use convert_case::{Case, Casing};

pub type SharedState = Arc<RwLock<AppState>>;
#[derive(Default, Debug, Deserialize, Clone)]
pub struct DimensionMap(HashMap<String, String>);

#[derive(Default, Debug, Deserialize, Clone)]
pub struct LabelsValues {
    pub key: String,
    pub value: String
}

#[derive(Default)]
pub struct AppState {
    // leaving this unimplemented for now
    _string: Option<String>,
}
#[derive(Default, Deserialize)]
pub struct FirehoseData {
    pub(crate) data: String,
}

#[derive(Default, Deserialize)]
pub struct Firehose {
    pub(crate) message: Option<String>,
    pub(crate) records: Option<Vec<FirehoseData>>,
    #[serde(rename = "requestId")]
    pub(crate) request_id: Option<String>,
    pub(crate) source_arn: Option<String>,
    pub(crate) source_type: Option<String>,
    pub(crate) timestamp: Option<u64>,
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
    Seconds,
    Average,
    Milliseconds,
    Microseconds,
    #[serde(rename = "Count/Second")]
    CountPerSecond,
    #[serde(rename = "Bytes/Second")]
    BytesPerSecond,
    None,
}

impl DimensionMap {
    pub fn to_kv(&self) -> String {
        let mut dims: Vec<String> = vec![];
        for (k, v) in self.0.iter() {
            //dims.push(format!("{k}->{v}"));
            dims.push(format!("{k}={v}"));
        }
        //let d = dims.join(".");
        let d = dims.join(",");
        trace!("to_kv: {d}");
        d
    }
    pub fn to_labels_values(&self) -> Vec<LabelsValues> {
            let mut response: Vec<LabelsValues> = vec![];
            for (k,value) in self.0.clone() {
                let mut key = k.to_case(Case::Snake);
                match key.as_str() {
                    "region" => key = String::from("dimension_region"),
                    _ => {}
                }
                let kv = LabelsValues{key,value};
                response.push(kv);
            };

            response
        }

}
