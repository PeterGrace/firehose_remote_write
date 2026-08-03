# firehose_remote_write

Receives Amazon Data Firehose metric-stream payloads and forwards their samples to a Prometheus-compatible remote-write endpoint.

## Configuration

The service requires `PROM_WRITE_ADDR`, the base URL of the remote-write receiver. Standard AWS SDK environment and workload-identity configuration is used for CloudWatch freshness queries.

Writer behavior can be tuned with these environment variables:

| Variable | Default | Description |
| --- | ---: | --- |
| `REORDER_DELAY_SECS` | `60` | Holds CloudWatch samples for this many seconds before they are eligible to be pushed. Set to `0` to disable reordering. |
| `FLUSH_INTERVAL_SECS` | `1` | How often the writer checks for eligible samples. |
| `FLUSH_MAX_SERIES` | `2000` | Also checks for an eligible flush when this many series are buffered. |
| `CHANNEL_CAPACITY` | `1024` | Number of parsed Firehose batches that can wait for the writer. |
| `PUSH_MAX_ATTEMPTS` | `3` | Maximum remote-write attempts per payload; `0` and `1` both mean one attempt. |

### Choosing a reorder delay

Firehose delivery is not timestamp-ordered across batches. The reorder window lets late samples merge with newer samples already in memory, after which each series is sent in timestamp order and its high-water mark advances. This prevents routine delivery disorder from turning valid late samples into high-water-mark drops.

The trade-off is intentional: `REORDER_DELAY_SECS` is added to end-to-end metric latency, and samples inside the window exist only in process memory. Start with 60–120 seconds and tune it against the exported `queue_freshness_seconds` gauge, which reports the maximum age of records still queued in Firehose. A graceful shutdown flushes samples that are still inside the window.

`FLUSH_MAX_SERIES` triggers an eligibility check; it does not bypass the reorder window or impose a hard cap on young samples. The buffer can therefore exceed that threshold, with memory use bounded by the distinct series and samples received during `REORDER_DELAY_SECS`. Size the delay and container memory together.

## Development

```sh
just fmt
cargo clippy --all-targets -- -D warnings
just test
```
