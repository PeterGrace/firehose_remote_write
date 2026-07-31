use convert_case::{Case, Casing};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use strum::Display;
use tokio::sync::RwLock;
use tracing_subscriber::registry::Data;

pub type SharedState = Arc<RwLock<AppState>>;
#[derive(Default, Debug, Deserialize, Clone)]
pub struct DimensionMap(HashMap<String, String>);

#[derive(Default, Debug, Deserialize, Clone)]
pub struct LabelsValues {
    pub key: String,
    pub value: String,
}

#[derive(Default)]
pub struct AppState {
    pub(crate) firehose_arns: HashSet<String>,
}
#[derive(Default, Deserialize, Debug)]
pub struct FirehoseData {
    pub(crate) data: String,
}
#[derive(Default, Serialize, Debug)]
pub struct FirehoseResponse {
    #[serde(rename = "requestId")]
    pub(crate) request_id: String,
    pub(crate) timestamp: u64,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub(crate) error_message: Option<String>,
}

#[derive(Default, Deserialize, Debug)]
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
    Bytes,
    Count,
    Percent,
    Seconds,
    Average,
    Milliseconds,
    Microseconds,
    Kilobytes,
    Megabytes,
    Gigabytes,
    Terabytes,
    Bits,
    Kilobits,
    Megabits,
    Gigabits,
    Terabits,
    #[serde(rename = "Count/Second")]
    CountPerSecond,
    #[serde(rename = "Bytes/Second")]
    BytesPerSecond,
    #[serde(rename = "Kilobytes/Second")]
    KilobytesPerSecond,
    #[serde(rename = "Megabytes/Second")]
    MegabytesPerSecond,
    #[serde(rename = "Gigabytes/Second")]
    GigabytesPerSecond,
    #[serde(rename = "Terabytes/Second")]
    TerabytesPerSecond,
    #[serde(rename = "Bits/Second")]
    BitsPerSecond,
    #[serde(rename = "Kilobits/Second")]
    KilobitsPerSecond,
    #[serde(rename = "Megabits/Second")]
    MegabitsPerSecond,
    #[serde(rename = "Gigabits/Second")]
    GigabitsPerSecond,
    #[serde(rename = "Terabits/Second")]
    TerabitsPerSecond,
    None,
    #[default]
    #[serde(other)]
    Unknown,
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
        for (k, value) in self.0.clone() {
            let mut key = k.to_case(Case::Snake);
            match key.as_str() {
                "region" => key = String::from("dimension_region"),
                _ => {}
            }
            let kv = LabelsValues { key, value };
            response.push(kv);
        }

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_unit(unit: &str) -> MetricUnit {
        serde_json::from_str(&format!("\"{unit}\"")).expect("unit should deserialize")
    }

    #[test]
    fn known_units_deserialize_to_their_variant() {
        assert!(matches!(parse_unit("Bytes"), MetricUnit::Bytes));
        assert!(matches!(parse_unit("Kilobytes"), MetricUnit::Kilobytes));
        assert!(matches!(parse_unit("Megabytes"), MetricUnit::Megabytes));
        assert!(matches!(parse_unit("Gigabytes"), MetricUnit::Gigabytes));
        assert!(matches!(parse_unit("Terabytes"), MetricUnit::Terabytes));
        assert!(matches!(parse_unit("Bits"), MetricUnit::Bits));
        assert!(matches!(parse_unit("Kilobits"), MetricUnit::Kilobits));
        assert!(matches!(parse_unit("Megabits"), MetricUnit::Megabits));
        assert!(matches!(parse_unit("Gigabits"), MetricUnit::Gigabits));
        assert!(matches!(parse_unit("Terabits"), MetricUnit::Terabits));
        assert!(matches!(
            parse_unit("Kilobytes/Second"),
            MetricUnit::KilobytesPerSecond
        ));
        assert!(matches!(
            parse_unit("Megabytes/Second"),
            MetricUnit::MegabytesPerSecond
        ));
        assert!(matches!(
            parse_unit("Gigabytes/Second"),
            MetricUnit::GigabytesPerSecond
        ));
        assert!(matches!(
            parse_unit("Terabytes/Second"),
            MetricUnit::TerabytesPerSecond
        ));
        assert!(matches!(
            parse_unit("Bits/Second"),
            MetricUnit::BitsPerSecond
        ));
        assert!(matches!(
            parse_unit("Kilobits/Second"),
            MetricUnit::KilobitsPerSecond
        ));
        assert!(matches!(
            parse_unit("Megabits/Second"),
            MetricUnit::MegabitsPerSecond
        ));
        assert!(matches!(
            parse_unit("Gigabits/Second"),
            MetricUnit::GigabitsPerSecond
        ));
        assert!(matches!(
            parse_unit("Terabits/Second"),
            MetricUnit::TerabitsPerSecond
        ));
    }

    /// The bug this guards: before `#[serde(other)]`, an uncovered unit failed to deserialize
    /// `MetricUnit` at all, which failed the whole `CloudWatchMetric` and dropped the entire
    /// record. Now it degrades to `Unknown` and only that unit's metric is skipped.
    #[test]
    fn unrecognized_unit_falls_back_to_unknown_instead_of_failing() {
        assert!(matches!(parse_unit("SomeFutureUnit"), MetricUnit::Unknown));
    }

    #[test]
    fn cloudwatch_metric_with_uncovered_unit_still_deserializes() {
        let json = r#"{"metric_stream_name":"test-stream","account_id":"123456789012",
             "region":"us-east-1","namespace":"AWS/Test","metric_name":"Whatever",
             "dimensions":{},"timestamp":1700000000000,"value":{"max":1.0},
             "unit":"SomeFutureUnit"}"#;
        let metric: CloudWatchMetric =
            serde_json::from_str(json).expect("record should deserialize despite unknown unit");
        assert!(matches!(metric.unit, MetricUnit::Unknown));
    }
}
