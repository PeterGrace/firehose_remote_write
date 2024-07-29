use crate::consts::PROM_NAMESPACE;
use crate::structs::{CloudWatchMetric, MetricUnit};
use axum::http::StatusCode;
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
use convert_case::{Case, Casing};
use tokio::sync::Mutex;
use url::Url;
use std::collections::BTreeMap;
use crate::aws::AWSState;

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
        sanitize_metric_name(incoming_metric.metric_name.to_lowercase()),
        &incoming_metric.unit
    );

    let dims = incoming_metric.dimensions.to_labels_values();
    let mut labels: Vec<&str> = vec!["metric_stream_name", "account_id", "region", "measurement"];
    let aws = AWSState::initialize(incoming_metric.region.clone()).await;
    let metric_list = aws.cloudwatch.list_metrics()
        .namespace(incoming_metric.namespace.clone())
        .metric_name(incoming_metric.metric_name.clone())
        .send().await.unwrap();
    let mut dim_strs: Vec<String> = vec![];
    for metric in metric_list.metrics().iter() {
        for dim in metric.dimensions().iter() {
            let mut key = dim.clone().name.unwrap().to_case(Case::Snake);
            match key.as_str() {
                "region" => key = String::from("dimension_region"),
                _ => {}
            };
            dim_strs.push(key);
        }
    }
    dim_strs.sort();
    dim_strs.dedup();
    labels.extend(dim_strs.iter().map(|s| { s.as_str() }));
    let mut lv_tree: BTreeMap<&str, &str> = BTreeMap::new();
    for label in labels.iter() {
        lv_tree.insert(label, "");
    }
    lv_tree.insert("metric_stream_name", incoming_metric.metric_stream_name.as_str());
    lv_tree.insert("account_id", incoming_metric.account_id.as_str());
    lv_tree.insert("region", incoming_metric.region.as_str());

    let ordered_labels: Vec<&str> =lv_tree.iter().map(|(k, v)| *k).collect();

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
            let mut recorder = GAUGES.lock().await;
            let outgoing_gauge: GaugeVec = match recorder.get(&incoming_metric.metric_name) {
                None => {
                    let gv = register_gauge_vec!(
                        app_opts!(
                            metric_name.clone(),
                            "autogenerated metric from firehose"
                        ),
                        &ordered_labels
                    )
                        .unwrap();
                    recorder.insert(incoming_metric.metric_name.clone(), gv.clone());
                    gv
                }
                Some(m) => m.clone(),
            };

            if incoming_metric.value.max.is_some() {
                let mut local_lv_tree = lv_tree.clone();
                local_lv_tree.insert("measurement","max");
                for dim in dims.iter() {
                    local_lv_tree.insert(dim.key.as_str(), dim.value.as_str());
                }
                let ordered_values: Vec<&str> = local_lv_tree.iter().map(|(k, v)| *v).collect();
                let m = outgoing_gauge.with_label_values(&ordered_values);
                m.set_timestamp_ms(incoming_metric.timestamp as i64);
                m.set(incoming_metric.value.max.unwrap() as f64);
            }
            if incoming_metric.value.min.is_some() {
                let mut local_lv_tree = lv_tree.clone();
                local_lv_tree.insert("measurement","min");
                for dim in dims.iter() {
                    local_lv_tree.insert(dim.key.as_str(), dim.value.as_str());
                }
                let ordered_values: Vec<&str> = local_lv_tree.iter().map(|(k, v)| *v).collect();
                let m = outgoing_gauge.with_label_values(&ordered_values.clone());
                m.set_timestamp_ms(incoming_metric.timestamp as i64);
                m.set(incoming_metric.value.min.unwrap() as f64);
            }
            if incoming_metric.value.sum.is_some() {
                let mut local_lv_tree = lv_tree.clone();
                local_lv_tree.insert("measurement","sum");
                for dim in dims.iter() {
                    local_lv_tree.insert(dim.key.as_str(), dim.value.as_str());
                }
                let ordered_values: Vec<&str> = local_lv_tree.iter().map(|(k, v)| *v).collect();
                let m = outgoing_gauge.with_label_values(&ordered_values.clone());
                m.set_timestamp_ms(incoming_metric.timestamp as i64);
                m.set(incoming_metric.value.sum.unwrap() as f64);
            }
            if incoming_metric.value.count.is_some() {
                let mut local_lv_tree = lv_tree.clone();
                local_lv_tree.insert("measurement","count");
                for dim in dims.iter() {
                    local_lv_tree.insert(dim.key.as_str(), dim.value.as_str());
                }
                let ordered_values: Vec<&str> = local_lv_tree.iter().map(|(k, v)| *v).collect();
                let m = outgoing_gauge.with_label_values(&ordered_values);
                m.set_timestamp_ms(incoming_metric.timestamp as i64);
                m.set(incoming_metric.value.count.unwrap() as f64);
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
