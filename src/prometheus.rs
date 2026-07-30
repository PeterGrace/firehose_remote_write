use crate::aws::{get_dimensions, AWSState};
use crate::consts::PROM_NAMESPACE;
use crate::structs::{CloudWatchMetric, LabelsValues, MetricUnit};
use axum::http::StatusCode;
use convert_case::{Case, Casing};
use lazy_static::lazy_static;
use prometheus::core::{Collector, Metric};
use prometheus::{
    labels, opts, register_counter_vec, register_gauge_vec, register_histogram_vec, CounterVec,
    Error, Gauge, GaugeVec, HistogramVec, TextEncoder,
};
use prometheus_remote_write::WriteRequest;
use reqwest::Client;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;

macro_rules! app_opts {
    ($a:expr, $b:expr) => {
        opts!($a, $b).namespace(PROM_NAMESPACE)
    };
}
macro_rules! app_histogram_opts {
    ($a:expr, $b:expr, $c:expr) => {
        histogram_opts!($a, $b, $c).namespace(PROM_NAMESPACE)
    };
}

type GaugeHash = Arc<Mutex<HashMap<String, GaugeVec>>>;
type CounterHash = Arc<Mutex<HashMap<String, CounterVec>>>;
type HistoHash = Arc<Mutex<HashMap<String, HistogramVec>>>;

type DimensionHash = Arc<Mutex<HashMap<String, Vec<String>>>>;

lazy_static! {
    pub static ref GAUGES: GaugeHash = Arc::new(Mutex::new(HashMap::new()));
    pub static ref COUNTERS: CounterHash = Arc::new(Mutex::new(HashMap::new()));
    pub static ref HISTOGRAMS: HistoHash = Arc::new(Mutex::new(HashMap::new()));
    pub static ref DIMENSION_HASH: DimensionHash = Arc::new(Mutex::new(HashMap::new()));
    pub static ref APP_INFO: GaugeVec = register_gauge_vec!(
        app_opts!(
            "firehose_app_info",
            "static app labels that potentially only change at restart"
        ),
        &["crate_version", "git_hash"]
    )
    .unwrap();
    pub static ref FRESHNESS_INFO: GaugeVec = register_gauge_vec!(
        app_opts!(
            "queue_freshness_seconds",
            "The maximum age of currently enqueued records in the firehose queue, in seconds"
        ),
        &["queue_arn"]
    )
    .unwrap();
    pub static ref STREAMS_RECEIVED: CounterVec = register_counter_vec!(
        app_opts!(
            "self_kinesis_payloads_received_count",
            "The number of kinesis payloads received"
        ),
        &[]
    )
    .unwrap();
    pub static ref TOTAL_WRITES_SENT: CounterVec = register_counter_vec!(
        app_opts!(
            "self_remote_writes_sent_count",
            "The number of remnote writes attempted"
        ),
        &["status_code"]
    )
    .unwrap();
}

pub async fn push_firehose_metrics() -> anyhow::Result<bool> {
    let addr = env::var("PROM_WRITE_ADDR").expect("Can't push without PROM_WRITE_ADDR defined");
    let user: Option<String> = env::var("PROM_USERNAME").ok();
    let pass: Option<String> = env::var("PROM_PASSWORD").ok();

    let client = Client::new();

    let metric_families = prometheus::gather();
    let text_metric_families = TextEncoder::new().encode_to_string(&metric_families)?;
    //info!("{text_metric_families}");
    let encoded_write_request = WriteRequest::from_text_format(text_metric_families).unwrap();
    //info!("{:#?}", encoded_write_request);
    let url = format!("{addr}/api/v1/write");
    let body = encoded_write_request.encode_compressed()?;
    let rs = client.post(url).body(body).send().await?;
    TOTAL_WRITES_SENT
        .with_label_values(&[rs.status().clone().as_str()])
        .inc();
    if rs.status().clone() == StatusCode::BAD_REQUEST {
        let text = rs.text().await?;
        match text.as_str().trim() {
            "out of order sample"
            | "duplicate sample for timestamp"
            | "Out of order sample from remote write" => {
                debug!("One or more samples in this push were duplicated or out-of-order.  Not much we can do about this.")
            }
            _ => {
                // 2024-11-01: if we don't clear the collectors, the daemon just keeps sending the bad data every
                // attempt
                clear_collectors().await;

                bail!("400 Bad request: {text}")
            }
        };
    }
    // now that we've sent the metrics, lets delete them so that they don't pollute future samples
    clear_collectors().await;
    Ok(true)
}

