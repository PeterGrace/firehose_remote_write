use prometheus::TextEncoder;
use prometheus_remote_write::{Label, Sample, TimeSeries, WriteRequest};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// Buffers samples between flushes.
///
/// `BTreeMap<i64, f64>` does two required jobs for free: inserting the same timestamp
/// twice keeps the last value (matching CloudWatch re-emitting a revised datapoint for
/// a minute as late data lands), and iteration is timestamp-ordered, which the
/// remote-write specification requires within a series.
///
/// # Precondition on the key
///
/// The key is `Vec<Label>`, which is **order-sensitive**, whereas the Prometheus series
/// identity it stands in for is not. Two callers passing the same labels in different
/// orders would split one stored series across two accumulator entries, and the samples
/// would then interleave on push — the out-of-order and duplicate-timestamp errors this
/// buffer exists to prevent. `series::labels_for` sorts by label name before returning,
/// which is what makes the key well-defined; that sort is load-bearing here, not cosmetic.
///
/// Likewise, an empty label *value* is equivalent to the label being absent in the
/// Prometheus data model, so `{foo=""}` and `{}` are one stored series but two distinct
/// `Vec<Label>` keys. `labels_for` drops every empty-valued label for that reason.
/// Both preconditions are asserted by tests in this module rather than enforced here,
/// because normalizing on every insert would cost a sort per sample for a property the
/// only producer already guarantees.
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

/// Combine CloudWatch series and app self-metrics into one request.
///
/// `sorted()` enforces the remote-write requirement that labels are sorted by name and
/// samples by timestamp within each series.
///
/// # `sorted()` is belt-and-braces, and deliberately so
///
/// `WriteRequest::encode_proto3` — which `encode_compressed` calls — already sorts before
/// encoding, so deleting the call here would still put sorted bytes on the wire *today*.
/// It is kept because this function's contract is "returns a spec-conforming request",
/// not "returns something that happens to be fixed up two layers down": every caller that
/// inspects, logs or re-encodes the returned request without going through
/// `encode_compressed` would otherwise see unsorted labels. That upstream sort is in a git
/// dependency we do not control, and the failure it guards against — a receiver rejecting
/// the batch for out-of-order labels — is silent data loss, not a compile error.
///
/// Note what `sorted()` does *not* do: it does not deduplicate series, so two `TimeSeries`
/// carrying identical labels stay two entries in the payload. The accumulator guarantees
/// one entry per label set on its side; the self-metric side is guaranteed by
/// `from_text_format`, which folds samples into one series per label set as it parses.
pub fn build_request(cloudwatch: Vec<TimeSeries>, self_metrics: Vec<TimeSeries>) -> WriteRequest {
    let mut timeseries = cloudwatch;
    timeseries.extend(self_metrics);
    WriteRequest { timeseries }.sorted()
}

/// Gather app self-metrics from the client registry and convert them to series.
///
/// THIS IS THE ONLY PATH SELF-METRICS TAKE TO THE WIRE once Task 11 deletes
/// `push_firehose_metrics`. If this is not called from the flush, every `self_*` counter
/// and gauge becomes write-only: incremented forever, never exported, and invisible
/// exactly when something is going wrong.
///
/// Returns an empty vec on failure rather than losing the CloudWatch data sharing this flush.
///
/// # The round-trip through the text format is lossy, in ways that matter
///
/// `gather()` → text → `from_text_format` is not an encoding detail, it is a conversion with
/// three observable consequences, all measured by `self_metric_series_round_trips_*`:
///
/// * **Timestamps are invented.** The text encoder omits a timestamp for any metric that
///   was never given one, and `prometheus_parse` stamps those samples with `Utc::now()` at
///   parse time. Self-metrics are therefore stamped at flush time, which is the correct
///   answer for a counter read at flush time — but it is a *parse-time* clock, not the
///   value's own, so it is only as accurate as the flush is prompt.
/// * **Histograms and summaries are rejected outright.** `samples_to_timeseries` returns
///   `Err` for both, and it fails the *whole* request, not the offending family — one
///   registered histogram would silently zero every self-metric on every flush. We register
///   none today (the `HISTOGRAMS` map is never populated); if that changes, this must stop
///   going through the text format.
/// * **`HELP`/`TYPE` lines are metadata and simply do not become samples**, which is why
///   a family that has never been touched contributes nothing rather than a zero.
///
/// # One mutant survives here, knowingly
///
/// Making the `Err` arm below return a non-empty vec kills no test, and that is *not* a
/// claim that the branch is unreachable in principle — it is that nothing in this process
/// can reach it. `TextEncoder::encode_to_string` writes into a `String`, so its only I/O
/// failure is `std::fmt::Error`, which `String` never returns; its other failure is
/// `check_metric_family`, which rejects a family with no metrics or no name — and
/// `Registry::gather` prunes empty families before returning and validates names at
/// registration. The arm is a guard against a future caller that feeds this something other
/// than `gather()`'s output, and it is written to fail the same way the parse arm does.
/// Recorded rather than deleted, and recorded rather than left to look like an oversight.
pub fn self_metric_series() -> Vec<TimeSeries> {
    let families = ::prometheus::gather();
    match TextEncoder::new().encode_to_string(&families) {
        Ok(text) => series_from_text(text),
        Err(e) => {
            error!("could not encode self-metrics: {e}");
            vec![]
        }
    }
}

