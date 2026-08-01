use crate::config::Config;
use crate::prometheus::{
    BATCHES_DROPPED, BUFFER_SERIES, FLUSH_COUNT_TOTAL, FLUSH_DURATION_SECONDS_TOTAL,
};
use prometheus::TextEncoder;
use prometheus_remote_write::{Label, Sample, TimeSeries, WriteRequest};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
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
/// THIS IS THE ONLY PATH SELF-METRICS TAKE TO THE WIRE. If this is not called from the
/// flush, every `self_*` counter
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
///   none today, and `prometheus.rs` says why not; if that changes, this must stop
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

/// The budget governing how long `push_with_retry` may spend *between* attempts.
///
/// The writer is a single serialization point: while it is sleeping it is not draining its
/// channel, so the handler starts rejecting payloads and Firehose backs up behind it. The
/// attempt count alone does not bound that -- at the 5s cap, `PUSH_MAX_ATTEMPTS=100` is over
/// eight minutes of stall spent on one batch that is being dropped anyway, and every batch
/// behind it pays for the attempt. Trading one lost batch for a stalled pipeline is the wrong
/// trade in a system whose upstream will redeliver.
///
/// # This constant does NOT bound the call on its own, and used to claim it did
///
/// It is consulted *before each sleep*, which means it only ever gates time the loop spends
/// waiting. Time spent inside `send` is unbounded by it: the check reads `started.elapsed()`
/// only after `send` has returned, so a `send` that never returns is never checked at all.
/// Measured, not theorised -- with a non-resolving `send`, one virtual hour later the writer
/// had consumed nothing and the channel was still full. That is not an exotic failure:
/// `reqwest::Client::new()` has **no default request timeout**, so a blackholed TCP
/// connection to the remote-write endpoint stalls the only task draining the channel forever.
///
/// [`PER_ATTEMPT_TIMEOUT`] is what closes that hole. The two together give the real ceiling
/// on one `push_with_retry` call:
///
/// ```text
/// MAX_TOTAL_RETRY_TIME + PER_ATTEMPT_TIMEOUT  ==  40s
/// ```
///
/// The overshoot is one attempt wide because the final attempt is entered while still inside
/// the budget and may then run to its own timeout; there is no check that can prevent that
/// without cutting a legitimately slow request short. Pinned by
/// `a_blackholed_endpoint_cannot_stall_the_writer_indefinitely`.
const MAX_TOTAL_RETRY_TIME: Duration = Duration::from_secs(30);

