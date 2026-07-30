# Direct TimeSeries construction and a single writer task

Date: 2026-07-30
Issues: #2 (direct TimeSeries), #3 (writer task), #8 (self-metrics), #9 (return 200 once buffered)

## Problem

Prometheus reports out-of-order samples and Thanos compactor reports duplicate timestamps.
Both trace back to one root cause: the CloudWatch data path stages every sample in the
`prometheus` client library's global registry, serializes it to text format, and re-parses
that text into remote-write protobuf.

```rust
// src/prometheus.rs
let metric_families = prometheus::gather();
let text_metric_families = TextEncoder::new().encode_to_string(&metric_families)?;
let encoded_write_request = WriteRequest::from_text_format(text_metric_families).unwrap();
```

The client library's data model cannot represent what CloudWatch metric streams send:

1. **One sample per series per push.** The timestamp is a single `AtomicI64` on the child
   `Value` (`rust-prometheus/src/value.rs:35`). A single Firehose POST routinely carries the
   same series at several minute-aligned timestamps, because Firehose's buffering interval
   is at least the 1-minute metric resolution. Last write wins; the rest are dropped. When
   the last line carries an earlier timestamp than one already pushed, we emit an
   out-of-order sample.

2. **A fixed label set per metric name.** `GaugeVec` requires a fixed label vector at
   registration. That constraint is the only reason `get_dimensions` exists.

3. **A process-global registry shared by concurrent handlers.** `prometheus::gather()`
   collects the whole registry, not just the calling request's samples, and
   `clear_collectors()` then wipes it. Concurrent requests contaminate each other, push in
   nondeterministic order, and can have their `GaugeVec` handles unregistered mid-flight.

## Goals

- Represent multiple samples per series, as CloudWatch actually delivers them.
- Remove the global registry from the CloudWatch data path.
- Serialize all pushes through one task so ordering is deterministic.
- Stop manufacturing duplicates by returning 500 and inviting a Firehose replay.
- Never panic on malformed input.

## Non-goals

