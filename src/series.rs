//! Pure translation from [`CloudWatchMetric`] into remote-write wire types.
//!
//! Everything in this module is a total function of its arguments: no I/O, no shared state,
//! no async. Config reading, the HTTP client, retry/backoff, and the batch accumulator belong
//! in the writer, not here — keeping that boundary is what lets these conversions be tested
//! exhaustively without a server or a runtime.

use crate::consts::PROM_NAMESPACE;
use crate::structs::{CloudWatchMetric, MetricUnit};
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
}
