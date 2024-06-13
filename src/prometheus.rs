use std::collections::HashMap;
use lazy_static::lazy_static;
use prometheus::{CounterVec, HistogramVec, GaugeVec, labels, opts, register_gauge_vec, register_counter_vec, register_histogram_vec};
use crate::consts::PROM_NAMESPACE;
use std::env;
use std::sync::Arc;
use prometheus_push::prometheus_crate::PrometheusMetricsPusher;
use reqwest::Client;
use tokio::sync::Mutex;
use url::Url;
use crate::structs::{CloudWatchMetric, MetricUnit};
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
        "static app labels that potentially only change at restart"),
        &["crate_version", "git_hash"]
    ).unwrap();
}

pub async fn push_firehose_metrics() -> anyhow::Result<bool> {
    let addr = env::var("PROM_PUSH_ADDR").expect("Can't push without PROM_PUSH_ADDR defined");
    let user: Option<String> = env::var("PROM_USERNAME").ok();
    let pass: Option<String> = env::var("PROM_PASSWORD").ok();
    let metric_families = prometheus::gather();
    let client = Client::new();
    let url = Url::parse(&addr)?;
    let metrics_pusher = PrometheusMetricsPusher::from(client, &url)?;
    match metrics_pusher.push_all(
        "firehose_push",
        &labels! { "instance" => "firehose_to_prom_push"},
        metric_families,
    ).await {
        Ok(_) => {
            info!("Pushed metrics.");
        }
        Err(e) => {
            error!("Couldn't push metrics: {e}");
        }
    }
    Ok(true)
}

pub async fn record_metric(incoming_metric: CloudWatchMetric) -> anyhow::Result<()> {
    let namespace: String = incoming_metric.clone().namespace.split("/").collect::<Vec<&str>>()[1].to_lowercase();
    let metric_name = format!("{namespace}_{}_{}", &incoming_metric.metric_name.to_lowercase(), &incoming_metric.unit);

    let dims = incoming_metric.dimensions.to_kv();
    let mut label_values: Vec<&str> = vec![
        incoming_metric.metric_stream_name.as_str(),
        incoming_metric.account_id.as_str(),
        incoming_metric.region.as_str(),
        dims.as_str()
    ];
    match incoming_metric.unit {
        MetricUnit::Bytes | MetricUnit::Percent | MetricUnit::Average => {
            let mut recorder = GAUGES.lock().await;
            let outgoing_gauge: GaugeVec = match recorder.get(&incoming_metric.metric_name) {
                None => {
                    let gv = register_gauge_vec!(
        app_opts!(
            metric_name.clone(),
            "autogenerated metric from firehose"),
            &["stream_name",
                "account_id",
                "region",
                "dimensions",
                "measurement"
            ]).unwrap();
                    recorder.insert(incoming_metric.metric_name, gv.clone());
                    gv
                }
                Some(m) => m.clone()
            };
            if incoming_metric.value.max.is_some() {
                let mut dims = label_values.clone();
                dims.push("max");
                outgoing_gauge.with_label_values(&dims.clone()).set(incoming_metric.value.min.unwrap() as f64)
            }
            if incoming_metric.value.min.is_some() {
                let mut dims = label_values.clone();
                dims.push("min");
                outgoing_gauge.with_label_values(&dims.clone()).set(incoming_metric.value.min.unwrap() as f64)
            }
            if incoming_metric.value.sum.is_some() {
                let mut dims = label_values.clone();
                dims.push("sum");
                outgoing_gauge.with_label_values(&dims.clone()).set(incoming_metric.value.sum.unwrap() as f64)
            }
            if incoming_metric.value.count.is_some() {
                let mut dims = label_values.clone();
                dims.push("count");
                outgoing_gauge.with_label_values(&dims.clone()).set(incoming_metric.value.count.unwrap() as f64)
            }
        }
        MetricUnit::Count => {},
        MetricUnit::Unknown => {
            warn!("Received unknown metric, {:#?}", incoming_metric);
        }
    }
    Ok(())
}