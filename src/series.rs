//! Pure translation from [`CloudWatchMetric`] into remote-write wire types.
//!
//! Everything in this module is a total function of its arguments: no I/O, no shared state,
//! no async. Config reading, the HTTP client, retry/backoff, and the batch accumulator belong
//! in the writer, not here — keeping that boundary is what lets these conversions be tested
//! exhaustively without a server or a runtime.

use crate::consts::PROM_NAMESPACE;
use crate::structs::{CloudWatchMetric, MetricUnit};
use convert_case::{Case, Casing};
use prometheus_remote_write::{Label, Sample};

/// Strip characters Prometheus does not allow in a metric name component.
///
/// Deletes invalid characters rather than replacing them with `_`, and that is deliberate:
/// deletion is what the legacy path did, so it is what keeps existing series names intact.
/// A live metric here is `DeliveryToHttpEndpoint.DataFreshness`, which renders as
/// `deliverytohttpendpointdatafreshness`; switching to `_` replacement would rename it to
/// `deliverytohttpendpoint_datafreshness` and break the user's dashboards. Avoiding renames
/// is the whole reason this module re-applies the `firehose_` prefix by hand — do not
/// undo it one line later.
///
/// Known tradeoff: deletion can collide, e.g. `a.b` and `ab` both yield `ab`. That is
/// accepted. It only affects inputs already malformed today, and the blast radius is bounded
/// at two metrics sharing a series.
///
/// Label *names* deliberately do NOT use this function — they use `to_case(Case::Snake)` with
/// `_` replacement, matching their own legacy behavior. The rules differ because the stakes
/// differ: a duplicate label name gets the entire write request rejected with a 400, whereas
/// a colliding metric name merely merges two series.
fn sanitize_metric_name(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// Build the metric name stem, without the aggregate suffix.
///
/// The `firehose_` prefix used to come from `app_opts!().namespace(PROM_NAMESPACE)`.
/// Building labels directly means we must add it here or every series is renamed.
pub fn metric_base_name(metric: &CloudWatchMetric) -> anyhow::Result<String> {
    let service = sanitize_metric_name(
        metric
            .namespace
            .split('/')
            .nth(1)
            .ok_or_else(|| {
                anyhow::anyhow!("namespace {:?} has no '/' separator", metric.namespace)
            })?
            .to_lowercase()
            .as_str(),
    );

    // Checked after sanitizing, since sanitizing can itself empty a segment ("AWS/!!!").
    // Without this, "AWS/", "AWS//Deep" and "AWS/!!!" all collapse to one malformed name.
    if service.is_empty() {
        anyhow::bail!(
            "namespace {:?} has no usable service segment",
            metric.namespace
        );
    }

    // Same defect class as the service guard above: "!!!" and "..." both sanitize to empty
    // and would collapse onto one series. Bailing costs one dropped record; emitting the
    // malformed name silently corrupts a series other records write to legitimately.
    let name = sanitize_metric_name(&metric.metric_name.to_lowercase());
    if name.is_empty() {
        anyhow::bail!(
            "metric name {:?} has no usable characters",
            metric.metric_name
        );
    }

    Ok(format!("{PROM_NAMESPACE}_{service}_{name}_{}", metric.unit))
}

/// Reserved label names we set ourselves. A dimension normalizing onto any of these
/// would overwrite our own label — `__name__` worst of all, which would clobber the
/// metric name — so collisions are prefixed instead.
///
/// Reachability differs per entry, which matters when reading the tests:
/// - `account_id` and `metric_stream_name` are reachable end-to-end (`AccountId`,
///   `MetricStreamName`); this check is the only thing protecting them.
/// - `region` is unreachable through `labels_for`, because
///   `DimensionMap::to_labels_values` already remaps it. The redundancy is deliberate, so
///   it is pinned by a direct unit test rather than a whole-pipeline one.
/// - `__name__` is unreachable through *any* input: `to_case(Case::Snake)` strips leading
///   and trailing underscores, so nothing can clean to `__name__`. Kept as defence in
///   depth in case that normalization ever changes.
const RESERVED_LABELS: [&str; 4] = ["__name__", "metric_stream_name", "account_id", "region"];

/// Normalize a CloudWatch dimension name into a valid Prometheus label name.
///
/// Unlike metric names, label names get no `firehose_` prefix to make them valid, so
/// every rule has to be enforced here. A label name must match
/// `[a-zA-Z_][a-zA-Z0-9_]*` — note a leading digit is invalid, which `5xxCode` hits.
///
/// Replacement (not deletion) is used here, matching the legacy `to_case(Case::Snake)`
/// behaviour. This deliberately differs from `sanitize_metric_name`, which deletes for
/// byte-parity with legacy metric names. Duplicate label names get the entire write
/// request rejected, so collisions matter far more here than in a metric name.
///
/// Returns `None` when the name cannot be salvaged, so the caller can skip the label
/// rather than emit an invalid one.
fn label_name_for_dimension(name: &str) -> Option<String> {
    let cleaned: String = name
        .to_case(Case::Snake)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Nothing usable survived. Note this must test for "all underscores", not just empty:
    // replacement means `!!!`, `...` and `???` clean to `___`, which is a *technically valid*
    // Prometheus label name and so would sail past an `is_empty()` check. Emitting it is the
    // worse outcome of the two — `___` carries no information and every all-punctuation
    // dimension name collapses onto it, which is the duplicate-label collision that gets the
    // whole write request rejected. `all()` is vacuously true on the empty string, so this
    // subsumes the empty case rather than needing a second check.
    if cleaned.chars().all(|c| c == '_') {
        return None;
    }

    // A leading digit is not a valid label name; prefix rather than drop the dimension.
    let cleaned = if cleaned.starts_with(|c: char| c.is_ascii_digit()) {
        format!("d_{cleaned}")
    } else {
        cleaned
    };

    if RESERVED_LABELS.contains(&cleaned.as_str()) {
        return Some(format!("dimension_{cleaned}"));
    }
    Some(cleaned)
}

/// Build the full label set for one sample, sorted by label name.
///
/// Labels come straight off the record. Absent dimensions are simply absent:
/// Prometheus treats an empty label value as equivalent to a missing label, so the
/// old `""` padding never reached storage.
pub fn labels_for(metric: &CloudWatchMetric, full_metric_name: &str) -> Vec<Label> {
    let mut labels = vec![
        Label {
            name: "__name__".into(),
            value: full_metric_name.to_string(),
        },
        Label {
            name: "metric_stream_name".into(),
            value: metric.metric_stream_name.clone(),
        },
        Label {
            name: "account_id".into(),
            value: metric.account_id.clone(),
        },
        Label {
            name: "region".into(),
            value: metric.region.clone(),
        },
    ];

    for dim in metric.dimensions.to_labels_values() {
        // An empty label value is *equivalent to an absent label* in the Prometheus data
        // model — `foo=""` and no `foo` at all select and store identically. Emitting it
        // therefore costs wire bytes for zero semantic content, and rollup records (the
        // common case) are exactly the ones carrying empty dimensions. Dropping here yields
        // byte-identical stored series, so this is not data loss and must not be "restored"
        // by someone later reading it as such.
        if dim.value.is_empty() {
            continue;
        }

        let Some(name) = label_name_for_dimension(&dim.key) else {
            warn!("dropping dimension with unusable name {:?}", dim.key);
            continue;
        };
        labels.push(Label {
            name,
            value: dim.value,
        });
    }

    // Sorting is a wire requirement, not a nicety, and `DimensionMap` is a `HashMap` so
    // iteration order is nondeterministic. Sort first so dedup sees duplicates adjacent.
    labels.sort_by(|a, b| a.name.cmp(&b.name));

    // A duplicate label name gets the ENTIRE write request rejected by the receiver,
    // killing every good sample batched alongside it. Distinct dimensions can normalize
    // onto one name (`InstanceId`, `instance_id` and `Instance-Id` all become
    // `instance_id`), so this is reachable from real input.
    //
    // Which value survives is genuinely nondeterministic and cannot be made otherwise here.
    // `sort_by` is stable, so ties keep insertion order — but insertion order for dimensions
    // is `HashMap` iteration order. (Our own four labels are always inserted first and so
    // always win a tie, but `RESERVED_LABELS` already prefixes any dimension that could
    // collide with them, so no such tie is reachable; every reachable collision is
    // dimension-vs-dimension.) The same record can therefore produce a different series on
    // two runs, which is precisely why discarding silently is unacceptable: log it so a
    // missing dimension leaves a thread to pull.
    //
    // `dedup_by` passes elements in reverse slice order — returning true removes `a` and
    // keeps `b` — so `b` is the earlier element and the survivor.
    labels.dedup_by(|a, b| {
        if a.name == b.name {
            warn!(
                "dropping duplicate label {:?}: kept value {:?}, discarded {:?}",
                a.name, b.value, a.value
            );
            return true;
        }
        false
    });

    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock};

    fn metric_from(json: &str) -> CloudWatchMetric {
        serde_json::from_str(json).expect("fixture should deserialize")
    }

    fn base(namespace: &str, metric_name: &str, unit: &str) -> CloudWatchMetric {
        metric_from(&format!(
            r#"{{"metric_stream_name":"s","account_id":"1","region":"us-east-1",
                 "namespace":"{namespace}","metric_name":"{metric_name}",
                 "dimensions":{{}},"timestamp":1700000000000,
                 "value":{{"max":1.0}},"unit":"{unit}"}}"#
        ))
    }

    #[test]
    fn metric_base_name_includes_prom_namespace_prefix() {
        let m = base("AWS/ApplicationELB", "RequestCount", "Count");
        assert_eq!(
            metric_base_name(&m).unwrap(),
            "firehose_applicationelb_requestcount_count"
        );
    }

    #[test]
    fn metric_base_name_strips_non_alphanumeric_from_metric_name() {
        let m = base(
            "AWS/Firehose",
            "DeliveryToHttpEndpoint.DataFreshness",
            "Seconds",
        );
        assert_eq!(
            metric_base_name(&m).unwrap(),
            "firehose_firehose_deliverytohttpendpointdatafreshness_seconds"
        );
    }

    /// Pin *which* guard fired, not just "some error".
    ///
    /// `discriminator` must be the phrase unique to one failure mode, not a general category.
    /// Passing a word the sibling guards share (e.g. "namespace", which every namespace error
    /// interpolates) makes this assertion satisfiable by the wrong guard, so deleting the
    /// guard under test leaves the suite green — coverage that reads real but detects nothing.
    /// Mutation testing found exactly that hole here. Keep one distinct phrase per mode.
    fn assert_error_names(err: anyhow::Error, discriminator: &str, value: &str) {
        let msg = format!("{err:#}");
        assert!(
            msg.contains(discriminator) && msg.contains(value),
            "expected error matching {discriminator:?} and naming {value:?}, got: {msg}"
        );
    }

    #[test]
    fn metric_base_name_errors_when_namespace_has_no_slash() {
        let m = base("NoSlashHere", "Whatever", "Count");
        let err = metric_base_name(&m).expect_err("should not build a name");
        assert_error_names(err, "no '/' separator", "NoSlashHere");
    }

    #[test]
    fn metric_base_name_errors_when_service_segment_is_empty() {
        let m = base("AWS/", "Whatever", "Count");
        let err = metric_base_name(&m).expect_err("should not build a name");
        assert_error_names(err, "no usable service segment", "AWS/");
    }

    #[test]
    fn metric_base_name_errors_when_service_segment_is_empty_between_slashes() {
        let m = base("AWS//Deep", "Whatever", "Count");
        let err = metric_base_name(&m).expect_err("should not build a name");
        assert_error_names(err, "no usable service segment", "AWS//Deep");
    }

    #[test]
    fn metric_base_name_errors_when_service_segment_sanitizes_to_empty() {
        let m = base("AWS/!!!", "Whatever", "Count");
        let err = metric_base_name(&m).expect_err("should not build a name");
        assert_error_names(err, "no usable service segment", "AWS/!!!");
    }

    #[test]
    fn metric_base_name_errors_when_metric_name_sanitizes_to_empty() {
        let m = base("AWS/Firehose", "!!!", "Count");
        let err = metric_base_name(&m).expect_err("should not build a name");
        assert_error_names(err, "no usable characters", "!!!");
    }

    #[test]
    fn metric_base_name_errors_when_metric_name_is_all_punctuation() {
        let m = base("AWS/Firehose", "...", "Count");
        let err = metric_base_name(&m).expect_err("should not build a name");
        assert_error_names(err, "no usable characters", "...");
    }

    #[test]
    fn metric_base_name_strips_non_alphanumeric_from_service() {
        let m = base("MyCo/My-App", "Whatever", "Count");
        assert_eq!(
            metric_base_name(&m).unwrap(),
            "firehose_myapp_whatever_count"
        );
    }

    /// The unit is interpolated unsanitized, which is only safe because `MetricUnit` derives
    /// `strum::Display` with `serialize_all = "snake_case"` over in `src/structs.rs`. Pin that
    /// cross-file invariant here: a future variant rendering a `/` or `.` must fail this test
    /// rather than silently emit an invalid metric name and get the whole batch rejected.
    #[test]
    fn metric_base_name_renders_compound_unit_without_separators() {
        let m = base("AWS/Firehose", "Whatever", "Count/Second");
        assert_eq!(
            metric_base_name(&m).unwrap(),
            "firehose_firehose_whatever_count_per_second"
        );
    }

    fn with_dims(dims: &str) -> CloudWatchMetric {
        metric_from(&format!(
            r#"{{"metric_stream_name":"my-stream","account_id":"123456789012",
                 "region":"us-east-1","namespace":"AWS/ApplicationELB",
                 "metric_name":"RequestCount","dimensions":{dims},
                 "timestamp":1700000000000,"value":{{"max":1.0}},"unit":"Count"}}"#
        ))
    }

    /// Capture `tracing` output so tests can assert on log lines.
    ///
    /// A dropped label that is never logged is invisible in production, so "does it warn" is
    /// a real behaviour and needs a real assertion — without this, deleting the `warn!` kills
    /// no test and the guard rots.
    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Install a process-wide log sink exactly once, and hand back its buffer.
    ///
    /// This is deliberately *global* rather than `tracing::subscriber::with_default`, and the
    /// distinction is load-bearing. `tracing` caches per callsite whether anyone is listening.
    /// `with_default` only redirects the calling thread, so a test running concurrently on
    /// another thread sees no subscriber, hits the `warn!` in `labels_for`, and re-caches that
    /// callsite as "nobody is listening" — silently emptying the buffer of whichever test was
    /// asserting on it. Measured, not theorised: with `with_default` this suite failed 2 runs
    /// in 30, and adding `rebuild_interest_cache` only narrowed the window rather than closing
    /// it, because the race is against other threads re-caching afterwards.
    ///
    /// A global default is installed for every thread and never removed, so once
    /// `set_global_default` rebuilds the interest cache the callsite stays enabled for good.
    /// `main()` installs its own subscriber but is never called under test, so there is no
    /// conflict over the single global slot.
    fn log_sink() -> &'static Arc<Mutex<Vec<u8>>> {
        static LOG_SINK: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
        LOG_SINK.get_or_init(|| {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::fmt()
                .with_writer(CaptureWriter(buffer.clone()))
                .with_ansi(false)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("test binary installs exactly one global subscriber");
            buffer
        })
    }

    /// Everything logged so far, by any test. Assertions must therefore key off values unique
    /// to the calling test rather than assuming the buffer holds only their own output.
    fn captured_logs() -> String {
        let bytes = log_sink().lock().unwrap().clone();
        String::from_utf8(bytes).expect("log output should be utf-8")
    }

    fn label_pairs(labels: &[Label]) -> Vec<(String, String)> {
        labels
            .iter()
            .map(|l| (l.name.clone(), l.value.clone()))
            .collect()
    }

    #[test]
    fn labels_are_sorted_by_name_and_include_name_label() {
        let m = with_dims(r#"{"LoadBalancer":"app/foo"}"#);
        let labels = labels_for(&m, "firehose_applicationelb_requestcount_count_max");
        assert_eq!(
            label_pairs(&labels),
            vec![
                (
                    "__name__".into(),
                    "firehose_applicationelb_requestcount_count_max".into()
                ),
                ("account_id".into(), "123456789012".into()),
                ("load_balancer".into(), "app/foo".into()),
                ("metric_stream_name".into(), "my-stream".into()),
                ("region".into(), "us-east-1".into()),
            ]
        );
    }

    #[test]
    fn rollup_and_specific_records_produce_different_label_sets() {
        let rollup = labels_for(&with_dims(r#"{"LoadBalancer":"app/foo"}"#), "m");
        let specific = labels_for(
            &with_dims(r#"{"LoadBalancer":"app/foo","TargetGroup":"tg/bar"}"#),
            "m",
        );
        assert_ne!(rollup, specific);
        assert!(specific.iter().any(|l| l.name == "target_group"));
        assert!(!rollup.iter().any(|l| l.name == "target_group"));
    }

    #[test]
    fn dimension_named_region_is_prefixed_to_avoid_collision() {
        let m = with_dims(r#"{"Region":"eu-west-1"}"#);
        let labels = labels_for(&m, "m");
        // Top-level region label survives untouched.
        assert!(labels
            .iter()
            .any(|l| l.name == "region" && l.value == "us-east-1"));
        // The dimension is renamed rather than overwriting it.
        assert!(labels
            .iter()
            .any(|l| l.name == "dimension_region" && l.value == "eu-west-1"));
    }

    #[test]
    fn invalid_characters_in_dimension_names_are_sanitized() {
        let m = with_dims(r#"{"Some-Weird.Name":"v"}"#);
        let labels = labels_for(&m, "m");
        assert!(
            labels.iter().any(|l| l.name == "some_weird_name"),
            "got {:?}",
            label_pairs(&labels)
        );
    }

    #[test]
    fn leading_digit_label_names_are_repaired() {
        let labels = labels_for(&with_dims(r#"{"5xxCode":"500"}"#), "m");
        let name = &labels.iter().find(|l| l.value == "500").unwrap().name;
        assert!(
            !name.starts_with(|c: char| c.is_ascii_digit()),
            "leading digit is an invalid Prometheus label name, got {name:?}"
        );
    }

    #[test]
    fn dimensions_colliding_with_reserved_labels_are_prefixed() {
        let labels = labels_for(&with_dims(r#"{"AccountId":"999"}"#), "m");
        // Our own account_id must survive untouched.
        assert!(labels
            .iter()
            .any(|l| l.name == "account_id" && l.value == "123456789012"));
        assert!(labels
            .iter()
            .any(|l| l.name == "dimension_account_id" && l.value == "999"));
    }

    /// `MetricStreamName` is a reachable collision with nothing upstream to catch it: unlike
    /// `region`, `DimensionMap::to_labels_values` has no special case for it, so
    /// `RESERVED_LABELS` is the only thing standing between a customer-chosen dimension and
    /// our own label.
    #[test]
    fn dimension_named_metric_stream_name_is_prefixed() {
        let labels = labels_for(&with_dims(r#"{"MetricStreamName":"evil"}"#), "m");
        assert!(labels
            .iter()
            .any(|l| l.name == "metric_stream_name" && l.value == "my-stream"));
        assert!(labels
            .iter()
            .any(|l| l.name == "dimension_metric_stream_name" && l.value == "evil"));
    }

    /// Pin the reserved-name check directly, not just through `labels_for`.
    ///
    /// `region` cannot reach it end-to-end — `DimensionMap::to_labels_values` remaps a
    /// `region` dimension before we ever see it — so a pipeline-level test leaves that entry
    /// free to be deleted unnoticed. The redundancy with `structs.rs` is deliberate: it is
    /// what makes this function correct in isolation, so it gets its own assertion.
    #[test]
    fn label_name_for_dimension_prefixes_reserved_names_in_isolation() {
        assert_eq!(
            label_name_for_dimension("region").as_deref(),
            Some("dimension_region")
        );
        assert_eq!(
            label_name_for_dimension("account_id").as_deref(),
            Some("dimension_account_id")
        );
        assert_eq!(
            label_name_for_dimension("metric_stream_name").as_deref(),
            Some("dimension_metric_stream_name")
        );
    }

    /// Documents why the `__name__` entry of `RESERVED_LABELS` kills no mutant: snake-casing
    /// strips leading and trailing underscores, so no input can ever clean to `__name__` and
    /// the entry is unreachable. If `convert_case` ever stops stripping them, this test flips
    /// and that entry starts earning its keep — which is exactly when we want to be told.
    #[test]
    fn underscore_wrapped_dimension_names_lose_their_underscores() {
        assert_eq!(
            label_name_for_dimension("__name__").as_deref(),
            Some("name")
        );
    }

    #[test]
    fn dimension_named_like_the_name_label_cannot_clobber_the_metric_name() {
        let labels = labels_for(&with_dims(r#"{"__name__":"evil"}"#), "real_metric_name");
        assert!(labels
            .iter()
            .any(|l| l.name == "__name__" && l.value == "real_metric_name"));
    }

    #[test]
    fn unusable_dimension_names_are_dropped_not_emitted_empty() {
        let labels = labels_for(&with_dims(r#"{"!!!":"v"}"#), "m");
        assert!(labels.iter().all(|l| !l.name.is_empty()));
        assert!(labels.iter().all(|l| l.value != "v"));
    }

    /// The sharp edge behind the "unusable" check, kept separate so it can fail on its own.
    ///
    /// Because sanitizing *replaces* rather than deletes, every all-punctuation dimension
    /// name cleans to the same run of underscores. `___` is a technically valid Prometheus
    /// label name, so nothing downstream rejects it individually — but three of them in one
    /// record is a duplicate label name, which gets the entire write request rejected and
    /// takes every good sample batched alongside it. Dropping is the only safe answer.
    #[test]
    fn distinct_all_punctuation_dimension_names_do_not_collapse_onto_one_label() {
        let labels = labels_for(&with_dims(r#"{"!!!":"a","...":"b","???":"c"}"#), "m");
        assert!(
            labels
                .iter()
                .all(|l| l.name.chars().any(|c| c.is_ascii_alphanumeric())),
            "an all-underscore label name carries no information and collides, got {:?}",
            label_pairs(&labels)
        );
        assert!(
            !labels
                .iter()
                .any(|l| ["a", "b", "c"].contains(&l.value.as_str())),
            "got {:?}",
            label_pairs(&labels)
        );
    }

    /// An empty dimension name must not produce an empty label name, which is invalid on the
    /// wire. Covered by the same guard as the all-punctuation case above.
    #[test]
    fn empty_dimension_name_is_dropped() {
        let labels = labels_for(&with_dims(r#"{"":"v"}"#), "m");
        assert!(
            labels.iter().all(|l| !l.name.is_empty() && l.value != "v"),
            "got {:?}",
            label_pairs(&labels)
        );
    }

    #[test]
    fn label_names_are_unique_after_normalization() {
        // All three normalize onto `instance_id`; a duplicate on the wire gets the whole
        // write request rejected.
        let labels = labels_for(
            &with_dims(r#"{"InstanceId":"a","instance_id":"b","Instance-Id":"c"}"#),
            "m",
        );
        let mut names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            before,
            "duplicate label names would 400 the batch"
        );

        // Deliberately NOT asserting *which* value survives. Ties keep insertion order and
        // insertion order here is `HashMap` iteration order, so the survivor genuinely varies
        // between runs; pinning one would flake. Assert only what is actually guaranteed.
        let survivor = labels.iter().find(|l| l.name == "instance_id").unwrap();
        assert!(
            ["a", "b", "c"].contains(&survivor.value.as_str()),
            "survivor should be one of the colliding dimensions, got {:?}",
            survivor.value
        );
    }

    /// The collision above discards a value. Silent, nondeterministic data loss is the worst
    /// combination available — the same input yields different series run to run and nothing
    /// says so — so the discard must leave a diagnosable trace.
    #[test]
    fn discarded_duplicate_labels_are_logged_with_both_values() {
        // Install the sink *before* emitting, so the callsite is enabled when we hit it.
        log_sink();
        // Values are unique to this test: the buffer is shared with every other test, and
        // `label_names_are_unique_after_normalization` collides on the same label name.
        labels_for(
            &with_dims(r#"{"InstanceId":"dup-aa","instance_id":"dup-bb","Instance-Id":"dup-cc"}"#),
            "m",
        );
        let logs = captured_logs();

        assert!(
            logs.contains("dropping duplicate label"),
            "a discarded label must be diagnosable from the logs, got: {logs}"
        );
        assert!(
            logs.contains("instance_id"),
            "the log must name the colliding label, got: {logs}"
        );
        // Two of the three values are discarded and one is kept; whichever way the HashMap
        // ordered them, all three must appear across the log lines for an operator to
        // reconstruct the collision.
        for v in ["dup-aa", "dup-bb", "dup-cc"] {
            assert!(
                logs.contains(&format!("{v:?}")),
                "value {v:?} missing from logs, got: {logs}"
            );
        }
    }

    /// `foo=""` and an absent `foo` are indistinguishable once stored, so sending the empty
    /// one is pure wire overhead — and rollup records, the common case, are the ones that
    /// carry empty dimensions.
    #[test]
    fn dimensions_with_empty_values_are_dropped() {
        let labels = labels_for(
            &with_dims(r#"{"LoadBalancer":"","TargetGroup":"tg/bar"}"#),
            "m",
        );
        assert!(
            !labels.iter().any(|l| l.name == "load_balancer"),
            "empty-valued dimension should not be emitted, got {:?}",
            label_pairs(&labels)
        );
        assert!(
            labels.iter().all(|l| !l.value.is_empty()),
            "no label should carry an empty value, got {:?}",
            label_pairs(&labels)
        );
        // The non-empty sibling is untouched.
        assert!(labels
            .iter()
            .any(|l| l.name == "target_group" && l.value == "tg/bar"));
    }

    #[test]
    fn labels_are_sorted_regardless_of_hashmap_iteration_order() {
        let labels = labels_for(&with_dims(r#"{"Zebra":"1","Alpha":"2","Middle":"3"}"#), "m");
        let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }
}
