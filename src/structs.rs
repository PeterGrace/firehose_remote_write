use convert_case::{Case, Casing};
use prometheus_remote_write::{Label, Sample};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use strum::Display;
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;

#[derive(Default, Debug, Deserialize, Clone)]
pub struct DimensionMap(HashMap<String, String>);

#[derive(Default, Debug, Deserialize, Clone)]
pub struct LabelsValues {
    pub key: String,
    pub value: String,
}

/// Everything a request handler needs, cloned per request rather than locked as a whole.
///
/// The previous shape was `Arc<RwLock<AppState>>`, which put *one* lock in front of every
/// field the handler touches. The only mutable field is the discovered-ARN set, and it is
/// written at most once per ARN for the life of the process, so the outer lock cost every
/// request an exclusive acquisition to read a `Sender` that is `Clone + Send + Sync` and
/// needs no synchronisation at all. Moving the lock inside means the handler serialises on
/// nothing in the common path -- see the read-then-write check in `get_firehose`.
#[derive(Clone)]
pub struct AppState {
    pub(crate) firehose_arns: Arc<RwLock<HashSet<String>>>,
    pub(crate) tx: Sender<Vec<(Vec<Label>, Sample)>>,
}

impl AppState {
    pub fn new(tx: Sender<Vec<(Vec<Label>, Sample)>>) -> Self {
        Self {
            firehose_arns: Arc::new(RwLock::new(HashSet::new())),
            tx,
        }
    }
}

#[derive(Default, Deserialize, Debug)]
pub struct FirehoseData {
    pub(crate) data: String,
}

/// The response body Firehose requires on **every** exit path, success or failure.
///
/// `requestId` and `timestamp` are both `required` in the AWS response schema, and a
/// response that does not conform is treated by the Firehose server "as though it had a 500
/// status code with no body" -- so a malformed 200 is a retried failure, not a success.
#[derive(Default, Serialize, Debug)]
pub struct FirehoseResponse {
    #[serde(rename = "requestId")]
    pub(crate) request_id: String,
    pub(crate) timestamp: u64,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub(crate) error_message: Option<String>,
}

/// The AWS response schema caps `errorMessage` at 8192 characters. Overrunning it makes the
/// body non-conforming, which converts whatever status we chose into "500 with no body" and
/// throws away the diagnostic we were trying to send -- so the message that explains the
/// failure would be lost precisely when it is the only evidence available.
const MAX_ERROR_MESSAGE_CHARS: usize = 8192;

impl FirehoseResponse {
    /// Epoch **milliseconds**, which is what the AWS response schema specifies:
    /// "The timestamp (milliseconds since epoch) at which the server processed this request."
    ///
    /// The legacy handler used `as_secs()`, which is a thousandfold undercount -- it dates
    /// every response to January 1970 from Firehose's point of view. Nothing observably
    /// broke, because Firehose does not appear to validate the value, but "the receiver
    /// tolerates our bug" is not a contract.
    ///
    /// `unwrap_or_default` rather than `unwrap`: a clock set before 1970 makes
    /// `duration_since` return `Err`, and panicking in a handler over the host's clock would
    /// fail a delivery that has nothing wrong with it.
    fn now_millis() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub fn ok(request_id: String) -> Self {
        Self {
            request_id,
            timestamp: Self::now_millis(),
            error_message: None,
        }
    }

    /// A failure response. The message matters operationally: after the retry window
    /// expires, "the last instance of the error message is copied to error output S3 bucket
    /// if configured", making it the only post-mortem breadcrumb attached to the lost data.
    pub fn error(request_id: String, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.chars().count() > MAX_ERROR_MESSAGE_CHARS {
            // Truncate on a character boundary, not a byte one: `String::truncate` panics
            // mid-codepoint, and a metric name or ARN carrying multibyte text is enough to
            // land there.
            let cut = message
                .char_indices()
                .nth(MAX_ERROR_MESSAGE_CHARS)
                .map_or(message.len(), |(i, _)| i);
            message.truncate(cut);
        }
        Self {
            request_id,
            timestamp: Self::now_millis(),
            error_message: Some(message),
        }
    }
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

    /// `known_units_deserialize_to_their_variant` only pins deserialization. `metric_base_name`
    /// interpolates `MetricUnit`'s `strum::Display` output unsanitized into the series name, and
    /// only `Count/Second` (pre-existing) is pinned there for separator-free rendering
    /// (`metric_base_name_renders_compound_unit_without_separators` in series.rs). A new
    /// variant whose rendering somehow gained a `/` or `.` would slip past both of those and
    /// only surface as a rejected series name at ingest time.
    #[test]
    fn new_units_render_via_display_without_separators() {
        assert_eq!(MetricUnit::Kilobytes.to_string(), "kilobytes");
        assert_eq!(MetricUnit::Megabytes.to_string(), "megabytes");
        assert_eq!(MetricUnit::Gigabytes.to_string(), "gigabytes");
        assert_eq!(MetricUnit::Terabytes.to_string(), "terabytes");
        assert_eq!(MetricUnit::Bits.to_string(), "bits");
        assert_eq!(MetricUnit::Kilobits.to_string(), "kilobits");
        assert_eq!(MetricUnit::Megabits.to_string(), "megabits");
        assert_eq!(MetricUnit::Gigabits.to_string(), "gigabits");
        assert_eq!(MetricUnit::Terabits.to_string(), "terabits");
        assert_eq!(
            MetricUnit::KilobytesPerSecond.to_string(),
            "kilobytes_per_second"
        );
        assert_eq!(
            MetricUnit::MegabytesPerSecond.to_string(),
            "megabytes_per_second"
        );
        assert_eq!(
            MetricUnit::GigabytesPerSecond.to_string(),
            "gigabytes_per_second"
        );
        assert_eq!(
            MetricUnit::TerabytesPerSecond.to_string(),
            "terabytes_per_second"
        );
        assert_eq!(MetricUnit::BitsPerSecond.to_string(), "bits_per_second");
        assert_eq!(
            MetricUnit::KilobitsPerSecond.to_string(),
            "kilobits_per_second"
        );
        assert_eq!(
            MetricUnit::MegabitsPerSecond.to_string(),
            "megabits_per_second"
        );
        assert_eq!(
            MetricUnit::GigabitsPerSecond.to_string(),
            "gigabits_per_second"
        );
        assert_eq!(
            MetricUnit::TerabitsPerSecond.to_string(),
            "terabits_per_second"
        );
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
