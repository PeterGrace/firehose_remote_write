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

/// Reserved label names we set ourselves. A dimension normalizing onto one of these is
/// prefixed rather than emitted as-is.
///
/// What this actually buys is **data preservation**, not protection of our own labels. Our
/// four are pushed first and `sort_by` is stable, so on a tie they are already the `dedup_by`
/// survivor — a colliding dimension could never clobber them. Without this check the
/// dimension's *value* would simply be discarded; with it, the value survives under a
/// `dimension_`-prefixed name.
///
/// Reachability differs per entry, which matters when reading the tests:
/// - `account_id` and `metric_stream_name` are reachable end-to-end (`AccountId`,
///   `MetricStreamName`); this check is the only thing keeping their values.
/// - `region` is unreachable through `labels_for`, because
///   `DimensionMap::to_labels_values` already remaps it. The redundancy is deliberate, so
///   it is pinned by a direct unit test rather than a whole-pipeline one.
/// - `__name__` is unreachable *because `label_name_for_dimension` trims leading and
///   trailing underscores* — nothing else prevents it. Before that trim existed, `!!name!!`
///   reached here and came out as `dimension___name__`. Kept as defence in depth precisely
///   because it is one edit to the normalization away from being reachable again.
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
    // Splitting on every non-alphanumeric run and rejoining with a single `_` does three
    // jobs at once: replaces invalid characters, collapses runs, and trims the ends.
    //
    // The trimming is not cosmetic, and naive per-character replacement is not enough.
    // `to_case(Case::Snake)` treats underscores as word boundaries but *not* punctuation, so
    // punctuation survives it and only becomes `_` afterwards. Per-character replacement
    // therefore let `!!name!!` through as `__name__`, and — because a leading `__` is the
    // reserved namespace — `//replica//` as `__replica__` and `**tenant_id**` as
    // `__tenant_id__`. Those are Thanos's and Mimir's own internal labels. They are valid per
    // the label-name grammar, so nothing downstream rejects them; they simply collide with
    // system labels. `RESERVED_LABELS` cannot catch that, because these are not names we set.
    //
    // Accepted tradeoff: `!!name!!` and `name` now normalize onto the same label and one gets
    // discarded. That is the ordinary dedup path below, which logs, and is strictly better
    // than emitting a label in the reserved namespace.
    let cleaned = name
        .to_case(Case::Snake)
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");

    // Exactly correct now that the ends are trimmed: an all-punctuation name like `!!!` has
    // nothing left to join and lands here rather than escaping as a run of underscores.
    if cleaned.is_empty() {
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

    // An empty label value is equivalent to the label being absent in the Prometheus data
    // model, so `{account_id=""}` and `{}` are ONE stored series but TWO distinct
    // `Vec<Label>` values — and `Vec<Label>` is exactly what the writer's accumulator keys
    // on. Two keys landing on one stored series is how duplicate timestamps and out-of-order
    // samples get reintroduced, which is the entire defect this rewrite removes.
    //
    // The dimension loop above already drops empties; the four labels we set ourselves were
    // pushed unconditionally and did not. `serde` accepts `""` for a `String` field without
    // complaint, so `{"account_id":""}` is one malformed record away — pinned by
    // `empty_base_label_values_are_dropped_like_dimensions`, which failed before this line
    // existed. Dropping uniformly here is what makes the key isomorphic to the stored
    // identity by construction rather than by coincidence.
    //
    // `__name__` is deliberately exempt: an empty metric name is an ERROR, not a label to
    // drop. `metric_base_name` already bails on a name that sanitizes to empty, so this is
    // unreachable from `to_series` — but dropping it here would silently produce a nameless
    // series nothing rejects and no query finds, and would break the
    // `expect("labels_for always inserts __name__")` in `to_series`. Keeping it makes such a
    // record fail loudly at the receiver instead.
    //
    // The dimension-level check is NOT made redundant by this one: it also suppresses a
    // misleading "unusable name" warning for empty-valued dimensions with bad names.
    labels.retain(|l| l.name == "__name__" || !l.value.is_empty());

    // Do NOT delete this as redundant with the encoder. `WriteRequest::encode_compressed`
    // does sort labels itself, so the wire format is satisfied either way — but this sort is
    // load-bearing for two things the library does not do:
    //
    //  1. `dedup_by` below only removes *adjacent* duplicates, and the library does not
    //     dedup at all. Unsorted, colliding names slip through and the receiver 400s the
    //     whole batch.
    //  2. Task 4 keys its batch accumulator on `Vec<Label>`, where element order determines
    //     map identity. Unsorted, `DimensionMap`'s nondeterministic `HashMap` order splits
    //     one logical series across several accumulator entries.
    //
    // Removing it keeps almost every test green while silently breaking both.
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
/// `MetricUnit` has `#[serde(other)]` on `Unknown`, so an unrecognised unit string reaches
/// here as `Unknown` rather than failing deserialization of the whole record (issue #12).
pub fn to_series(
    metric: &CloudWatchMetric,
    now_ms: i64,
) -> anyhow::Result<Vec<(Vec<Label>, Sample)>> {
    if matches!(metric.unit, MetricUnit::Unknown) {
        warn!(
            "skipping record with unknown unit: {} {}",
            metric.namespace, metric.metric_name
        );
        return Ok(vec![]);
    }

    // `saturating_sub`, not `-`. `timestamp` is a plain `i64` straight off the wire, so
    // `i64::MIN` is one malformed record away: the plain subtraction panics on it in a debug
    // build and, in release, wraps to a small age that sails through the window check — the
    // guard delivering precisely the outcome it exists to prevent. Saturating pins both
    // extremes outside the window, where they belong.
    //
    // Read the range as the two bounds it is: a negative age is a future timestamp, so the
    // window runs from `-MAX_FUTURE_MS` (5 minutes ahead) to `MAX_PAST_MS` (24 hours behind).
    let age = now_ms.saturating_sub(metric.timestamp);
    if !(-MAX_FUTURE_MS..=MAX_PAST_MS).contains(&age) {
        warn!(
            "dropping {} {} with out-of-window timestamp {} (now {})",
            metric.namespace, metric.metric_name, metric.timestamp, now_ms
        );
        return Ok(vec![]);
    }

    let base = metric_base_name(metric)?;

    // Build the label set ONCE per record, not once per aggregate.
    //
    // The tempting argument for this — that four `labels_for` calls could disagree about
    // which value survives a dimension-name collision — does not hold, and is worth writing
    // down as *not* holding so nobody re-derives it as a reason to change something else.
    // `to_labels_values` clones the map, and cloning a `HashMap` preserves iteration order,
    // so four calls in one process agree. The survivor varies between runs, never within one.
    //
    // The reasons that do hold: four calls fire the dedup warning four times for one dropped
    // dimension, which tells an operator something false about how much data was lost (pinned
    // by `the_label_set_is_built_once_per_record_not_once_per_aggregate`), and they clone and
    // re-sort the dimension map four times on the hot ingest path for an identical result.
    // Building once also stops the agreement being load-bearing at all.
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

        // Clone the shared label set and retarget `__name__`. Do NOT assume it is at index 0.
        //
        // Being precise about why, because the obvious justification is currently false:
        // nothing `label_name_for_dimension` can emit today sorts ahead of `__name__`. Every
        // dimension-derived name starts with an ASCII lowercase letter — the ends are trimmed,
        // so no leading `_`, and a leading digit is prefixed with `d_` — and `_` (0x5F) sorts
        // below every lowercase letter. `labels[0]` would therefore work, by coincidence, and
        // swapping this `find` for it kills no test.
        //
        // The coincidence is one edit away from ending. Before the trim existed, `!!abc`
        // normalized to `__abc`, which does sort ahead. Under `labels[0]` that record would
        // have had its dimension value overwritten with the metric name and been shipped with
        // no `__name__` — silent, and not something the receiver would reject in a way that
        // points back here. `find` costs a linear scan of five-ish labels and removes the
        // coupling entirely. `no_dimension_derived_label_sorts_before_the_name_label` watches
        // the precondition, since no test can watch this line directly.
        let mut labels = base_labels.clone();
        let name_label = labels
            .iter_mut()
            .find(|l| l.name == "__name__")
            .expect("labels_for always inserts __name__");
        name_label.value = format!("{base}_{suffix}");

        out.push((
            labels,
            Sample {
                value: value as f64,
                timestamp: metric.timestamp,
            },
        ));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testlog::{captured_logs, log_sink};

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

    /// Documents why the `__name__` entry of `RESERVED_LABELS` kills no mutant — and it is
    /// *only* the trim in `label_name_for_dimension` that makes it so, not snake-casing.
    ///
    /// This was originally recorded as "provably unreachable" on the strength of a surviving
    /// mutant. That inference was wrong, and expensively so: a surviving mutant means nothing
    /// is watching the behaviour, never that the behaviour cannot occur. Punctuation is not a
    /// word boundary for `to_case`, so `!!name!!` survived it intact and became `__name__` in
    /// the replacement step — see the sibling test below, which is the assertion that was
    /// missing. Delete the trim and this entry becomes live again.
    #[test]
    fn underscore_wrapped_dimension_names_lose_their_underscores() {
        assert_eq!(
            label_name_for_dimension("__name__").as_deref(),
            Some("name")
        );
    }

    /// The regression that a surviving mutant hid.
    ///
    /// `to_case(Case::Snake)` uses underscores as word boundaries but passes punctuation
    /// straight through, so before the trim `!!name!!` reached `RESERVED_LABELS` as
    /// `__name__` and was emitted as `dimension___name__`.
    #[test]
    fn punctuation_wrapped_names_do_not_reach_the_reserved_namespace() {
        assert_eq!(
            label_name_for_dimension("!!name!!").as_deref(),
            Some("name")
        );
        assert_eq!(
            label_name_for_dimension("..name..").as_deref(),
            Some("name")
        );
        for input in ["!!name!!", "..name..", "!!abc", "%%foo"] {
            let got = label_name_for_dimension(input).expect("should salvage a name");
            assert!(
                !got.starts_with("__"),
                "{input:?} produced {got:?}, which is in the reserved `__` namespace"
            );
        }
    }

    /// `__`-prefixed names are valid per the label-name grammar, so nothing downstream
    /// rejects them — they just collide with system labels. This deployment runs Thanos
    /// (`__replica__`); Mimir uses `__tenant_id__`. `RESERVED_LABELS` cannot catch these
    /// because they are not names we set, so the trim is the only thing preventing them.
    #[test]
    fn dimension_names_cannot_collide_with_system_reserved_labels() {
        assert_eq!(
            label_name_for_dimension("//replica//").as_deref(),
            Some("replica")
        );
        assert_eq!(
            label_name_for_dimension("**tenant_id**").as_deref(),
            Some("tenant_id")
        );
    }

    /// No emitted label may sit in the `__` namespace, checked end-to-end rather than on the
    /// helper, since that is what actually reaches the wire. `__name__` is ours and exempt.
    #[test]
    fn no_emitted_label_is_in_the_reserved_underscore_namespace() {
        let labels = labels_for(&with_dims(r#"{"!!abc":"1","//replica//":"2"}"#), "m");
        for label in &labels {
            assert!(
                label.name == "__name__" || !label.name.starts_with("__"),
                "label {:?} is in the reserved `__` namespace, got {:?}",
                label.name,
                label_pairs(&labels)
            );
        }
        assert!(labels.iter().any(|l| l.name == "abc" && l.value == "1"));
        assert!(labels.iter().any(|l| l.name == "replica" && l.value == "2"));
    }

    /// Regression guard for the trim: it must not disturb ordinary names.
    ///
    /// Deliberately broad — a characterization test over the whole normalizer, since its
    /// output is a wire contract and *any* drift matters. It therefore fails alongside the
    /// specific guards' own tests when one of them is broken. That overlap is intended, not a
    /// discrimination failure: the narrow tests localize the fault, this one notices drift the
    /// narrow tests were never pointed at.
    #[test]
    fn trimming_does_not_alter_well_formed_names() {
        assert_eq!(
            label_name_for_dimension("Some-Weird.Name").as_deref(),
            Some("some_weird_name")
        );
        assert_eq!(
            label_name_for_dimension("InstanceId").as_deref(),
            Some("instance_id")
        );
        assert_eq!(
            label_name_for_dimension("5xxCode").as_deref(),
            Some("d_5_xx_code")
        );
        assert_eq!(label_name_for_dimension("!!!"), None);
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
    /// Sanitizing replaces rather than deletes, so every all-punctuation dimension name
    /// reduces to nothing once the ends are trimmed. Before the trim they became `___` — a
    /// technically valid Prometheus label name that nothing downstream would reject
    /// individually, but three of them in one record is a duplicate label name, which gets
    /// the entire write request rejected and takes every good sample batched alongside it.
    /// Dropping is the only safe answer.
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

    /// The three labels we set from record fields are pushed unconditionally, so unlike
    /// dimensions they had no empty-value check. An empty value is equivalent to an absent
    /// label in the Prometheus data model, so emitting one makes the `Vec<Label>` the
    /// accumulator keys on no longer isomorphic to the stored series identity — which is the
    /// exact class of defect (duplicate timestamps, out-of-order samples) this rewrite
    /// exists to remove. `""` for these fields is one malformed record away: `serde` accepts
    /// an empty string for a `String` field without complaint.
    #[test]
    fn empty_base_label_values_are_dropped_like_dimensions() {
        let m = metric_from(
            r#"{"metric_stream_name":"","account_id":"","region":"",
                "namespace":"AWS/ApplicationELB","metric_name":"RequestCount",
                "dimensions":{"TargetGroup":"tg/bar"},"timestamp":1700000000000,
                "value":{"max":1.0},"unit":"Count"}"#,
        );
        let labels = labels_for(&m, "m");
        assert!(
            labels.iter().all(|l| !l.value.is_empty()),
            "no label should carry an empty value, got {:?}",
            label_pairs(&labels)
        );
        // Dropping is all that changes: the useful labels survive.
        assert_eq!(
            label_pairs(&labels),
            vec![
                ("__name__".to_string(), "m".to_string()),
                ("target_group".to_string(), "tg/bar".to_string()),
            ]
        );
    }

    /// `__name__` is exempt from the empty-value drop. It is unreachable today —
    /// `metric_base_name` bails before `to_series` can pass an empty name, pinned by
    /// `metric_base_name_errors_when_metric_name_sanitizes_to_empty` — so this calls
    /// `labels_for` directly. Dropping it would produce a nameless series that no receiver
    /// rejects and no query finds; keeping it makes the failure loud instead of silent, and
    /// keeps `to_series`'s `expect("labels_for always inserts __name__")` honest.
    #[test]
    fn an_empty_name_label_is_kept_not_dropped() {
        let labels = labels_for(&with_dims(r#"{"TargetGroup":"tg/bar"}"#), "");
        assert!(
            labels
                .iter()
                .any(|l| l.name == "__name__" && l.value.is_empty()),
            "__name__ must survive the empty-value drop, got {:?}",
            label_pairs(&labels)
        );
    }

    #[test]
    fn labels_are_sorted_regardless_of_hashmap_iteration_order() {
        let labels = labels_for(&with_dims(r#"{"Zebra":"1","Alpha":"2","Middle":"3"}"#), "m");
        let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    const NOW: i64 = 1700000000000;

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
                labels
                    .iter()
                    .find(|l| l.name == "__name__")
                    .unwrap()
                    .value
                    .clone()
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
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.unit = MetricUnit::Unknown;
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    /// The bug this guards: before `#[serde(other)]` was added to `MetricUnit::Unknown`
    /// (issue #12), a unit CloudWatch actually emits but this enum did not yet enumerate
    /// failed to deserialize `MetricUnit` at all, which failed the whole `CloudWatchMetric`
    /// and dropped every aggregate in the record -- not just the unrecognised one.
    #[test]
    fn record_with_uncovered_unit_deserializes_and_produces_no_series() {
        let m = values_json(r#"{"max":1.0}"#, "SomeFutureUnit");
        assert!(matches!(m.unit, MetricUnit::Unknown));
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
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.value.max = Some(f32::NAN);
        assert!(to_series(&m, NOW).unwrap().is_empty());

        m.value.max = Some(f32::INFINITY);
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    /// `is_finite` covers the negative pole too; a `> 0.0` style check would not.
    #[test]
    fn negative_infinity_is_dropped() {
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.value.max = Some(f32::NEG_INFINITY);
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    /// One bad aggregate must not take its healthy siblings with it.
    #[test]
    fn a_non_finite_aggregate_does_not_drop_the_others() {
        let mut m = values_json(r#"{"max":1.0,"min":2.0}"#, "Count");
        m.value.max = Some(f32::NAN);
        let series = to_series(&m, NOW).unwrap();
        assert_eq!(names(&series), vec!["firehose_test_m_count_min"]);
        assert_eq!(series[0].1.value, 2.0);
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
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.timestamp = 0;
        assert!(to_series(&m, NOW).unwrap().is_empty());
    }

    /// The degenerate sibling of the two window tests above.
    ///
    /// `now_ms - metric.timestamp` on the extremes of `i64` overflows: in a debug build that
    /// is a panic that takes down the request handler, and in release it wraps silently to a
    /// small age and lets the garbage timestamp straight through the window check — the guard
    /// producing exactly the outcome it exists to prevent. `timestamp` is an `i64` parsed
    /// from attacker-adjacent JSON, so both extremes are one malformed record away.
    #[test]
    fn extreme_timestamps_are_dropped_rather_than_overflowing() {
        let mut m = values_json(r#"{"max":1.0}"#, "Count");

        m.timestamp = i64::MIN;
        assert!(
            to_series(&m, NOW).unwrap().is_empty(),
            "i64::MIN must be rejected, not overflow the age computation"
        );

        m.timestamp = i64::MAX;
        assert!(
            to_series(&m, NOW).unwrap().is_empty(),
            "i64::MAX must be rejected, not overflow the age computation"
        );
    }

    #[test]
    fn recent_backfill_is_kept() {
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.timestamp = NOW - 2 * 60 * 60 * 1000;
        assert_eq!(
            to_series(&m, NOW).unwrap().len(),
            1,
            "Firehose backfill must survive"
        );
    }

    #[test]
    fn all_aggregates_share_one_label_set_apart_from_the_name() {
        let m = values_json(r#"{"max":1.0,"min":2.0}"#, "Count");
        let series = to_series(&m, NOW).unwrap();
        let strip = |labels: &Vec<Label>| -> Vec<(String, String)> {
            labels
                .iter()
                .filter(|l| l.name != "__name__")
                .map(|l| (l.name.clone(), l.value.clone()))
                .collect()
        };
        assert_eq!(strip(&series[0].0), strip(&series[1].0));
    }

    /// The `?` on `metric_base_name` must propagate, not swallow.
    ///
    /// Swallowing would emit a series under a malformed or defaulted name, corrupting a
    /// series that good records write to legitimately — the exact failure the name guards in
    /// `metric_base_name` were added to prevent. Dropping the record is the caller's call to
    /// make, so the error has to reach it.
    #[test]
    fn metric_name_errors_are_propagated_not_swallowed() {
        let mut m = values_json(r#"{"max":1.0}"#, "Count");
        m.namespace = "NoSlashHere".into();
        let err = to_series(&m, NOW).expect_err("should not build a series");
        assert_error_names(err, "no '/' separator", "NoSlashHere");
    }

    #[test]
    fn dimension_labels_appear_on_every_aggregate_series() {
        let mut m = with_dims(r#"{"LoadBalancer":"app/foo"}"#);
        m.value.min = Some(2.0);
        let series = to_series(&m, NOW).unwrap();
        assert_eq!(series.len(), 2);
        for (labels, _) in &series {
            assert!(
                labels
                    .iter()
                    .any(|l| l.name == "load_balancer" && l.value == "app/foo"),
                "got {:?}",
                label_pairs(labels)
            );
        }
        assert_eq!(
            names(&series),
            vec![
                "firehose_applicationelb_requestcount_count_max",
                "firehose_applicationelb_requestcount_count_min",
            ]
        );
    }

    /// The label set is built once per record, not once per aggregate.
    ///
    /// Four calls to `labels_for` would produce four identical results today, so no
    /// assertion on the *labels* can tell the two apart. The observable difference is the
    /// dedup warning: one collision in one record must leave one line in the log, not four.
    /// That is worth pinning on its own terms — an operator reading four warnings for one
    /// dropped dimension is being told something false about how much data was lost.
    #[test]
    fn the_label_set_is_built_once_per_record_not_once_per_aggregate() {
        log_sink();
        // Values unique to this test: the log buffer is shared with every other test.
        let mut m = with_dims(r#"{"InstanceId":"per-record-aa","instance_id":"per-record-bb"}"#);
        m.value.min = Some(2.0);
        m.value.sum = Some(3.0);
        m.value.count = Some(4.0);
        assert_eq!(to_series(&m, NOW).unwrap().len(), 4);

        let lines = captured_logs()
            .lines()
            .filter(|l| l.contains("per-record-"))
            .count();
        assert_eq!(
            lines, 1,
            "one collision in one record must warn once, not once per aggregate"
        );
    }

    /// Hostile dimension names must survive the whole conversion, not just `labels_for`.
    ///
    /// Today these normalize to ordinary names and this test is unremarkable. Its second job
    /// is as a tripwire: these are the inputs that would lead the sorted label set if the trim
    /// in `label_name_for_dimension` were ever loosened (`!!abc` used to become `__abc`, which
    /// sorts ahead of `__name__`). Paired with `to_series` locating `__name__` by name, that
    /// makes the pair of changes needed to corrupt a metric name fail here rather than ship.
    #[test]
    fn hostile_dimension_names_survive_the_whole_conversion() {
        let mut m = with_dims(r#"{"!!abc":"v","//replica//":"w"}"#);
        m.value.min = Some(2.0);
        let series = to_series(&m, NOW).unwrap();
        assert_eq!(
            names(&series),
            vec![
                "firehose_applicationelb_requestcount_count_max",
                "firehose_applicationelb_requestcount_count_min",
            ]
        );
        for (labels, _) in &series {
            assert!(
                labels.iter().any(|l| l.name == "abc" && l.value == "v"),
                "got {:?}",
                label_pairs(labels)
            );
            assert!(
                labels.iter().any(|l| l.name == "replica" && l.value == "w"),
                "got {:?}",
                label_pairs(labels)
            );
        }
    }

    /// Pins the precondition that an index shortcut in `to_series` would silently depend on.
    ///
    /// `to_series` finds `__name__` by name rather than at index 0. Nothing today produces a
    /// label sorting ahead of it — every dimension-derived name starts with an ASCII letter,
    /// since `label_name_for_dimension` trims the ends and prefixes a leading digit, and `_`
    /// (0x5F) sorts below every lowercase letter. So `labels[0]` would work *by coincidence*,
    /// and swapping the `find` for it kills no test.
    ///
    /// This test watches the coincidence instead of the shortcut. Loosen the normalizer — drop
    /// the trim, stop prefixing leading digits — and a dimension can lead the sorted set; a
    /// `labels[0]` implementation would then overwrite that dimension's value with the metric
    /// name and emit the series unnamed. Failing here is the warning that the shortcut has
    /// become unsafe.
    #[test]
    fn no_dimension_derived_label_sorts_before_the_name_label() {
        for input in [
            "!!abc",
            "//replica//",
            "5xxCode",
            "__name__",
            "AccountId",
            "_leading",
            "0",
            "...",
            "Some-Weird.Name",
        ] {
            let Some(name) = label_name_for_dimension(input) else {
                continue;
            };
            assert!(
                name.as_str() > "__name__",
                "{input:?} normalized to {name:?}, which sorts ahead of `__name__`"
            );
        }
    }
}
