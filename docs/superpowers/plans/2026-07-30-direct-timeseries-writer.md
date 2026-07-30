# Direct TimeSeries + Writer Task Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the prometheus client registry on the CloudWatch data path with directly-constructed remote-write `TimeSeries`, and serialize all pushes through a single writer task.

**Architecture:** Axum handlers decode and convert Firehose records into `(Vec<Label>, Sample)` pairs and hand them to a bounded mpsc channel, returning 200 on enqueue and 503 when full. One writer task owns a `HashMap<Vec<Label>, BTreeMap<i64, f64>>` accumulator, flushing on an interval or a series-count threshold, merging app self-metrics, and pushing with bounded retry.

**Tech Stack:** Rust 2021, axum 0.7, tokio, `prometheus_remote_write` (`WriteRequest`/`TimeSeries`/`Label`/`Sample`, all fields `pub`), `prometheus` (self-metrics only), reqwest.

**Spec:** `docs/superpowers/specs/2026-07-30-direct-timeseries-design.md`

---

## Critical context

**The `firehose_` prefix is now yours to add.** Today `app_opts!` calls
`.namespace(PROM_NAMESPACE)`, and the prometheus crate builds `fq_name = "{namespace}_{name}"`.
That is why the current metric is `firehose_test_happypath_count_max`. Once we build `Label`s
directly, nothing adds that prefix — the code must. Omitting it renames every series.

**Empty label values are fine.** Prometheus treats `foo=""` as equivalent to an absent label,
so whatever CloudWatch sends is preserved verbatim and no padding is needed.

**Existing constants:** `PROM_NAMESPACE = "firehose"` in `src/consts.rs`.

## File structure

| File | Responsibility |
|---|---|
| `src/series.rs` (new) | Pure conversion: `CloudWatchMetric` → `Vec<(Vec<Label>, Sample)>`. No I/O. |
| `src/writer.rs` (new) | Accumulator, flush, retry, the writer task, writer self-metrics. |
| `src/config.rs` (new) | Env-derived `Config`. |
| `src/prometheus.rs` | Self-metric statics only; everything else deleted. |
| `src/aws/mod.rs` | `get_freshness` only. |
| `src/structs.rs` | `AppState` gains the channel sender. |
| `src/main.rs` | Handler: decode, parse, enqueue, respond. |

---

### Task 1: Metric name construction

**Files:**
- Create: `src/series.rs`
- Modify: `src/main.rs` (add `mod series;`)

- [ ] **Step 1: Write the failing test**

Create `src/series.rs`:

```rust
use crate::consts::PROM_NAMESPACE;
use crate::structs::{CloudWatchMetric, MetricUnit};
use prometheus_remote_write::{Label, Sample};

#[cfg(test)]
mod tests {
    use super::*;

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
        let m = base("AWS/Firehose", "DeliveryToHttpEndpoint.DataFreshness", "Seconds");
        assert_eq!(
            metric_base_name(&m).unwrap(),
            "firehose_firehose_deliverytohttpendpointdatafreshness_seconds"
        );
    }

    #[test]
    fn metric_base_name_errors_when_namespace_has_no_slash() {
        let m = base("NoSlashHere", "Whatever", "Count");
        assert!(metric_base_name(&m).is_err());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write series::tests`
Expected: FAIL to compile — `cannot find function metric_base_name in this scope`.

Add `mod series;` to `src/main.rs` beside the other `mod` declarations if the module is not found.

- [ ] **Step 3: Write minimal implementation**

Add above the `#[cfg(test)]` block in `src/series.rs`:

```rust
/// Strip characters Prometheus does not allow in a metric name component.
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
    let service = metric
        .namespace
        .split('/')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("namespace {:?} has no '/' separator", metric.namespace))?
        .to_lowercase();

    Ok(format!(
        "{PROM_NAMESPACE}_{}_{}_{}",
        sanitize_metric_name(&service),
        sanitize_metric_name(&metric.metric_name.to_lowercase()),
        metric.unit
    ))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write series::tests`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/series.rs src/main.rs