- Cross-batch high-water mark (#4).
- Reorder buffer (#5).

Both become straightforward once the writer owns the flush; they are deliberately deferred
so this change stays reviewable.

## Decisions

### Labels come straight off each record

Today `record_metric` seeds every label from `get_dimensions` with `""` and overwrites only
the dimensions the record carries, so a rollup record emits `target_group=""`.

The Prometheus data model states: *"Labels with an empty label value are considered
equivalent to labels that do not exist."* The padding therefore never reaches storage — it
travels the wire and is discarded at ingestion. Stored series today are already identical to
what omitting the label produces.

| | rollup record | specific record |
|---|---|---|
| today, on the wire | `{load_balancer="app/foo", target_group=""}` | `{load_balancer="app/foo", target_group="tg/bar"}` |
| today, as stored | `{load_balancer="app/foo"}` | `{load_balancer="app/foo", target_group="tg/bar"}` |
| after this change, as stored | `{load_balancer="app/foo"}` | `{load_balancer="app/foo", target_group="tg/bar"}` |

Rollup and specific series remain distinct. `metric{target_group=""}` still selects exactly
the rollups, because in PromQL an empty matcher matches absent labels. Existing queries,
dashboards and recording rules keep working.

Consequence: `get_dimensions`, `fetch_dimension_names`, `normalize_dimension_name` and
`DIMENSION_HASH` are deleted, along with the `ListMetrics` calls, the pagination added in
#6, and the mutex held across an await.

Open item to confirm during implementation: whether CloudWatch emits `"TargetGroup": ""`
explicitly for rollups or omits the key. Taking labels off the record preserves whichever it
does, and Prometheus discards empty values either way, so the outcome is the same. Worth
confirming against a captured payload if one becomes available.

### Handlers convert, the writer only merges

If the writer converted raw `CloudWatchMetric` records it would be a single-threaded CPU
bottleneck on the hot path. Handlers do parsing and conversion in parallel; the writer
merges already-built `(labels, sample)` pairs.

### Returning 200 on enqueue is structural

Once the writer batches across requests, the handler cannot report a push outcome — the push
happens later, for a batch spanning several requests. This is the core of #9 and follows
necessarily from #3.

It also shifts durability. Today a failed push returns 500 and Firehose replays; Firehose is
the retry mechanism. After this change, a 200 means we own the data. Hence the failure
policy below.

## Architecture

```
POST / ──> handler: decode base64 → parse JSON lines → build (labels, sample) pairs
                          │
                          ├── malformed record → skip + count, continue batch
                          │
                          └── try_send on bounded mpsc ──> 200 OK
                                     │ (channel full)
                                     └────────────────────> 503, Firehose replays

writer task (single):
   select! { recv → merge into accumulator , tick → flush }
   accumulator: HashMap<Vec<Label>, BTreeMap<i64, f64>>
   flush: build WriteRequest + merge self-metrics → push → bounded retry
```

### Accumulator

```rust
HashMap<Vec<Label>, BTreeMap<i64, f64>>
```

`Label` derives `Hash + Eq`, so a sorted `Vec<Label>` is a valid key. `BTreeMap<i64, f64>`
provides two required behaviours with no extra code:

- **Dedup by timestamp, last write wins.** Matches CloudWatch's revision semantics, where a
  minute's datapoint is re-emitted with an updated value as late data lands.
- **Timestamp-ordered samples**, as the remote-write specification requires within a series.

At flush, each entry becomes a `TimeSeries { labels, samples }`.

Within-batch dedup is required, not optional: without it a batch carrying the same series at
the same timestamp with two different values would send both and the whole push would be
rejected.

## Module layout

| Module | Responsibility |
|---|---|
| `series.rs` (new) | `CloudWatchMetric` → `Vec<(Vec<Label>, Sample)>`. Pure, no I/O. Absorbs metric naming and label derivation from `record_metric`. |
| `writer.rs` (new) | Writer task, accumulator, flush, retry. |
| `prometheus.rs` | Self-metric statics only. |
| `aws/mod.rs` | `get_freshness` only. |
| `main.rs` | Handler: decode, parse, enqueue, respond. |

Deleted: `GAUGES`, `COUNTERS`, `HISTOGRAMS`, `clear_collectors`, `get_or_register_metric`,
`record_metric`, `record_aggregate`, `get_dimensions`, `fetch_dimension_names`,
`normalize_dimension_name`, `DIMENSION_HASH`.

`COUNTERS` and `HISTOGRAMS` are already unused today; only `GAUGES` was ever populated.

### Self-metrics

Self-metrics keep using the client registry, which is a genuine fit for them. The writer
gathers them at flush time, converts through the existing `from_text_format` path, and
merges the result into the same `WriteRequest`, so a flush is one HTTP call.

This removes the per-request push that gave them duplicate and out-of-order timestamps: they
carry no explicit timestamp, so `prometheus_parse::Scrape::parse` stamps them with
`Utc::now()` at parse time, and at any real request rate two pushes landed in the same
millisecond.

`TOTAL_WRITES_SENT` is currently incremented after `gather()` has already run, so it is
always one push behind. The writer increments it before building the request.

An `instance` label is added, sourced from `HOSTNAME`, falling back to `"unknown"`.

**This applies to self-metrics only.** CloudWatch series must not carry a per-replica label:
the same CloudWatch datapoint delivered to two replicas must remain one series, or we
manufacture exactly the duplicate-series problem we are trying to remove.

## Error handling

| Situation | Behaviour |
|---|---|
| Malformed base64, UTF-8 or JSON line | Skip record, increment `self_records_skipped_count`, continue batch |
| Missing `requestId` | Respond without it; never `unwrap()` |
| Non-ASCII header value | Treat source ARN as absent, log, continue |
| Channel full | 503; Firehose replays and holds the data |
| Push fails (transport, 5xx) | Exponential backoff, bounded attempts, then drop batch and increment `self_batches_dropped_count` |
| Push returns 400 | Log response body, drop batch; retrying a malformed request cannot help |

Bounded retry with a visible drop counter is chosen over retrying indefinitely, which would
grow the buffer until the process OOMs during a prolonged Prometheus outage, and over
dropping immediately, which loses a batch to any transient blip.

The 503-on-full-channel path deliberately hands durability back to Firehose's at-least-once
replay at exactly the moment we cannot hold data ourselves. Non-2xx is returned only when we
genuinely cannot accept the payload.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `PROM_WRITE_ADDR` | (required, existing) | Remote-write endpoint |
| `PROM_USERNAME` / `PROM_PASSWORD` | unset (existing) | Currently read but unused; see note |
| `FLUSH_INTERVAL_SECS` | `1` | Max time before a flush |
| `FLUSH_MAX_SERIES` | `2000` | Flush early once the accumulator reaches this many series |
| `CHANNEL_CAPACITY` | `1024` | Bounded channel depth before 503 |
| `PUSH_MAX_RETRIES` | `3` | Attempts before dropping a batch |

Note: `PROM_USERNAME` and `PROM_PASSWORD` are read in `push_firehose_metrics` today but
never applied to the request. Preserved as-is; wiring up basic auth is out of scope and
should be its own issue.

## New self-metrics

Failure modes that are currently invisible. Names follow the existing `self_*` convention
and inherit the `firehose` namespace from the `app_opts!` macro:

- `self_records_skipped_count` — malformed records dropped during parse
- `self_batches_dropped_count` — batches abandoned after exhausting retries
- `self_buffer_series` — accumulator depth, for tuning `CHANNEL_CAPACITY` and flush thresholds
- `self_flush_duration_seconds` — push latency
- `self_rejected_payloads_count` — 503s returned due to a full channel

## Testing

**`series.rs`** carries the bulk, as pure functions with no mocking: metric naming, unit
suffixes, label derivation, rollup vs specific records, and the `region` dimension collision
(a CloudWatch dimension named `region` must not clash with the top-level `region` label).

**Accumulator:** dedup by timestamp keeps the last value; samples come out timestamp-ordered;
a later batch carrying an earlier timestamp still merges into the correct position; distinct
label sets stay distinct.

**Handler:** malformed base64, malformed UTF-8, malformed JSON and a missing `requestId` all
return a response rather than panicking; a full channel returns 503.

**Writer:** a failing sink is retried up to the bound and then drops with the counter
incremented; a 400 drops without retrying.

Tests must not share process-global state where avoidable. The existing suite already relies
on distinct metric names per test to avoid collisions in the global registry; the new
accumulator and `series.rs` tests have no global state at all, which removes that fragility
from the majority of the suite.

## Rollout

No series identity change is expected, so no dashboard or recording-rule migration should be
needed. Confirm after deploy by checking that existing series continue rather than fork.

The pre-existing `test_convert_to_labels_values` failure is unrelated: it reads
`testdata/just-post-payload.json`, which was never committed. It is touched by this change
only insofar as the code it exercises is being restructured, and will be updated or removed
as part of the work.