pub async fn clear_collectors() {
    let mut collectors = GAUGES.lock().await;
    for (key, collector) in collectors.iter() {
        if let Err(e) = prometheus::unregister(Box::new(collector.clone())) {
            error!("Couldn't unregister collector: {e}");
        }
    }
    collectors.clear();
    let mut collectors = COUNTERS.lock().await;
    for (key, collector) in collectors.iter() {
        if let Err(e) = prometheus::unregister(Box::new(collector.clone())) {
            error!("Couldn't unregister collector: {e}");
        }
    }
    collectors.clear();
    let mut collectors = HISTOGRAMS.lock().await;
    for (key, collector) in collectors.iter() {
        if let Err(e) = prometheus::unregister(Box::new(collector.clone())) {
            error!("Couldn't unregister collector: {e}");
        }
    }
    collectors.clear();
}

/// Record one CloudWatch aggregate (max/min/sum/count) of a datapoint as a gauge sample.
///
/// A record may carry a dimension that is absent from `ordered_labels`, in which case the
/// value list is wider than the registered gauge's label list. That must surface as an
/// error rather than a panic: the caller skips the offending record and moves on.
async fn record_aggregate<'a>(
    metric_name: &str,
    suffix: &str,
    value: f64,
    timestamp_ms: i64,
    lv_tree: &BTreeMap<&'a str, &'a str>,
    ordered_labels: &Vec<&str>,
    dims: &'a [LabelsValues],
) -> anyhow::Result<()> {
    let mut local_lv_tree = lv_tree.clone();
    for dim in dims.iter() {
        local_lv_tree.insert(dim.key.as_str(), dim.value.as_str());
    }
    let ordered_values: Vec<&str> = local_lv_tree.values().copied().collect();
    let full_metric_name = format!("{metric_name}_{suffix}");
    let outgoing_gauge = get_or_register_metric(full_metric_name, ordered_labels).await;

    let m = outgoing_gauge
        .get_metric_with_label_values(&ordered_values)
        .map_err(|e| {
            if let Error::InconsistentCardinality { .. } = e {
                warn!("{metric_name}_{suffix} inconsistent cardinality\n labels: {ordered_labels:#?}\nvalues: {ordered_values:#?}");
            }
            anyhow!(e)
        })?;
    m.set_timestamp_ms(timestamp_ms);
    m.set(value);
    Ok(())
}

pub async fn record_metric(incoming_metric: CloudWatchMetric) -> anyhow::Result<()> {
    let namespace: String = incoming_metric
        .clone()
        .namespace
        .split("/")
        .collect::<Vec<&str>>()[1]
        .to_lowercase();

    let metric_name = format!(
        "{namespace}_{}_{}",
        sanitize_metric_name(incoming_metric.metric_name.to_lowercase()),
        &incoming_metric.unit
    );

    let dims = incoming_metric.dimensions.to_labels_values();
    let mut labels: Vec<&str> = vec!["metric_stream_name", "account_id", "region"];
    let dim_strs = get_dimensions(
        incoming_metric.region.clone(),
        incoming_metric.namespace.clone(),
        incoming_metric.metric_name.clone(),
    )
    .await;
    labels.extend(dim_strs.iter().map(|s| s.as_str()));
    let mut lv_tree: BTreeMap<&str, &str> = BTreeMap::new();
    for label in labels.iter() {
        lv_tree.insert(label, "");
    }
    lv_tree.insert(
        "metric_stream_name",
        incoming_metric.metric_stream_name.as_str(),
    );
    lv_tree.insert("account_id", incoming_metric.account_id.as_str());
    lv_tree.insert("region", incoming_metric.region.as_str());

    let ordered_labels: Vec<&str> = lv_tree.iter().map(|(k, v)| *k).collect();

    match incoming_metric.unit {
        MetricUnit::Count
        | MetricUnit::Bytes
        | MetricUnit::Percent
        | MetricUnit::Average
        | MetricUnit::Seconds
        | MetricUnit::CountPerSecond
        | MetricUnit::BytesPerSecond
        | MetricUnit::Milliseconds
        | MetricUnit::Microseconds
        | MetricUnit::None => {
            for (suffix, value) in [
                ("max", incoming_metric.value.max),
                ("min", incoming_metric.value.min),
                ("sum", incoming_metric.value.sum),
                ("count", incoming_metric.value.count),
            ] {
                if let Some(value) = value {
                    record_aggregate(
                        &metric_name,
                        suffix,
                        value as f64,
                        incoming_metric.timestamp,
                        &lv_tree,
                        &ordered_labels,
                        &dims,
                    )
                    .await?;
                }
            }
        }
        // MetricUnit::Count => {
        //     warn!("Received a count -- need to implement this")
        // }
        MetricUnit::Unknown => {
            warn!("Received unknown metric, {:#?}", incoming_metric.clone());
        }
    }
    Ok(())
}

