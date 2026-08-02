use crate::consts::PROM_NAMESPACE;
use lazy_static::lazy_static;
use prometheus::{
    opts, register_counter, register_counter_vec, register_gauge, register_gauge_vec, Counter,
    CounterVec, Gauge, GaugeVec,
};
use std::env;

macro_rules! app_opts {
    ($a:expr, $b:expr) => {
        opts!($a, $b).namespace(PROM_NAMESPACE)
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

// # NO HISTOGRAM OR SUMMARY MAY BE REGISTERED IN THIS BLOCK
//
// Not a style preference. Self-metrics reach the remote through
// `writer::self_metric_series`, which is `prometheus::gather()` -> `TextEncoder` ->
// `WriteRequest::from_text_format`. That last step calls `samples_to_timeseries`, which
// returns `Err("histogram not supported yet")` on the first histogram or summary sample it
// meets -- and the `?` propagating it **discards every series already parsed**. It fails the
// whole payload, not the offending family.
//
// So one registered histogram anywhere in this process silently zeroes *every* self-metric
// on *every* flush, for as long as it stays registered. The CloudWatch data keeps flowing,
// the process keeps serving, nothing crashes, and the only trace is one `error!` line per
// flush saying "could not convert self-metrics". The dashboards for the exporter itself go
// flat at exactly the moment you would want to look at them.
//
// This is easy to trip over because the instinct that leads here is a reasonable one:
// somebody wants latency percentiles for `self_flush_duration_seconds` and reaches for the
// obvious tool. Workable shapes through the text format:
//
// * a `_total` counter pair -- `self_x_seconds_total` and `self_x_count_total` -- giving a
//   `rate()`-able average;
// * a max-since-last-export gauge, if the tail is what matters.
//
// Both are plain counters/gauges and both round-trip. Neither `register_histogram_vec` nor
// a histogram-opts helper is imported here, deliberately -- reaching for one is meant to
// require a conscious edit rather than a tab-complete -- and
// `histograms_are_rejected_by_the_self_metric_text_round_trip` pins the consequence if
// somebody does. Anything genuinely needing native histograms has to bypass the text format
// entirely and build `TimeSeries` directly, the way the CloudWatch path does.
lazy_static! {
    // `crate_version` and `git_hash` are as process-constant as `instance` is and could be
    // const labels too, but they are left as variable labels here: this is pre-existing
    // shape, and the value that fills them is supplied once, by `main::set_app_info`.
    //
    // The name is `app_info`, NOT `firehose_app_info`: `app_opts!` prefixes the `firehose`
    // namespace, so the latter exported as `firehose_firehose_app_info`. The metric had
    // never been populated by anything but a test, so the rename cost nothing -- it does
    // now, which is the point of doing it in the same change that starts populating it.
    pub static ref APP_INFO: GaugeVec = register_gauge_vec!(
        self_metric_opts!(
            "app_info",
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
    pub static ref HIGH_WATER_SAMPLES_DROPPED: Counter = register_counter!(self_metric_opts!(
        "self_high_water_samples_dropped_count",
        "Samples dropped because their timestamp was at or below the series high-water mark"
    ))
    .unwrap();
    pub static ref REJECTED_PAYLOADS: Counter = register_counter!(self_metric_opts!(
        "self_rejected_payloads_count",
        "Payloads rejected because the buffer was full"
    ))
    .unwrap();
    // Help text measured against what the metric actually exports, not against what it is
    // named. `writer::flush` gathers self-metrics *inside* the payload it is building, so the
    // value that reaches the remote is the one held at gather time -- never the value written
    // after the push. See the comments at the call sites.
    pub static ref BUFFER_SERIES: Gauge = register_gauge!(self_metric_opts!(
        "self_buffer_series",
        "Series buffered at the moment the most recent flush began (never observed as 0: the reset after a push is not itself exported)"
    ))
    .unwrap();
    pub static ref HIGH_WATER_SERIES: Gauge = register_gauge!(self_metric_opts!(
        "self_high_water_series",
        "Series currently tracked by the writer high-water mark"
    ))
    .unwrap();
    pub static ref FLUSH_DURATION_SECONDS_TOTAL: Counter = register_counter!(self_metric_opts!(
        "self_flush_duration_seconds_total",
        "Cumulative duration of completed flushes in seconds"
    ))
    .unwrap();
    pub static ref FLUSH_COUNT_TOTAL: Counter = register_counter!(self_metric_opts!(
        "self_flush_count_total",
        "Number of completed flushes"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testlog::LogTail;
    use prometheus::TextEncoder;
    use prometheus_remote_write::WriteRequest;
    use std::collections::BTreeMap;

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
    /// *touched*. A duplicate metric name or a malformed label would otherwise sail through
    /// the whole suite and panic on the `.unwrap()` in production, at the first record we
    /// tried to count.
    ///
    /// The load-bearing assertion is that `instance` is on the **gathered output**. Baking it
    /// in as a const label is worthless if the label then silently fails to appear -- the
    /// metric would still register, still increment, still look healthy, and simply not be
    /// attributable to a replica. Nothing else in the suite would notice, because with a const
    /// label there is no call site left that mentions `instance` at all.
    ///
    /// This also pins the `firehose_` namespace onto every name: `app_opts!` supplies it, and
    /// losing it would rename every self-metric at once. `firehose_app_info` in that list is
    /// the *single* prefix: the metric is registered as `app_info` precisely so the namespace
    /// does not stutter, and a name reverting to `firehose_app_info` in the `lazy_static!`
    /// block fails here rather than silently exporting `firehose_firehose_app_info`.
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
        HIGH_WATER_SAMPLES_DROPPED.inc();
        REJECTED_PAYLOADS.inc();
        BUFFER_SERIES.set(7.0);
        HIGH_WATER_SERIES.set(4.0);
        FLUSH_DURATION_SECONDS_TOTAL.inc_by(0.25);
        FLUSH_COUNT_TOTAL.inc();

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
            "firehose_app_info",
            "firehose_queue_freshness_seconds",
            "firehose_self_kinesis_payloads_received_count",
            "firehose_self_remote_writes_sent_count",
            "firehose_self_records_skipped_count",
            "firehose_self_batches_dropped_count",
            "firehose_self_high_water_samples_dropped_count",
            "firehose_self_rejected_payloads_count",
            "firehose_self_buffer_series",
            "firehose_self_high_water_series",
            "firehose_self_flush_duration_seconds_total",
            "firehose_self_flush_count_total",
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

        // The namespace is prefixed exactly once. Restoring the old `firehose_app_info`
        // registration name would make this family gather as `firehose_firehose_app_info`,
        // which the loop above would catch -- but only by panicking with a name-not-found
        // message that reads like a missing registration. Say what actually went wrong.
        assert!(
            !families
                .iter()
                .any(|f| f.get_name().starts_with("firehose_firehose_")),
            "the firehose namespace is prefixed by app_opts!; a metric must not restate it, \
             got: {:?}",
            families.iter().map(|f| f.get_name()).collect::<Vec<_>>()
        );

        // The five converted metrics are plain, so `instance` is their ENTIRE label set. An
        // extra label here would mean something re-introduced a dimension.
        for name in [
            "firehose_self_kinesis_payloads_received_count",
            "firehose_self_records_skipped_count",
            "firehose_self_batches_dropped_count",
            "firehose_self_high_water_samples_dropped_count",
            "firehose_self_rejected_payloads_count",
            "firehose_self_buffer_series",
            "firehose_self_high_water_series",
            "firehose_self_flush_duration_seconds_total",
            "firehose_self_flush_count_total",
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

        // Gauge semantics: a gauge reports the last value set. A `Counter` substituted for
        // `BUFFER_SERIES` would turn "series currently buffered" into "series ever buffered"
        // while still compiling everywhere it is read.
        BUFFER_SERIES.set(10.0);
        BUFFER_SERIES.set(3.0);
        assert_eq!(BUFFER_SERIES.get(), 3.0);

        // The counters accumulate rather than replacing their previous samples.
        let before = RECORDS_SKIPPED.get();
        RECORDS_SKIPPED.inc();
        assert_eq!(RECORDS_SKIPPED.get(), before + 1.0);
        let duration_before = FLUSH_DURATION_SECONDS_TOTAL.get();
        let count_before = FLUSH_COUNT_TOTAL.get();
        FLUSH_DURATION_SECONDS_TOTAL.inc_by(1.5);
        FLUSH_DURATION_SECONDS_TOTAL.inc_by(0.5);
        FLUSH_COUNT_TOTAL.inc();
        FLUSH_COUNT_TOTAL.inc();
        assert_eq!(FLUSH_DURATION_SECONDS_TOTAL.get(), duration_before + 2.0);
        assert_eq!(FLUSH_COUNT_TOTAL.get(), count_before + 2.0);
    }

    /// The landmine described on the `lazy_static!` block, pinned end to end against the real
    /// encoder rather than against hand-written text.
    ///
    /// `writer::tests::from_text_format_rejects_the_entire_payload_when_a_histogram_is_present`
    /// already asserts the parser's behaviour on a literal string. This is the stronger claim
    /// and the one that matters: that a histogram *registered the ordinary way* and rendered
    /// by the *real* `TextEncoder` destroys the whole self-metric payload, including the
    /// healthy counter sitting next to it. Text a test author wrote by hand can be wrong about
    /// what the encoder emits; this cannot.
    ///
    /// # Why a scratch `Registry` and not the global one
    ///
    /// Registering a histogram in the default registry would make this test *cause* the bug it
    /// is describing, permanently, for every other test in the binary: `prometheus::gather()`
    /// reads a process-global registry that is never torn down, so from that moment on
    /// `self_metric_series()` returns empty and every writer test asserting on self-metrics
    /// starts failing for reasons unrelated to itself. `Registry::new()` is isolated, and
    /// `TextEncoder`/`from_text_format` do not care which registry produced the families.
    #[test]
    fn histograms_are_rejected_by_the_self_metric_text_round_trip() {
        use prometheus::{HistogramOpts, HistogramVec, Opts, Registry};

        let registry = Registry::new();

        // A perfectly ordinary counter, registered first, so the assertion below is about
        // collateral damage rather than about a payload that was empty anyway.
        let healthy = Counter::with_opts(Opts::new("scratch_healthy_total", "a normal counter"))
            .expect("counter opts should be valid");
        registry
            .register(Box::new(healthy.clone()))
            .expect("scratch registry should accept a counter");
        healthy.inc();

        // Control: on its own, this round-trips. Without this the assertion below would also
        // pass if `from_text_format` simply rejected everything we ever gave it.
        let healthy_only = TextEncoder::new()
            .encode_to_string(&registry.gather())
            .expect("encoding a counter should succeed");
        let parsed = WriteRequest::from_text_format(healthy_only)
            .expect("CONTROL: a lone counter must round-trip");
        assert_eq!(
            parsed.timeseries.len(),
            1,
            "CONTROL: the healthy metric must be present before the histogram is added"
        );
        // Assert the *value*, not merely the series. A `Counter` that was never incremented
        // still gathers and still renders as an explicit `0`, so `len() == 1` alone holds
        // whether or not `inc()` above ever ran -- measured: neutralising the `inc()` left
        // this control passing. Pinning 1.0 makes the round trip carry data, not just shape.
        assert_eq!(
            parsed.timeseries[0].samples[0].value, 1.0,
            "CONTROL: the counter's value must survive the round trip"
        );

        let histogram = HistogramVec::new(
            HistogramOpts::new("scratch_latency_seconds", "the tempting mistake"),
            &["route"],
        )
        .expect("histogram opts should be valid");
        registry
            .register(Box::new(histogram.clone()))
            .expect("scratch registry should accept a histogram");
        histogram.with_label_values(&["/"]).observe(0.5);

        let both = TextEncoder::new()
            .encode_to_string(&registry.gather())
            .expect("encoding a histogram should succeed -- the encoder is not the problem");
        assert!(
            both.contains("scratch_healthy_total"),
            "the healthy counter must still be in the encoded text, got: {both}"
        );

        let err = WriteRequest::from_text_format(both)
            .expect_err("a registered histogram must not be silently accepted");
        assert!(
            err.to_string().contains("histogram"),
            "the failure must name the unsupported type, got: {err}"
        );
        // The point of the whole test: the counter did not survive either. One histogram
        // takes down every self-metric in the process, not just its own family.
    }
}
