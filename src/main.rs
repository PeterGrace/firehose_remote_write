pub(crate) mod aws;
mod config;
mod consts;
mod prometheus;
mod series;
pub(crate) mod structs;
#[cfg(test)]
mod testlog;
mod writer;

#[macro_use]
extern crate tracing;
#[macro_use]
extern crate anyhow;

use crate::aws::get_freshness;
use crate::config::Config;
use crate::prometheus::{
    APP_INFO, FRESHNESS_INFO, RECORDS_SKIPPED, REJECTED_PAYLOADS, STREAMS_RECEIVED,
    TOTAL_WRITES_SENT,
};
use crate::series::to_series;
use crate::structs::{AppState, FirehoseData, FirehoseResponse};
use crate::structs::{CloudWatchMetric, Firehose};
use crate::writer::run_writer;
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{debug_handler, Json, Router};
use base64::prelude::*;
use prometheus_remote_write::{Label, Sample};
use std::env;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::map_request_body::MapRequestBodyLayer;
use tracing_subscriber::EnvFilter;

/// The largest request body this endpoint will buffer, **after** decompression.
///
/// This constant exists because two products' defaults compose into silent, unrecoverable
/// data loss, and neither side looks wrong on its own:
///
/// * axum's `DefaultBodyLimit` is **2 MiB** and applies to every extractor that goes through
///   `Bytes` -- which is `Bytes`, `String`, `Json` and `Form` alike, so switching extractor
///   does not escape it.
/// * `HttpEndpointBufferingHints.SizeInMBs` defaults to **5** and may be set as high as 64.
///
/// So an untouched delivery stream pointed at an untouched axum server produces requests the
/// server rejects. That would merely be an outage, except for *which* status axum rejects
/// with: 413. Per the AWS specification, "Response code 413 (size exceeded) is considered as
/// a permanent failure and the record batch is **not sent to error bucket** if configured."
/// It is the only status with that property -- everything else in 2xx/4xx/5xx is either
/// success or a retryable error that eventually reaches the S3 backup. A 413 destroys the
/// batch with no retry and no copy.
///
/// 64 MiB is the documented ceiling on the request body ("can be up to a maximum of 64 MiB,
/// before compression"), so a limit at that value cannot be reached by a conforming sender.
/// It is measured against the *decompressed* body because the extractor sits inside the
/// decompression layer, which is also the direction that keeps a compression bomb bounded.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// What we put in `requestId` when neither the header nor the body carried one.
///
/// There is no correct value here. The response schema makes `requestId` required and says
/// it "must match the requestId in the request", so when the request has none, *no* response
/// can conform -- and a non-conforming response is treated by Firehose as a 500 with no body.
/// Defaulting to `""` merely hides that behind something that reads like a real value in a
/// log. A visible sentinel says which of the two failure modes happened, and pairs with the
/// 400 and the `errorMessage` that accompany it.
///
/// Reaching this from Firehose itself should be impossible: `requestId` is `required` in the
/// request schema and duplicated into the `X-Amz-Firehose-Request-Id` header. It is reachable
/// from anything else that POSTs here, which is why it must not panic.
const MISSING_REQUEST_ID: &str = "missing-request-id";

/// How long one whole remote-write request may take before reqwest abandons it.
///
/// Chosen *above* [`writer::PER_ATTEMPT_TIMEOUT`] (10s) so that the retry policy's own bound
/// is the one that normally fires: the policy counts a timed-out attempt as a failed attempt
/// and retries it, whereas a reqwest timeout surfaces as an error the policy then has to
/// interpret. Being the outer of the two also means this cannot cut short a request the
/// policy considers healthy.
///
/// # As wired today this can never fire, and that is deliberate rather than an oversight
///
/// `push_with_retry` wraps every `send` in `tokio::time::timeout(PER_ATTEMPT_TIMEOUT, ..)`,
/// which at 10s always wins against 15s -- and dropping that future cancels the reqwest
/// request outright. So neither of these two settings is observable in the current call
/// graph. They are kept because the thing they guard against is the closure below being
/// reused, or `PER_ATTEMPT_TIMEOUT` being raised, by somebody who does not know that
/// `reqwest::Client::new()` has **no default request timeout at all** -- and the cost of that
/// mistake is the single task draining the channel hanging forever on a blackholed socket.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the TCP connect and TLS handshake may take. Subsumed by
/// [`HTTP_REQUEST_TIMEOUT`], and kept for the same reason.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Publish which build is running, as a label set on a gauge that is always 1.
///
/// Takes its two values as parameters rather than reading `env!` itself so that the trimming
/// below is reachable from a test. That is not ceremony: without the seam, the only input
/// this function can ever see is whatever `GIT_HASH` happens to hold in the tree it was
/// compiled in, so deleting the `trim()` would kill nothing.
///
/// # `GIT_HASH` does NOT carry a trailing newline, contrary to how `build.rs` reads
///
/// `build.rs` does `String::from_utf8(git rev-parse HEAD)` without trimming, so the string it
/// interpolates genuinely ends in `\n`. It never reaches the binary: cargo parses build
/// script stdout **line by line**, so `cargo:rustc-env=GIT_HASH=<sha>\n` sets the variable to
/// the line's contents and the newline becomes the line terminator. Measured, not reasoned:
/// a scratch crate with the identical `build.rs` reports `len=40`.
///
/// The `trim()` therefore does nothing today. It stays because it makes this function total
/// over its input -- a label value with whitespace in it is a different series from the same
/// value without, so a future `build.rs` that emitted the hash by another route (a file, a
/// multi-line directive) would otherwise split `app_info` in two -- and
/// `app_info_labels_are_trimmed` is what keeps it honest.
fn set_app_info(crate_version: &str, git_hash: &str) {
    APP_INFO
        .with_label_values(&[crate_version.trim(), git_hash.trim()])
        .set(1.0);
}

/// Resolve when the process is asked to stop, by either of the two routes that can ask.
///
/// SIGTERM is what Kubernetes sends first, and it is the one that matters: the container gets
/// `terminationGracePeriodSeconds` to finish before SIGKILL, and everything still sitting in
/// the channel or the accumulator at SIGKILL is gone with nothing upstream to replay it (see
/// the durability note on [`writer::run_writer`]). Ctrl-C is for running this by hand.
///
/// Both handlers are installed *before* either is awaited, so there is no window in which one
/// signal is armed and the other is not.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install the Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received Ctrl-C; draining before exit"),
        _ = terminate => info!("received SIGTERM; draining before exit"),
    }
}

/// Serve until `shutdown` resolves, then let the writer finish.
///
/// # The order of the four steps below is the whole point of this function
///
/// `run_writer` reaches its final-flush arm when `rx.recv()` returns `None`, and that happens
/// only once **every** `Sender` has dropped. So:
///
/// 1. **Serve first.** The router holds a `Sender` and clones it into every in-flight handler.
///    Releasing it before the server has stopped would make `try_send` fail in handlers that
///    are mid-request, turning a clean shutdown into a burst of 503s on deliveries that were
///    already accepted onto the socket.
/// 2. **Then release the router.** `app` is moved into the `Serve` future, and the temporary
///    holding it is dropped at the end of the `let served = ...;` statement — before anything
///    below runs. This is load-bearing and easy to lose: binding the future to a variable that
///    outlives the `drop(state)` below, or keeping a second `Router` clone anywhere, leaves a
///    `Sender` alive and step 4 waits for it forever.
/// 3. **Then drop `state`,** which is the last `Sender` outside the router. Now the channel is
///    closed and the writer's drain arm becomes reachable.
/// 4. **Then wait for the writer,** because the drain arm still has to *run*: it flushes the
///    accumulator and pushes it, with retries. Returning from `main` without this await drops
///    the runtime and takes the task with it mid-flush.
///
/// The `firehose_arns` freshness task is deliberately not given an `AppState` — see the
/// comment at its spawn site. A single parked clone anywhere in the process defeats all four
/// steps above, and does so silently: the process still exits, it just exits by being killed
/// rather than by finishing.
///
/// Generic over the shutdown future so `shutdown_flushes_data_that_is_still_buffered` can
/// drive the real ordering from a test without raising a real signal.
async fn serve_and_drain(
    listener: TcpListener,
    app: Router,
    state: AppState,
    writer: JoinHandle<()>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) {
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;
    if let Err(e) = served {
        error!("the http listener stopped with an error: {e}");
    }

    // Step 3. Explicit rather than end-of-scope, because "the last sender is released here,
    // before the await below" is the property, and a later edit that added a use of `state`
    // after this point would silently extend its life past the await.
    drop(state);

    // Step 4. A panic in the writer is reported rather than propagated: we are on the way out
    // either way, and an unwrap here would replace a legible error with a second panic.
    if let Err(e) = writer.await {
        error!("the writer task did not exit cleanly: {e}");
    }
}