pub fn sanitize_metric_name(input: String) -> String {
    input
        .as_str()
        .chars()
        .filter(|s| s.is_ascii_alphanumeric() || *s == '_')
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed the dimension cache so `record_metric` does not call out to AWS, and so the
    /// test controls exactly which dimension names it believes the metric has.
    async fn seed_dimensions(metric: &str, dims: &[&str]) {
        DIMENSION_HASH.lock().await.insert(
            format!("us-east-1.AWS/Test.{metric}"),
            dims.iter().map(|s| s.to_string()).collect(),
        );
    }

    /// A record carrying two dimensions, with only the named aggregate populated.
    fn metric_with(metric_name: &str, value: &str) -> CloudWatchMetric {
        let json = format!(
            r#"{{"metric_stream_name":"test-stream","account_id":"123456789012",
                 "region":"us-east-1","namespace":"AWS/Test","metric_name":"{metric_name}",
                 "dimensions":{{"LoadBalancer":"app/foo","TargetGroup":"tg/bar"}},
                 "timestamp":1700000000000,"value":{value},"unit":"Count"}}"#
        );
        serde_json::from_str(&json).expect("test fixture should deserialize")
    }

    // The seeded dimension set omits `target_group`, so the record carries a dimension the
    // registered GaugeVec has no label for. That is the InconsistentCardinality condition
    // the `max` branch already handles; these three branches must not panic on it either.

    /// Characterization test guarding the extraction of `record_aggregate`: it pins the
    /// label/value pairing and the sample itself, which the error-path tests below cannot
    /// see. Written after the refactor and expected to pass on both sides of it.
    #[tokio::test]
    async fn record_metric_writes_sample_with_matching_labels() {
        seed_dimensions("HappyPath", &["load_balancer", "target_group"]).await;
        record_metric(metric_with("HappyPath", r#"{"max":42.0}"#))
            .await
            .expect("matching label set should record cleanly");

        let families = prometheus::gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "firehose_test_happypath_count_max")
            .expect("metric family should be registered");
        let sample = &family.get_metric()[0];

        assert_eq!(sample.get_gauge().get_value(), 42.0);
        assert_eq!(sample.get_timestamp_ms(), 1700000000000);

        let labels: BTreeMap<&str, &str> = sample
            .get_label()
            .iter()
            .map(|l| (l.get_name(), l.get_value()))
            .collect();
        assert_eq!(labels.get("load_balancer"), Some(&"app/foo"));
        assert_eq!(labels.get("target_group"), Some(&"tg/bar"));
        assert_eq!(labels.get("metric_stream_name"), Some(&"test-stream"));
        assert_eq!(labels.get("account_id"), Some(&"123456789012"));
        assert_eq!(labels.get("region"), Some(&"us-east-1"));
    }

    #[tokio::test]
    async fn record_metric_errors_on_cardinality_mismatch_for_min() {
        seed_dimensions("MinOnly", &["load_balancer"]).await;
        let metric = metric_with("MinOnly", r#"{"min":1.0}"#);
        assert!(record_metric(metric).await.is_err());
    }

    #[tokio::test]
    async fn record_metric_errors_on_cardinality_mismatch_for_sum() {
        seed_dimensions("SumOnly", &["load_balancer"]).await;
        let metric = metric_with("SumOnly", r#"{"sum":1.0}"#);
        assert!(record_metric(metric).await.is_err());
    }

    #[tokio::test]
    async fn record_metric_errors_on_cardinality_mismatch_for_count() {
        seed_dimensions("CountOnly", &["load_balancer"]).await;
        let metric = metric_with("CountOnly", r#"{"count":1.0}"#);
        assert!(record_metric(metric).await.is_err());
    }
}

pub async fn get_or_register_metric(metric_name: String, ordered_labels: &Vec<&str>) -> GaugeVec {
    let mut recorder = GAUGES.lock().await;
    match recorder.get(&metric_name) {
        None => {
            let gv = register_gauge_vec!(
                app_opts!(metric_name.clone(), "autogenerated metric from firehose"),
                &ordered_labels
            )
            .unwrap();
            recorder.insert(metric_name.clone(), gv.clone());
            gv
        }
        Some(m) => m.clone(),
    }
}