git commit -m "feat: metric name construction for direct TimeSeries (#2)"
```

---

### Task 2: Label derivation

**Files:**
- Modify: `src/series.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/series.rs`:

```rust
    fn with_dims(dims: &str) -> CloudWatchMetric {
        metric_from(&format!(
            r#"{{"metric_stream_name":"my-stream","account_id":"123456789012",
                 "region":"us-east-1","namespace":"AWS/ApplicationELB",
                 "metric_name":"RequestCount","dimensions":{dims},
                 "timestamp":1700000000000,"value":{{"max":1.0}},"unit":"Count"}}"#
        ))
    }

    fn label_pairs(labels: &[Label]) -> Vec<(String, String)> {
        labels.iter().map(|l| (l.name.clone(), l.value.clone())).collect()
    }

    #[test]
    fn labels_are_sorted_by_name_and_include_name_label() {
        let m = with_dims(r#"{"LoadBalancer":"app/foo"}"#);
        let labels = labels_for(&m, "firehose_applicationelb_requestcount_count_max");
        assert_eq!(
            label_pairs(&labels),
            vec![
                ("__name__".into(), "firehose_applicationelb_requestcount_count_max".into()),
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
        assert!(labels.iter().any(|l| l.name == "region" && l.value == "us-east-1"));
        // The dimension is renamed rather than overwriting it.
        assert!(labels.iter().any(|l| l.name == "dimension_region" && l.value == "eu-west-1"));
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write series::tests`
Expected: FAIL to compile — `cannot find function labels_for in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/series.rs`:

```rust
use convert_case::{Case, Casing};

/// Reserved label names we set ourselves. A dimension normalizing onto any of these
/// would overwrite our own label, so collisions are prefixed instead.
///
/// The value of an entry here is DATA PRESERVATION, not protecting our own label: our
/// labels are inserted first and stable sort makes them the dedup survivor anyway.
/// Without the prefix, a colliding dimension's value is silently discarded.
///
/// Reachability per entry:
/// - `metric_stream_name`, `account_id`: reachable from a real dimension name.
/// - `region`: unreachable via `labels_for` because `to_labels_values()` pre-maps it,
///   but reachable when calling this function directly, so it is pinned by a unit test.
/// - `__name__`: unreachable ONLY because normalization trims leading/trailing `_`.
///   Do not weaken that trim without restoring live protection here. Before the trim
///   existed, `!!name!!` reached `__name__` — `to_case` strips underscores but NOT
///   punctuation, which becomes `_` in the later mapping step.
///
/// A mutant that kills zero tests means nothing is watching the entry. It does NOT
/// mean the entry is unreachable — that inference was made once here and was wrong.
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
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();

    // NOT `is_empty()`. Sanitization REPLACES rather than deletes, so `!!!` becomes
    // `___`, not `""` — a technically-valid label name that would ship silently, and
    // one that every all-punctuation dimension name collapses onto, producing the
    // duplicate that gets the whole batch rejected. `all()` is vacuously true on "",
    // so this subsumes the empty case.
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
        Label { name: "__name__".into(), value: full_metric_name.to_string() },
        Label { name: "metric_stream_name".into(), value: metric.metric_stream_name.clone() },
        Label { name: "account_id".into(), value: metric.account_id.clone() },
        Label { name: "region".into(), value: metric.region.clone() },
    ];

    for dim in metric.dimensions.to_labels_values() {
        let Some(name) = label_name_for_dimension(&dim.key) else {
            warn!("dropping dimension with unusable name {:?}", dim.key);
            continue;
        };
        labels.push(Label { name, value: dim.value });
    }

    // Sorting is a wire requirement, not a nicety, and `DimensionMap` is a `HashMap` so
    // iteration order is nondeterministic. Sort first so dedup sees duplicates adjacent.
    labels.sort_by(|a, b| a.name.cmp(&b.name));

    // A duplicate label name gets the ENTIRE write request rejected by the receiver,
    // killing every good sample batched alongside it. Distinct dimensions can normalize
    // onto one name (`InstanceId`, `instance_id` and `Instance-Id` all become
    // `instance_id`), so this is reachable from real input, and because the source is a
    // HashMap, which one survives is nondeterministic across runs.
    labels.dedup_by(|a, b| a.name == b.name);

    labels
}
```

Note: `DimensionMap::to_labels_values` already snake-cases keys and maps `region`;
`label_name_for_dimension` re-applies both idempotently plus character sanitization,
leading-digit repair, and the full reserved-name check.

**Add these tests alongside the ones above** — each covers a degenerate case that produces
either an invalid label name or a batch-killing duplicate:

```rust
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
        assert!(labels.iter().any(|l| l.name == "account_id" && l.value == "123456789012"));
        assert!(labels.iter().any(|l| l.name == "dimension_account_id" && l.value == "999"));
    }

    #[test]
    fn dimension_named_like_the_name_label_cannot_clobber_the_metric_name() {
        let labels = labels_for(&with_dims(r#"{"__name__":"evil"}"#), "real_metric_name");
        assert!(labels.iter().any(|l| l.name == "__name__" && l.value == "real_metric_name"));
    }

    #[test]
    fn unusable_dimension_names_are_dropped_not_emitted_empty() {
        let labels = labels_for(&with_dims(r#"{"!!!":"v"}"#), "m");
        assert!(labels.iter().all(|l| !l.name.is_empty()));
        assert!(labels.iter().all(|l| l.value != "v"));
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
        assert_eq!(names.len(), before, "duplicate label names would 400 the batch");
    }

    #[test]
    fn labels_are_sorted_regardless_of_hashmap_iteration_order() {
        let labels = labels_for(
            &with_dims(r#"{"Zebra":"1","Alpha":"2","Middle":"3"}"#),
            "m",
        );
        let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write series::tests`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add src/series.rs
git commit -m "feat: derive labels directly from each record (#2)"
```

---

### Task 3: Sample extraction

**Files:**
- Modify: `src/series.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
    fn values_json(values: &str, unit: &str) -> CloudWatchMetric {
        metric_from(&format!(
            r#"{{"metric_stream_name":"s","account_id":"1","region":"us-east-1",
                 "namespace":"AWS/Test","metric_name":"M","dimensions":{{}},
                 "timestamp":1700000000000,"value":{values},"unit":"{unit}"}}"#
        ))
    }

    fn names(series: &[(Vec<Label>, Sample)]) -> Vec<String> {
        series
            .iter()
            .map(|(labels, _)| {
                labels.iter().find(|l| l.name == "__name__").unwrap().value.clone()
            })
            .collect()
    }

    #[test]
    fn all_four_aggregates_become_separate_series() {
        let m = values_json(r#"{"max":1.0,"min":2.0,"sum":3.0,"count":4.0}"#, "Count");
        let series = to_series(&m, NOW).unwrap();
        assert_eq!(
            names(&series),
            vec![
                "firehose_test_m_count_max",
                "firehose_test_m_count_min",
                "firehose_test_m_count_sum",
                "firehose_test_m_count_count",
            ]
        );
    }

    #[test]
    fn absent_aggregates_produce_no_series() {
        let m = values_json(r#"{"min":2.0}"#, "Count");
        let series = to_series(&m, NOW).unwrap();
        assert_eq!(names(&series), vec!["firehose_test_m_count_min"]);
    }

    #[test]
    fn sample_carries_record_timestamp_and_value() {
        let m = values_json(r#"{"max":42.0}"#, "Count");
        let series = to_series(&m, NOW).unwrap();
        assert_eq!(series[0].1.timestamp, 1700000000000);
        assert_eq!(series[0].1.value, 42.0);
    }

    #[test]
    fn unknown_unit_produces_no_series() {
        // NOTE: `MetricUnit` has no `#[serde(other)]`, so an unrecognised unit string
        // fails deserialization outright rather than becoming `Unknown` (see issue #12).
        // `Unknown` is only reachable via `Default`, so build it directly.
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.unit = MetricUnit::Unknown;
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    #[test]
    fn record_with_no_populated_aggregates_produces_no_series() {
        let m = values_json(r#"{}"#, "Count");
        assert!(
            to_series(&m, NOW).unwrap().is_empty(),
            "must yield zero series, not one with an empty sample vec"
        );
    }

    #[test]
    fn non_finite_values_are_dropped() {
        // A specific NaN payload is Prometheus's staleness marker; passing NaN through is
        // at best meaningless and at worst indistinguishable from "this series ended".
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.value.max = Some(f32::NAN);
        assert!(to_series(&m, NOW).unwrap().is_empty());

        m.value.max = Some(f32::INFINITY);
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    #[test]
    fn aggregate_order_is_stable() {
        let m = values_json(r#"{"count":4.0,"sum":3.0,"min":2.0,"max":1.0}"#, "Count");
        let series = to_series(&m, NOW).unwrap();
        assert_eq!(
            names(&series),
            vec![
                "firehose_test_m_count_max",
                "firehose_test_m_count_min",
                "firehose_test_m_count_sum",
                "firehose_test_m_count_count",
            ],
            "order must not depend on JSON field order"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write series::tests`
Expected: FAIL to compile — `cannot find function to_series in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/series.rs`:

```rust
/// How far outside "now" a CloudWatch timestamp may be before we drop the sample.
///
/// Receivers reject too-far-future samples (Mimir's `creation_grace_period` defaults to
/// 10 minutes) and, once configured, too-old ones. Rejecting one bad sample here costs
/// one record; letting it reach the push costs the WHOLE flush, because a single bad
/// sample 400s the entire write request. Firehose backfill after an outage legitimately
/// delivers hours-old records, so the past window is generous.
const MAX_FUTURE_MS: i64 = 5 * 60 * 1000;
const MAX_PAST_MS: i64 = 24 * 60 * 60 * 1000;

/// Convert one CloudWatch record into zero or more remote-write series samples.
///
/// Each populated aggregate becomes its own series, suffixed `_max`/`_min`/`_sum`/`_count`.
///
/// `now_ms` is passed in rather than read from the clock so this module stays a total
/// function of its arguments — no I/O, no hidden inputs, trivially testable.
///
/// Note `MetricUnit::Unknown` is reachable only via `Default`: the enum has no
/// `#[serde(other)]`, so an unrecognised unit string fails deserialization of the whole
/// record before this is called (see issue #12).
pub fn to_series(
    metric: &CloudWatchMetric,
    now_ms: i64,
) -> anyhow::Result<Vec<(Vec<Label>, Sample)>> {
    if matches!(metric.unit, MetricUnit::Unknown) {
        warn!("skipping record with unknown unit: {} {}", metric.namespace, metric.metric_name);
        return Ok(vec![]);
    }

    let age = now_ms - metric.timestamp;
    if age < -MAX_FUTURE_MS || age > MAX_PAST_MS {
        warn!(
            "dropping {} {} with out-of-window timestamp {} (now {})",
            metric.namespace, metric.metric_name, metric.timestamp, now_ms
        );
        return Ok(vec![]);
    }

    let base = metric_base_name(metric)?;

    // Build the label set ONCE per record, not once per aggregate. Calling `labels_for`
    // four times would re-run dedup four times, and which value survives a dimension-name
    // collision depends on `HashMap` iteration order — four calls agreeing is an accident,
    // not a guarantee. If it ever stopped holding, `_max` would carry one value and `_min`
    // another, silently splitting one metric across two series with no error anywhere.
    // It also fires the dedup warning 4x per record and clones the dimension map 4x on the
    // hot ingest path.
    let base_labels = labels_for(metric, &base);

    let mut out = Vec::new();

    // Fixed order, independent of JSON field order.
    for (suffix, value) in [
        ("max", metric.value.max),
        ("min", metric.value.min),
        ("sum", metric.value.sum),
        ("count", metric.value.count),
    ] {
        let Some(value) = value else { continue };

        // NaN and +/-Inf survive the `as f64` widening. A specific NaN payload is
        // Prometheus's staleness marker, so passing one through can read as "series ended".
        if !value.is_finite() {
            warn!("dropping non-finite {suffix} for {}", metric.metric_name);
            continue;
        }

        // Clone the shared label set and retarget `__name__`. Do NOT assume it is at
        // index 0 — labels are sorted by name, and a dimension normalizing to something
        // like `__abc` would sort ahead of it.
        let mut labels = base_labels.clone();
        let name_label = labels
            .iter_mut()
            .find(|l| l.name == "__name__")
            .expect("labels_for always inserts __name__");
        name_label.value = format!("{base}_{suffix}");

        out.push((
            labels,
            Sample { value: value as f64, timestamp: metric.timestamp },
        ));
    }

    Ok(out)
}
```

**Error propagation — important for Task 10.** `to_series` is fallible only for
per-record, non-retryable conditions (an unusable namespace or metric name). The handler
must log-and-skip, never `?` out of the request: returning non-2xx makes Firehose replay
the whole batch, and one permanently-malformed record becomes an infinite poison-pill
loop. The legacy handler already gets this right at src/main.rs:126-131. Task 10 counts
these as `self_records_skipped_count`.

**Add these tests** for the timestamp window and the shared label set:

```rust
    const NOW: i64 = 1700000000000;

    #[test]
    fn timestamps_far_in_the_future_are_dropped() {
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.timestamp = NOW + 60 * 60 * 1000;
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    #[test]
    fn timestamps_far_in_the_past_are_dropped() {
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.timestamp = NOW - 48 * 60 * 60 * 1000;
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    #[test]
    fn a_zero_timestamp_is_dropped() {
        // `CloudWatchMetric` derives Default, and serde accepts 0 without complaint.
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.timestamp = 0;
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    #[test]
    fn recent_backfill_is_kept() {
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.timestamp = NOW - 2 * 60 * 60 * 1000;
        assert_eq!(to_series(&m, NOW).unwrap().len(), 1, "Firehose backfill must survive");
    }

    #[test]
    fn all_aggregates_share_one_label_set_apart_from_the_name() {
        let m = values_json(r#"{"max":1.0,"min":2.0}"#, "Count");
        let series = to_series(&m, NOW).unwrap();
        let strip = |labels: &Vec<Label>| -> Vec<(String, String)> {
            labels.iter().filter(|l| l.name != "__name__")
                .map(|l| (l.name.clone(), l.value.clone())).collect()
        };
        assert_eq!(strip(&series[0].0), strip(&series[1].0));
    }
```

**Note on `f32` → `f64` widening:** `1.1f32 as f64` is `1.100000023841858`. The legacy path
does the same `as f64`, so this is pass-through, not a regression — do not "fix" it, and
expect exact-value assertions in tests to need whole numbers.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write series::tests`
Expected: PASS, 11 tests.

- [ ] **Step 5: Commit**

```bash
git add src/series.rs
git commit -m "feat: convert CloudWatch records into remote-write samples (#2)"
```

---

### Task 4: Accumulator

**Files:**
- Create: `src/writer.rs`
- Modify: `src/main.rs` (add `mod writer;`)

- [ ] **Step 1: Write the failing test**

Create `src/writer.rs`:

```rust
use prometheus_remote_write::{Label, Sample, TimeSeries, WriteRequest};
use std::collections::{BTreeMap, HashMap};

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(name: &str) -> Vec<Label> {
        vec![Label { name: "__name__".into(), value: name.into() }]
    }

    fn samples_of(series: &TimeSeries) -> Vec<(i64, f64)> {
        series.samples.iter().map(|s| (s.timestamp, s.value)).collect()
    }

    #[test]
    fn duplicate_timestamps_keep_the_last_value() {
        let mut acc = Accumulator::new();
        acc.insert(labels("m"), Sample { value: 1.0, timestamp: 100 });
        acc.insert(labels("m"), Sample { value: 2.0, timestamp: 100 });

        let out = acc.drain();
        assert_eq!(out.len(), 1);
        assert_eq!(samples_of(&out[0]), vec![(100, 2.0)]);
    }

    #[test]
    fn samples_come_out_timestamp_ordered_even_when_inserted_backwards() {
        let mut acc = Accumulator::new();
        acc.insert(labels("m"), Sample { value: 3.0, timestamp: 300 });
        acc.insert(labels("m"), Sample { value: 1.0, timestamp: 100 });
        acc.insert(labels("m"), Sample { value: 2.0, timestamp: 200 });

        let out = acc.drain();
        assert_eq!(samples_of(&out[0]), vec![(100, 1.0), (200, 2.0), (300, 3.0)]);
    }

    #[test]
    fn distinct_label_sets_stay_distinct() {
        let mut acc = Accumulator::new();
        acc.insert(labels("a"), Sample { value: 1.0, timestamp: 100 });
        acc.insert(labels("b"), Sample { value: 2.0, timestamp: 100 });
        assert_eq!(acc.series_count(), 2);
        assert_eq!(acc.drain().len(), 2);
    }

    #[test]
    fn drain_empties_the_accumulator() {
        let mut acc = Accumulator::new();
        acc.insert(labels("m"), Sample { value: 1.0, timestamp: 100 });
        assert_eq!(acc.drain().len(), 1);
        assert_eq!(acc.series_count(), 0);
        assert!(acc.drain().is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write writer::tests`
Expected: FAIL to compile — `cannot find type Accumulator in this scope`.

Add `mod writer;` to `src/main.rs`.

- [ ] **Step 3: Write minimal implementation**

Add above the `tests` module in `src/writer.rs`:

```rust
/// Buffers samples between flushes.
///
/// `BTreeMap<i64, f64>` does two required jobs for free: inserting the same timestamp
/// twice keeps the last value (matching CloudWatch re-emitting a revised datapoint for
/// a minute as late data lands), and iteration is timestamp-ordered, which the
/// remote-write specification requires within a series.
#[derive(Default)]
pub struct Accumulator {
    series: HashMap<Vec<Label>, BTreeMap<i64, f64>>,
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, labels: Vec<Label>, sample: Sample) {
        self.series
            .entry(labels)
            .or_default()
            .insert(sample.timestamp, sample.value);
    }

    pub fn series_count(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// Take everything buffered, leaving the accumulator empty.
    pub fn drain(&mut self) -> Vec<TimeSeries> {
        std::mem::take(&mut self.series)
            .into_iter()
            .map(|(labels, samples)| TimeSeries {
                labels,
                samples: samples
                    .into_iter()
                    .map(|(timestamp, value)| Sample { value, timestamp })
                    .collect(),
            })
            .collect()
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write writer::tests`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/writer.rs src/main.rs
git commit -m "feat: accumulator with per-series dedup and ordering (#3)"
```

---

### Task 5: Config from environment

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs` (add `mod config;`)

- [ ] **Step 1: Write the failing test**

Create `src/config.rs`:

```rust
use std::env;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_when_unset() {
        let c = Config::from_values(None, None, None, None);
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
        assert_eq!(c.push_max_retries, 3);
    }

    #[test]
    fn values_are_parsed_when_present() {
        let c = Config::from_values(Some("5"), Some("10"), Some("20"), Some("7"));
        assert_eq!(c.flush_interval_secs, 5);
        assert_eq!(c.flush_max_series, 10);
        assert_eq!(c.channel_capacity, 20);
        assert_eq!(c.push_max_retries, 7);
    }

    #[test]
    fn unparseable_values_fall_back_to_defaults() {
        let c = Config::from_values(Some("banana"), None, None, None);
        assert_eq!(c.flush_interval_secs, 1);
    }

    #[test]
    fn zero_flush_interval_falls_back_to_default() {
        let c = Config::from_values(Some("0"), None, None, None);
        assert_eq!(c.flush_interval_secs, 1);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write config::tests`
Expected: FAIL to compile — `cannot find type Config in this scope`.

Add `mod config;` to `src/main.rs`.

- [ ] **Step 3: Write minimal implementation**

Add above the `tests` module in `src/config.rs`:

```rust
#[derive(Debug, Clone)]
pub struct Config {
    pub flush_interval_secs: u64,
    pub flush_max_series: usize,
    pub channel_capacity: usize,
    pub push_max_retries: u32,
}

fn parse_or<T: std::str::FromStr>(raw: Option<&str>, default: T) -> T {
    raw.and_then(|v| v.parse::<T>().ok()).unwrap_or(default)
}

impl Config {
    /// Split out from `from_env` so the parsing is testable without touching process env.
    pub fn from_values(
        flush_interval_secs: Option<&str>,
        flush_max_series: Option<&str>,
        channel_capacity: Option<&str>,
        push_max_retries: Option<&str>,
    ) -> Self {
        let flush_interval_secs = parse_or(flush_interval_secs, 1u64);
        let flush_max_series = parse_or(flush_max_series, 2000usize);
        let channel_capacity = parse_or(channel_capacity, 1024usize);

        Self {
            // A zero interval would spin the writer loop; fall back to the default.
            flush_interval_secs: if flush_interval_secs == 0 { 1 } else { flush_interval_secs },
            flush_max_series: if flush_max_series == 0 { 2000 } else { flush_max_series },
            channel_capacity: if channel_capacity == 0 { 1024 } else { channel_capacity },
            push_max_retries: parse_or(push_max_retries, 3u32),
        }
    }

    pub fn from_env() -> Self {
        Self::from_values(
            env::var("FLUSH_INTERVAL_SECS").ok().as_deref(),
            env::var("FLUSH_MAX_SERIES").ok().as_deref(),
            env::var("CHANNEL_CAPACITY").ok().as_deref(),
            env::var("PUSH_MAX_RETRIES").ok().as_deref(),
        )
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write config::tests`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/main.rs
git commit -m "feat: env-derived writer configuration (#3)"
```

---

### Task 6: Push with bounded retry

**Files:**
- Modify: `src/writer.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/writer.rs`:

```rust
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn push_succeeds_on_first_attempt() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(
            vec![1, 2, 3],
            3,
            move |_body| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(200u16)
                }
            },
        )
        .await;

        assert_eq!(outcome, PushOutcome::Delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transport_failure_is_retried_to_the_bound_then_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(
            vec![1],
            3,
            move |_body| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("connection refused"))
                }
            },
        )
        .await;

        assert_eq!(outcome, PushOutcome::Dropped);
        assert_eq!(calls.load(Ordering::SeqCst), 3, "should attempt exactly max_retries times");
    }

    #[tokio::test]
    async fn bad_request_is_dropped_without_retrying() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(
            vec![1],
            3,
            move |_body| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(400u16)
                }
            },
        )
        .await;

        assert_eq!(outcome, PushOutcome::Dropped);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "400 must not be retried");
    }

    #[tokio::test]
    async fn server_error_is_retried_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(
            vec![1],
            3,
            move |_body| {
                let c = c.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::SeqCst);
                    if n == 0 { Ok(503u16) } else { Ok(200u16) }
                }
            },
        )
        .await;

        assert_eq!(outcome, PushOutcome::Delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write writer::tests`
Expected: FAIL to compile — `cannot find function push_with_retry` / `cannot find type PushOutcome`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/writer.rs`:

```rust
use std::future::Future;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Delivered,
    Dropped,
}

/// Send a body, retrying transport errors and 5xx with exponential backoff.
///
/// A 400 is never retried: the remote-write endpoint rejected the content, and
/// resending identical bytes cannot succeed.
///
/// Generic over the send closure rather than a trait so the retry policy is testable
/// without an HTTP stack or a mocking dependency.
pub async fn push_with_retry<F, Fut>(body: Vec<u8>, max_retries: u32, mut send: F) -> PushOutcome
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = anyhow::Result<u16>>,
{
    let attempts = max_retries.max(1);
    for attempt in 0..attempts {
        match send(body.clone()).await {
            Ok(status) if (200..300).contains(&status) => return PushOutcome::Delivered,
            Ok(400) => {
                error!("remote write rejected the batch with 400; dropping without retry");
                return PushOutcome::Dropped;
            }
            Ok(status) => warn!("remote write returned {status} (attempt {})", attempt + 1),
            Err(e) => warn!("remote write failed: {e} (attempt {})", attempt + 1),
        }

        if attempt + 1 < attempts {
            // 100ms, 200ms, 400ms, ...
            tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(attempt))).await;
        }
    }

    error!("giving up on batch after {attempts} attempts");
    PushOutcome::Dropped
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write writer::tests`
Expected: PASS, 8 tests.

- [ ] **Step 5: Commit**

```bash
git add src/writer.rs
git commit -m "feat: bounded retry policy for remote-write pushes (#3)"
```

---

### Task 7: Writer self-metrics and instance label

**Files:**
- Modify: `src/prometheus.rs`

- [ ] **Step 1: Write the failing test**

Add a `tests` module at the end of `src/prometheus.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_label_falls_back_when_hostname_unset() {
        assert_eq!(instance_label_value(None), "unknown");
    }

    #[test]
    fn instance_label_uses_hostname_when_set() {
        assert_eq!(instance_label_value(Some("pod-abc123".into())), "pod-abc123");
    }

    #[test]
    fn empty_hostname_falls_back() {
        assert_eq!(instance_label_value(Some(String::new())), "unknown");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write prometheus::tests`
Expected: FAIL to compile — `cannot find function instance_label_value`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/prometheus.rs`:

```rust
/// Resolve the value for the `instance` label on app self-metrics.
///
/// This applies to self-metrics only. CloudWatch series must NOT carry a per-replica
/// label: the same CloudWatch datapoint delivered to two replicas must remain one
/// series, or we manufacture the duplicate-series problem we are trying to remove.
pub fn instance_label_value(hostname: Option<String>) -> String {
    match hostname {
        Some(h) if !h.is_empty() => h,
        _ => String::from("unknown"),
    }
}

pub fn instance_label() -> String {
    instance_label_value(std::env::var("HOSTNAME").ok())
}
```

Add these new self-metric statics inside the existing `lazy_static!` block:

```rust
    pub static ref RECORDS_SKIPPED: CounterVec = register_counter_vec!(
        app_opts!("self_records_skipped_count", "Malformed records dropped during parse"),
        &["instance"]
    )
    .unwrap();
    pub static ref BATCHES_DROPPED: CounterVec = register_counter_vec!(
        app_opts!("self_batches_dropped_count", "Batches abandoned after exhausting retries"),
        &["instance"]
    )
    .unwrap();
    pub static ref REJECTED_PAYLOADS: CounterVec = register_counter_vec!(
        app_opts!("self_rejected_payloads_count", "Payloads rejected because the buffer was full"),
        &["instance"]
    )
    .unwrap();
    pub static ref BUFFER_SERIES: GaugeVec = register_gauge_vec!(
        app_opts!("self_buffer_series", "Series currently buffered awaiting flush"),
        &["instance"]
    )
    .unwrap();
    pub static ref FLUSH_DURATION: GaugeVec = register_gauge_vec!(
        app_opts!("self_flush_duration_seconds", "Duration of the most recent flush"),
        &["instance"]
    )
    .unwrap();
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write prometheus::tests`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/prometheus.rs
git commit -m "feat: writer self-metrics and instance label (#8)"
```

---

### Task 8: Flush — merge self-metrics and build the request

**Files:**
- Modify: `src/writer.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/writer.rs`:

```rust
    #[test]
    fn build_request_carries_accumulated_series() {
        let mut acc = Accumulator::new();
        acc.insert(labels("a"), Sample { value: 1.0, timestamp: 100 });
        acc.insert(labels("b"), Sample { value: 2.0, timestamp: 100 });

        let req = build_request(acc.drain(), vec![]);
        assert_eq!(req.timeseries.len(), 2);
    }

    #[test]
    fn build_request_merges_self_metrics_into_the_same_payload() {
        let mut acc = Accumulator::new();
        acc.insert(labels("cloudwatch_series"), Sample { value: 1.0, timestamp: 100 });

        let self_series = vec![TimeSeries {
            labels: labels("firehose_self_metric"),
            samples: vec![Sample { value: 9.0, timestamp: 100 }],
        }];

        let req = build_request(acc.drain(), self_series);
        assert_eq!(req.timeseries.len(), 2, "one HTTP call carries both");
    }

    #[test]
    fn build_request_sorts_labels_within_each_series() {
        let unsorted = vec![
            Label { name: "zzz".into(), value: "1".into() },
            Label { name: "aaa".into(), value: "2".into() },
        ];
        let req = build_request(
            vec![TimeSeries { labels: unsorted, samples: vec![Sample { value: 1.0, timestamp: 1 }] }],
            vec![],
        );
        let names: Vec<&str> = req.timeseries[0].labels.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["aaa", "zzz"]);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write writer::tests`
Expected: FAIL to compile — `cannot find function build_request`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/writer.rs`:

```rust
use crate::prometheus::instance_label;
use prometheus::TextEncoder;

/// Combine CloudWatch series and app self-metrics into one request.
///
/// `sorted()` enforces the remote-write requirement that labels are sorted by name and
/// samples by timestamp within each series.
pub fn build_request(cloudwatch: Vec<TimeSeries>, self_metrics: Vec<TimeSeries>) -> WriteRequest {
    let mut timeseries = cloudwatch;
    timeseries.extend(self_metrics);
    WriteRequest { timeseries }.sorted()
}

/// Gather app self-metrics from the client registry and convert them to series.
///
/// Self-metrics legitimately belong on the client registry; only the CloudWatch path
/// needed to move off it. Returns an empty vec on failure rather than losing the
/// CloudWatch data that shares this flush.
pub fn self_metric_series() -> Vec<TimeSeries> {
    let families = prometheus::gather();
    let text = match TextEncoder::new().encode_to_string(&families) {
        Ok(t) => t,
        Err(e) => {
            error!("could not encode self-metrics: {e}");
            return vec![];
        }
    };
    match WriteRequest::from_text_format(text) {
        Ok(req) => req.timeseries,
        Err(e) => {
            error!("could not convert self-metrics: {e}");
            vec![]
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write writer::tests`
Expected: PASS, 11 tests.

- [ ] **Step 5: Commit**

```bash
git add src/writer.rs
git commit -m "feat: build write requests merging self-metrics (#3, #8)"
```

---

### Task 9: The writer task loop

**Files:**
- Modify: `src/writer.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/writer.rs`:

```rust
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn writer_flushes_when_series_threshold_is_reached() {
        let (tx, rx) = mpsc::channel(16);
        let flushed = Arc::new(AtomicUsize::new(0));
        let f = flushed.clone();

        let config = crate::config::Config::from_values(Some("3600"), Some("2"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, move |body| {
            let f = f.clone();
            async move {
                assert!(!body.is_empty(), "flush should never push an empty body");
                f.fetch_add(1, Ordering::SeqCst);
                Ok(200u16)
            }
        }));

        // Two distinct series hits flush_max_series = 2 without waiting for the tick.
        tx.send(vec![(labels("a"), Sample { value: 1.0, timestamp: 1 })]).await.unwrap();
        tx.send(vec![(labels("b"), Sample { value: 1.0, timestamp: 1 })]).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            while flushed.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("writer should flush once the series threshold is reached");

        drop(tx);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn writer_flushes_remaining_series_on_shutdown() {
        let (tx, rx) = mpsc::channel(16);
        let flushed = Arc::new(AtomicUsize::new(0));
        let f = flushed.clone();

        // Huge interval and threshold: only the shutdown path can flush this.
        let config = crate::config::Config::from_values(Some("3600"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, move |_body| {
            let f = f.clone();
            async move {
                f.fetch_add(1, Ordering::SeqCst);
                Ok(200u16)
            }
        }));

        tx.send(vec![(labels("a"), Sample { value: 1.0, timestamp: 1 })]).await.unwrap();
        drop(tx);
        handle.await.unwrap();

        assert_eq!(flushed.load(Ordering::SeqCst), 1, "pending series must not be lost on shutdown");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write writer::tests`
Expected: FAIL to compile — `cannot find function run_writer`.

- [ ] **Step 3: Write minimal implementation**

Add to `src/writer.rs`:

```rust
use crate::config::Config;
use crate::prometheus::{BATCHES_DROPPED, BUFFER_SERIES, FLUSH_DURATION};
use tokio::sync::mpsc::Receiver;

pub type SeriesBatch = Vec<(Vec<Label>, Sample)>;

/// Own the accumulator and be the only thing that ever pushes.
///
/// Generic over the send closure so tests can drive it without an HTTP stack.
/// Exits when the channel closes, flushing anything still buffered.
pub async fn run_writer<F, Fut>(mut rx: Receiver<SeriesBatch>, config: Config, mut send: F)
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = anyhow::Result<u16>>,
{
    let instance = instance_label();
    let mut acc = Accumulator::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(config.flush_interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let should_flush = tokio::select! {
            received = rx.recv() => match received {
                Some(batch) => {
                    for (labels, sample) in batch {
                        acc.insert(labels, sample);
                    }
                    BUFFER_SERIES.with_label_values(&[&instance]).set(acc.series_count() as f64);
                    acc.series_count() >= config.flush_max_series
                }
                // Channel closed: flush what is left, then stop.
                None => {
                    flush(&mut acc, &config, &instance, &mut send).await;
                    return;
                }
            },
            _ = ticker.tick() => true,
        };

        if should_flush {
            flush(&mut acc, &config, &instance, &mut send).await;
        }
    }
}

async fn flush<F, Fut>(acc: &mut Accumulator, config: &Config, instance: &str, send: &mut F)
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = anyhow::Result<u16>>,
{
    if acc.is_empty() {
        return;
    }

    let started = tokio::time::Instant::now();
    let request = build_request(acc.drain(), self_metric_series());

    let body = match request.encode_compressed() {
        Ok(b) => b,
        Err(e) => {
            error!("could not encode write request: {e}");
            BATCHES_DROPPED.with_label_values(&[instance]).inc();
            return;
        }
    };

    // `&mut F` implements `FnMut` when `F: FnMut`, so reborrowing satisfies
    // `push_with_retry`'s by-value parameter without giving up ownership of `send`.
    if push_with_retry(body, config.push_max_retries, &mut *send).await == PushOutcome::Dropped {
        BATCHES_DROPPED.with_label_values(&[instance]).inc();
    }

    BUFFER_SERIES.with_label_values(&[instance]).set(0.0);
    FLUSH_DURATION
        .with_label_values(&[instance])
        .set(started.elapsed().as_secs_f64());
}
```

**Note on the first tick:** `tokio::time::interval` completes its first tick immediately, so
`run_writer` evaluates `should_flush = true` on the very first loop iteration. `flush`
returns early when the accumulator is empty, so this is harmless — but do not mistake it for
the interval having elapsed when reading the tests.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write writer::tests`
Expected: PASS, 13 tests.

- [ ] **Step 5: Commit**

```bash
git add src/writer.rs
git commit -m "feat: single writer task owning flush and push (#3)"
```

---

### Task 10: Handler rewrite — no panics, enqueue and respond

**Files:**
- Modify: `src/main.rs`, `src/structs.rs`

- [ ] **Step 1: Write the failing test**

Replace the existing `test_convert_to_labels_values` in `src/main.rs` (it reads
`testdata/just-post-payload.json`, which was never committed, and has been failing since
before this work) with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use base64::prelude::*;

    fn record(payload: &str) -> String {
        BASE64_STANDARD.encode(payload)
    }

    #[test]
    fn decode_payloads_skips_malformed_base64_and_keeps_going() {
        let records = vec![
            FirehoseData { data: record("first\n") },
            FirehoseData { data: String::from("!!!not base64!!!") },
            FirehoseData { data: record("third\n") },
        ];
        let decoded = decode_payloads(records);
        assert!(decoded.contains("first"));
        assert!(decoded.contains("third"));
    }

    #[test]
    fn decode_payloads_skips_invalid_utf8() {
        let records = vec![
            FirehoseData { data: BASE64_STANDARD.encode([0xff, 0xfe, 0xfd]) },
            FirehoseData { data: record("good\n") },
        ];
        assert!(decode_payloads(records).contains("good"));
    }

    #[test]
    fn parse_lines_skips_malformed_json_and_returns_valid_series() {
        let text = concat!(
            "{not json}\n",
            r#"{"metric_stream_name":"s","account_id":"1","region":"us-east-1","#,
            r#""namespace":"AWS/Test","metric_name":"M","dimensions":{},"#,
            r#""timestamp":1700000000000,"value":{"max":1.0},"unit":"Count"}"#,
            "\n",
        );
        let series = parse_lines(text, 1700000000000);
        assert_eq!(series.len(), 1);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin firehose_remote_write tests::`
Expected: FAIL to compile — `decode_payloads` currently returns
`Result<String, Box<dyn Error>>` and is `async`; `parse_lines` does not exist.

- [ ] **Step 3: Write minimal implementation**

In `src/structs.rs`, replace the `SharedState`/`AppState` definitions:

```rust
use prometheus_remote_write::{Label, Sample};
use tokio::sync::mpsc::Sender;

#[derive(Clone)]
pub struct AppState {
    pub firehose_arns: Arc<RwLock<HashSet<String>>>,
    pub tx: Sender<Vec<(Vec<Label>, Sample)>>,
}
```

Delete `pub type SharedState = Arc<RwLock<AppState>>;` and the `#[derive(Default)]` on
`AppState`.

In `src/main.rs`, replace `decode_payloads` and add `parse_lines`:

```rust
/// Decode Firehose records, skipping any that are malformed.
///
/// A single bad record must not fail the batch: returning non-2xx makes Firehose replay
/// everything, including the records that were already accepted.
fn decode_payloads(records: Vec<FirehoseData>) -> String {
    let mut out = String::new();
    for record in records {
        let bytes = match BASE64_STANDARD.decode(&record.data) {
            Ok(b) => b,
            Err(e) => {
                debug!("skipping record with invalid base64: {e}");
                RECORDS_SKIPPED.with_label_values(&[&instance_label()]).inc();
                continue;
            }
        };
        match String::from_utf8(bytes) {
            Ok(s) => out.push_str(&s),
            Err(e) => {
                debug!("skipping record with invalid utf-8: {e}");
                RECORDS_SKIPPED.with_label_values(&[&instance_label()]).inc();
            }
        }
    }
    out
}

/// Parse newline-delimited CloudWatch metric JSON into remote-write series.
///
/// `now_ms` is read once for the whole batch and threaded through, so every record in
/// one payload is judged against the same clock reading.
fn parse_lines(text: &str, now_ms: i64) -> Vec<(Vec<Label>, Sample)> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let metric: CloudWatchMetric = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                debug!("skipping unparseable line: {e}");
                RECORDS_SKIPPED.with_label_values(&[&instance_label()]).inc();
                continue;
            }
        };
        match crate::series::to_series(&metric, now_ms) {
            Ok(series) => out.extend(series),
            Err(e) => {
                debug!("skipping record: {e}");
                RECORDS_SKIPPED.with_label_values(&[&instance_label()]).inc();
            }
        }
    }
    out
}
```

Replace the handler body:

```rust
#[debug_handler]
async fn get_firehose(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Firehose>,
) -> Result<Json<FirehoseResponse>, (StatusCode, Json<FirehoseResponse>)> {
    let request_id = payload.request_id.clone().unwrap_or_default();

    // A non-ASCII header value is not a reason to fail the batch.
    let source_arn = headers
        .get("X-Amz-Firehose-Source-Arn")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or(payload.source_arn);

    match source_arn {
        Some(arn) => {
            state.firehose_arns.write().await.insert(arn);
        }
        None => warn!("no source arn in headers or payload for this request"),
    }

    let mut text = payload.records.map(decode_payloads).unwrap_or_default();
    if let Some(message) = payload.message {
        text = message;
    }

    // Read the clock once per batch so every record is judged against the same reading.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let series = parse_lines(&text, now_ms);
    STREAMS_RECEIVED.with_label_values(&[]).inc();

    if series.is_empty() {
        return Ok(Json(FirehoseResponse::ok(request_id)));
    }

    // A full buffer is the one case where we genuinely cannot accept the payload.
    // Returning non-2xx hands durability back to Firehose's at-least-once replay.
    match state.tx.try_send(series) {
        Ok(()) => Ok(Json(FirehoseResponse::ok(request_id))),
        Err(e) => {
            REJECTED_PAYLOADS.with_label_values(&[&instance_label()]).inc();
            warn!("buffer full, rejecting payload so Firehose retries: {e}");
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(FirehoseResponse::error(request_id, "buffer full".into())),
            ))
        }
    }
}
```

In `src/structs.rs`, add constructors so the timestamp logic is not repeated:

```rust
impl FirehoseResponse {
    /// Seconds, matching the units the handler has always returned.
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn ok(request_id: String) -> Self {
        Self { request_id, timestamp: Self::now_secs(), error_message: None }
    }

    pub fn error(request_id: String, message: String) -> Self {
        Self { request_id, timestamp: Self::now_secs(), error_message: Some(message) }
    }
}
```

`src/main.rs` will need these imports added:

```rust
use crate::config::Config;
use crate::prometheus::{instance_label, RECORDS_SKIPPED, REJECTED_PAYLOADS};
use crate::writer::run_writer;
use prometheus_remote_write::{Label, Sample};
use std::collections::HashSet;
```
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin firehose_remote_write tests::`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/structs.rs
git commit -m "fix: never panic on malformed input, return 200 once buffered (#9)"
```

---

### Task 11: Wire up main() and delete dead code

**Files:**
- Modify: `src/main.rs`, `src/prometheus.rs`, `src/aws/mod.rs`

- [ ] **Step 1: Wire the writer into main()**

Replace the setup in `main()`:

```rust
    let config = Config::from_env();
    let addr = env::var("PROM_WRITE_ADDR").expect("Can't push without PROM_WRITE_ADDR defined");
    let url = format!("{addr}/api/v1/write");
    let (tx, rx) = tokio::sync::mpsc::channel(config.channel_capacity);

    let state = AppState {
        firehose_arns: Arc::new(RwLock::new(HashSet::new())),
        tx,
    };

    let client = reqwest::Client::new();
    tokio::spawn(run_writer(rx, config.clone(), move |body| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let rs = client.post(url).body(body).send().await?;
            let status = rs.status().as_u16();
            TOTAL_WRITES_SENT.with_label_values(&[&status.to_string()]).inc();
            if status == 400 {
                error!("400 from remote write: {}", rs.text().await.unwrap_or_default());
            }
            Ok(status)
        }
    }));

    let app = Router::new()
        .route("/", post(get_firehose).put(get_firehose))
        .with_state(state.clone());
```

Update the freshness task to use `state.firehose_arns.read().await.clone()`.

Remove the unreachable `loop {}` after `axum::serve(...).await.unwrap();`.

- [ ] **Step 2: Delete dead code**

From `src/prometheus.rs` delete: `GAUGES`, `COUNTERS`, `HISTOGRAMS`, the `GaugeHash`,
`CounterHash`, `HistoHash`, `DimensionHash` type aliases, `DIMENSION_HASH`,
`clear_collectors`, `get_or_register_metric`, `record_metric`, `record_aggregate`,
`sanitize_metric_name`, `push_firehose_metrics`, and the now-unused
`prometheus::tests` cardinality tests (the code they guard is gone).

From `src/aws/mod.rs` delete: `get_dimensions`, `fetch_dimension_names`,
`normalize_dimension_name`, and the `aws::tests` module testing pagination. Keep
`AWSState` and `get_freshness`.

`COUNTERS` and `HISTOGRAMS` were only ever declared and cleared, never populated.

Deleting `push_firehose_metrics` also removes the only reads of `PROM_USERNAME` and
`PROM_PASSWORD`. They were read into locals and never applied to the request, so nothing
changes behaviourally — but the env vars become entirely unreferenced. Leave them
undocumented rather than pretending they work; wiring up basic auth deserves its own issue.

- [ ] **Step 3: Verify the whole suite**

Run: `cargo build --tests 2>&1 | grep -E "^(error|warning: unused)"`
Expected: no errors. Remove any imports the deletions orphaned.

Run: `cargo test --bin firehose_remote_write`
Expected: PASS, all tests, zero failures. The previously-failing
`test_convert_to_labels_values` is gone, so the suite should now be fully green for the
first time.

Run: `cargo clippy --bin firehose_remote_write 2>&1 | grep -E "^error"`
Expected: no output.

Run: `cargo fmt --check`
Expected: no diffs in `src/series.rs`, `src/writer.rs`, `src/config.rs`. Pre-existing
diffs elsewhere are acceptable but prefer fixing files you rewrote.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor: wire up writer task and delete the registry data path (#2, #3)"
```

---

### Task 12: Update the issues and open the PR

- [ ] **Step 1: Push and open the PR**

```bash
git push -u origin feat/2-direct-timeseries-writer
gh pr create --repo PeterGrace/firehose_remote_write --base main \
  --title "feat: direct TimeSeries construction and a single writer task" \
  --body "Closes #2. Closes #3. Closes #8. Closes #9."
```

The PR body should additionally record:
- The empty-label-equivalence finding that justifies deleting `get_dimensions`.
- That durability moved off Firehose and onto bounded retry, and what the new
  `self_batches_dropped_count` metric means operationally.
- That #4 (cross-batch high-water mark) and #5 (reorder buffer) remain open and are now
  straightforward to add inside `flush`.
- That the pagination added in #6 is deleted along with `get_dimensions`, as anticipated
  in that issue.

---

## Verification checklist

- [ ] Every CloudWatch series name still begins `firehose_` (the prefix that used to come from `app_opts!`)
- [ ] Rollup and specific records still produce distinct series
- [ ] A batch with the same series at two timestamps emits two samples, not one
- [ ] A batch with the same series+timestamp twice emits one sample, last value winning
- [ ] Malformed base64, UTF-8, JSON and missing `requestId` do not panic
- [ ] A full channel returns 503, not 200
- [ ] Self-metrics carry an `instance` label; CloudWatch series do not
- [ ] `cargo test` fully green
