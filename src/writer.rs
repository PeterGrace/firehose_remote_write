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