/// Parse gathered self-metrics, or give up on them without giving up on the flush.
///
/// Split out from [`self_metric_series`] purely so the failure arm can be *reached*. Driving
/// it through the real function means registering a histogram, and `prometheus::gather()`
/// reads a process-global registry that is never torn down — so a test that did it would
/// permanently empty the self-metric payload for every other test in the binary, which is
/// the production failure this arm exists to survive, reproduced inside the test harness.
/// With the seam, `a_parse_failure_returns_no_series_and_says_why` drives the arm directly
/// on text; without it, deleting the arm's `vec![]` (or its `error!`) killed nothing.
fn series_from_text(text: String) -> Vec<TimeSeries> {
    match WriteRequest::from_text_format(text) {
        Ok(req) => req.timeseries,
        Err(e) => {
            error!("could not convert self-metrics: {e}");
            vec![]
        }
    }
}

/// The longest any single backoff delay may grow to.
///
/// Two jobs. The obvious one is that 100ms doubling reaches 6.4s by attempt 6 and 1.8 hours
/// by attempt 16, which is not a retry, it is an outage. The less obvious one is arithmetic:
/// `100 * 2u64.pow(attempt)` overflows `u64` at attempt 58 and `2u64.pow` panics outright at
/// attempt 64 in a debug build. `PUSH_MAX_ATTEMPTS=100` is a number an operator can plausibly
/// type, and the result would be a panicking writer task -- the one task draining the channel.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// The longest `push_with_retry` may stay in its retry loop, whatever the attempt count says.
///
/// The writer is a single serialization point: while it is sleeping it is not draining its
/// channel, so the handler starts rejecting payloads and Firehose backs up behind it. The
/// attempt count alone does not bound that -- at the 5s cap, `PUSH_MAX_ATTEMPTS=100` is over
/// eight minutes of stall spent on one batch that is being dropped anyway, and every batch
/// behind it pays for the attempt. Trading one lost batch for a stalled pipeline is the wrong
/// trade in a system whose upstream will redeliver.
const MAX_TOTAL_RETRY_TIME: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Delivered,
    Dropped,
}

/// Which HTTP statuses are worth sending the identical bytes to the identical URL again.
///
/// The Prometheus remote-write 1.0 specification is normative here: senders MUST retry on 5xx
/// and on 429, and MUST NOT retry other 4xx. Special-casing 400 alone -- the obvious reading
/// of "a 400 cannot succeed on a retry" -- leaves 401, 403 and 404 being retried three times
/// each, and bad credentials or a wrong URL are exactly as hopeless as a malformed body. All
/// the retries buy there is triple the request volume and triple the log noise before the
/// batch is dropped anyway, while an operator reading the logs sees a transient-looking
/// failure instead of a configuration error.
///
/// 429 is the one 4xx that must stay retryable: it means the receiver is overloaded, which is
/// precisely when losing the batch is least acceptable and when backing off actually helps.
///
/// Anything else -- 3xx, 1xx -- is non-retryable by default. A redirect only reaches us if
/// reqwest's redirect policy already gave up, so the same bytes will be redirected again.
fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Delay before the attempt following `attempt` (zero-based): 100ms, 200ms, 400ms, ... capped.
///
/// Saturating rather than `2u64.pow(attempt) * 100`, for the reason on [`MAX_BACKOFF`]: the
/// direct form panics in a debug build once the attempt count passes 63, turning a config
/// value into a crash.
fn backoff_delay(attempt: u32) -> Duration {
    2u64.checked_pow(attempt)
        .and_then(|factor| factor.checked_mul(100))
        .map_or(MAX_BACKOFF, Duration::from_millis)
        .min(MAX_BACKOFF)
}

