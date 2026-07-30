use crate::aws::{get_dimensions, AWSState};
use crate::consts::PROM_NAMESPACE;
use crate::structs::{CloudWatchMetric, LabelsValues, MetricUnit};
use axum::http::StatusCode;
use convert_case::{Case, Casing};
use lazy_static::lazy_static;
use prometheus::core::{Collector, Metric};
use prometheus::{
    labels, opts, register_counter, register_counter_vec, register_gauge, register_gauge_vec,
    register_histogram_vec, Counter, CounterVec, Error, Gauge, GaugeVec, HistogramVec, TextEncoder,
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

/// `app_opts!` plus the `instance` label, baked in at registration.
///
/// Every metric describing *this process* goes through here, and the reason it is a macro
/// rather than a convention is that a convention is exactly what fails silently. `instance`
/// is fixed for the lifetime of the process -- it is a constant, not a dimension -- and
/// expressing a constant as a `*Vec` label means the value is supplied again at every call
/// site. Two call sites that disagree (one passing the hostname, one passing `"unknown"`)
/// do not fail; they create two child series for one replica and *split* the counts between
/// them, so the metric stays plausible while being wrong. As a const label the value is
/// resolved once, at registration, and no call site can restate it.
///
/// It also removes the per-increment `HashMap` lookup and `&[&str]` that `with_label_values`
/// costs, on a counter incremented once per skipped record.
///
/// Metrics with genuinely runtime-varying dimensions -- `TOTAL_WRITES_SENT`'s `status_code`,
/// `FRESHNESS_INFO`'s `queue_arn` -- stay `*Vec` and use this for the const part only.
macro_rules! self_metric_opts {
    ($a:expr, $b:expr) => {
        app_opts!($a, $b).const_label("instance", instance_label())
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
    // `crate_version` and `git_hash` are as process-constant as `instance` is and could be
    // const labels too, but they are left as variable labels here: this is pre-existing
    // shape with pre-existing call sites, and the coordinator's scope is adding `instance`.
    pub static ref APP_INFO: GaugeVec = register_gauge_vec!(
        self_metric_opts!(
            "firehose_app_info",
            "static app labels that potentially only change at restart"
        ),
        &["crate_version", "git_hash"]
    )
    .unwrap();
    // `queue_arn` genuinely varies at runtime -- one child per discovered firehose -- so this
    // stays a Vec and takes `instance` as the const part only.
    pub static ref FRESHNESS_INFO: GaugeVec = register_gauge_vec!(
        self_metric_opts!(
            "queue_freshness_seconds",
            "The maximum age of currently enqueued records in the firehose queue, in seconds"
        ),
        &["queue_arn"]
    )
    .unwrap();
    // Was a `CounterVec` with an empty label set, which is a `*Vec` that can only ever hold
    // one child: all the indirection of a dimension with none of the dimensionality.
    pub static ref STREAMS_RECEIVED: Counter = register_counter!(self_metric_opts!(
        "self_kinesis_payloads_received_count",
        "The number of kinesis payloads received"
    ))
    .unwrap();
    // `status_code` is the point of this metric, so it stays a Vec.
    pub static ref TOTAL_WRITES_SENT: CounterVec = register_counter_vec!(
        self_metric_opts!(
            "self_remote_writes_sent_count",
            "The number of remnote writes attempted"
        ),
        &["status_code"]
    )
    .unwrap();
    pub static ref RECORDS_SKIPPED: Counter = register_counter!(self_metric_opts!(
        "self_records_skipped_count",
        "Malformed records dropped during parse"
    ))
    .unwrap();
    pub static ref BATCHES_DROPPED: Counter = register_counter!(self_metric_opts!(
        "self_batches_dropped_count",
        "Batches abandoned after exhausting retries"
    ))
    .unwrap();
    pub static ref REJECTED_PAYLOADS: Counter = register_counter!(self_metric_opts!(
        "self_rejected_payloads_count",
        "Payloads rejected because the buffer was full"
    ))
    .unwrap();
    pub static ref BUFFER_SERIES: Gauge = register_gauge!(self_metric_opts!(
        "self_buffer_series",
        "Series currently buffered awaiting flush"
    ))
    .unwrap();
    pub static ref FLUSH_DURATION: Gauge = register_gauge!(self_metric_opts!(
        "self_flush_duration_seconds",
        "Duration of the most recent flush"
    ))
    .unwrap();
}

/// Resolve the value for the `instance` label on app self-metrics.
///
/// This applies to self-metrics ONLY. CloudWatch series must NOT carry a per-replica
/// label: the same CloudWatch datapoint delivered to two replicas must remain one
/// series, or we manufacture the duplicate-series problem we are trying to remove.
pub fn instance_label_value(hostname: Option<String>) -> String {
    match hostname {
        Some(h) if !h.is_empty() => h,
        _ => String::from("unknown"),
    }
}

/// The `instance` label value for this process, resolved once.
///
/// Cached rather than re-read, for two reasons beyond the obvious one that `HOSTNAME` cannot
/// change during a process's life. `env::var` allocates a `String` and takes a lock over the
/// process environment on every call, and this is called per flush and potentially per
/// request. More importantly, `env::set_var` is unsound while another thread is calling
/// `env::var` -- the reason Rust 2024 made it `unsafe` -- and this crate's own tests do call
/// `set_var` (`config::tests::from_env_reads_each_variable_into_its_own_field`). Resolving
/// once at first use closes that window instead of reopening it on every metric increment.
///
/// Returning `&'static str` rather than `String` is the point of the cache: a cached value
/// that is cloned on the way out still allocates per call. It is also what `const_label` and
/// `with_label_values` both want.
///
/// Note the division of labour with [`self_metric_opts!`]: this is called once per metric, at
/// registration, not once per increment. The hazard it used to guard against -- two call
/// sites disagreeing about the value and silently splitting one replica's counts across two
/// child series -- is now structurally impossible for these metrics, because the value is
/// baked into the descriptor and there is no call site that can restate it.
pub fn instance_label() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| instance_label_value(env::var("HOSTNAME").ok()))
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
    ordered_labels: &[&str],
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
    .await?;
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
    use crate::testlog::LogTail;

    #[test]
    fn instance_label_falls_back_when_hostname_unset() {
        assert_eq!(instance_label_value(None), "unknown");
    }

    #[test]
    fn instance_label_uses_hostname_when_set() {
        assert_eq!(
            instance_label_value(Some("pod-abc123".into())),
            "pod-abc123"
        );
    }

    #[test]
    fn empty_hostname_falls_back() {
        assert_eq!(instance_label_value(Some(String::new())), "unknown");
    }

    /// `HOSTNAME` is read once and the answer kept, so this asserts the *same* string, not an
    /// equal one. Pointer identity is the only thing that distinguishes a cache from a
    /// function that re-reads the environment and happens to get the same answer twice.
    #[test]
    fn instance_label_is_resolved_once_and_reused() {
        let first = instance_label();
        let second = instance_label();
        assert!(
            std::ptr::eq(first, second),
            "instance_label must hand back the cached value, not re-resolve it"
        );
    }

    /// Whatever it cached must be what `instance_label_value` would have produced from the
    /// real environment. Written against the live `HOSTNAME` rather than a fixture because
    /// the cache means the value cannot be re-resolved after any other test has touched it,
    /// and this crate's tests deliberately confine all env mutation to `config::tests`.
    ///
    /// # This is an environment-dependent guard, measured
    ///
    /// It is the only test watching *which* variable `instance_label` reads, and it only
    /// catches a wrong one when `HOSTNAME` is actually set. Mutating the lookup to `HOST` was
    /// measured killing this test with `HOSTNAME` set and SURVIVING under `env -u HOSTNAME`:
    /// with neither variable present both sides resolve to `"unknown"` and agree. Every
    /// deployment target here (Docker, Kubernetes) sets `HOSTNAME` unconditionally, so this
    /// holds where it matters, but a CI runner that scrubs the environment would silently
    /// stop watching it. Making it unconditional means injecting the variable name into
    /// `instance_label`, which buys a test-only seam for a value that is read once per
    /// process -- judged not worth it, but recorded rather than left to be rediscovered.
    #[test]
    fn instance_label_agrees_with_the_environment_it_read() {
        assert_eq!(
            instance_label(),
            instance_label_value(env::var("HOSTNAME").ok())
        );
        assert!(
            !instance_label().is_empty(),
            "the label must never be empty"
        );
    }

    /// `lazy_static` registers a metric into the global registry the first time it is
    /// *touched*, and nothing touches most of these until tasks 9-11 wire them up. Until then
    /// a duplicate metric name or a malformed label would sail through the whole suite and
    /// panic on the `.unwrap()` in production, at the first record we tried to count.
    ///
    /// The load-bearing assertion is that `instance` is on the **gathered output**. Baking it
    /// in as a const label is worthless if the label then silently fails to appear -- the
    /// metric would still register, still increment, still look healthy, and simply not be
    /// attributable to a replica. Nothing else in the suite would notice, because with a const
    /// label there is no call site left that mentions `instance` at all.
    ///
    /// This also pins the `firehose_` namespace onto every name: `app_opts!` supplies it, and
    /// losing it would rename every self-metric at once.
    ///
    /// # One caveat, measured
    ///
    /// The assertion is that the label equals `instance_label()`, so a mutation hardcoding the
    /// const label to `"unknown"` is only caught when `HOSTNAME` is actually set -- verified
    /// killed with it set and SURVIVING under `env -u HOSTNAME`, where both sides are
    /// `"unknown"` and agree. Same class of gap as
    /// `instance_label_agrees_with_the_environment_it_read`, and the same judgement: every
    /// deployment target sets `HOSTNAME`, so it holds where it matters. The label's *presence*
    /// and *name* are watched unconditionally; only its value has this dependency.
    ///
    /// # Why this is one test and not several
    ///
    /// A plain `Counter`/`Gauge` is a process-wide singleton with no label to isolate on, so
    /// two tests writing the same static race on its value -- which is the cost of dropping
    /// the `*Vec`, and worth stating rather than discovering. The previous `*Vec` version of
    /// this test split the work across two tests and flaked 4 runs in 10 for exactly that
    /// reason. All writes to the plain self-metrics therefore live here.
    #[test]
    fn every_self_metric_registers_with_the_instance_const_label() {
        let _log = LogTail::start();
        let instance = instance_label();

        // Touch every self-metric so `lazy_static` registers it.
        APP_INFO
            .with_label_values(&["0.0.0-test", "deadbeef"])
            .set(1.0);
        FRESHNESS_INFO
            .with_label_values(&["arn:aws:firehose:test"])
            .set(12.0);
        TOTAL_WRITES_SENT.with_label_values(&["200"]).inc();
        STREAMS_RECEIVED.inc();
        RECORDS_SKIPPED.inc();
        BATCHES_DROPPED.inc();
        REJECTED_PAYLOADS.inc();
        BUFFER_SERIES.set(7.0);
        FLUSH_DURATION.set(0.25);

        let families = prometheus::gather();
        let labels_of = |name: &str| -> Vec<BTreeMap<String, String>> {
            let family = families
                .iter()
                .find(|f| f.get_name() == name)
                .unwrap_or_else(|| {
                    panic!(
                        "{name} should be registered, got: {:?}",
                        families.iter().map(|f| f.get_name()).collect::<Vec<_>>()
                    )
                });
            family
                .get_metric()
                .iter()
                .map(|m| {
                    m.get_label()
                        .iter()
                        .map(|l| (l.get_name().to_string(), l.get_value().to_string()))
                        .collect()
                })
                .collect()
        };

        // Every self-metric, whether or not it also has runtime dimensions, must carry
        // `instance` on every child it emits.
        for name in [
            "firehose_firehose_app_info",
            "firehose_queue_freshness_seconds",
            "firehose_self_kinesis_payloads_received_count",
            "firehose_self_remote_writes_sent_count",
            "firehose_self_records_skipped_count",
            "firehose_self_batches_dropped_count",
            "firehose_self_rejected_payloads_count",
            "firehose_self_buffer_series",
            "firehose_self_flush_duration_seconds",
        ] {
            let children = labels_of(name);
            assert!(!children.is_empty(), "{name} gathered no series at all");
            for child in &children {
                assert_eq!(
                    child.get("instance").map(String::as_str),
                    Some(instance),
                    "{name} must carry the instance const label, got: {child:?}"
                );
            }
        }

        // The five converted metrics are plain, so `instance` is their ENTIRE label set. An
        // extra label here would mean something re-introduced a dimension.
        for name in [
            "firehose_self_kinesis_payloads_received_count",
            "firehose_self_records_skipped_count",
            "firehose_self_batches_dropped_count",
            "firehose_self_rejected_payloads_count",
            "firehose_self_buffer_series",
            "firehose_self_flush_duration_seconds",
        ] {
            let children = labels_of(name);
            assert_eq!(
                children.len(),
                1,
                "{name} is plain and can only have one series"
            );
            assert_eq!(
                children[0].keys().collect::<Vec<_>>(),
                vec!["instance"],
                "{name} must carry instance and nothing else"
            );
        }

        // The two that kept a real dimension must have BOTH, not one or the other: dropping
        // the const label and dropping the dimension are different bugs with the same shape.
        let writes = labels_of("firehose_self_remote_writes_sent_count");
        assert!(
            writes
                .iter()
                .any(|c| c.get("status_code").map(String::as_str) == Some("200")),
            "status_code must survive alongside the const label, got: {writes:?}"
        );
        let freshness = labels_of("firehose_queue_freshness_seconds");
        assert!(
            freshness
                .iter()
                .any(|c| c.get("queue_arn").map(String::as_str) == Some("arn:aws:firehose:test")),
            "queue_arn must survive alongside the const label, got: {freshness:?}"
        );

        // Gauge semantics, asserted here because this test owns all writes to these two: a
        // gauge reports the last value set, not an accumulated one. A `Counter` substituted
        // for `BUFFER_SERIES` would turn "series currently buffered" into "series ever
        // buffered" while still compiling everywhere it is read.
        BUFFER_SERIES.set(10.0);
        BUFFER_SERIES.set(3.0);
        FLUSH_DURATION.set(1.5);
        FLUSH_DURATION.set(0.5);
        assert_eq!(BUFFER_SERIES.get(), 3.0);
        assert_eq!(FLUSH_DURATION.get(), 0.5);

        // And the counters only go up.
        let before = RECORDS_SKIPPED.get();
        RECORDS_SKIPPED.inc();
        assert_eq!(RECORDS_SKIPPED.get(), before + 1.0);
    }

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

    /// Pin the failure mode, not just "some error". Without this a future change that made
    /// `record_metric` fail earlier for an unrelated reason would keep these tests green
    /// while no longer exercising the panic path they exist to guard.
    fn assert_inconsistent_cardinality(err: anyhow::Error) {
        match err.downcast_ref::<Error>() {
            Some(Error::InconsistentCardinality { .. }) => {}
            _ => panic!("expected InconsistentCardinality, got: {err:?}"),
        }
    }

    #[tokio::test]
    async fn record_metric_errors_on_cardinality_mismatch_for_min() {
        seed_dimensions("MinOnly", &["load_balancer"]).await;
        let metric = metric_with("MinOnly", r#"{"min":1.0}"#);
        let err = record_metric(metric).await.expect_err("should not record");
        assert_inconsistent_cardinality(err);
    }

    #[tokio::test]
    async fn record_metric_errors_on_cardinality_mismatch_for_sum() {
        seed_dimensions("SumOnly", &["load_balancer"]).await;
        let metric = metric_with("SumOnly", r#"{"sum":1.0}"#);
        let err = record_metric(metric).await.expect_err("should not record");
        assert_inconsistent_cardinality(err);
    }

    #[tokio::test]
    async fn record_metric_errors_on_cardinality_mismatch_for_count() {
        seed_dimensions("CountOnly", &["load_balancer"]).await;
        let metric = metric_with("CountOnly", r#"{"count":1.0}"#);
        let err = record_metric(metric).await.expect_err("should not record");
        assert_inconsistent_cardinality(err);
    }

    /// `max` is evaluated first and fails on the mismatched label set, so `min` must never
    /// be reached. `get_or_register_metric` runs before the label lookup, so the presence
    /// of a gauge in GAUGES is the observable signal that an aggregate was attempted.
    #[tokio::test]
    async fn record_metric_stops_at_first_failing_aggregate() {
        seed_dimensions("ShortCircuit", &["load_balancer"]).await;
        let metric = metric_with("ShortCircuit", r#"{"max":1.0,"min":2.0}"#);
        let err = record_metric(metric).await.expect_err("should not record");
        assert_inconsistent_cardinality(err);

        let gauges = GAUGES.lock().await;
        assert!(
            gauges.contains_key("test_shortcircuit_count_max"),
            "max should have been attempted before failing"
        );
        assert!(
            !gauges.contains_key("test_shortcircuit_count_min"),
            "min must not be reached once max has failed"
        );
    }
}

pub async fn get_or_register_metric(metric_name: String, ordered_labels: &[&str]) -> GaugeVec {
    let mut recorder = GAUGES.lock().await;
    match recorder.get(&metric_name) {
        None => {
            let gv = register_gauge_vec!(
                app_opts!(metric_name.clone(), "autogenerated metric from firehose"),
                ordered_labels
            )
            .unwrap();
            recorder.insert(metric_name.clone(), gv.clone());
            gv
        }
        Some(m) => m.clone(),
    }
}
