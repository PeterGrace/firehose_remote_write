use std::env;

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
            flush_interval_secs: if flush_interval_secs == 0 {
                1
            } else {
                flush_interval_secs
            },
            // Zero would flush on every single sample, turning one write request per batch
            // into one per sample.
            flush_max_series: if flush_max_series == 0 {
                2000
            } else {
                flush_max_series
            },
            // A zero-capacity `tokio::sync::mpsc` channel panics on construction, so this
            // guard is what stops a stray `CHANNEL_CAPACITY=0` from crashing startup.
            channel_capacity: if channel_capacity == 0 {
                1024
            } else {
                channel_capacity
            },
            // Deliberately NOT zero-guarded. Zero attempts is a coherent thing to ask for in
            // a way that a zero flush interval is not, and `push_with_retry` reads this as an
            // attempt count floored at 1 -- so `PUSH_MAX_RETRIES=0` means "try once, do not
            // retry", which is a real operational choice (fail fast, let Firehose redeliver).
            // Silently rewriting it to 3 would be worse than the floor. See the module test
            // `zero_push_max_retries_is_preserved_not_defaulted`.
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

    /// Neutralizing this guard failed no test before this existed. Zero means "flush after
    /// every series", which turns one write request per batch into one per sample — the
    /// request storm the accumulator exists to avoid.
    #[test]
    fn zero_flush_max_series_falls_back_to_default() {
        let c = Config::from_values(None, Some("0"), None, None);
        assert_eq!(c.flush_max_series, 2000);
    }

    /// Also unwatched before this existed, and the most severe of the three: a zero-capacity
    /// `tokio::sync::mpsc::channel` panics on construction, so `CHANNEL_CAPACITY=0` would
    /// take the process down at startup rather than merely misbehaving.
    #[test]
    fn zero_channel_capacity_falls_back_to_default() {
        let c = Config::from_values(None, None, Some("0"), None);
        assert_eq!(c.channel_capacity, 1024);
    }

    /// `push_max_retries` is deliberately NOT zero-guarded, so pin that as intent rather than
    /// leaving it looking like an omission. Zero attempts is a coherent request ("try once,
    /// never retry; let Firehose redeliver") in a way that a zero flush interval or a
    /// zero-capacity channel is not, and `push_with_retry` floors it at 1. Rewriting 0 to the
    /// default 3 would silently give an operator triple the request volume they asked for.
    #[test]
    fn zero_push_max_retries_is_preserved_not_defaulted() {
        let c = Config::from_values(None, None, None, Some("0"));
        assert_eq!(c.push_max_retries, 0);
    }

    /// Every field is an unsigned type, so a negative value fails to parse and takes the
    /// default. Distinct from `banana`: a negative is a plausible operator mistake rather
    /// than a nonsense string, and it is worth pinning that it cannot wrap to a huge
    /// unsigned value.
    #[test]
    fn negative_values_fall_back_to_defaults() {
        let c = Config::from_values(Some("-1"), Some("-1"), Some("-1"), Some("-1"));
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
        assert_eq!(c.push_max_retries, 3);
    }

    /// Documented sharp edge, not an endorsement. `FromStr` for the integer types rejects
    /// surrounding whitespace, so `FLUSH_INTERVAL_SECS=" 5"` — trivially produced by a YAML
    /// list, a Helm value, or a trailing space in a `.env` file — silently becomes the
    /// default with no log line. If this is ever changed to trim, this test is the one that
    /// should be updated, deliberately.
    #[test]
    fn whitespace_padded_values_do_not_parse_and_take_the_default() {
        let c = Config::from_values(Some(" 5"), Some("10 "), None, None);
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
    }

    /// The only test that can catch a typo'd or transposed environment variable name.
    /// `from_values` is exhaustively tested, but nothing else checks that `from_env` reads
    /// `FLUSH_MAX_SERIES` into `flush_max_series` rather than into `channel_capacity` — a
    /// mistake that compiles, passes every other test, and means the operator's settings
    /// silently never apply.
    ///
    /// All process-env mutation in this crate's tests is confined to this one function so it
    /// cannot race a sibling: `cargo test` runs tests as threads in one process, and nothing
    /// else here reads these four variables. The values are pairwise distinct so a
    /// transposition cannot pass.
    #[test]
    fn from_env_reads_each_variable_into_its_own_field() {
        const VARS: [(&str, &str); 4] = [
            ("FLUSH_INTERVAL_SECS", "11"),
            ("FLUSH_MAX_SERIES", "22"),
            ("CHANNEL_CAPACITY", "33"),
            ("PUSH_MAX_RETRIES", "44"),
        ];
        for (k, v) in VARS {
            env::set_var(k, v);
        }

        let c = Config::from_env();

        for (k, _) in VARS {
            env::remove_var(k);
        }

        assert_eq!(c.flush_interval_secs, 11);
        assert_eq!(c.flush_max_series, 22);
        assert_eq!(c.channel_capacity, 33);
        assert_eq!(c.push_max_retries, 44);

        // With the variables removed again, `from_env` must fall back to the defaults —
        // which also proves the removal above actually took effect and left no state behind
        // for another run.
        let c = Config::from_env();
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
        assert_eq!(c.push_max_retries, 3);
    }
}