/// Send a body, retrying transport errors, 5xx and 429 with exponential backoff.
///
/// Bounded three ways, because the attempt count alone bounds neither the wall-clock stall nor
/// the arithmetic: by `max_attempts`, by [`MAX_BACKOFF`] per sleep, and by
/// [`MAX_TOTAL_RETRY_TIME`] across the whole loop. Whichever bound is reached first ends it.
///
/// Generic over the send closure rather than a trait so the retry policy is testable without
/// an HTTP stack or a mocking dependency.
pub async fn push_with_retry<F, Fut>(
    mut body: Vec<u8>,
    max_attempts: u32,
    mut send: F,
) -> PushOutcome
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = anyhow::Result<u16>>,
{
    // Named attempts, not retries: with a floor of 1, `0` and `1` both mean a single
    // attempt, and calling the field `retries` made those two settings read as different.
    let attempts = max_attempts.max(1);
    // tokio's `Instant`, not std's, so the budget is virtual under `tokio::time::pause` and
    // the test for it does not have to spend 30 real seconds proving the bound holds.
    let started = Instant::now();

    for attempt in 0..attempts {
        let is_final = attempt + 1 == attempts;
        // Nothing can need the body after the final attempt, so hand it over instead of
        // copying it. `send` needs an owned `Vec` and may be called again, so N attempts need
        // N owned copies from one original -- N-1 clones is the floor, and cloning on the
        // final attempt too was one copy of a multi-megabyte batch above that floor. It also
        // makes `PUSH_MAX_ATTEMPTS=1`, the documented fail-fast setting, copy-free outright.
        let payload = if is_final {
            std::mem::take(&mut body)
        } else {
            body.clone()
        };

        match send(payload).await {
            Ok(status) if (200..300).contains(&status) => return PushOutcome::Delivered,
            Ok(status) if !is_retryable_status(status) => {
                error!("remote write rejected the batch with {status}; dropping without retry");
                return PushOutcome::Dropped;
            }
            Ok(status) => warn!("remote write returned {status} (attempt {})", attempt + 1),
            Err(e) => warn!("remote write failed: {e} (attempt {})", attempt + 1),
        }

        if is_final {
            break;
        }

        // 100ms, 200ms, 400ms, ... capped.
        let delay = backoff_delay(attempt);
        if started.elapsed() + delay > MAX_TOTAL_RETRY_TIME {
            error!(
                "giving up on batch after {} attempts; {}s retry budget exhausted",
                attempt + 1,
                MAX_TOTAL_RETRY_TIME.as_secs()
            );
            return PushOutcome::Dropped;
        }
        tokio::time::sleep(delay).await;
    }

    error!("giving up on batch after {attempts} attempts");
    PushOutcome::Dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testlog::LogTail;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn labels(name: &str) -> Vec<Label> {
        vec![Label {
            name: "__name__".into(),
            value: name.into(),
        }]
    }

    fn samples_of(series: &TimeSeries) -> Vec<(i64, f64)> {
        series
            .samples
            .iter()
            .map(|s| (s.timestamp, s.value))
            .collect()
    }

    #[test]
    fn duplicate_timestamps_keep_the_last_value() {
        let mut acc = Accumulator::new();
        acc.insert(
            labels("m"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        acc.insert(
            labels("m"),
            Sample {
                value: 2.0,
                timestamp: 100,
            },
        );

        let out = acc.drain();
        assert_eq!(out.len(), 1);
        assert_eq!(samples_of(&out[0]), vec![(100, 2.0)]);
    }

    #[test]
    fn samples_come_out_timestamp_ordered_even_when_inserted_backwards() {
        let mut acc = Accumulator::new();
        acc.insert(
            labels("m"),
            Sample {
                value: 3.0,
                timestamp: 300,
            },
        );
        acc.insert(
            labels("m"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        acc.insert(
            labels("m"),
            Sample {
                value: 2.0,
                timestamp: 200,
            },
        );

        let out = acc.drain();
        assert_eq!(
            samples_of(&out[0]),
            vec![(100, 1.0), (200, 2.0), (300, 3.0)]
        );
    }

    #[test]
    fn distinct_label_sets_stay_distinct() {
        let mut acc = Accumulator::new();
        acc.insert(
            labels("a"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        acc.insert(
            labels("b"),
            Sample {
                value: 2.0,
                timestamp: 100,
            },
        );
        assert_eq!(acc.series_count(), 2);
        assert_eq!(acc.drain().len(), 2);
    }

    #[test]
    fn drain_empties_the_accumulator() {
        let mut acc = Accumulator::new();
        acc.insert(
            labels("m"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        assert_eq!(acc.drain().len(), 1);
        assert_eq!(acc.series_count(), 0);
        assert!(acc.drain().is_empty());
    }

    /// The three-sample ordering test above is a *probabilistic* detector, not a guarantee.
    /// Swapping the sample map to a `HashMap` was measured killing it only 19 runs in 25:
    /// with three elements there is roughly a one-in-six chance the hash order comes out
    /// sorted by luck and the mutant survives. An ordering regression that CI catches three
    /// times in four is an ordering regression that reaches production.
    ///
    /// Twelve scrambled samples drops the odds of an accidentally-sorted iteration to about
    /// one in 12! (~2e-9), which makes this a deterministic guard in practice. Keep both:
    /// the small one reads as documentation, this one is the actual net.
    #[test]
    fn ordering_holds_for_enough_samples_that_luck_cannot_explain_it() {
        let mut acc = Accumulator::new();
        // Deliberately scrambled, and not a rotation of sorted order.
        let scrambled = [
            700, 100, 1200, 400, 900, 200, 1100, 300, 600, 1000, 500, 800,
        ];
        for ts in scrambled {
            acc.insert(
                labels("m"),
                Sample {
                    value: ts as f64,
                    timestamp: ts,
                },
            );
        }

        let out = acc.drain();
        assert_eq!(out.len(), 1);
        let got: Vec<i64> = out[0].samples.iter().map(|s| s.timestamp).collect();
        let mut want = scrambled.to_vec();
        want.sort_unstable();
        assert_eq!(got, want);
    }

    /// `is_empty` had no test at all: inverting its body killed nothing. `drain_empties_the_
    /// accumulator` looks like it covers this, but its `assert!(acc.drain().is_empty())` is
    /// `Vec::is_empty`, not `Accumulator::is_empty`. The writer loop (task 9) uses this to
    /// decide whether a flush tick has anything to send, so an inverted answer means either
    /// empty write requests every tick or buffered samples that never ship.
    #[test]
    fn is_empty_tracks_whether_anything_is_buffered() {
        let mut acc = Accumulator::new();
        assert!(acc.is_empty());
        assert!(Accumulator::default().is_empty());

        acc.insert(
            labels("m"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        assert!(!acc.is_empty());

        acc.drain();
        assert!(acc.is_empty());
    }

    /// Key equality is `Vec<Label>` equality, which is ORDER-SENSITIVE, while the Prometheus
    /// series identity it stands in for is not. This asserts the sharp edge rather than
    /// hiding it: two orderings of the same labels are two buffer entries, and on push they
    /// become two writes into one stored series — duplicate timestamps and out-of-order
    /// samples, the exact failure this buffer exists to prevent.
    ///
    /// Nothing here can enforce the precondition cheaply (normalizing would cost a sort per
    /// sample), so it is discharged upstream: `series::labels_for` sorts by label name before
    /// returning, watched by `labels_are_sorted_regardless_of_hashmap_iteration_order`. If
    /// this test ever starts failing because someone made the key order-insensitive, that is
    /// an improvement — but deleting `labels_for`'s sort while this still passes is the
    /// silent break.
    #[test]
    fn key_equality_is_label_order_sensitive() {
        let a = Label {
            name: "aaa".into(),
            value: "1".into(),
        };
        let z = Label {
            name: "zzz".into(),
            value: "2".into(),
        };

        let mut acc = Accumulator::new();
        acc.insert(
            vec![a.clone(), z.clone()],
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        acc.insert(
            vec![z, a],
            Sample {
                value: 2.0,
                timestamp: 100,
            },
        );

        assert_eq!(
            acc.series_count(),
            2,
            "sorted-label precondition is real: unsorted input splits one logical series"
        );
    }

    /// The same trap in its other form. An empty label value is equivalent to an absent
    /// label in the Prometheus data model, so `{__name__="m", foo=""}` and `{__name__="m"}`
    /// are ONE stored series but TWO keys here. This is why `series::labels_for` drops every
    /// empty-valued label, including the ones it sets itself — see
    /// `empty_base_label_values_are_dropped_like_dimensions`. Asserting the split here means
    /// the accumulator states the precondition it depends on instead of assuming it.
    #[test]
    fn an_empty_label_value_makes_a_distinct_key_though_prometheus_would_not() {
        let mut with_empty = labels("m");
        with_empty.push(Label {
            name: "foo".into(),
            value: String::new(),
        });

        let mut acc = Accumulator::new();
        acc.insert(
            labels("m"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        acc.insert(
            with_empty,
            Sample {
                value: 2.0,
                timestamp: 100,
            },
        );

        assert_eq!(
            acc.series_count(),
            2,
            "empty-value labels must be dropped upstream or keys diverge from stored identity"
        );
    }

    /// Degenerate but legal: an empty label vector is a perfectly good `HashMap` key, so it
    /// buffers like any other rather than panicking or colliding with a populated series.
    /// `labels_for` cannot produce one (`__name__` is exempt from the empty-value drop), so
    /// this pins the accumulator's own behaviour, not the pipeline's.
    #[test]
    fn an_empty_label_set_is_a_key_like_any_other() {
        let mut acc = Accumulator::new();
        acc.insert(
            vec![],
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        acc.insert(
            labels("m"),
            Sample {
                value: 2.0,
                timestamp: 100,
            },
        );
        assert_eq!(acc.series_count(), 2);

        let out = acc.drain();
        let empty_keyed = out
            .iter()
            .find(|s| s.labels.is_empty())
            .expect("empty key kept");
        assert_eq!(samples_of(empty_keyed), vec![(100, 1.0)]);
    }

    /// A `TimeSeries` carrying zero samples is pure wire overhead and some receivers reject
    /// it. It is unreachable through this API — `insert` is the only way to create a series
    /// entry and it always lands a sample — so this asserts the property rather than
    /// exercising a branch. It would start failing the moment someone adds a removal path.
    #[test]
    fn drain_never_emits_a_series_with_no_samples() {
        let mut acc = Accumulator::new();
        acc.insert(
            labels("a"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        acc.insert(
            labels("b"),
            Sample {
                value: 2.0,
                timestamp: 200,
            },
        );

        let out = acc.drain();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|s| !s.samples.is_empty()), "got {out:?}");
    }

    /// Labels are moved through `drain` untouched — no reordering, no dropping. The writer
    /// pushes exactly the identity it keyed on.
    #[test]
    fn drain_returns_the_key_labels_verbatim() {
        let key = vec![
            Label {
                name: "__name__".into(),
                value: "m".into(),
            },
            Label {
                name: "account_id".into(),
                value: "123".into(),
            },
            Label {
                name: "region".into(),
                value: "us-east-1".into(),
            },
        ];

        let mut acc = Accumulator::new();
        acc.insert(
            key.clone(),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );

        let out = acc.drain();
        let got: Vec<(&str, &str)> = out[0]
            .labels
            .iter()
            .map(|l| (l.name.as_str(), l.value.as_str()))
            .collect();
        let want: Vec<(&str, &str)> = key
            .iter()
            .map(|l| (l.name.as_str(), l.value.as_str()))
            .collect();
        assert_eq!(got, want);
    }

    // ---------------------------------------------------------------------------------
    // build_request
    // ---------------------------------------------------------------------------------

    #[test]
    fn build_request_carries_accumulated_series() {
        let mut acc = Accumulator::new();
        acc.insert(
            labels("a"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );
        acc.insert(
            labels("b"),
            Sample {
                value: 2.0,
                timestamp: 100,
            },
        );

        let req = build_request(acc.drain(), vec![]);
        assert_eq!(req.timeseries.len(), 2);
    }

    #[test]
    fn build_request_merges_self_metrics_into_the_same_payload() {
        let mut acc = Accumulator::new();
        acc.insert(
            labels("cloudwatch_series"),
            Sample {
                value: 1.0,
                timestamp: 100,
            },
        );

        let self_series = vec![TimeSeries {
            labels: labels("firehose_self_metric"),
            samples: vec![Sample {
                value: 9.0,
                timestamp: 100,
            }],
        }];

        let req = build_request(acc.drain(), self_series);
        assert_eq!(req.timeseries.len(), 2, "one HTTP call carries both");
    }

    #[test]
    fn build_request_sorts_labels_within_each_series() {
        let unsorted = vec![
            Label {
                name: "zzz".into(),
                value: "1".into(),
            },
            Label {
                name: "aaa".into(),
                value: "2".into(),
            },
        ];
        let req = build_request(
            vec![TimeSeries {
                labels: unsorted,
                samples: vec![Sample {
                    value: 1.0,
                    timestamp: 1,
                }],
            }],
            vec![],
        );
        let names: Vec<&str> = req.timeseries[0]
            .labels
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(names, vec!["aaa", "zzz"]);
    }

    /// Pull one named series out of a `self_metric_series()` result.
    fn find_series<'a>(series: &'a [TimeSeries], name: &str) -> Option<&'a TimeSeries> {
        series.iter().find(|s| {
            s.labels
                .iter()
                .any(|l| l.name == "__name__" && l.value == name)
        })
    }

    fn label_of<'a>(series: &'a TimeSeries, name: &str) -> Option<&'a str> {
        series
            .labels
            .iter()
            .find(|l| l.name == name)
            .map(|l| l.value.as_str())
    }

    /// The whole self-metric export is a round trip through the Prometheus **text** format:
    /// `gather()` produces protobuf families, `TextEncoder` renders them to text, and
    /// `from_text_format` re-parses that text into series. Three separate representations,
    /// two conversions, and no compiler checking that anything survives.
    ///
    /// The load-bearing assertion is the `instance` **const** label. It is baked in at
    /// registration by `self_metric_opts!` and has no call site anywhere, so if the text
    /// round trip dropped it nothing else in the suite would notice: the metric would still
    /// register, still increment, still gather with the label attached (which is all
    /// `every_self_metric_registers_with_the_instance_const_label` checks), and simply arrive
    /// at the remote unattributable to a replica.
    ///
    /// Measured result: const labels DO survive. `TextEncoder` does not distinguish const
    /// labels from variable ones — by the time it sees a `MetricFamily` they are both just
    /// entries in the metric's label list — so they render as ordinary `name="value"` pairs
    /// and parse straight back.
    #[test]
    fn self_metric_series_round_trips_a_counter_with_its_const_label() {
        let _log = LogTail::start();
        // Registers the metric via `lazy_static` if some earlier test has not already, and
        // guarantees it is non-zero so the text encoder emits a line for it.
        crate::prometheus::RECORDS_SKIPPED.inc();

        let series = self_metric_series();
        let found = find_series(&series, "firehose_self_records_skipped_count")
            .unwrap_or_else(|| panic!("self-metric missing from the round trip: {series:?}"));

        assert_eq!(
            label_of(found, "instance"),
            Some(crate::prometheus::instance_label()),
            "the instance const label must survive gather -> text -> parse, got {found:?}"
        );
        assert!(
            !found.samples.is_empty(),
            "a touched counter must carry a sample"
        );
        assert!(
            found.samples.iter().all(|s| s.value >= 1.0),
            "the counter's value must survive, got {:?}",
            found.samples
        );
    }

    /// Untimestamped metrics — which every self-metric is, since nothing calls
    /// `set_timestamp_ms` on them — are stamped by `prometheus_parse` with `Utc::now()` at
    /// **parse** time. That is the right answer for a counter read during the flush, but it
    /// is worth pinning as behaviour rather than leaving it to be rediscovered: it means the
    /// self-metric timestamp is the flush clock, not the value's own clock, and a series
    /// arriving with timestamp 0 (or a 1970 date, or a far-future one) would be silently
    /// rejected or misfiled by the receiver.
    ///
    /// The window is deliberately generous — a stopped clock or a seconds/millis unit mixup
    /// is what this catches, not a slow test runner.
    #[test]
    fn self_metric_series_stamps_untimestamped_samples_at_parse_time() {
        let _log = LogTail::start();
        crate::prometheus::REJECTED_PAYLOADS.inc();

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let series = self_metric_series();
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let found = find_series(&series, "firehose_self_rejected_payloads_count")
            .expect("self-metric should round trip");
        let ts = found.samples[0].timestamp;
        assert!(
            ts >= before - 60_000 && ts <= after + 60_000,
            "expected a parse-time millisecond timestamp near {before}..{after}, got {ts}"
        );
    }

    /// `from_text_format` fails the **whole request** on a histogram or summary rather than
    /// skipping the offending family: `samples_to_timeseries` returns `Err` on the first one
    /// it meets and the `?` discards everything parsed so far. One registered histogram would
    /// therefore silently zero *every* self-metric on *every* flush, forever, and the only
    /// evidence would be one `error!` line per flush.
    ///
    /// We register no histograms today — the `HISTOGRAMS` map exists but nothing inserts into
    /// it — so this pins the hazard against the parser directly rather than polluting the
    /// process-global registry with a histogram that every other test would then inherit.
    #[test]
    fn from_text_format_rejects_the_entire_payload_when_a_histogram_is_present() {
        let _log = LogTail::start();
        let healthy = "# HELP ok a counter\n# TYPE ok counter\nok{instance=\"x\"} 5\n";
        assert_eq!(
            WriteRequest::from_text_format(healthy.to_string())
                .expect("a plain counter must parse")
                .timeseries
                .len(),
            1,
            "control: the healthy half of this input parses on its own"
        );

        let with_histogram = format!(
            "{healthy}# HELP h a histogram\n# TYPE h histogram\n\
             h_bucket{{le=\"1\"}} 1\nh_bucket{{le=\"+Inf\"}} 1\nh_sum 0.5\nh_count 1\n"
        );
        let err = WriteRequest::from_text_format(with_histogram)
            .expect_err("a histogram must not be silently accepted");
        assert!(
            err.to_string().contains("histogram"),
            "the failure must name the unsupported type, got: {err}"
        );
    }

    /// `HELP` and `TYPE` lines are metadata, not samples, so a registered-but-never-touched
    /// family contributes nothing to the payload rather than a spurious zero. Pinned because
    /// the alternative reading — that every registered family emits something — is what would
    /// make `self_metric_series` look like a safe place to register speculative metrics.
    ///
    /// Note this is a property of the *text* format, not of `gather()`: a `Counter` that has
    /// never been incremented still gathers, and still renders as an explicit `0`. It is a
    /// family with no children (an untouched `*Vec`) that vanishes.
    #[test]
    fn help_and_type_lines_alone_produce_no_series() {
        let _log = LogTail::start();
        let metadata_only = "# HELP lonely a metric nobody touched\n# TYPE lonely counter\n";
        let req = WriteRequest::from_text_format(metadata_only.to_string())
            .expect("metadata-only input must parse rather than error");
        assert!(
            req.timeseries.is_empty(),
            "metadata must not manufacture series, got {:?}",
            req.timeseries
        );
    }

    /// `build_request` is handed self-metric series it did not build, so it cannot assume the
    /// accumulator's guarantees about them. `sorted()` fixes label order and sample order —
    /// this pins the sample half, which `build_request_sorts_labels_within_each_series` does
    /// not see, and which the accumulator would never produce a violation of.
    #[test]
    fn build_request_sorts_samples_by_timestamp_within_each_series() {
        let scrambled = vec![
            Sample {
                value: 3.0,
                timestamp: 300,
            },
            Sample {
                value: 1.0,
                timestamp: 100,
            },
            Sample {
                value: 2.0,
                timestamp: 200,
            },
        ];
        let req = build_request(
            vec![],
            vec![TimeSeries {
                labels: labels("m"),
                samples: scrambled,
            }],
        );
        assert_eq!(
            samples_of(&req.timeseries[0]),
            vec![(100, 1.0), (200, 2.0), (300, 3.0)]
        );
    }

    /// `sorted()` does NOT deduplicate: two `TimeSeries` with identical labels stay two
    /// entries in the payload, and a remote write receiver reading them in order sees a
    /// duplicate or out-of-order sample. Stated as a test because it is the precondition
    /// `build_request` silently relies on — the accumulator guarantees one entry per label
    /// set, and `from_text_format` folds parsed samples into one series per label set, so
    /// neither input can produce a collision today. A future caller passing hand-built
    /// series has no such protection.
    #[test]
    fn build_request_does_not_merge_two_series_sharing_a_label_set() {
        let req = build_request(
            vec![TimeSeries {
                labels: labels("m"),
                samples: vec![Sample {
                    value: 1.0,
                    timestamp: 100,
                }],
            }],
            vec![TimeSeries {
                labels: labels("m"),
                samples: vec![Sample {
                    value: 2.0,
                    timestamp: 100,
                }],
            }],
        );
        assert_eq!(
            req.timeseries.len(),
            2,
            "sorted() does not dedup; callers must not hand it colliding label sets"
        );
    }

    /// The failure arm must yield *nothing*, not a partial or placeholder payload: a
    /// conversion that failed but returned a series would push a value nobody computed, and
    /// one that propagated would take the CloudWatch data sharing this flush down with it.
    ///
    /// It must also *say so*. This arm is how "every self-metric silently reads zero" looks
    /// from the inside, and the only evidence available to whoever is debugging it is this
    /// log line — the CloudWatch data keeps flowing, the process keeps serving, and the
    /// dashboards for the exporter itself go flat.
    #[test]
    fn a_parse_failure_returns_no_series_and_says_why() {
        let tail = LogTail::start();
        let unparseable = "# TYPE writer_hist_aa histogram\nwriter_hist_aa_bucket{le=\"+Inf\"} 1\n\
             writer_hist_aa_sum 1\nwriter_hist_aa_count 1\n";

        let series = series_from_text(unparseable.to_string());
        let logs = tail.tail();

        assert!(
            series.is_empty(),
            "a failed conversion must contribute nothing, got {series:?}"
        );
        assert!(
            logs.contains("could not convert self-metrics"),
            "the failure must be visible in the log, got: {logs}"
        );
    }

    /// The other half of the contract, and the one that makes "empty on failure" mean
    /// anything: the healthy path is genuinely non-empty. Without this, a `self_metric_series`
    /// that always returned `vec![]` would satisfy every failure assertion above while
    /// exporting nothing at all.
    #[test]
    fn the_success_path_returns_series_so_empty_is_a_real_signal() {
        let _log = LogTail::start();
        crate::prometheus::BATCHES_DROPPED.inc();
        let series = self_metric_series();
        assert!(
            !series.is_empty(),
            "the success path must return series, or the empty-on-failure contract is vacuous"
        );
        assert!(
            find_series(&series, "firehose_self_batches_dropped_count").is_some(),
            "the metric this test touched must be in the payload, got {series:?}"
        );
    }

    // ---------------------------------------------------------------------------------
    // push_with_retry
    //
    // Every test below opens a `LogTail`, including the ones that never read the buffer.
    // `push_with_retry` warns on every failed attempt, and a test that emits log lines while
    // not holding the lock writes straight into whichever module's `LogTail` window happens
    // to be open — which is exactly what makes `config`'s absence assertions flaky. The
    // `Accumulator` tests above deliberately do not take it: they emit nothing at all, so
    // they cannot pollute anyone's window, and taking it would serialize them for free.
    // ---------------------------------------------------------------------------------

    #[tokio::test]
    async fn push_succeeds_on_first_attempt() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(vec![1, 2, 3], 3, move |_body| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(200u16)
            }
        })
        .await;

        assert_eq!(outcome, PushOutcome::Delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transport_failure_is_retried_to_the_bound_then_dropped() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(vec![1], 3, move |_body| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("connection refused"))
            }
        })
        .await;

        assert_eq!(outcome, PushOutcome::Dropped);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "should attempt exactly max_attempts times"
        );
    }

    #[tokio::test]
    async fn bad_request_is_dropped_without_retrying() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(vec![1], 3, move |_body| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(400u16)
            }
        })
        .await;

        assert_eq!(outcome, PushOutcome::Dropped);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "400 must not be retried");
    }

    #[tokio::test]
    async fn server_error_is_retried_then_succeeds() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(vec![1], 3, move |_body| {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(503u16)
                } else {
                    Ok(200u16)
                }
            }
        })
        .await;

        assert_eq!(outcome, PushOutcome::Delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn zero_attempts_still_tries_once() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(vec![1], 0, move |_body| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(200u16)
            }
        })
        .await;

        assert_eq!(outcome, PushOutcome::Delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Count the attempts a fixed status provokes, so the classification tests below read as
    /// a table rather than five copies of the same closure.
    async fn attempts_for_status(status: u16, max_attempts: u32) -> (PushOutcome, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(vec![1], max_attempts, move |_body| {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(status)
            }
        })
        .await;
        (outcome, calls.load(Ordering::SeqCst))
    }

    /// 400 was special-cased on its own, which left 401/403/404 being retried three times
    /// each. Bad credentials and a wrong URL are exactly as hopeless as a malformed body:
    /// resending identical bytes to the same endpoint cannot turn a 403 into a 200, so all it
    /// buys is triple the request volume and triple the log noise while the batch is dropped
    /// anyway. The remote-write 1.0 specification says the same thing normatively — senders
    /// MUST NOT retry 4xx other than 429.
    #[tokio::test]
    async fn client_errors_other_than_429_are_dropped_without_retrying() {
        let _log = LogTail::start();
        for status in [400u16, 401, 403, 404, 405, 413, 422] {
            let (outcome, calls) = attempts_for_status(status, 3).await;
            assert_eq!(outcome, PushOutcome::Dropped, "{status} should drop");
            assert_eq!(calls, 1, "{status} must not be retried");
        }
    }

    /// The one 4xx that must NOT be lumped in with the rest. 429 means "you are going too
    /// fast", which is the textbook case for backing off and trying again — treating it as
    /// non-retryable would silently discard every batch produced while the receiver is under
    /// load, which is precisely when the data matters.
    #[tokio::test]
    async fn rate_limiting_is_retried() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let outcome = push_with_retry(vec![1], 3, move |_body| {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Ok(429u16)
                } else {
                    Ok(200u16)
                }
            }
        })
        .await;

        assert_eq!(outcome, PushOutcome::Delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "429 must be retried");
    }

    /// The whole 5xx range, not just the 503 the happy-path test happens to use: a receiver
    /// restarting returns 502, an overloaded one 500, a misconfigured proxy 504.
    #[tokio::test]
    async fn server_errors_are_retried_to_the_bound() {
        let _log = LogTail::start();
        for status in [500u16, 502, 503, 504] {
            let (outcome, calls) = attempts_for_status(status, 3).await;
            assert_eq!(outcome, PushOutcome::Dropped, "{status} should end dropped");
            assert_eq!(calls, 3, "{status} should be retried to the bound");
        }
    }

    /// 2xx is a range, not the literal 200. A receiver answering 204 No Content has accepted
    /// the batch; treating it as a failure would re-send data the receiver already stored,
    /// manufacturing the duplicate samples this pipeline exists to avoid.
    #[tokio::test]
    async fn any_2xx_counts_as_delivered() {
        let _log = LogTail::start();
        for status in [200u16, 201, 202, 204, 299] {
            let (outcome, calls) = attempts_for_status(status, 3).await;
            assert_eq!(outcome, PushOutcome::Delivered, "{status} should deliver");
            assert_eq!(calls, 1, "{status} should not be re-sent");
        }
    }

    /// A redirect reaching us means reqwest's redirect policy already gave up, so the same
    /// bytes to the same URL will be redirected again. Pinned because the classification is
    /// "retry 5xx and 429, drop everything else" rather than "drop 4xx" — without this, a
    /// 3xx falling into the retryable bucket would go unnoticed.
    #[tokio::test]
    async fn redirects_are_not_retried() {
        let _log = LogTail::start();
        let (outcome, calls) = attempts_for_status(301, 3).await;
        assert_eq!(outcome, PushOutcome::Dropped);
        assert_eq!(calls, 1);
    }

    /// The delay schedule, tested directly rather than through the clock, so the sequence is
    /// pinned exactly instead of being inferred from a total.
    #[test]
    fn backoff_doubles_then_saturates_at_the_cap() {
        let _log = LogTail::start();
        assert_eq!(backoff_delay(0), Duration::from_millis(100));
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
        assert_eq!(backoff_delay(2), Duration::from_millis(400));
        assert_eq!(backoff_delay(3), Duration::from_millis(800));
        assert_eq!(backoff_delay(4), Duration::from_millis(1600));
        assert_eq!(backoff_delay(5), Duration::from_millis(3200));
        // 6400ms would exceed the cap.
        assert_eq!(backoff_delay(6), MAX_BACKOFF);
        assert_eq!(backoff_delay(20), MAX_BACKOFF);
    }

    /// `100 * 2u64.pow(attempt)` overflows `u64` at attempt 58 and `2u64.pow` panics outright
    /// at attempt 64 in a debug build. `PUSH_MAX_ATTEMPTS=100` is a value an operator can
    /// plausibly type, and the result would be a panicking writer task rather than a slow
    /// retry — an unhandled panic in the one task that drains the channel.
    #[test]
    fn backoff_cannot_overflow_at_any_attempt_number() {
        let _log = LogTail::start();
        for attempt in [57u32, 58, 63, 64, 100, 1000, u32::MAX] {
            assert_eq!(
                backoff_delay(attempt),
                MAX_BACKOFF,
                "attempt {attempt} must saturate, not panic or wrap"
            );
        }
    }

    /// Virtual time: with `start_paused` tokio auto-advances the clock whenever the runtime
    /// goes idle, so the assertion is on an exact virtual duration rather than a wall-clock
    /// window that a loaded CI box can widen. Three failing attempts must sleep 100ms then
    /// 200ms and then stop — deleting the sleep gives 0ms, sleeping after the final attempt
    /// gives 700ms, and a constant (non-doubling) backoff gives 200ms.
    #[tokio::test(start_paused = true)]
    async fn backoff_is_exponential_and_no_sleep_follows_the_final_attempt() {
        let _log = LogTail::start();
        let started = tokio::time::Instant::now();
        let outcome = push_with_retry(vec![1], 3, |_body| async {
            Err(anyhow::anyhow!("connection refused"))
        })
        .await;

        assert_eq!(outcome, PushOutcome::Dropped);
        assert_eq!(
            started.elapsed(),
            Duration::from_millis(300),
            "expected sleeps of 100ms and 200ms and nothing after the last attempt"
        );
    }

    /// The single-attempt configuration must not pause at all before giving up. `PUSH_MAX_
    /// ATTEMPTS=0` is documented as "try once, let Firehose redeliver"; a 100ms sleep on the
    /// way out would make the fail-fast setting not fail fast.
    #[tokio::test(start_paused = true)]
    async fn a_single_attempt_never_sleeps() {
        let _log = LogTail::start();
        let started = tokio::time::Instant::now();
        let outcome = push_with_retry(vec![1], 0, |_body| async {
            Err(anyhow::anyhow!("refused"))
        })
        .await;

        assert_eq!(outcome, PushOutcome::Dropped);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    /// The writer task is a single serialization point: while it sleeps it is not draining
    /// its channel, so the handler starts rejecting payloads and Firehose backs up behind it.
    /// `PUSH_MAX_ATTEMPTS=100` at a 5s cap is over eight minutes of that, spent on one batch
    /// that is being dropped anyway. The budget bounds the stall independently of the attempt
    /// count; without it this test would run for 8 virtual minutes and make ~100 calls.
    #[tokio::test(start_paused = true)]
    async fn total_retry_time_is_bounded_regardless_of_the_attempt_count() {
        let _log = LogTail::start();
        let started = tokio::time::Instant::now();
        let (outcome, calls) = attempts_for_status(503, 1000).await;

        assert_eq!(outcome, PushOutcome::Dropped);
        assert!(
            started.elapsed() <= MAX_TOTAL_RETRY_TIME,
            "stalled the writer for {:?}, budget is {MAX_TOTAL_RETRY_TIME:?}",
            started.elapsed()
        );
        assert!(
            calls < 1000,
            "the budget must cut the loop short of the attempt count, made {calls} calls"
        );
        assert!(
            calls > 3,
            "the budget must not truncate to a token retry or two, made {calls}"
        );
    }

    /// The last attempt is handed the body itself rather than a copy, which is only sound if
    /// no attempt after it exists. Assert every attempt sees the bytes intact: taking the
    /// body one attempt early would send an empty payload as the final try, silently pushing
    /// nothing while reporting a normal failure.
    #[tokio::test(start_paused = true)]
    async fn every_attempt_receives_the_body_intact() {
        let _log = LogTail::start();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
        let s = seen.clone();
        push_with_retry(vec![7, 8, 9], 3, move |body| {
            let s = s.clone();
            async move {
                s.lock().unwrap().push(body);
                Ok(503u16)
            }
        })
        .await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        for (i, body) in seen.iter().enumerate() {
            assert_eq!(
                body,
                &vec![7u8, 8, 9],
                "attempt {} got the wrong body",
                i + 1
            );
        }
    }

    /// Giving up must be visible. A batch that vanishes with nothing in the log is
    /// indistinguishable from a batch that was delivered, and the whole point of a bounded
    /// retry is that the bound is observable when it is hit.
    #[tokio::test(start_paused = true)]
    async fn exhausting_the_attempts_is_logged() {
        let tail = LogTail::start();
        push_with_retry(vec![1], 2, |_body| async {
            Err(anyhow::anyhow!("push-retry-transport-aa"))
        })
        .await;
        let logs = tail.tail();

        assert!(
            logs.contains("push-retry-transport-aa"),
            "each failed attempt must report why, got: {logs}"
        );
        assert!(
            logs.contains("giving up"),
            "exhausting the bound must say so, got: {logs}"
        );
    }

    /// The non-retryable path drops the batch after a single attempt, so it is the easiest
    /// one to mistake for success when reading logs. It must name the status that caused it.
    #[tokio::test]
    async fn a_non_retryable_status_is_logged_with_its_status_code() {
        let tail = LogTail::start();
        attempts_for_status(403, 3).await;
        let logs = tail.tail();

        assert!(
            logs.contains("403"),
            "the drop must name the status that caused it, got: {logs}"
        );
        assert!(
            logs.contains("without retry"),
            "the drop must say it was deliberate rather than exhaustion, got: {logs}"
        );
    }

    /// Delivery must be silent at warn/error. A push that succeeds on the first attempt is
    /// the overwhelmingly common case; a line per flush would bury the failures that matter.
    ///
    /// This asserts the absence of *this module's* vocabulary rather than an empty tail.
    /// `LogTail` only excludes other lock holders, and `series::tests` asserts on logs via
    /// `captured_logs` without taking the lock — so a sibling's "dropping dimension with
    /// unusable name" can legitimately land inside this window. An `assert_eq!(tail, "")`
    /// here would be a flake, not a guard.
    #[tokio::test]
    async fn a_successful_push_logs_nothing() {
        let tail = LogTail::start();
        let outcome = push_with_retry(vec![1], 3, |_body| async { Ok(200u16) }).await;
        assert_eq!(outcome, PushOutcome::Delivered);
        let logs = tail.tail();

        for phrase in ["remote write", "giving up"] {
            assert!(
                !logs.contains(phrase),
                "a clean push must not log {phrase:?}, got: {logs}"
            );
        }
    }
}
