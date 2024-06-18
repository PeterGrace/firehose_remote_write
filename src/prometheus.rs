use crate::consts::PROM_NAMESPACE;
use crate::structs::{CloudWatchMetric, MetricUnit};
use lazy_static::lazy_static;
use prometheus::core::Metric;
use prometheus::{
    labels, opts, register_counter_vec, register_gauge_vec, register_histogram_vec, CounterVec,
    Gauge, GaugeVec, HistogramVec, TextEncoder,
};
use prometheus_remote_write::WriteRequest;
use reqwest::Client;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use axum::http::StatusCode;
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


lazy_static! {
    pub static ref GAUGES: GaugeHash = Arc::new(Mutex::new(HashMap::new()));
    pub static ref COUNTERS: CounterHash = Arc::new(Mutex::new(HashMap::new()));
    pub static ref HISTOGRAMS: HistoHash = Arc::new(Mutex::new(HashMap::new()));
    pub static ref APP_INFO: GaugeVec = register_gauge_vec!(
        app_opts!(
            "firehose_app_info",
            "static app labels that potentially only change at restart"
        ),
        &["crate_version", "git_hash"]
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
    TOTAL_WRITES_SENT.with_label_values(&[rs.status().clone().as_str()]).inc();
    if rs.status().clone() == StatusCode::BAD_REQUEST {
        let text = rs.text().await?;
        match text.as_str().trim() {
            "out of order sample"
            | "duplicate sample for timestamp"
            | "Out of order sample from remote write" => {
                debug!("One or more samples in this push were duplicated or out-of-order.  Not much we can do about this.")
            }
            _ => {
                bail!("400 Bad request: {text}")
            }
        };
    }
    // now that we've sent them, lets delete them so that they don't pollute future samples
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
    Ok(true)
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
        &incoming_metric.metric_name.to_lowercase(),
        &incoming_metric.unit
    );

    let dims = incoming_metric.dimensions.to_kv();
    let mut label_values: Vec<&str> = vec![
        incoming_metric.metric_stream_name.as_str(),
        incoming_metric.account_id.as_str(),
        incoming_metric.region.as_str(),
        dims.as_str(),
    ];
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
        | MetricUnit::None
        => {
            let mut recorder = GAUGES.lock().await;
            let outgoing_gauge: GaugeVec = match recorder.get(&incoming_metric.metric_name) {
                None => {
                    let gv = register_gauge_vec!(
                        app_opts!(sanitize_metric_name(metric_name.clone()), "autogenerated metric from firehose"),
                        &[
                            "stream_name",
                            "account_id",
                            "region",
                            "dimensions",
                            "measurement"
                        ]
                    )
                        .unwrap();
                    recorder.insert(incoming_metric.metric_name, gv.clone());
                    gv
                }
                Some(m) => m.clone(),
            };

            if incoming_metric.value.max.is_some() {
                let mut labels = label_values.clone();
                labels.push("max");

                let m = outgoing_gauge
                    .with_label_values(&labels.clone());
                m.set_timestamp_ms(incoming_metric.timestamp as i64);
                m.set(incoming_metric.value.max.unwrap() as f64);
            }
            if incoming_metric.value.min.is_some() {
                let mut labels = label_values.clone();
                labels.push("min");

                let m = outgoing_gauge
                    .with_label_values(&labels.clone());
                m.set_timestamp_ms(incoming_metric.timestamp as i64);
                m.set(incoming_metric.value.min.unwrap() as f64);
            }
            if incoming_metric.value.sum.is_some() {
                let mut labels = label_values.clone();
                labels.push("sum");

                let m = outgoing_gauge
                    .with_label_values(&labels.clone());
                m.set_timestamp_ms(incoming_metric.timestamp as i64);
                m.set(incoming_metric.value.sum.unwrap() as f64);
            }
            if incoming_metric.value.count.is_some() {
                let mut labels = label_values.clone();
                labels.push("count");

                let m = outgoing_gauge
                    .with_label_values(&labels.clone());
                m.set_timestamp_ms(incoming_metric.timestamp as i64);
                m.set(incoming_metric.value.count.unwrap() as f64);
            }
        }
        // MetricUnit::Count => {
        //     warn!("Received a count -- need to implement this")
        // }
        MetricUnit::Unknown => {
            warn!("Received unknown metric, {:#?}", incoming_metric);
        }
    }
    Ok(())
}

pub fn sanitize_metric_name(input: String) -> String {
    input
        .as_str()
        .chars()
        .filter(|s| {s.is_ascii_alphanumeric() || *s == '_'}
        ).collect::<String>()


}