#[tokio::main]
async fn main() {
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();
    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
        .with_file(true)
        .with_line_number(true)
        .init();

    set_app_info(env!("CARGO_PKG_VERSION"), env!("GIT_HASH"));

    let config = Config::from_env();
    let addr = env::var("PROM_WRITE_ADDR").expect("Can't push without PROM_WRITE_ADDR defined");
    let url = format!("{addr}/api/v1/write");
    let (tx, rx) = tokio::sync::mpsc::channel(config.channel_capacity);
    let state = AppState::new(tx);

    let client = reqwest::Client::builder()
        .timeout(HTTP_REQUEST_TIMEOUT)
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .build()
        .expect("failed to build the remote-write HTTP client");
    // The `JoinHandle` is kept, not discarded: `serve_and_drain` awaits it so the final flush
    // finishes before the runtime is torn down.
    let writer = tokio::spawn(run_writer(rx, config, move |body| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let rs = client.post(url).body(body).send().await?;
            let status = rs.status().as_u16();
            TOTAL_WRITES_SENT
                .with_label_values(&[&status.to_string()])
                .inc();
            if status == 400 {
                error!(
                    "400 from remote write: {}",
                    rs.text().await.unwrap_or_default()
                );
            }
            Ok(status)
        }
    }));

    // The freshness task is handed the ARN set alone, NOT a clone of `AppState`.
    //
    // `AppState` owns a `Sender`, and `run_writer` only reaches its shutdown-drain arm once
    // *every* `Sender` has dropped. This task loops forever, so a clone of `AppState` parked
    // inside it would keep one alive for the life of the process -- making the drain
    // unreachable and quietly discarding the buffer's tail on every shutdown. See
    // [`serve_and_drain`], which is what depends on this.
    let firehose_arns = Arc::clone(&state.firehose_arns);
    let app = app(state.clone());

    tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            // Check discovered firehose arns and check their freshness
            let firehose_arns = firehose_arns.read().await.clone();
            for firehose_arn in firehose_arns.iter() {
                if let Ok(freshness) = get_freshness(firehose_arn.clone()).await {
                    info!("Freshness for {firehose_arn}: {freshness}");
                    FRESHNESS_INFO
                        .with_label_values(&[firehose_arn])
                        .set(freshness);
                }
            }
        }
    });

    let listener = TcpListener::bind("0.0.0.0:3000").await.unwrap();
    info!("Spawning axum listener.");
    serve_and_drain(listener, app, state, writer, shutdown_signal()).await;
    info!("Drained; exiting.");
}

/// The real router, layers and all.
///
/// Tests drive *this*, not `get_firehose` directly, because two of the three ways this
/// endpoint silently loses data live in the layers rather than in the handler: the body
/// limit and the gzip decoding. A test that called the handler function would pass with both
/// removed.
pub fn app(state: AppState) -> Router {
    app_with_body_limit(state, MAX_BODY_BYTES)
}

/// [`app`] with the body limit as a parameter, so a test can prove what happens when the
/// limit is exceeded without allocating 64 MiB to do it.
///
/// # Layer order is load-bearing
///
/// `.layer()` calls stack outward, so the last one added is the outermost. The request
/// therefore passes through `DefaultBodyLimit` (which only records the limit as an extension
/// for the extractor to read), then decompression, then the body-type remap, then the route.
///
/// `MapRequestBodyLayer` is not decoration: `RequestDecompression<S>` hands its inner service
/// a `Request<DecompressionBody<B>>`, and axum's `Route` accepts only `Request<Body>`. The
/// remap is what makes the two typecheck against each other.
///
/// Because the limit is applied by the *extractor*, it is measured against the decompressed
/// body. That is the safe direction -- a compressed payload that expands past the limit is
/// cut off rather than buffered whole -- and it is measured rather than assumed, by
/// `the_body_limit_is_measured_after_decompression`.
///
/// `pass_through_unaccepted(true)` matters for the same reason every other exit path in this
/// file does. Left at its default, an unsupported `Content-Encoding` makes the layer answer
/// 415 with a plain-text body and no `requestId`, which Firehose reads as "500 with no body"
/// -- a retry with no explanation attached. Passing it through instead lets the handler
/// answer with a conforming body that names the encoding it could not decode.
fn app_with_body_limit(state: AppState, limit: usize) -> Router {
    Router::new()
        .route("/", post(get_firehose).put(get_firehose))
        .layer(MapRequestBodyLayer::new(Body::new))
        .layer(
            RequestDecompressionLayer::new()
                .gzip(true)
                .pass_through_unaccepted(true),
        )
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

/// Decode Firehose records, skipping any that are malformed.
///
/// Not `async`, and not fallible. Both are deliberate. There is no I/O here, and there is no
/// error worth returning: a single bad record must not fail the batch, because the only way
/// to fail a batch is a non-200, and a non-200 makes Firehose replay *everything* -- the
/// records that decoded fine along with the one that did not. If the bad record is
/// permanently bad, that replay never terminates on its own; it just runs until the retry
/// window expires, having re-delivered the good records every time.
///
/// Records are concatenated, not joined with a separator: CloudWatch metric streams in JSON
/// format already terminate each record with a newline, and inserting another would produce
/// blank lines that `parse_lines` would then have to skip.
fn decode_payloads(records: Vec<FirehoseData>) -> String {
    let mut out = String::new();
    for record in records {
        let bytes = match BASE64_STANDARD.decode(&record.data) {
            Ok(b) => b,
            Err(e) => {
                debug!("skipping record with invalid base64: {e}");
                RECORDS_SKIPPED.inc();
                continue;
            }
        };
        match String::from_utf8(bytes) {
            Ok(s) => out.push_str(&s),
            Err(e) => {
                debug!("skipping record with invalid utf-8: {e}");
                RECORDS_SKIPPED.inc();
            }
        }
    }
    out
}

/// Parse newline-delimited CloudWatch metric JSON into remote-write samples.
///
/// `now_ms` is a parameter rather than a clock read, so that every record in one payload is
/// judged against one reading. Reading the clock per record would put a batch's records on
/// either side of the freshness window boundary depending on how long the batch took to
/// parse -- a decision that has nothing to do with the data.
///
/// `to_series` fails only for per-record, non-retryable conditions (a namespace or metric
/// name with nothing usable in it). Those are logged and skipped for the same reason bad
/// base64 is: `?`-ing one out of the handler would fail the delivery and replay the batch.
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
                RECORDS_SKIPPED.inc();
                continue;
            }
        };
        match to_series(&metric, now_ms) {
            Ok(series) => out.extend(series),
            Err(e) => {
                debug!("skipping record: {e}");
                RECORDS_SKIPPED.inc();
            }
        }
    }
    out
}