/// The longest a single `send` may run before it is abandoned and counted as a failed attempt.
///
/// This is the bound that makes [`MAX_TOTAL_RETRY_TIME`] mean anything, and it deliberately
/// lives in the retry policy rather than in the caller's closure. `send` is a generic
/// parameter: Task 11 will pass a `reqwest` client and will also set that client's own
/// timeouts, but nothing in the type system obliges any future caller to do so, and the cost
/// of getting it wrong is the single-threaded writer hanging forever. Defence in depth here
/// is cheap; discovering the omission in production is not.
///
/// # Why ten seconds
///
/// The number is chosen against the budget and the default attempt count, not by feel:
///
/// * **Not larger.** At 30s a single hung attempt consumes the entire budget, so the
///   pre-sleep check fires immediately afterwards and the batch is abandoned after *one*
///   attempt -- the retry policy would stop existing for exactly the transport failures it
///   is for.
/// * **Not smaller.** A remote-write receiver under load, or a multi-megabyte batch over a
///   slow link, can legitimately take seconds. At 5s a working-but-loaded remote starts
///   being cut off and retried, adding request volume at the moment it is least wanted.
/// * **Ten fits the arithmetic.** Three hung attempts (100ms + 200ms of backoff between
///   them) land at 30.3s, so the default `PUSH_MAX_ATTEMPTS=3` gets all three of its attempts
///   against a dead endpoint and the budget cuts in exactly where it should. Raising the
///   attempt count does not extend the stall: at `PUSH_MAX_ATTEMPTS=1000` the third attempt
///   ends at 30.3s and the pre-sleep check abandons the batch there.
const PER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

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

        // A `send` that never resolves is a *transport* failure, not a slow success, and is
        // treated as one: it consumes an attempt, it backs off, and it lets the total budget
        // apply. Without this wrapper the budget check below is unreachable, because it reads
        // `started.elapsed()` only after `send` has returned.
        match tokio::time::timeout(PER_ATTEMPT_TIMEOUT, send(payload)).await {
            Ok(Ok(status)) if (200..300).contains(&status) => return PushOutcome::Delivered,
            Ok(Ok(status)) if !is_retryable_status(status) => {
                error!("remote write rejected the batch with {status}; dropping without retry");
                return PushOutcome::Dropped;
            }
            Ok(Ok(status)) => warn!("remote write returned {status} (attempt {})", attempt + 1),
            Ok(Err(e)) => warn!("remote write failed: {e} (attempt {})", attempt + 1),
            Err(_elapsed) => warn!(
                "remote write did not respond within {}s (attempt {})",
                PER_ATTEMPT_TIMEOUT.as_secs(),
                attempt + 1
            ),
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

/// One handler's worth of samples, already converted to series identity.
pub type SeriesBatch = Vec<(Vec<Label>, Sample)>;

/// Own the accumulator and be the only thing that ever pushes.
///
/// Generic over the send closure so tests can drive it without an HTTP stack.
/// Exits when the channel closes, flushing anything still buffered.
///
/// # This task is deliberately a serialization point, and that is the backpressure
///
/// `flush` is `await`ed from inside the `select!` body, so while a push is in flight — up to
/// [`MAX_TOTAL_RETRY_TIME`] with retries — nothing is draining `rx`. The channel fills, the
/// handler starts rejecting payloads, and Firehose retries the delivery. That is the intended
/// design, not an oversight: the alternative is to keep accepting records into a buffer that
/// grows for as long as the remote is down, which converts a recoverable downstream outage
/// into an unrecoverable OOM, and loses *everything* buffered instead of pushing the loss
/// back to an upstream that is built to redeliver. Nothing queued during a stall is lost —
/// it is still in the channel when the writer returns to the loop, and it carries its own
/// CloudWatch timestamps, so a late flush is late data rather than wrong data.
///
/// `nothing_drains_the_channel_while_a_push_is_in_flight` pins this so that a future change
/// making the push concurrent has to argue with a test rather than slip through.
///
/// # Durability: a 200 to Firehose is a promise this process cannot keep
///
/// This is the most important operational property of the system, so it is written here
/// rather than left to be inferred. The handler returns 200 as soon as a batch is accepted
/// into the channel, and 200 means "accepted" to Kinesis Firehose — it will not redeliver.
/// But the data is only in memory: in this channel, or in this task's accumulator, or in a
/// request in flight. **A crash, an OOM kill, a SIGKILL or a node eviction loses everything
/// accumulated since the last successful flush, and nothing upstream will replay it.**
///
/// There is no write-ahead log and adding one is not obviously right. The exposure is bounded
/// by `FLUSH_INTERVAL_SECS` (default 1s) plus whatever a push takes, so a normal restart
/// loses on the order of a second of metrics — for CloudWatch data delivered at minute
/// granularity and used for dashboards and alerting, that is a gap of at most one datapoint
/// in a series that is already sampled coarsely. Paying for durability with fsyncs on the
/// hot path, or with the operational weight of a spool directory that can itself fill up,
/// buys very little against that.
///
/// What *does* follow from this, and is easy to get wrong:
///
/// * **`FLUSH_INTERVAL_SECS` is a durability setting, not just a batching one.** Raising it
///   to reduce request volume raises the amount of data a crash destroys, linearly.
/// * **Graceful shutdown matters more than it looks.** The channel-closed arm below is the
///   only thing that saves the tail of the buffer, and it only runs if the process is allowed
///   to finish. A `SIGKILL`, or a container `terminationGracePeriodSeconds` shorter than one
///   flush plus one push, silently discards it — which is why that path logs what it is
///   carrying before it attempts it.
/// * **Replica count does not help.** Each replica buffers only what was routed to it, so
///   losing one loses that share outright rather than degrading it.
pub async fn run_writer<F, Fut>(mut rx: Receiver<SeriesBatch>, config: Config, mut send: F)
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = anyhow::Result<u16>>,
{
    let mut acc = Accumulator::new();
    let period = Duration::from_secs(config.flush_interval_secs);

    // `interval_at(now + period, ...)`, NOT `interval(period)`. `tokio::time::interval`
    // completes its first tick immediately, and that tick is not consumed until a `select!`
    // arm actually wins it — so the ticker branch stays ready across *every* iteration until
    // it does. With both arms ready, `select!` picks at random.
    //
    // For correctness that is harmless: `flush` returns early when the accumulator is empty.
    // For *observability* it is fatal, and this was measured rather than estimated. The
    // random pick only matters while both arms are ready; `rx.recv()` eventually goes
    // pending, at which point the still-unconsumed tick-0 is the only ready arm and wins
    // unconditionally. So with `interval` the writer performs one flush of whatever is
    // buffered *no matter what the threshold says*, and deleting the series-threshold check
    // left `writer_flushes_when_series_threshold_is_reached` passing 25 runs out of 25. Not
    // a flaky detector — no detector at all. (With `interval_at`: killed 25 out of 25.)
    //
    // Skipping the immediate tick changes nothing else: `interval` fires at 0 and then at
    // `period`, and the tick at 0 has nothing to flush.
    let mut ticker = tokio::time::interval_at(Instant::now() + period, period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // When we last *started* talking to the remote, successful or not. Records the attempt
    // rather than the delivery on purpose: the heartbeat exists to prove this process is
    // alive and pushing, and a remote that is rejecting everything is not a reason to push
    // more often. Updated after each push completes, which is also what stops a long push
    // from being followed by a burst -- see [`IDLE_HEARTBEAT`].
    let mut last_push_attempt = Instant::now();

    loop {
        let should_flush = tokio::select! {
            received = rx.recv() => match received {
                Some(batch) => {
                    for (labels, sample) in batch {
                        acc.insert(labels, sample);
                    }
                    BUFFER_SERIES.set(acc.series_count() as f64);
                    acc.series_count() >= config.flush_max_series
                }
                // Channel closed: flush what is left, then stop.
                None => {
                    if !acc.is_empty() {
                        // The final flush is the one nobody is watching -- it happens while
                        // the process is on its way out, and a failure here loses the tail of
                        // the data with no next flush to carry it. Say what is at stake
                        // before attempting it, so the drop that may follow has a size.
                        warn!(
                            "channel closed; flushing {} buffered series before exit",
                            acc.series_count()
                        );
                    }
                    flush(&mut acc, &config, &mut send).await;
                    return;
                }
            },
            _ = ticker.tick() => true,
        };

        if should_flush {
            if acc.is_empty() {
                // An idle tick. There is no CloudWatch data to send, but total silence is
                // itself misleading -- see [`IDLE_HEARTBEAT`].
                if last_push_attempt.elapsed() >= IDLE_HEARTBEAT {
                    push_heartbeat(&config, &mut send).await;
                    last_push_attempt = Instant::now();
                }
            } else {
                flush(&mut acc, &config, &mut send).await;
                last_push_attempt = Instant::now();
            }
        }
    }
}

/// How long the writer may stay completely silent before it pushes its own metrics alone.
///
/// `flush` returning early on an empty accumulator is correct -- an empty CloudWatch payload
/// is pure wire overhead -- but the consequence is that an idle exporter pushes *nothing at
/// all*. From the remote's point of view "the exporter is dead" and "no CloudWatch data is
/// arriving" then look identical, and every `firehose_self_*` series goes stale at exactly
/// the moment somebody would go looking at them: `self_batches_dropped_count`,
/// `self_records_skipped_count` and `queue_freshness_seconds` are most interesting when the
/// pipeline has gone quiet, and that is precisely when they stop being exported.
///
/// A constant rather than another environment variable on purpose. This is a liveness signal,
/// not a tuning knob: too fast and it is pointless request volume, too slow and it stops being
/// a heartbeat. Sixty seconds sits comfortably inside the staleness window of every common
/// scrape interval while costing one request a minute from a replica that is doing nothing.
const IDLE_HEARTBEAT: Duration = Duration::from_secs(60);

/// Push self-metrics on their own, with no CloudWatch series attached.
///
/// Deliberately *not* a `flush`: nothing is drained, `BUFFER_SERIES` and the flush counters
/// are left alone (this is not a flush and reporting it as one would corrupt them), and a
/// failure does **not** increment `BATCHES_DROPPED` -- no CloudWatch data was lost, so counting
/// it there would make the data-loss counter fire during an outage in which no data existed.
/// `push_with_retry` already logs the failure, which is the whole of what is owed here.
async fn push_heartbeat<F, Fut>(config: &Config, send: &mut F)
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = anyhow::Result<u16>>,
{
    push_series_only(self_metric_series(), config, send).await
}

/// Push exactly these series and nothing else.
///
/// Split from [`push_heartbeat`] so the empty case can be *reached*. That case is not
/// theoretical the way the encode arms elsewhere in this module are: `self_metric_series`
/// returns an empty vec whenever the registry fails to convert, and the way that happens in
/// practice is somebody registering a histogram — see the note on the `lazy_static!` block in
/// `prometheus.rs`. In that state, without the guard, the writer would push a **completely
/// empty `WriteRequest` every sixty seconds forever**, which some receivers reject and all of
/// them count as traffic, while the operator sees a healthy-looking request rate from an
/// exporter that is in fact exporting nothing.
///
/// Driving it through `push_heartbeat` would mean registering a histogram in the global
/// registry, which never gets torn down and would empty the self-metric payload for every
/// other test in the binary. The seam costs one function; the alternative costs the suite.
async fn push_series_only<F, Fut>(series: Vec<TimeSeries>, config: &Config, send: &mut F)
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = anyhow::Result<u16>>,
{
    if series.is_empty() {
        return;
    }

    let body = match build_request(vec![], series).encode_compressed() {
        Ok(b) => b,
        Err(e) => {
            error!("could not encode heartbeat: {e}");
            return;
        }
    };
    push_with_retry(body, config.push_max_attempts, &mut *send).await;
}

/// Drain the accumulator into one request and push it, or account for losing it.
///
/// # A dropped batch is gone
///
/// `acc.drain()` empties the accumulator before the push, so on [`PushOutcome::Dropped`] the
/// samples do not exist anywhere. That is the intended trade — bounded retry, then drop, so
/// one unreachable remote cannot stall the pipeline behind it — but it means the *only*
/// record of the loss is what is written here and in `push_with_retry`. `BATCHES_DROPPED`
/// alone answers "did we lose data" and not "how much", which is the first question anyone
/// asks; the `error!` supplies the series count so the two together are enough to size an
/// incident from the logs of a single replica.
///
/// # One mutant survives here, knowingly
///
/// Deleting the `BATCHES_DROPPED.inc()` in the *encode* arm (or its `error!`) kills no test,
/// and as with [`self_metric_series`] that is a statement about reachability from inside this
/// process, not a claim that the branch does not matter. `encode_compressed` is
/// `prost::encode_to_vec` followed by `snap::raw::Encoder::compress_vec`, and snappy's only
/// failure is `Error::TooBig`, raised when the input exceeds `MAX_INPUT_SIZE` — `u32::MAX`,
/// four gigabytes of encoded protobuf in a single flush. Reaching it in a test means building
/// that payload. The arm is kept, and written to account for the loss the same way the push
/// arm does, so that a future change to the encoding does not find an unhandled `Result`.
///
/// Note that the accumulator is *already drained* by the time this can fire, so this arm is a
/// real data-loss path and not merely a skipped push — which is why it counts the batch.
async fn flush<F, Fut>(acc: &mut Accumulator, config: &Config, send: &mut F)
where
    F: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = anyhow::Result<u16>>,
{
    if acc.is_empty() {
        return;
    }

    let started = tokio::time::Instant::now();
    let series_count = acc.series_count();
    let request = build_request(acc.drain(), self_metric_series());

    let body = match request.encode_compressed() {
        Ok(b) => b,
        Err(e) => {
            error!("could not encode write request: {e}");
            BATCHES_DROPPED.inc();
            return;
        }
    };

    // `&mut F` implements `FnMut` when `F: FnMut`, so reborrowing satisfies
    // `push_with_retry`'s by-value parameter without giving up ownership of `send`.
    if push_with_retry(body, config.push_max_attempts, &mut *send).await == PushOutcome::Dropped {
        error!("dropped a batch of {series_count} series after exhausting the push policy");
        BATCHES_DROPPED.inc();
    }

    // `BUFFER_SERIES`: sound because this task is the only writer of `acc` and it is inside
    // `flush`, so no batch can have been accumulated during the push -- anything that arrived
    // is still in the channel, uncounted, and will set this again on the next receive. But
    // the next receive is also what overwrites this 0 before anyone gathers it, so the
    // exported value is always the pre-drain depth and the remote never sees 0 at all.
    BUFFER_SERIES.set(0.0);

    // These counters are updated after the push because its duration is unknowable before
    // the await. Although this flush's payload was gathered earlier, the cumulative values
    // preserve every completed flush for the next export; a receiver can calculate average
    // duration with rate(duration_total) / rate(count_total) without sampling one arbitrary
    // flush. A histogram CANNOT be used here -- see the note in `prometheus.rs`.
    FLUSH_DURATION_SECONDS_TOTAL.inc_by(started.elapsed().as_secs_f64());
    FLUSH_COUNT_TOTAL.inc();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testlog::LogTail;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::mpsc;

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
    /// We register no histograms today, and the `lazy_static!` block in `prometheus.rs` says
    /// at length why none may be added — so this pins the hazard against the parser directly
    /// rather than polluting the process-global registry with a histogram that every other
    /// test would then inherit.
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

    /// A `send` that never resolves must be abandoned, not waited on.
    ///
    /// This is the hole `PER_ATTEMPT_TIMEOUT` exists to close, and it was a live bug rather
    /// than a hypothetical: `MAX_TOTAL_RETRY_TIME` is only consulted before a *sleep*, and it
    /// reads `started.elapsed()` after `send` has already returned — so a `send` that never
    /// returns is never checked. `reqwest::Client::new()` sets no default request timeout, so
    /// a blackholed connection to the remote-write endpoint would hang the one task draining
    /// the channel, permanently, with no liveness signal.
    ///
    /// The whole call is wrapped in `within` so that deleting the timeout fails this test
    /// instead of hanging the suite forever.
    #[tokio::test(start_paused = true)]
    async fn a_send_that_never_responds_is_abandoned_and_counted_as_a_failed_attempt() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();

        let outcome = within(
            "push_with_retry to give up on a non-resolving send",
            push_with_retry(vec![1], 3, move |_body| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    // Never resolves, exactly like a connection to a blackholed endpoint.
                    std::future::pending::<anyhow::Result<u16>>().await
                }
            }),
        )
        .await;

        assert_eq!(outcome, PushOutcome::Dropped);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "a timed-out attempt must consume an attempt like any other transport failure"
        );
    }

    /// The timeout must be *retryable*, not fatal. A single stalled connection followed by a
    /// working one is the ordinary shape of a receiver restarting behind a load balancer;
    /// treating the elapse as a hard drop would discard a batch the very next attempt would
    /// have delivered.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_attempt_is_retried_and_can_still_succeed() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();

        let outcome = within(
            "a retry after a timeout",
            push_with_retry(vec![1], 3, move |_body| {
                let c = c.clone();
                async move {
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        std::future::pending::<anyhow::Result<u16>>().await
                    } else {
                        Ok(200u16)
                    }
                }
            }),
        )
        .await;

        assert_eq!(outcome, PushOutcome::Delivered);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The other side of the bound: a request that is merely *slow* must not be cut off.
    /// A loaded remote-write receiver, or a multi-megabyte batch over a slow link, can
    /// legitimately take seconds — and a timeout that fired on those would add request volume
    /// at exactly the moment the receiver is least able to absorb it. Nine virtual seconds is
    /// inside the ten-second bound and must deliver on the first attempt.
    #[tokio::test(start_paused = true)]
    async fn a_slow_but_responsive_send_is_not_cut_off() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();

        let outcome = within(
            "a slow success",
            push_with_retry(vec![1], 3, move |_body| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(PER_ATTEMPT_TIMEOUT - Duration::from_secs(1)).await;
                    Ok(200u16)
                }
            }),
        )
        .await;

        assert_eq!(outcome, PushOutcome::Delivered);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a request inside the bound must not be retried"
        );
    }

    /// The ceiling on the whole call, stated as a number rather than as an intention.
    ///
    /// `MAX_TOTAL_RETRY_TIME` alone never bounded this — see its doc comment. With the
    /// per-attempt timeout in place the worst case is one budget plus one attempt, because
    /// the final attempt is entered while still inside the budget and may then run to its own
    /// timeout. A thousand attempts against a dead endpoint must therefore cost ~30s of
    /// writer stall, not eight minutes and not forever.
    #[tokio::test(start_paused = true)]
    async fn a_blackholed_endpoint_cannot_stall_the_writer_indefinitely() {
        let _log = LogTail::start();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let started = Instant::now();

        let outcome = within(
            "a bounded give-up against a blackholed endpoint",
            push_with_retry(vec![1], 1000, move |_body| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<anyhow::Result<u16>>().await
                }
            }),
        )
        .await;

        let elapsed = started.elapsed();
        let ceiling = MAX_TOTAL_RETRY_TIME + PER_ATTEMPT_TIMEOUT;
        assert_eq!(outcome, PushOutcome::Dropped);
        assert!(
            elapsed <= ceiling,
            "stalled the writer for {elapsed:?}; the documented ceiling is {ceiling:?}"
        );
        let calls = calls.load(Ordering::SeqCst);
        assert!(
            calls < 1000,
            "the budget must cut the loop short of the attempt count, made {calls} calls"
        );
        assert!(
            calls > 1,
            "a hung endpoint must still be retried, not abandoned after one attempt: {calls}"
        );
    }

    /// The elapse must be diagnosable. "Nothing happened for ten seconds" and "the connection
    /// was refused" call for completely different investigations — one is a network path or a
    /// hung receiver, the other is a wrong address or a dead process — so the log must not
    /// collapse them into a single "remote write failed".
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_attempt_says_it_timed_out() {
        let tail = LogTail::start();
        within(
            "a timed-out push",
            push_with_retry(vec![1], 1, |_body| async {
                std::future::pending::<anyhow::Result<u16>>().await
            }),
        )
        .await;
        let logs = tail.tail();

        assert!(
            logs.contains("did not respond within 10s"),
            "a timeout must be distinguishable from a transport error, got: {logs}"
        );
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

    // ---------------------------------------------------------------------------------
    // run_writer
    //
    // # Every test here holds a `LogTail`, and it is the isolation, not just the capture
    //
    // `run_writer` writes `BUFFER_SERIES`, the flush counters and `BATCHES_DROPPED`, which are
    // plain (non-`Vec`) self-metrics with a const `instance` label and therefore *one*
    // process-wide value each — there is no dimension to isolate a test on. Two writer tests
    // running concurrently would race on them, and so would
    // `prometheus::tests::every_self_metric_registers_with_the_instance_const_label`, which
    // asserts `BUFFER_SERIES.get() == 3.0` on the same static.
    //
    // `prometheus.rs` solved its version of this by consolidating every write into a single
    // test. That does not generalise here: the writer's self-metric updates are spread across
    // the flush path and the receive path, and folding six behaviours into one test would
    // make the failures unreadable. The `LogTail` lock is a strictly better fit — it is
    // already process-wide, it is already held by the `prometheus.rs` test that reads these
    // statics, and rule "every test in a module that logs must hold it" independently
    // requires it here, since `run_writer` logs on the drop and shutdown paths.
    //
    // Bind it to a named `_log`. `let _ = LogTail::start()` drops the guard immediately.
    // ---------------------------------------------------------------------------------

    /// What [`flush_spy`] hands back, named so the return type stays readable.
    ///
    /// `std::future::Ready` rather than an `async move` block because an `async` block's type
    /// is unnameable, and `run_writer`'s `Fut` parameter needs a concrete one for the closure
    /// to be returned from a function at all.
    trait SpySend: FnMut(Vec<u8>) -> std::future::Ready<anyhow::Result<u16>> {}
    impl<T: FnMut(Vec<u8>) -> std::future::Ready<anyhow::Result<u16>>> SpySend for T {}

    /// Observe flushes without a wall clock.
    ///
    /// The writer hands each pushed body to an unbounded channel and the test awaits it.
    /// This replaces the "poll a counter every 10ms until it moves" shape, which is a
    /// probabilistic detector: it reports a pass whenever the flush happens *at all* within
    /// the timeout, so it cannot distinguish "flushed for the reason under test" from
    /// "flushed for some other reason, slowly".
    ///
    /// `std::future::ready` rather than an `async move` block so the closure has a nameable
    /// return type and can be handed back from a function.
    fn flush_spy(status: u16) -> (impl SpySend, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let send = move |body: Vec<u8>| {
            // Ignore a closed receiver rather than panicking inside the writer task: a test
            // that has finished asserting should not turn into a panic in a detached task.
            let _ = tx.send(body);
            std::future::ready(Ok(status))
        };
        (send, rx)
    }

    fn batch(name: &str, timestamp: i64) -> SeriesBatch {
        vec![(
            labels(name),
            Sample {
                value: 1.0,
                timestamp,
            },
        )]
    }

    /// Decode the bytes the writer actually pushed.
    ///
    /// Asserting on the pushed *body* rather than on the `WriteRequest` that was built is a
    /// different and stronger claim: the encode step sits between the two, and it is where
    /// "the body is snappy-compressed protobuf the remote can read" is either true or
    /// silently not. Both `expect`s below are part of the assertion, not ceremony.
    fn decode_body(body: &[u8]) -> WriteRequest {
        let raw = snap::raw::Decoder::new()
            .decompress_vec(body)
            .expect("body must be snappy-compressed");
        prost::Message::decode(raw.as_slice()).expect("body must decode as a WriteRequest")
    }

    /// A sound synchronisation point under `start_paused`, and the reason these tests do not
    /// need retries or generous windows.
    ///
    /// Tokio only auto-advances the virtual clock when **every** task is idle. `run_writer`
    /// is idle exactly when its `rx.recv()` is pending, which is exactly when the channel is
    /// empty — so a `sleep` that returns proves the writer has already consumed and processed
    /// every batch sent before it. That is a guarantee, not a probability, which is what
    /// separates this from `sleep(Duration::from_millis(10))` on a real clock.
    async fn let_the_writer_catch_up() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    /// Await something that must happen, and *fail* rather than hang if it does not.
    ///
    /// Under `start_paused` a test whose only remaining work is a never-ready future has no
    /// timer to advance to, so it blocks forever rather than finishing. Several of the
    /// mutations these tests exist to catch — removing the ticker arm, removing the shutdown
    /// return — produce exactly that, and a hung CI job is a much worse signal than a failed
    /// one: it reports nothing, it reports it slowly, and it usually gets retried. The bound
    /// is sixty *virtual* seconds, so it costs a healthy test nothing.
    ///
    /// The bound must stay comfortably clear of every deadline a test legitimately waits on,
    /// `IDLE_HEARTBEAT` above all. At sixty seconds it *tied* with the heartbeat's own
    /// deadline and won, so `the_idle_heartbeat_fires_once_per_quiet_period` failed
    /// against a perfectly correct heartbeat. Expressed as a multiple of `IDLE_HEARTBEAT` so
    /// retuning that constant cannot silently reintroduce the collision.
    async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
        match tokio::time::timeout(IDLE_HEARTBEAT * 5, fut).await {
            Ok(value) => value,
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }

    #[tokio::test]
    async fn writer_flushes_when_series_threshold_is_reached() {
        let _log = LogTail::start();
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

        tx.send(vec![(
            labels("a"),
            Sample {
                value: 1.0,
                timestamp: 1,
            },
        )])
        .await
        .unwrap();
        tx.send(vec![(
            labels("b"),
            Sample {
                value: 1.0,
                timestamp: 1,
            },
        )])
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            while flushed.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("writer should flush once the series threshold is reached");

        drop(tx);
        within("the writer to exit", handle).await.unwrap();
    }

    #[tokio::test]
    async fn writer_flushes_remaining_series_on_shutdown() {
        let _log = LogTail::start();
        let (tx, rx) = mpsc::channel(16);
        let flushed = Arc::new(AtomicUsize::new(0));
        let f = flushed.clone();

        // Huge interval and threshold: only the shutdown path can flush this.
        let config =
            crate::config::Config::from_values(Some("3600"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, move |_body| {
            let f = f.clone();
            async move {
                f.fetch_add(1, Ordering::SeqCst);
                Ok(200u16)
            }
        }));

        tx.send(vec![(
            labels("a"),
            Sample {
                value: 1.0,
                timestamp: 1,
            },
        )])
        .await
        .unwrap();
        drop(tx);
        within("the writer to exit", handle).await.unwrap();

        assert_eq!(
            flushed.load(Ordering::SeqCst),
            1,
            "pending series must not be lost on shutdown"
        );
    }

    /// The ticker arm, on its own. The threshold is set out of reach, so the *only* thing
    /// that can push here is the interval — and the assertion is on the exact virtual instant
    /// it fires, not merely that it did. Under `start_paused` tokio advances the clock to the
    /// next deadline and no further, so `elapsed() == 1s` distinguishes "the interval flushed
    /// it" from "something else flushed it and the interval was never involved".
    #[tokio::test(start_paused = true)]
    async fn the_interval_flushes_on_its_own_when_the_threshold_is_out_of_reach() {
        let _log = LogTail::start();
        let (send, mut flushes) = flush_spy(200);
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("1"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, send));

        tx.send(batch("a", 1)).await.unwrap();

        let started = Instant::now();
        let body = within("the interval flush", flushes.recv())
            .await
            .expect("the interval must flush without help from the threshold");
        assert!(!body.is_empty());
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1),
            "flushed off-schedule: the interval arm is not what pushed this"
        );

        drop(tx);
        within("the writer to exit", handle).await.unwrap();
    }

    /// The test above states the ticker's schedule; this one is the net that actually holds
    /// it, and the difference is measured.
    ///
    /// `tokio::select!` picks at random among branches that are *simultaneously* ready, and
    /// `tokio::time::interval`'s first tick is ready from the moment the writer starts and
    /// stays ready until an arm wins it. So whether a buffered batch gets flushed immediately
    /// or one full interval later comes down to a coin flip in the first loop iteration:
    /// swapping `interval_at` back to `interval` was measured killing the single-trial test
    /// **14 runs in 25**. A regression CI catches half the time is not caught.
    ///
    /// Twelve independent trials put a surviving mutant at 2^-12, which makes this a
    /// deterministic guard in practice. Keep both tests: the one above reads as the
    /// specification of the interval, this one is what enforces it. (Same reasoning, and the
    /// same shape, as `ordering_holds_for_enough_samples_that_luck_cannot_explain_it`.)
    #[tokio::test(start_paused = true)]
    async fn a_buffered_batch_is_never_flushed_before_the_first_interval_elapses() {
        let _log = LogTail::start();
        for trial in 0..12 {
            let (send, mut flushes) = flush_spy(200);
            let (tx, rx) = mpsc::channel(16);
            let config = Config::from_values(Some("1"), Some("100000"), None, Some("1"));
            let handle = tokio::spawn(run_writer(rx, config, send));

            tx.send(batch("a", 1)).await.unwrap();
            // The writer has not been polled yet -- `send` on a channel with a free slot
            // completes without yielding -- so it builds its ticker at this same instant.
            let started = Instant::now();
            within("a flush", flushes.recv())
                .await
                .expect("the interval must flush the batch");
            assert_eq!(
                started.elapsed(),
                Duration::from_secs(1),
                "trial {trial}: flushed off-schedule; a first tick that is ready immediately \
                 flushes whatever happens to be buffered, whenever it happens to win the select"
            );

            drop(tx);
            within("the writer to exit", handle).await.unwrap();
        }
    }

    /// The `is_empty` early return, and the strongest form of the assertion: not "the body
    /// was non-empty" but "nothing was sent at all".
    ///
    /// `!body.is_empty()` is a weak guard here and it is worth saying why, because the given
    /// threshold test uses it. A flush with an empty accumulator still calls
    /// `self_metric_series()`, and the self-metrics are never empty — so removing the early
    /// return produces a perfectly non-empty body containing nothing but this process's own
    /// counters, ten times a second, forever. Only "no push happened" catches that.
    #[tokio::test(start_paused = true)]
    async fn an_idle_writer_pushes_nothing_before_the_heartbeat_is_due() {
        let _log = LogTail::start();
        let (send, mut flushes) = flush_spy(200);
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("1"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, send));

        // Deliberately inside the heartbeat window: this test owns the "an empty tick sends
        // nothing" property, and `the_idle_heartbeat_fires_once_per_quiet_period`
        // owns the property that silence eventually ends. Asserting the relationship rather
        // than hardcoding ten seconds keeps them from drifting into contradiction if
        // `IDLE_HEARTBEAT` is ever retuned.
        let quiet = Duration::from_secs(10);
        assert!(
            quiet < IDLE_HEARTBEAT,
            "this test is only meaningful inside the heartbeat window"
        );
        tokio::time::sleep(quiet).await;
        assert!(
            flushes.try_recv().is_err(),
            "an idle tick must not push a self-metrics-only body before the heartbeat is due"
        );

        drop(tx);
        within("the writer to exit", handle).await.unwrap();
        assert!(
            flushes.try_recv().is_err(),
            "shutdown with nothing buffered must not push either"
        );
    }

    /// Silence must eventually end. An exporter that pushes nothing while idle is
    /// indistinguishable at the remote from an exporter that has died, and every
    /// `firehose_self_*` series goes stale exactly when someone would go looking at it.
    ///
    /// The assertion is on the exact virtual instant, which is what makes this a test of the
    /// heartbeat rather than of "something eventually happened": the heartbeat is due
    /// `IDLE_HEARTBEAT` after the last push *attempt*, so a first flush at t=1s puts the
    /// heartbeat at t=61s and nowhere else.
    #[tokio::test(start_paused = true)]
    async fn the_idle_heartbeat_fires_once_per_quiet_period() {
        let _log = LogTail::start();
        let (send, mut flushes) = flush_spy(200);
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("1"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, send));

        // A real flush first, so the heartbeat clock starts from a known push rather than
        // from the writer's construction.
        tx.send(batch("cloudwatch_only_series", 1)).await.unwrap();
        let first = within("the interval flush", flushes.recv())
            .await
            .expect("the interval must flush the batch");
        let flushed_at = Instant::now();

        let heartbeat = within("the idle heartbeat", flushes.recv())
            .await
            .expect("an idle writer must eventually push its own metrics");
        assert_eq!(
            flushed_at.elapsed(),
            IDLE_HEARTBEAT,
            "the heartbeat must fire one quiet period after the last push, not sooner or later"
        );

        // The heartbeat carries self-metrics and NO CloudWatch series. The series from the
        // first flush is the discriminator: it was drained, so it must not reappear.
        let first = decode_body(&first);
        assert!(
            find_series(&first.timeseries, "cloudwatch_only_series").is_some(),
            "control: the first push really did carry the CloudWatch series"
        );

        let heartbeat = decode_body(&heartbeat);
        assert!(
            find_series(&heartbeat.timeseries, "firehose_self_buffer_series").is_some(),
            "the heartbeat must carry the app's own metrics -- that is its entire purpose"
        );
        assert!(
            find_series(&heartbeat.timeseries, "cloudwatch_only_series").is_none(),
            "the heartbeat must not resurrect drained CloudWatch series"
        );

        // No series may appear twice. `sorted()` does not deduplicate — two `TimeSeries`
        // sharing a label set stay two entries and the receiver reads them as duplicate
        // samples for one series, which is the error this whole pipeline exists to avoid.
        // The heartbeat builds its payload from one source, so a duplicate here means
        // something is feeding `build_request` the same series on both sides.
        let mut seen = std::collections::HashSet::new();
        for series in &heartbeat.timeseries {
            let key: Vec<(&str, &str)> = series
                .labels
                .iter()
                .map(|l| (l.name.as_str(), l.value.as_str()))
                .collect();
            assert!(
                seen.insert(key.clone()),
                "the heartbeat pushed the same label set twice: {key:?}"
            );
        }

        // A heartbeat, not a heartbeat storm. `last_push_attempt` must be restamped by the
        // heartbeat itself; if it is not, every subsequent tick also sees a quiet period
        // elapsed and the writer pushes once per `FLUSH_INTERVAL_SECS` forever — sixty times
        // the intended request volume, from a replica that is doing nothing.
        let first_beat_at = Instant::now();
        within("a second heartbeat", flushes.recv())
            .await
            .expect("the heartbeat must keep repeating while idle");
        assert_eq!(
            first_beat_at.elapsed(),
            IDLE_HEARTBEAT,
            "heartbeats must be one quiet period apart, not one flush interval apart"
        );

        drop(tx);
        within("the writer to exit", handle).await.unwrap();
    }

    /// An empty heartbeat must not go on the wire at all.
    ///
    /// This is reachable in production, unlike the encode guards elsewhere in this module:
    /// `self_metric_series` returns empty whenever the registry fails to convert, and the way
    /// that happens is a registered histogram. Without the guard the writer would push a
    /// completely empty `WriteRequest` once a quiet period, forever — traffic that looks
    /// healthy on a request-rate graph and carries nothing at all, from an exporter that has
    /// silently stopped exporting.
    #[tokio::test(start_paused = true)]
    async fn an_empty_heartbeat_is_not_pushed() {
        let _log = LogTail::start();
        let (mut send, mut flushes) = flush_spy(200);
        let config = Config::from_values(Some("1"), Some("100000"), None, Some("1"));

        push_series_only(vec![], &config, &mut send).await;
        assert!(
            flushes.try_recv().is_err(),
            "an empty series list must not produce a request"
        );

        // Control: the same call with real series does push, so the assertion above is about
        // the guard and not about `push_series_only` being inert.
        push_series_only(self_metric_series(), &config, &mut send).await;
        assert!(
            flushes.try_recv().is_ok(),
            "CONTROL: a non-empty series list must push"
        );
    }

    /// A heartbeat that fails must not be counted as lost data. `BATCHES_DROPPED` is the
    /// signal that CloudWatch samples were destroyed, and a heartbeat carries none — it is
    /// rebuilt from the registry every time and nothing is drained to produce it. Counting it
    /// would make the data-loss counter climb steadily during an outage in which no data
    /// existed to lose, which is precisely when someone is reading that counter to decide how
    /// bad things are.
    #[tokio::test(start_paused = true)]
    async fn a_failed_heartbeat_is_not_counted_as_dropped_data() {
        let _log = LogTail::start();
        let before = BATCHES_DROPPED.get();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("1"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, move |_body| {
            let c = c.clone();
            c.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(503u16))
        }));

        // Nothing is ever sent, so the only thing that can push here is the heartbeat.
        tokio::time::sleep(IDLE_HEARTBEAT + Duration::from_secs(5)).await;
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "control: the heartbeat must actually have been attempted"
        );
        assert_eq!(
            BATCHES_DROPPED.get(),
            before,
            "a failed heartbeat loses no CloudWatch data and must not touch the drop counter"
        );

        drop(tx);
        within("the writer to exit", handle).await.unwrap();
    }

    /// The heartbeat must not be able to mask a stall, which is the failure mode that would
    /// make it worse than useless: a long push suppresses ticks, and if those ticks queued up
    /// they would fire as a burst the moment the push returned, painting a healthy-looking
    /// run of heartbeats over the exact window in which the exporter was wedged.
    ///
    /// Two things prevent it, and this test covers both together.
    /// `MissedTickBehavior::Delay` reschedules from the tick that was actually serviced rather
    /// than replaying the missed ones, and `last_push_attempt` is stamped when the push
    /// *completes*. So a thirty-second push is followed by a full quiet period of silence, not
    /// by thirty seconds' worth of backlogged heartbeats.
    #[tokio::test(start_paused = true)]
    async fn a_long_push_is_not_followed_by_a_burst_of_heartbeats() {
        let _log = LogTail::start();
        let pushes = Arc::new(AtomicUsize::new(0));
        let p = pushes.clone();
        let (tx, rx) = mpsc::channel(16);

        // A slow *success*, deliberately inside `PER_ATTEMPT_TIMEOUT`. The first draft used
        // thirty seconds and was silently measuring something else entirely: the per-attempt
        // timeout abandoned the push at ten, so the test observed a timed-out attempt rather
        // than a long one. Derived from the constant so it cannot drift out of range again.
        let slow_push = PER_ATTEMPT_TIMEOUT - Duration::from_secs(2);
        assert!(
            slow_push < PER_ATTEMPT_TIMEOUT,
            "this test needs a push that completes, not one that is abandoned"
        );

        // Threshold of one so the batch pushes immediately; at a one-second interval, seven
        // ticks are missed while the push runs.
        let config = Config::from_values(Some("1"), Some("1"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, move |_body| {
            let p = p.clone();
            async move {
                p.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(slow_push).await;
                Ok(200u16)
            }
        }));

        tx.send(batch("a", 1)).await.unwrap();
        // Past the end of the push and still inside the quiet period that must follow it:
        // the heartbeat is due `slow_push + IDLE_HEARTBEAT` in, so a full `IDLE_HEARTBEAT`
        // from t=0 is comfortably before it and comfortably after the missed ticks.
        tokio::time::sleep(IDLE_HEARTBEAT).await;

        assert_eq!(
            pushes.load(Ordering::SeqCst),
            1,
            "the missed ticks must not replay as a burst of heartbeats once the push returns"
        );

        drop(tx);
        within("the writer to exit", handle).await.unwrap();
    }

    /// The threshold is a `>=` on a count, so it has two ways to be wrong and this watches
    /// the other one: `writer_flushes_when_series_threshold_is_reached` catches a threshold
    /// that never fires, this catches one that fires too early (an off-by-one, a `>` that
    /// became `>=` on the wrong side, or a check deleted in favour of flushing every batch).
    ///
    /// The timeout is 60 virtual seconds against a 3600-second interval, so a threshold that
    /// never fires ends this promptly with a clear failure rather than hanging: with no other
    /// timer to advance to, tokio jumps to the timeout and it expires.
    #[tokio::test(start_paused = true)]
    async fn a_batch_under_the_threshold_does_not_flush() {
        let _log = LogTail::start();
        let (send, mut flushes) = flush_spy(200);
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("3600"), Some("3"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, send));

        tx.send(batch("a", 1)).await.unwrap();
        tx.send(batch("b", 1)).await.unwrap();
        let_the_writer_catch_up().await;
        assert!(
            flushes.try_recv().is_err(),
            "two series must not trip a threshold of three"
        );

        tx.send(batch("c", 1)).await.unwrap();
        let body = tokio::time::timeout(Duration::from_secs(60), flushes.recv())
            .await
            .expect("the third series must trip the threshold")
            .expect("writer should still be running");
        assert!(!body.is_empty());

        drop(tx);
        within("the writer to exit", handle).await.unwrap();
    }

    /// Every self-metric the writer touches, in one test.
    ///
    /// Consolidated deliberately: these are plain (non-`Vec`) statics with a const `instance`
    /// label, so each has exactly one process-wide value and no dimension to isolate on.
    /// Splitting this into "one test per gauge" would have them racing each other on the same
    /// two floats. The `LogTail` lock keeps them away from `prometheus.rs`'s reader of the
    /// same statics; keeping them in one test keeps them away from each other.
    ///
    /// Flush duration is asserted against a *virtual* two-second push, which is why the send
    /// closure sleeps instead of returning `ready`: with an instant push the counter delta
    /// would be 0.0 and an assertion of `>= 0.0` would pass if it were never updated at all.
    #[tokio::test(start_paused = true)]
    async fn the_writer_reports_its_buffer_depth_and_flush_duration() {
        let _log = LogTail::start();
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("3600"), Some("3"), None, Some("1"));
        let duration_before = FLUSH_DURATION_SECONDS_TOTAL.get();
        let count_before = FLUSH_COUNT_TOTAL.get();
        let handle = tokio::spawn(run_writer(rx, config, |_body| async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(200u16)
        }));

        tx.send(batch("a", 1)).await.unwrap();
        tx.send(batch("b", 1)).await.unwrap();
        let_the_writer_catch_up().await;
        assert_eq!(
            BUFFER_SERIES.get(),
            2.0,
            "buffer depth must track what is actually accumulated"
        );

        // Trips the threshold of three, so the flush runs and the two-second push completes.
        tx.send(batch("c", 1)).await.unwrap();
        drop(tx);
        within("the writer to exit", handle).await.unwrap();

        assert_eq!(
            BUFFER_SERIES.get(),
            0.0,
            "the buffer must read empty once it has been drained and pushed"
        );
        assert_eq!(
            FLUSH_DURATION_SECONDS_TOTAL.get() - duration_before,
            2.0,
            "flush duration total must accumulate the push duration"
        );
        assert_eq!(
            FLUSH_COUNT_TOTAL.get() - count_before,
            1.0,
            "flush count must increment exactly once per completed flush"
        );
    }

    /// A dropped batch must be both counted and *sized*. `BATCHES_DROPPED` answers "did we
    /// lose data"; on its own it cannot answer "how much", which is the first question asked
    /// during an incident and the one that decides whether anyone needs to be woken up.
    /// The series count in the log is the only place that number exists — `acc.drain()` has
    /// already destroyed the evidence by the time the push fails.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_batch_is_counted_and_its_size_is_logged() {
        let tail = LogTail::start();
        let before = BATCHES_DROPPED.get();
        let (tx, rx) = mpsc::channel(16);
        // Threshold of one, a single attempt, and a permanent 503: exactly one dropped batch.
        let config = Config::from_values(Some("3600"), Some("1"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, |_body| {
            std::future::ready(Ok(503u16))
        }));

        tx.send(batch("only-series", 1)).await.unwrap();
        drop(tx);
        within("the writer to exit", handle).await.unwrap();

        let logs = tail.tail();
        assert_eq!(
            BATCHES_DROPPED.get(),
            before + 1.0,
            "an abandoned batch must be counted exactly once"
        );
        assert!(
            logs.contains("dropped a batch of 1 series"),
            "the drop must say how much was lost, got: {logs}"
        );
    }

    /// A successful flush must not touch the drop counter. Without this, incrementing
    /// `BATCHES_DROPPED` unconditionally — or on the wrong side of the comparison — would go
    /// unnoticed by the test above, which only ever asserts that the counter *did* move.
    #[tokio::test(start_paused = true)]
    async fn a_delivered_batch_is_not_counted_as_dropped() {
        let _log = LogTail::start();
        let before = BATCHES_DROPPED.get();
        let (send, mut flushes) = flush_spy(200);
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("3600"), Some("1"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, send));

        tx.send(batch("delivered", 1)).await.unwrap();
        drop(tx);
        within("the writer to exit", handle).await.unwrap();

        assert!(
            flushes.try_recv().is_ok(),
            "the batch should have been sent"
        );
        assert_eq!(
            BATCHES_DROPPED.get(),
            before,
            "a 200 must not increment the drop counter"
        );
    }

    /// The shutdown drain must announce itself. It is the one flush with no next flush behind
    /// it: a failure here loses the tail of the data permanently, and it happens while the
    /// process is exiting and least likely to be watched. `writer_flushes_remaining_series_on
    /// _shutdown` proves the flush happens; this proves it is attributable afterwards.
    #[tokio::test(start_paused = true)]
    async fn the_shutdown_drain_says_how_much_it_is_flushing() {
        let tail = LogTail::start();
        let (send, _flushes) = flush_spy(200);
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("3600"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, send));

        tx.send(batch("a", 1)).await.unwrap();
        tx.send(batch("b", 1)).await.unwrap();
        drop(tx);
        within("the writer to exit", handle).await.unwrap();

        let logs = tail.tail();
        assert!(
            logs.contains("channel closed; flushing 2 buffered series before exit"),
            "the final drain must name what it is carrying out, got: {logs}"
        );
    }

    /// ...and must stay quiet when there is nothing to carry out. A line on every clean
    /// shutdown is the kind of noise that trains operators to skip the one that matters.
    #[tokio::test(start_paused = true)]
    async fn an_empty_shutdown_drain_is_silent() {
        let tail = LogTail::start();
        let (send, _flushes) = flush_spy(200);
        let (tx, rx) = mpsc::channel::<SeriesBatch>(16);
        let config = Config::from_values(Some("3600"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, send));

        drop(tx);
        within("the writer to exit", handle).await.unwrap();

        let logs = tail.tail();
        assert!(
            !logs.contains("channel closed"),
            "an empty shutdown must not announce a drain, got: {logs}"
        );
    }

    /// The backpressure design, pinned.
    ///
    /// `flush` is awaited from inside the `select!` body, so a push in flight stops the writer
    /// draining `rx` entirely. That is intentional — see [`run_writer`] — and it is the kind
    /// of property that a well-meaning refactor ("why are we blocking the loop on I/O?")
    /// removes without noticing, trading a bounded channel and a 503 back to Firehose for an
    /// unbounded in-memory buffer that grows for the whole duration of a remote outage.
    ///
    /// Making the push concurrent would make this test fail, which is the point.
    #[tokio::test(start_paused = true)]
    async fn nothing_drains_the_channel_while_a_push_is_in_flight() {
        let _log = LogTail::start();
        // Capacity two, so "the channel is full" is reached by two sends rather than by
        // guessing at a default.
        let (tx, rx) = mpsc::channel(2);
        let config = Config::from_values(Some("3600"), Some("1"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, |_body| async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(200u16)
        }));

        // Threshold of one: the writer takes this batch and goes straight into a ten-second
        // push, leaving the channel unattended.
        tx.send(batch("a", 1)).await.unwrap();
        let_the_writer_catch_up().await;

        tx.try_send(batch("b", 1)).expect("first slot is free");
        tx.try_send(batch("c", 1)).expect("second slot is free");
        assert!(
            matches!(
                tx.try_send(batch("d", 1)),
                Err(mpsc::error::TrySendError::Full(_))
            ),
            "a push in flight must stop the writer draining, so the channel fills and the \
             handler can push back on Firehose"
        );

        // And once the push finishes the writer catches up, so this is backpressure rather
        // than a deadlock.
        tokio::time::sleep(Duration::from_secs(20)).await;
        tx.try_send(batch("e", 1))
            .expect("the channel must drain again once the push completes");

        drop(tx);
        within("the writer to exit", handle).await.unwrap();
    }

    /// Batches are accumulated, not overwritten: two receives before a flush must both be in
    /// the payload. Without this, dropping the `for` loop's insert — or replacing the
    /// accumulator wholesale on each receive — would still flush, still push a non-empty
    /// body, and silently export only the last batch of every window.
    #[tokio::test(start_paused = true)]
    async fn every_received_batch_reaches_the_payload() {
        let _log = LogTail::start();
        let (send, mut flushes) = flush_spy(200);
        let (tx, rx) = mpsc::channel(16);
        let config = Config::from_values(Some("3600"), Some("100000"), None, Some("1"));
        let handle = tokio::spawn(run_writer(rx, config, send));

        // One batch carrying two samples for one series, and two more series after it.
        tx.send(vec![
            (
                labels("multi"),
                Sample {
                    value: 1.0,
                    timestamp: 100,
                },
            ),
            (
                labels("multi"),
                Sample {
                    value: 2.0,
                    timestamp: 200,
                },
            ),
        ])
        .await
        .unwrap();
        tx.send(batch("second", 1)).await.unwrap();
        tx.send(batch("third", 1)).await.unwrap();
        let_the_writer_catch_up().await;
        assert_eq!(BUFFER_SERIES.get(), 3.0, "three distinct series buffered");

        drop(tx);
        within("the writer to exit", handle).await.unwrap();

        let body = flushes.recv().await.expect("shutdown must flush");
        let decoded = decode_body(&body);

        for name in ["multi", "second", "third"] {
            let found = find_series(&decoded.timeseries, name)
                .unwrap_or_else(|| panic!("{name} missing from the pushed payload"));
            if name == "multi" {
                assert_eq!(
                    samples_of(found),
                    vec![(100, 1.0), (200, 2.0)],
                    "both samples of a repeated series must survive"
                );
            }
        }
        assert!(
            find_series(&decoded.timeseries, "firehose_self_buffer_series").is_some(),
            "the same push must also carry the app's own metrics"
        );
    }
}
