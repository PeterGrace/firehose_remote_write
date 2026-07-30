use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub flush_interval_secs: u64,
    pub flush_max_series: usize,
    pub channel_capacity: usize,
    pub push_max_retries: u32,
}

/// Parse one environment-derived value, falling back to `default` and saying so.
///
/// `name` exists only so the warning can identify which variable was rejected. A config
/// value that is silently replaced is the same failure mode this project keeps finding
/// elsewhere: `FLUSH_MAX_SERIES=20_000` is accepted by the operator's eyes, parsed as
/// nothing, and becomes 2000 — a twentieth of what they asked for, with no evidence.
///
/// The raw string is trimmed before parsing. `FromStr` for the integer types rejects
/// surrounding whitespace, and a trailing space is trivially produced by a Helm value, a
/// YAML scalar, or a `.env` file — none of which look wrong to the person who wrote them.
///
/// The warning reports the **untrimmed** value, quoted via `{:?}`. That matters for the
/// all-whitespace case: `"  "` trims to `""`, and a message ending in `using default 1`
/// after an invisible value reads like a bug in the logger rather than a rejected setting.
/// Quoting shows `"  "` and naming the original shows what the operator actually set.
fn parse_or<T: std::str::FromStr + std::fmt::Display>(
    name: &str,
    raw: Option<&str>,
    default: T,
) -> T {
    // An unset variable is not a mistake and must not warn — only a *present but unusable*
    // one is worth an operator's attention.
    let Some(raw) = raw else { return default };

    match raw.trim().parse::<T>() {
        Ok(value) => value,
        Err(_) => {
            warn!("{name}={raw:?} is not a valid value; using default {default}");
            default
        }
    }
}

impl Config {
    /// Split out from `from_env` so the parsing is testable without touching process env.
    pub fn from_values(
        flush_interval_secs: Option<&str>,
        flush_max_series: Option<&str>,
        channel_capacity: Option<&str>,
        push_max_retries: Option<&str>,
    ) -> Self {
        let flush_interval_secs = parse_or("FLUSH_INTERVAL_SECS", flush_interval_secs, 1u64);
        let flush_max_series = parse_or("FLUSH_MAX_SERIES", flush_max_series, 2000usize);
        let channel_capacity = parse_or("CHANNEL_CAPACITY", channel_capacity, 1024usize);

        // The three zero-guards below each warn for the same reason `parse_or` does, but they
        // are a genuinely different failure: the value parsed fine and the operator's intent
        // was unambiguous, we are simply refusing to honour it. Saying which value was
        // rejected AND why zero is unusable is what turns "my setting did nothing" into a
        // one-line diagnosis.
        Self {
            flush_interval_secs: if flush_interval_secs == 0 {
                warn!("FLUSH_INTERVAL_SECS=0 would spin the writer loop with no delay between flushes; using default 1");
                1
            } else {
                flush_interval_secs
            },
            flush_max_series: if flush_max_series == 0 {
                warn!("FLUSH_MAX_SERIES=0 would flush on every series, turning one write request per batch into one per sample; using default 2000");
                2000
            } else {
                flush_max_series
            },
            channel_capacity: if channel_capacity == 0 {
                warn!("CHANNEL_CAPACITY=0 would panic tokio's mpsc channel on construction; using default 1024");
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
            push_max_retries: parse_or("PUSH_MAX_RETRIES", push_max_retries, 3u32),
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

    /// Padding is tolerated. `FromStr` for the integer types rejects surrounding whitespace,
    /// so without the trim in `parse_or` these silently became the defaults — and a leading
    /// or trailing space is trivially produced by a YAML scalar, a Helm value, or a `.env`
    /// file, none of which look wrong to the person who wrote them. Both sides and both
    /// orders are covered so that trimming only one end fails here.
    #[test]
    fn whitespace_padded_values_are_trimmed_and_parsed() {
        let c = Config::from_values(Some(" 5"), Some("10 "), Some(" 20 "), Some("\t7\n"));
        assert_eq!(c.flush_interval_secs, 5);
        assert_eq!(c.flush_max_series, 10);
        assert_eq!(c.channel_capacity, 20);
        assert_eq!(c.push_max_retries, 7);
    }

    /// The interaction the trim creates: `"  "` trims to `""`, which parses as nothing and
    /// takes the default. That is the right outcome — an all-whitespace setting is not a
    /// number — but it is the case whose warning would otherwise read as `FLUSH_INTERVAL_SECS=
    /// is not a valid value`, so `parse_or` reports the untrimmed value quoted.
    #[test]
    fn an_all_whitespace_value_falls_back_to_the_default() {
        let c = Config::from_values(Some("   "), Some("\t"), Some(" "), Some("\n"));
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
        assert_eq!(c.push_max_retries, 3);
    }

    /// Trimming must not make an otherwise-invalid value parse. Internal whitespace is still
    /// an error, so `5 0` cannot quietly become `50`.
    #[test]
    fn internal_whitespace_is_still_unparseable() {
        let c = Config::from_values(Some("5 0"), None, None, None);
        assert_eq!(c.flush_interval_secs, 1);
    }

    /// A trimmed value still meets the zero-guards: `" 0 "` must be rejected exactly as `"0"`
    /// is, not slip past because the guard runs on a differently-parsed value.
    #[test]
    fn a_padded_zero_still_hits_the_zero_guards() {
        let c = Config::from_values(Some(" 0 "), Some(" 0 "), Some(" 0 "), None);
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
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