/// Trim a candidate id and discard it if nothing is left.
///
/// A present-but-empty `requestId` is indistinguishable from an absent one for the only
/// purpose the value has -- Firehose matching it against what it sent -- so it is treated as
/// absent and reported as such, rather than echoed back as `""`.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The `requestId`, header first.
///
/// The header is authoritative: the body copy is documented as being there "for
/// convenience", the header "is kept the same between multiple attempts of the same
/// request", and it survives a body we could not parse at all -- which is exactly when a
/// correctly-addressed error response is worth the most.
///
/// `to_str` fails on any non-visible-ASCII byte. That is a `Result` the legacy handler
/// `unwrap`ped; here it degrades to the body copy, because a header we cannot read is not a
/// reason to panic a request that may be otherwise perfectly good.
fn request_id_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("X-Amz-Firehose-Request-Id")
        .and_then(|v| v.to_str().ok())
        .and_then(non_empty)
}

/// The source ARN, header first, body second -- same reasoning, same `to_str` care.
fn source_arn_from(headers: &HeaderMap, payload: &Firehose) -> Option<String> {
    headers
        .get("X-Amz-Firehose-Source-Arn")
        .and_then(|v| v.to_str().ok())
        .and_then(non_empty)
        .or_else(|| payload.source_arn.as_deref().and_then(non_empty))
}

/// A `Content-Encoding` still on the request means the decompression layer did **not**
/// handle it -- it strips the header when it decodes -- so whatever we are holding is not
/// the JSON the sender thinks it sent.
fn undecoded_content_encoding(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .and_then(non_empty)
        .filter(|v| !v.eq_ignore_ascii_case("identity"))
}

/// Record the ARN we are receiving from, taking the exclusive lock only when it is new.
///
/// The previous version took `write()` unconditionally on every request, which serialised
/// every concurrent handler behind one exclusive acquisition in order to re-insert a value
/// that was already there. The set converges after the first request from each delivery
/// stream and then never changes again, so the write path is effectively startup-only while
/// the read path is per-request.
///
/// The read guard is dropped before the write is attempted -- `tokio::sync::RwLock` is not
/// reentrant, and upgrading in place by holding both would deadlock the handler against
/// itself. The gap between the two means two requests carrying the same new ARN can both
/// decide to write; `HashSet::insert` makes that idempotent.
async fn remember_source_arn(state: &AppState, arn: String) {
    if state.firehose_arns.read().await.contains(&arn) {
        return;
    }
    state.firehose_arns.write().await.insert(arn);
}

/// Accept a Firehose delivery, or say precisely why not.
///
/// # Every exit from this function returns the same body shape
///
/// `{"requestId": ..., "timestamp": ...}` plus an `errorMessage` on failure, as
/// `application/json`. That is not tidiness: "If a response fails to conform to the
/// requirements below, the Firehose server treats it as though it had a 500 status code with
/// no body", and **only 200 counts as success** -- 201, 202 and 204 are failures. The
/// `Json<Firehose>` extractor's own rejection is plain text with no `requestId`, so the body
/// is extracted as `Bytes` and deserialised here where a failure can still be answered
/// properly.
///
/// # Nothing here may panic
///
/// The legacy handler `unwrap`ped four things reachable from a single malformed request:
/// `payload.request_id`, the base64 decode, the UTF-8 conversion, and `HeaderValue::to_str`.
/// A panic in a handler is a dropped connection, which Firehose retries -- so a permanently
/// malformed record does not fail once, it fails forever, and takes the rest of its batch
/// with it every time.
///
/// # Which failures get which status
///
/// Firehose treats 429, 500, 503 and 400 identically: retry with exponential backoff and
/// jitter, `Retry-After` ignored, then the S3 error bucket once the window expires. So the
/// status is chosen for the operator reading it, with two hard rules: never 413 (the batch
/// would be destroyed rather than backed up), and never a non-200 2xx (read as failure but
/// with no diagnostic).
#[debug_handler]
async fn get_firehose(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> (StatusCode, Json<FirehoseResponse>) {
    STREAMS_RECEIVED.inc();

    // Resolved before anything that can fail, so an unreadable body still gets an addressed
    // reply. AWS asks endpoints to log this "for both successful and unsuccessful requests".
    let header_request_id = request_id_from_header(&headers);

    let body = match body {
        Ok(b) => b,
        Err(rejection) => {
            // NOT `rejection.status()`. For a body over the limit that is 413, and 413 is the
            // one status where Firehose gives up permanently *and* skips the S3 error bucket.
            // 500 is retried and then backed up, so the batch survives our refusal to read
            // it. The `errorMessage` is what tells the operator to lower `SizeInMBs`.
            let message = format!(
                "could not read request body ({}); the endpoint accepts up to {MAX_BODY_BYTES} \
                 bytes uncompressed",
                rejection.body_text()
            );
            error!("request {header_request_id:?}: {message}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(FirehoseResponse::error(
                    header_request_id.unwrap_or_else(|| MISSING_REQUEST_ID.to_string()),
                    message,
                )),
            );
        }
    };

    let mut payload: Firehose = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            let message = match undecoded_content_encoding(&headers) {
                Some(encoding) => format!(
                    "request body is Content-Encoding: {encoding}, which this endpoint cannot \
                     decode (only gzip and identity); disable compression on the delivery \
                     stream or use gzip"
                ),
                None => format!("request body is not a Firehose JSON document: {e}"),
            };
            error!("request {header_request_id:?}: {message}");
            return (
                StatusCode::BAD_REQUEST,
                Json(FirehoseResponse::error(
                    header_request_id.unwrap_or_else(|| MISSING_REQUEST_ID.to_string()),
                    message,
                )),
            );
        }
    };

    let request_id =
        header_request_id.or_else(|| payload.request_id.as_deref().and_then(non_empty));

    match source_arn_from(&headers, &payload) {
        Some(arn) => remember_source_arn(&state, arn).await,
        None => warn!("no source arn in headers or payload for this request"),
    }

    let mut text = payload
        .records
        .take()
        .map(decode_payloads)
        .unwrap_or_default();
    // Pre-existing convenience path for hand-made requests; not part of the AWS schema.
    if let Some(message) = payload.message.take() {
        text = message;
    }

    // Read once for the whole batch, then threaded through. See `parse_lines`.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let series = parse_lines(&text, now_ms);

    // Whole batch or nothing, and deliberately not "send what fits".
    //
    // Firehose delivery is all-or-nothing: there is no per-record status in the response, so
    // a partial accept followed by a non-200 tells the sender to replay the entire batch,
    // guaranteeing that the accepted prefix arrives twice. The duplicate itself is harmless
    // -- `Accumulator::insert` keys on labels plus timestamp and overwrites rather than
    // appends, so a redelivered datapoint replaces its earlier copy instead of becoming an
    // out-of-order sample. That property is precisely what makes at-least-once delivery safe
    // here, and it is worth not depending on it more than necessary.
    if !series.is_empty() {
        if let Err(e) = state.tx.try_send(series) {
            REJECTED_PAYLOADS.inc();
            let message = format!("write buffer full, retry this batch: {e}");
            warn!("request {request_id:?}: {message}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(FirehoseResponse::error(
                    request_id.unwrap_or_else(|| MISSING_REQUEST_ID.to_string()),
                    message,
                )),
            );
        }
    }

    match request_id {
        Some(request_id) => {
            debug!("accepted request {request_id}");
            (StatusCode::OK, Json(FirehoseResponse::ok(request_id)))
        }
        // The records above were still enqueued: they parsed, and discarding data we already
        // hold buys nothing. But the response cannot be a success, because a `requestId` that
        // does not match makes even a 200 count as a failure -- so say what is wrong instead
        // of pretending. See [`MISSING_REQUEST_ID`].
        None => {
            let message = "no requestId in the X-Amz-Firehose-Request-Id header or the request \
                           body; it is required in both";
            warn!("{message}");
            (
                StatusCode::BAD_REQUEST,
                Json(FirehoseResponse::error(
                    MISSING_REQUEST_ID.to_string(),
                    message,
                )),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testlog::LogTail;
    use axum::body::to_bytes;
    use axum::http::header::CONTENT_TYPE;
    use axum::http::Request;
    use serde_json::Value;
    use std::io::Write;
    use tokio::sync::mpsc::{channel, Receiver};
    use tower::ServiceExt;

    const NOW: i64 = 1_700_000_000_000;
    const REQ_ID: &str = "ed4acda5-034f-9f42-bba1-f29aea6d7d8f";

    type Batch = Vec<(Vec<Label>, Sample)>;

    fn record(payload: &str) -> FirehoseData {
        FirehoseData {
            data: BASE64_STANDARD.encode(payload),
        }
    }

    /// One well-formed CloudWatch metric-stream line, timestamped `NOW` so it lands inside
    /// the freshness window whatever the wall clock says.
    fn metric_line(name: &str, timestamp: i64) -> String {
        format!(
            r#"{{"metric_stream_name":"s","account_id":"1","region":"us-east-1",
                "namespace":"AWS/Test","metric_name":"{name}","dimensions":{{}},
                "timestamp":{timestamp},"value":{{"max":1.0}},"unit":"Count"}}"#
        )
        .replace('\n', "")
    }

    /// One well-formed record, stamped with the wall clock.
    ///
    /// Tests that go through the handler cannot use a fixed timestamp: the handler reads the
    /// real clock and `to_series` drops anything more than 24 hours old, so a hard-coded
    /// `NOW` silently produces an empty batch and every "the data was enqueued" assertion
    /// starts failing on a date that has nothing to do with the code. Tests calling
    /// `parse_lines` directly pass `NOW` for both sides and are unaffected.
    fn fresh_line() -> String {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        format!("{}\n", metric_line("M", now_ms))
    }

    fn state_with_capacity(capacity: usize) -> (AppState, Receiver<Batch>) {
        let (tx, rx) = channel(capacity);
        (AppState::new(tx), rx)
    }

    /// A request body carrying `records`, each already base64-encoded.
    fn firehose_body(records: &[&str]) -> String {
        let encoded: Vec<String> = records
            .iter()
            .map(|r| format!(r#"{{"data":"{}"}}"#, BASE64_STANDARD.encode(r)))
            .collect();
        format!(
            r#"{{"requestId":"{REQ_ID}","timestamp":{NOW},"records":[{}]}}"#,
            encoded.join(",")
        )
    }

    fn post() -> axum::http::request::Builder {
        Request::builder()
            .method("POST")
            .uri("/")
            .header(CONTENT_TYPE, "application/json")
    }

    /// Status plus the parsed JSON body, which is what every assertion below is about.
    async fn call(app: Router, request: Request<Body>) -> (StatusCode, Value, String) {
        let response = app.oneshot(request).await.expect("router is infallible");
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let json = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "every response must be a conforming JSON body, got {:?}: {e}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, json, content_type)
    }

    /// The shape AWS requires of every response, asserted in one place so no exit path can
    /// quietly stop conforming.
    fn assert_conforming(status: StatusCode, body: &Value, content_type: &str) {
        assert_eq!(
            content_type, "application/json",
            "the only acceptable content type is application/json"
        );
        assert!(
            body.get("requestId").and_then(Value::as_str).is_some(),
            "requestId is required in the response schema, got {body}"
        );
        let timestamp = body
            .get("timestamp")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| panic!("timestamp is required and must be an integer: {body}"));
        assert!(
            timestamp > 1_600_000_000_000,
            "timestamp must be epoch MILLISECONDS; {timestamp} is ~1000x too small, which is \
             what `as_secs()` produces"
        );
        assert_ne!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "413 is the one status where Firehose destroys the batch without an S3 backup"
        );
        if status != StatusCode::OK {
            assert!(
                status.is_client_error() || status.is_server_error(),
                "a non-200 2xx is read as a failure with no diagnostic, got {status}"
            );
            assert!(
                body.get("errorMessage")
                    .and_then(Value::as_str)
                    .is_some_and(|m| !m.is_empty()),
                "errorMessage is the only post-mortem breadcrumb copied to the S3 error \
                 bucket, got {body}"
            );
        }
    }

    // -------------------------------------------------------------------------------------
    // decode_payloads
    // -------------------------------------------------------------------------------------

    #[test]
    fn decode_payloads_skips_malformed_base64_and_keeps_going() {
        let _log = LogTail::start();
        let before = RECORDS_SKIPPED.get();

        let decoded = decode_payloads(vec![
            record("first\n"),
            FirehoseData {
                data: String::from("!!!not base64!!!"),
            },
            record("third\n"),
        ]);

        assert!(decoded.contains("first"), "got {decoded:?}");
        assert!(decoded.contains("third"), "got {decoded:?}");
        assert_eq!(
            RECORDS_SKIPPED.get(),
            before + 1.0,
            "the skipped record must be counted"
        );
    }

    #[test]
    fn decode_payloads_skips_invalid_utf8() {
        let _log = LogTail::start();
        let before = RECORDS_SKIPPED.get();

        let decoded = decode_payloads(vec![
            FirehoseData {
                // Valid base64, but 0xff is not valid UTF-8 in any position.
                data: BASE64_STANDARD.encode([0xff, 0xfe, 0xfd]),
            },
            record("good\n"),
        ]);

        assert!(decoded.contains("good"), "got {decoded:?}");
        assert_eq!(RECORDS_SKIPPED.get(), before + 1.0);
    }

    // -------------------------------------------------------------------------------------
    // parse_lines
    // -------------------------------------------------------------------------------------

    #[test]
    fn parse_lines_skips_malformed_json_and_returns_valid_series() {
        let _log = LogTail::start();
        let before = RECORDS_SKIPPED.get();

        let text = format!("{{not json}}\n{}\n", metric_line("M", NOW));
        let series = parse_lines(&text, NOW);

        assert_eq!(series.len(), 1, "the good line must survive the bad one");
        assert_eq!(RECORDS_SKIPPED.get(), before + 1.0);
    }

    /// `to_series` is fallible, and its failures are per-record and permanent -- a namespace
    /// with no `/` can never become parseable. Propagating one would fail the delivery, and
    /// Firehose would replay the same unparseable record forever.
    #[test]
    fn parse_lines_skips_records_that_to_series_rejects() {
        let _log = LogTail::start();
        let before = RECORDS_SKIPPED.get();

        // "NoSlash" has no service segment, which is exactly what `metric_base_name` bails on.
        let bad =
            metric_line("M", NOW).replace(r#""namespace":"AWS/Test""#, r#""namespace":"NoSlash""#);
        assert!(
            bad.contains("NoSlash"),
            "test setup: the substitution must apply"
        );
        let text = format!("{bad}\n{}\n", metric_line("Good", NOW));

        let series = parse_lines(&text, NOW);

        assert_eq!(
            series.len(),
            1,
            "the batch must survive one rejected record"
        );
        assert_eq!(RECORDS_SKIPPED.get(), before + 1.0);
    }

    /// The clock is read once per batch and threaded in. Without the parameter actually
    /// reaching `to_series`, a record dated `NOW` would be judged against the wall clock --
    /// which for a fixed test timestamp is years of drift, so this is a real detector rather
    /// than a restatement of the signature.
    #[test]
    fn parse_lines_judges_every_record_against_the_now_ms_it_was_given() {
        let _log = LogTail::start();
        let text = format!("{}\n{}\n", metric_line("A", NOW), metric_line("B", NOW));

        assert_eq!(parse_lines(&text, NOW).len(), 2, "both are fresh at NOW");
        assert!(
            parse_lines(&text, NOW + 48 * 60 * 60 * 1000).is_empty(),
            "two days later the same records are outside the window"
        );
    }

    #[test]
    fn parse_lines_ignores_blank_lines() {
        let _log = LogTail::start();
        let before = RECORDS_SKIPPED.get();
        let text = format!("\n\n{}\n\n   \n", metric_line("M", NOW));

        assert_eq!(parse_lines(&text, NOW).len(), 1);
        assert_eq!(
            RECORDS_SKIPPED.get(),
            before,
            "a blank line is not a skipped record"
        );
    }

    // -------------------------------------------------------------------------------------
    // requestId resolution
    // -------------------------------------------------------------------------------------

    #[tokio::test]
    async fn the_request_id_comes_from_the_header_and_the_response_echoes_it() {
        let _log = LogTail::start();
        let (state, mut rx) = state_with_capacity(4);
        let body = firehose_body(&[&fresh_line()]);

        let (status, json, content_type) = call(
            app(state),
            post()
                .header("X-Amz-Firehose-Request-Id", "header-id")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_conforming(status, &json, &content_type);
        assert_eq!(
            json["requestId"], "header-id",
            "the header wins over the body copy"
        );
        assert!(json.get("errorMessage").is_none(), "a 200 carries no error");
        assert_eq!(rx.try_recv().expect("the batch must be enqueued").len(), 1);
    }

    #[tokio::test]
    async fn the_request_id_falls_back_to_the_body_when_the_header_is_absent() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        let body = firehose_body(&[&fresh_line()]);

        let (status, json, content_type) =
            call(app(state), post().body(Body::from(body)).unwrap()).await;

        assert_eq!(status, StatusCode::OK);
        assert_conforming(status, &json, &content_type);
        assert_eq!(json["requestId"], REQ_ID);
    }

    /// `HeaderValue::to_str` returns `Err` for any byte outside visible ASCII, and the legacy
    /// handler `unwrap`ped it. A header a proxy mangled is not a reason to panic a request
    /// whose body is perfectly good.
    #[tokio::test]
    async fn a_non_ascii_header_value_does_not_panic_and_falls_back_to_the_body() {
        let _log = LogTail::start();
        let (state, mut rx) = state_with_capacity(4);
        let body = firehose_body(&[&fresh_line()]);

        let request = post()
            .header(
                "X-Amz-Firehose-Request-Id",
                axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
            )
            .header(
                "X-Amz-Firehose-Source-Arn",
                axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
            )
            .body(Body::from(body))
            .unwrap();

        let (status, json, content_type) = call(app(state), request).await;

        assert_eq!(status, StatusCode::OK);
        assert_conforming(status, &json, &content_type);
        assert_eq!(json["requestId"], REQ_ID, "the body copy is still usable");
        assert_eq!(rx.try_recv().expect("the batch survives").len(), 1);
    }

    /// Neither source carries one. The legacy handler called `.unwrap()` on this and panicked.
    #[tokio::test]
    async fn a_missing_request_id_everywhere_does_not_panic_and_still_conforms() {
        let _log = LogTail::start();
        let (state, mut rx) = state_with_capacity(4);
        let line = fresh_line();
        let body = format!(
            r#"{{"records":[{{"data":"{}"}}]}}"#,
            BASE64_STANDARD.encode(&line)
        );

        let (status, json, content_type) =
            call(app(state), post().body(Body::from(body)).unwrap()).await;

        assert_conforming(status, &json, &content_type);
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "no requestId can be matched, so this cannot be reported as success"
        );
        assert_eq!(json["requestId"], MISSING_REQUEST_ID);
        assert!(
            json["errorMessage"]
                .as_str()
                .unwrap()
                .contains("X-Amz-Firehose-Request-Id"),
            "the message must name what was missing, got {json}"
        );
        assert_eq!(
            rx.try_recv().expect("parsed records are still kept").len(),
            1,
            "discarding data we already hold buys nothing"
        );
    }

    /// An empty string is not a usable id: it can never match what Firehose sent, and echoing
    /// it back reads in a log like a real value.
    #[tokio::test]
    async fn an_empty_request_id_is_treated_as_missing_rather_than_echoed() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        let body = r#"{"requestId":"   ","records":[]}"#;

        let (status, json, content_type) = call(
            app(state),
            post()
                .header("X-Amz-Firehose-Request-Id", "  ")
                .body(Body::from(body))
                .unwrap(),
        )
        .await;

        assert_conforming(status, &json, &content_type);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["requestId"], MISSING_REQUEST_ID);
    }

    // -------------------------------------------------------------------------------------
    // Response contract
    // -------------------------------------------------------------------------------------

    /// The AWS response schema says milliseconds; the legacy handler emitted seconds. A
    /// seconds value is ~1.7e9 where a millisecond value is ~1.7e12, so magnitude alone
    /// separates them by three orders of magnitude.
    #[tokio::test]
    async fn the_response_timestamp_is_in_milliseconds_not_seconds() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        let body = firehose_body(&[]);

        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let (status, json, _ct) = call(app(state), post().body(Body::from(body)).unwrap()).await;
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        assert_eq!(status, StatusCode::OK);
        let timestamp = json["timestamp"].as_u64().unwrap();
        assert!(
            (before..=after).contains(&timestamp),
            "expected epoch millis in {before}..={after}, got {timestamp}"
        );
    }

    /// A body that is not JSON at all. The legacy `Json<Firehose>` extractor answered this
    /// with plain text and no `requestId`, which Firehose reads as a 500 with no body -- the
    /// `errorMessage` explaining the problem never reaches the S3 error records.
    #[tokio::test]
    async fn a_body_that_is_not_json_gets_a_conforming_400_that_keeps_the_request_id() {
        let _log = LogTail::start();
        let (state, mut rx) = state_with_capacity(4);

        let (status, json, content_type) = call(
            app(state),
            post()
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from("this is not json"))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_conforming(status, &json, &content_type);
        assert_eq!(
            json["requestId"], REQ_ID,
            "the header id survives a body we cannot read"
        );
        assert!(rx.try_recv().is_err(), "nothing parsed, nothing enqueued");
    }

    /// `errorMessage` is capped at 8192 characters by the response schema. Overrunning it
    /// makes the body non-conforming, which downgrades the whole response to "500 with no
    /// body" and throws away the diagnostic -- so the cap protects the message, it does not
    /// merely obey a rule.
    ///
    /// The boundary handling is the part that would otherwise bite: `String::truncate`
    /// **panics** if the byte index is not a character boundary, and a panic inside the error
    /// path is a dropped connection on a request that was already failing.
    #[test]
    fn an_overlong_error_message_is_truncated_on_a_character_boundary() {
        let _log = LogTail::start();

        // Multibyte on purpose: 9000 chars is 18000 bytes, so a naive byte truncate at 8192
        // lands mid-codepoint.
        let response = FirehoseResponse::error(REQ_ID.to_string(), "é".repeat(9000));
        let message = response.error_message.expect("an error carries a message");
        assert_eq!(
            message.chars().count(),
            8192,
            "the schema caps errorMessage at 8192 characters"
        );

        // Control: a message inside the cap is passed through untouched, so the assertion
        // above is about truncation rather than about the function mangling everything.
        let short = FirehoseResponse::error(REQ_ID.to_string(), "buffer full");
        assert_eq!(short.error_message.as_deref(), Some("buffer full"));
    }

    // -------------------------------------------------------------------------------------
    // Body limit and decompression -- the two failures that live in the layers
    // -------------------------------------------------------------------------------------

    /// axum's default limit is 2 MiB and Firehose's default buffering hint is 5 MB, so this
    /// is the exact configuration both products ship with. Driven through `app()` rather than
    /// `app_with_body_limit` so it is watching the constant the binary actually uses.
    #[tokio::test]
    async fn a_body_larger_than_axums_two_megabyte_default_is_accepted() {
        let _log = LogTail::start();
        let (state, mut rx) = state_with_capacity(4);

        // ~3 MiB of well-formed records: past axum's default, far short of ours.
        let line = fresh_line();
        let mut records = Vec::new();
        let mut total = 0usize;
        let mut i = 0;
        while total < 3 * 1024 * 1024 {
            let payload = line.replace(r#""metric_name":"M""#, &format!(r#""metric_name":"M{i}""#));
            total += payload.len();
            records.push(payload);
            i += 1;
        }
        let refs: Vec<&str> = records.iter().map(String::as_str).collect();
        let body = firehose_body(&refs);
        assert!(
            body.len() > 2 * 1024 * 1024,
            "test setup: the body must exceed axum's default, got {}",
            body.len()
        );

        let (status, json, content_type) =
            call(app(state), post().body(Body::from(body)).unwrap()).await;

        assert_eq!(status, StatusCode::OK);
        assert_conforming(status, &json, &content_type);
        assert_eq!(rx.try_recv().expect("enqueued").len(), records.len());
    }

    /// And the limit is genuinely at least the 64 MiB the AWS spec allows a sender to use.
    #[test]
    fn the_body_limit_covers_the_largest_batch_firehose_can_send() {
        assert!(
            MAX_BODY_BYTES >= 64 * 1024 * 1024,
            "HttpEndpointBufferingHints.SizeInMBs may be set to 64, got {MAX_BODY_BYTES}"
        );
    }

    /// Exceeding the limit must NOT produce 413. Driven through a deliberately tiny limit so
    /// the assertion does not cost 64 MiB of allocation to make.
    #[tokio::test]
    async fn an_oversized_body_is_retryable_rather_than_destroyed() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        let body = firehose_body(&[&fresh_line()]);
        assert!(
            body.len() > 64,
            "test setup: the body must exceed the limit"
        );

        let (status, json, content_type) = call(
            app_with_body_limit(state, 64),
            post()
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from(body))
                .unwrap(),
        )
        .await;

        assert_conforming(status, &json, &content_type);
        assert_ne!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "413 makes Firehose discard the batch without writing it to the error bucket"
        );
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json["requestId"], REQ_ID);
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    /// A delivery stream with compression enabled sends `Content-Encoding: gzip`. Without the
    /// decompression layer every such request fails to deserialise -- a 400 on every single
    /// delivery, retried until the window expires, with the whole stream ending up in S3.
    #[tokio::test]
    async fn a_gzip_encoded_body_is_decompressed_and_accepted() {
        let _log = LogTail::start();
        let (state, mut rx) = state_with_capacity(4);
        let body = firehose_body(&[&fresh_line()]);
        let compressed = gzip(body.as_bytes());
        assert_ne!(
            compressed,
            body.as_bytes(),
            "test setup: the body must actually be compressed"
        );

        let (status, json, content_type) = call(
            app(state),
            post()
                .header("Content-Encoding", "gzip")
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_conforming(status, &json, &content_type);
        assert_eq!(
            rx.try_recv().expect("the decoded batch is enqueued").len(),
            1
        );
    }

    /// A body that gunzips cleanly but is not JSON must be diagnosed as bad JSON, not as an
    /// undecodable encoding.
    ///
    /// This is the assertion behind `undecoded_content_encoding`'s premise -- that the layer
    /// *strips* `Content-Encoding` once it has decoded, so a surviving header means it did
    /// not. If that were wrong, every gzip-compressed request carrying one malformed record
    /// would be answered with a confident, wrong explanation, and the `errorMessage` in the
    /// S3 error bucket would send whoever reads it after a compression setting that is fine.
    #[tokio::test]
    async fn a_gzip_body_that_is_not_json_is_diagnosed_as_bad_json() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        let compressed = gzip(b"this gunzips fine but is not json");

        let (status, json, content_type) = call(
            app(state),
            post()
                .header("Content-Encoding", "gzip")
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_conforming(status, &json, &content_type);
        let message = json["errorMessage"].as_str().unwrap();
        assert!(
            message.contains("not a Firehose JSON document"),
            "expected a JSON diagnosis, got {message:?}"
        );
        assert!(
            !message.contains("cannot"),
            "the encoding was decoded successfully and must not be blamed, got {message:?}"
        );
    }

    /// `Content-Encoding: identity` means "not encoded" and is a legal header value, so a
    /// malformed body carrying one must be diagnosed as malformed rather than blamed on an
    /// encoding that is by definition a no-op.
    ///
    /// Added because mutating the `identity` filter out of `undecoded_content_encoding`
    /// survived the suite: the header is stripped after a real decode, so nothing else in
    /// these tests ever reaches that branch with a *surviving* header naming an encoding we
    /// did in fact understand.
    #[tokio::test]
    async fn an_identity_content_encoding_is_not_blamed_for_a_bad_body() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);

        let (status, json, content_type) = call(
            app(state),
            post()
                .header("Content-Encoding", "identity")
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from("this is not json"))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_conforming(status, &json, &content_type);
        let message = json["errorMessage"].as_str().unwrap();
        assert!(
            message.contains("not a Firehose JSON document"),
            "expected a JSON diagnosis, got {message:?}"
        );
        assert!(
            !message.contains("identity"),
            "identity is not an encoding we failed to decode, got {message:?}"
        );
    }

    /// The limit binds the **decompressed** body, not the wire bytes.
    ///
    /// Asserted rather than assumed, because the layer ordering could plausibly give either
    /// answer and the two differ by orders of magnitude on exactly the input that matters:
    /// repeated JSON gzips to a few percent of its size, so a limit applied to the compressed
    /// bytes would admit a payload that expands past it and then buffer the whole expansion.
    #[tokio::test]
    async fn the_body_limit_is_measured_after_decompression() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);

        let body = firehose_body(&[&fresh_line().repeat(400)]);
        let compressed = gzip(body.as_bytes());
        // Comfortably above the compressed size, far below the decompressed size.
        let limit = compressed.len() * 2;
        assert!(
            body.len() > limit,
            "test setup: decompressed {} must exceed the limit {limit}",
            body.len()
        );

        let (status, json, content_type) = call(
            app_with_body_limit(state, limit),
            post()
                .header("Content-Encoding", "gzip")
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from(compressed))
                .unwrap(),
        )
        .await;

        assert_conforming(status, &json, &content_type);
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a body that expands past the limit must be cut off, not buffered whole"
        );
    }

    /// An encoding the layer cannot handle still gets a conforming body naming the encoding,
    /// rather than the layer's own plain-text 415 with no `requestId`.
    #[tokio::test]
    async fn an_undecodable_content_encoding_is_reported_by_name() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        // Deliberately not JSON: an encoding we cannot decode leaves us holding bytes that
        // are not the document the sender meant to send, and the point of the test is which
        // of the two possible explanations the response gives.
        let body: Vec<u8> = vec![0x1b, 0xff, 0x00, 0x42];

        let (status, json, content_type) = call(
            app(state),
            post()
                .header("Content-Encoding", "br")
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from(body))
                .unwrap(),
        )
        .await;

        assert_conforming(status, &json, &content_type);
        assert_eq!(json["requestId"], REQ_ID);
        assert!(
            json["errorMessage"].as_str().unwrap().contains("br"),
            "the message must name the encoding, got {json}"
        );
    }

    // -------------------------------------------------------------------------------------
    // Backpressure
    // -------------------------------------------------------------------------------------

    #[tokio::test]
    async fn a_full_channel_is_rejected_with_503_and_a_conforming_body() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(1);
        // Fill the single slot so the handler's `try_send` cannot succeed.
        state.tx.try_send(vec![]).expect("the first slot is free");
        let before = REJECTED_PAYLOADS.get();

        let body = firehose_body(&[&fresh_line()]);
        let (status, json, content_type) = call(
            app(state),
            post()
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from(body))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_conforming(status, &json, &content_type);
        assert_eq!(json["requestId"], REQ_ID);
        assert_eq!(
            REJECTED_PAYLOADS.get(),
            before + 1.0,
            "a rejected payload must be counted"
        );
    }

    /// A batch that yields no series must not consume a channel slot: an empty send would
    /// burn buffer capacity on nothing and make the writer flush an empty request.
    #[tokio::test]
    async fn a_batch_with_no_usable_records_is_accepted_without_enqueueing_anything() {
        let _log = LogTail::start();
        let (state, mut rx) = state_with_capacity(4);
        let body = firehose_body(&["{not json}\n"]);

        let (status, json, content_type) = call(
            app(state),
            post()
                .header("X-Amz-Firehose-Request-Id", REQ_ID)
                .body(Body::from(body))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_conforming(status, &json, &content_type);
        assert!(rx.try_recv().is_err(), "nothing usable, nothing enqueued");
    }

    /// One bad record among good ones costs that record and nothing else.
    #[tokio::test]
    async fn a_malformed_record_is_skipped_and_the_rest_of_the_batch_is_delivered() {
        let _log = LogTail::start();
        let (state, mut rx) = state_with_capacity(4);
        let before = RECORDS_SKIPPED.get();

        // Hand-built so one record's `data` is not valid base64 at all.
        let good = BASE64_STANDARD.encode(fresh_line());
        let body = format!(
            r#"{{"requestId":"{REQ_ID}","records":[{{"data":"!!!"}},{{"data":"{good}"}}]}}"#
        );

        let (status, json, content_type) =
            call(app(state), post().body(Body::from(body)).unwrap()).await;

        assert_eq!(status, StatusCode::OK);
        assert_conforming(status, &json, &content_type);
        assert_eq!(rx.try_recv().expect("the good record survives").len(), 1);
        assert_eq!(RECORDS_SKIPPED.get(), before + 1.0);
    }

    // -------------------------------------------------------------------------------------
    // Source ARN discovery
    // -------------------------------------------------------------------------------------

    const ARN: &str = "arn:aws:firehose:us-east-1:123456789:deliverystream/testStream";

    #[tokio::test]
    async fn the_source_arn_is_recorded_from_the_header_and_from_the_body() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        let body = firehose_body(&[]);

        call(
            app(state.clone()),
            post()
                .header("X-Amz-Firehose-Source-Arn", ARN)
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await;
        assert!(state.firehose_arns.read().await.contains(ARN));

        let with_body_arn =
            format!(r#"{{"requestId":"{REQ_ID}","source_arn":"other-arn","records":[]}}"#);
        call(
            app(state.clone()),
            post().body(Body::from(with_body_arn)).unwrap(),
        )
        .await;
        assert!(state.firehose_arns.read().await.contains("other-arn"));
    }

    /// The write lock is taken only on a miss. Held read guard + known ARN must not block:
    /// `tokio::sync::RwLock` queues writers behind live readers, so a handler that took
    /// `write()` unconditionally would hang here rather than return.
    #[tokio::test]
    async fn a_known_arn_does_not_take_the_write_lock() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        state.firehose_arns.write().await.insert(ARN.to_string());

        let guard = state.firehose_arns.read().await;
        let body = firehose_body(&[]);
        let request = post()
            .header("X-Amz-Firehose-Source-Arn", ARN)
            .body(Body::from(body))
            .unwrap();

        let result =
            tokio::time::timeout(Duration::from_secs(5), app(state.clone()).oneshot(request)).await;

        drop(guard);
        assert!(
            result.is_ok(),
            "the handler must not wait for the write lock when the ARN is already known"
        );
        assert_eq!(result.unwrap().unwrap().status(), StatusCode::OK);
    }

    /// CONTROL for the test above: with an ARN that is *not* known, the same held read guard
    /// does block the handler. Without this, `a_known_arn_does_not_take_the_write_lock` would
    /// pass just as happily against an implementation that never locked at all, or against a
    /// `RwLock` whose writers do not queue behind readers.
    #[tokio::test]
    async fn an_unknown_arn_does_take_the_write_lock() {
        let _log = LogTail::start();
        let (state, _rx) = state_with_capacity(4);

        let guard = state.firehose_arns.read().await;
        let body = firehose_body(&[]);
        let request = post()
            .header("X-Amz-Firehose-Source-Arn", "an-arn-nobody-has-seen")
            .body(Body::from(body))
            .unwrap();

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            app(state.clone()).oneshot(request),
        )
        .await;

        assert!(
            result.is_err(),
            "control: a new ARN must block on the write lock, or the sibling test proves \
             nothing about the read-first path"
        );
        drop(guard);
    }

    // -------------------------------------------------------------------------------------
    // Build identity
    // -------------------------------------------------------------------------------------

    /// The `trim()` in [`set_app_info`], driven with input that actually needs trimming.
    ///
    /// This exists because the real input does *not* need it -- see the doc comment on
    /// `set_app_info` -- so without a seam and a hand-made value, deleting the `trim()` kills
    /// nothing and the guard rots. The failure it guards against is not cosmetic: a label
    /// value differing by a newline is a different series, so `app_info` would silently
    /// become two series describing one build.
    #[test]
    fn app_info_labels_are_trimmed() {
        let _log = LogTail::start();
        set_app_info("9.9.9-trimtest\n", "  0badc0de\n");

        let families = ::prometheus::gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "firehose_app_info")
            .expect("app_info must be registered under exactly one firehose_ prefix");
        let found = family.get_metric().iter().any(|m| {
            let labels: std::collections::BTreeMap<&str, &str> = m
                .get_label()
                .iter()
                .map(|l| (l.get_name(), l.get_value()))
                .collect();
            labels.get("crate_version") == Some(&"9.9.9-trimtest")
                && labels.get("git_hash") == Some(&"0badc0de")
        });
        assert!(
            found,
            "the label values must be trimmed, got: {:?}",
            family.get_metric()
        );
    }

    /// What `set_app_info` is actually handed in the binary, asserted against the build.
    ///
    /// `GIT_HASH` is produced by `build.rs` shelling out to `git rev-parse HEAD`, and the
    /// value that ends up in the binary is *not* what `build.rs` interpolates -- cargo splits
    /// build-script stdout into lines, so the trailing newline never survives. Pinned here
    /// rather than assumed, because the assumption cuts both ways: a `git_hash` label with a
    /// newline in it would split the series, and one that was empty (a build with no git
    /// available) would leave the metric unable to answer the one question it exists for.
    #[test]
    fn the_git_hash_baked_into_the_binary_is_a_bare_commit_id() {
        let raw = env!("GIT_HASH");
        assert_eq!(
            raw,
            raw.trim(),
            "cargo strips the newline build.rs emits; got {raw:?}"
        );
        assert_eq!(raw.len(), 40, "a full sha-1 commit id, got {raw:?}");
        assert!(raw.chars().all(|c| c.is_ascii_hexdigit()), "got {raw:?}");
    }

    // -------------------------------------------------------------------------------------
    // Graceful shutdown
    // -------------------------------------------------------------------------------------

    /// Decode the bytes the writer actually pushed, so the assertion is about the wire and
    /// not about a `WriteRequest` that was built and then never encoded.
    fn decode_write_request(body: &[u8]) -> prometheus_remote_write::WriteRequest {
        let raw = snap::raw::Decoder::new()
            .decompress_vec(body)
            .expect("body must be snappy-compressed");
        prost::Message::decode(raw.as_slice()).expect("body must decode as a WriteRequest")
    }

    fn metric_names(request: &prometheus_remote_write::WriteRequest) -> Vec<&str> {
        request
            .timeseries
            .iter()
            .filter_map(|s| {
                s.labels
                    .iter()
                    .find(|l| l.name == "__name__")
                    .map(|l| l.value.as_str())
            })
            .collect()
    }

    /// **The shutdown drain, end to end, over a real socket.**
    ///
    /// This is the test the whole graceful-shutdown wiring exists for, and it is written to
    /// fail rather than hang in every way the wiring can be got wrong:
    ///
    /// * **No `drop(state)`** -- the channel never closes, `run_writer` never returns, the
    ///   `writer.await` inside `serve_and_drain` blocks forever and the timeout below fires.
    ///   (Measured: this is exactly what happens.)
    /// * **A `Router` clone kept alive past the drop** -- same symptom, same detector.
    /// * **No `writer.await`** -- `serve_and_drain` returns while the final flush is still in
    ///   flight, which the gate below turns into a deterministic failure.
    /// * **Serving after the sender is released** -- the request would be answered 503.
    ///
    /// # The gate is what makes the `writer.await` assertion real
    ///
    /// The first version of this test simply asserted that the body had arrived once
    /// `serve_and_drain` returned. **That mutant survived**: dropping the `JoinHandle` instead
    /// of awaiting it detaches the writer rather than killing it, and on a current-thread
    /// runtime the detached task usually got polled during the `timeout` await anyway, so the
    /// push landed before the assertion looked. A pass that depends on the scheduler is not a
    /// detector.
    ///
    /// So the push is held open by a semaphore the test controls. A correct `serve_and_drain`
    /// is then *inside* `writer.await`, inside the flush, blocked -- and `is_finished()` must
    /// be false. Only an implementation that failed to wait can have returned. The healthy
    /// direction of that assertion needs no timing luck: while the gate is shut, a correct
    /// implementation cannot finish however long it is given.
    ///
    /// The control at the top matters as much. `FLUSH_INTERVAL_SECS` is an hour and the series
    /// threshold is out of reach, so the only arm of `run_writer` that can push here is the
    /// channel-closed one; `assert!(try_recv().is_err())` before the signal proves that,
    /// rather than leaving the final assertion satisfiable by an ordinary timed flush.
    #[tokio::test]
    async fn shutdown_flushes_data_that_is_still_buffered() {
        let _log = LogTail::start();

        let config =
            crate::config::Config::from_values(Some("3600"), Some("100000"), None, Some("1"));
        let (tx, rx) = channel(16);
        let state = AppState::new(tx);

        // Zero permits: the writer's push blocks here until the test opens it.
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let writer_gate = Arc::clone(&gate);
        let (pushed_tx, mut pushed_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let writer = tokio::spawn(run_writer(rx, config, move |body| {
            let pushed_tx = pushed_tx.clone();
            let gate = Arc::clone(&writer_gate);
            async move {
                let _permit = gate
                    .acquire_owned()
                    .await
                    .expect("the gate is never closed");
                let _ = pushed_tx.send(body);
                Ok(200u16)
            }
        }));

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_and_drain(
            listener,
            app(state.clone()),
            state,
            writer,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // A real HTTP delivery, so the `Sender` the drain waits on is the one axum cloned
        // into a handler -- not one this test conveniently kept a reference to.
        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{addr}/"))
            .header("X-Amz-Firehose-Request-Id", REQ_ID)
            .body(firehose_body(&[&fresh_line()]))
            .send()
            .await
            .expect("the server must accept the delivery");
        assert_eq!(response.status().as_u16(), 200, "the delivery was accepted");
        // Release the pooled keep-alive connection before signalling: an idle connection is
        // one `with_graceful_shutdown` has to wait out, and this test would then be measuring
        // reqwest's pool rather than the drain.
        drop(response);
        drop(client);

        assert!(
            pushed_rx.try_recv().is_err(),
            "CONTROL: the flush interval is an hour away and the threshold is out of reach, \
             so nothing may have been pushed yet"
        );

        shutdown_tx.send(()).expect("the server is still running");

        // With the gate shut, a correct `serve_and_drain` is parked inside the writer's final
        // flush and cannot have returned. This is the assertion that catches a dropped
        // `JoinHandle` -- see the note above on why the obvious version of it does not.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !server.is_finished(),
            "serve_and_drain returned while the final flush was still in flight; the writer's \
             JoinHandle must be awaited, not dropped"
        );
        assert!(
            pushed_rx.try_recv().is_err(),
            "CONTROL: the gate is shut, so nothing can have been pushed through it yet"
        );

        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(30), server)
            .await
            .expect("serve_and_drain must return; a sender that outlives it hangs here")
            .expect("serve_and_drain must not panic");

        let body = pushed_rx
            .try_recv()
            .expect("the buffered batch must reach the wire before the process exits");
        let request = decode_write_request(&body);
        let names = metric_names(&request);
        assert!(
            names.contains(&"firehose_test_m_count_max"),
            "the drained payload must carry the record that was accepted, got {names:?}"
        );
        assert!(
            pushed_rx.try_recv().is_err(),
            "one flush, not a loop of them"
        );
    }

    /// The other half: with nothing buffered, the same path still terminates promptly rather
    /// than waiting for an interval tick that is an hour away. Without this,
    /// `shutdown_flushes_data_that_is_still_buffered` would pass against an implementation
    /// that only ever exits because a flush happened to be due.
    #[tokio::test]
    async fn shutdown_returns_promptly_with_an_empty_buffer() {
        let _log = LogTail::start();

        let config =
            crate::config::Config::from_values(Some("3600"), Some("100000"), None, Some("1"));
        let (tx, rx) = channel(16);
        let state = AppState::new(tx);

        let (pushed_tx, mut pushed_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let writer = tokio::spawn(run_writer(rx, config, move |body| {
            let pushed_tx = pushed_tx.clone();
            async move {
                let _ = pushed_tx.send(body);
                Ok(200u16)
            }
        }));

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_and_drain(
            listener,
            app(state.clone()),
            state,
            writer,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        shutdown_tx.send(()).expect("the server is still running");
        tokio::time::timeout(Duration::from_secs(30), server)
            .await
            .expect("an idle server must still return after the shutdown signal")
            .expect("serve_and_drain must not panic");

        assert!(
            pushed_rx.try_recv().is_err(),
            "an empty accumulator must not push an empty write request"
        );
    }

    #[tokio::test]
    async fn a_request_with_no_source_arn_anywhere_is_still_accepted() {
        let tail = LogTail::start();
        let (state, _rx) = state_with_capacity(4);
        let body = firehose_body(&[]);

        let (status, json, content_type) =
            call(app(state.clone()), post().body(Body::from(body)).unwrap()).await;

        assert_eq!(status, StatusCode::OK);
        assert_conforming(status, &json, &content_type);
        assert!(state.firehose_arns.read().await.is_empty());
        assert!(
            tail.tail().contains("no source arn"),
            "the absence must be visible in the log"
        );
    }
}
