use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub flush_interval_secs: u64,
    pub flush_max_series: usize,
    pub channel_capacity: usize,
    pub push_max_attempts: u32,
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
        push_max_attempts: Option<&str>,
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
            // Deliberately NOT zero-guarded. Zero is a coherent thing to ask for in a way
            // that a zero flush interval is not: `PUSH_MAX_ATTEMPTS=0` means "try once, do
            // not retry", a real operational choice (fail fast, let Firehose redeliver).
            // Silently rewriting it to 3 would give an operator triple the request volume
            // they asked for. See `zero_push_max_attempts_is_preserved_not_defaulted`.
            //
            // Named *attempts*, not *retries*, because `push_with_retry` floors it at 1.
            // Under the old name, 0 and 1 both produced a single attempt -- two settings, one
            // behaviour, and a name implying otherwise. The floor is honest against this name.
            push_max_attempts: parse_or("PUSH_MAX_ATTEMPTS", push_max_attempts, 3u32),
        }
    }

    pub fn from_env() -> Self {
        Self::from_values(
            env::var("FLUSH_INTERVAL_SECS").ok().as_deref(),
            env::var("FLUSH_MAX_SERIES").ok().as_deref(),
            env::var("CHANNEL_CAPACITY").ok().as_deref(),
            env::var("PUSH_MAX_ATTEMPTS").ok().as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testlog::LogTail;

    #[test]
    fn defaults_apply_when_unset() {
        let _log = LogTail::start();
        let c = Config::from_values(None, None, None, None);
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
        assert_eq!(c.push_max_attempts, 3);
    }

    #[test]
    fn values_are_parsed_when_present() {
        let _log = LogTail::start();
        let c = Config::from_values(Some("5"), Some("10"), Some("20"), Some("7"));
        assert_eq!(c.flush_interval_secs, 5);
        assert_eq!(c.flush_max_series, 10);
        assert_eq!(c.channel_capacity, 20);
        assert_eq!(c.push_max_attempts, 7);
    }

    #[test]
    fn unparseable_values_fall_back_to_defaults() {
        let _log = LogTail::start();
        let c = Config::from_values(Some("banana"), None, None, None);
        assert_eq!(c.flush_interval_secs, 1);
    }

    #[test]
    fn zero_flush_interval_falls_back_to_default() {
        let _log = LogTail::start();
        let c = Config::from_values(Some("0"), None, None, None);
        assert_eq!(c.flush_interval_secs, 1);
    }

    /// Neutralizing this guard failed no test before this existed. Zero means "flush after
    /// every series", which turns one write request per batch into one per sample — the
    /// request storm the accumulator exists to avoid.
    #[test]
    fn zero_flush_max_series_falls_back_to_default() {
        let _log = LogTail::start();
        let c = Config::from_values(None, Some("0"), None, None);
        assert_eq!(c.flush_max_series, 2000);
    }

    /// Also unwatched before this existed, and the most severe of the three: a zero-capacity
    /// `tokio::sync::mpsc::channel` panics on construction, so `CHANNEL_CAPACITY=0` would
    /// take the process down at startup rather than merely misbehaving.
    #[test]
    fn zero_channel_capacity_falls_back_to_default() {
        let _log = LogTail::start();
        let c = Config::from_values(None, None, Some("0"), None);
        assert_eq!(c.channel_capacity, 1024);
    }

    /// `push_max_attempts` is deliberately NOT zero-guarded, so pin that as intent rather than
    /// leaving it looking like an omission. Zero attempts is a coherent request ("try once,
    /// never retry; let Firehose redeliver") in a way that a zero flush interval or a
    /// zero-capacity channel is not, and `push_with_retry` floors it at 1. Rewriting 0 to the
    /// default 3 would silently give an operator triple the request volume they asked for.
    #[test]
    fn zero_push_max_attempts_is_preserved_not_defaulted() {
        let _log = LogTail::start();
        let c = Config::from_values(None, None, None, Some("0"));
        assert_eq!(c.push_max_attempts, 0);
    }

    /// Every field is an unsigned type, so a negative value fails to parse and takes the
    /// default. Distinct from `banana`: a negative is a plausible operator mistake rather
    /// than a nonsense string, and it is worth pinning that it cannot wrap to a huge
    /// unsigned value.
    #[test]
    fn negative_values_fall_back_to_defaults() {
        let _log = LogTail::start();
        let c = Config::from_values(Some("-1"), Some("-1"), Some("-1"), Some("-1"));
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
        assert_eq!(c.push_max_attempts, 3);
    }

    /// Padding is tolerated. `FromStr` for the integer types rejects surrounding whitespace,
    /// so without the trim in `parse_or` these silently became the defaults — and a leading
    /// or trailing space is trivially produced by a YAML scalar, a Helm value, or a `.env`
    /// file, none of which look wrong to the person who wrote them. Both sides and both
    /// orders are covered so that trimming only one end fails here.
    #[test]
    fn whitespace_padded_values_are_trimmed_and_parsed() {
        let _log = LogTail::start();
        let c = Config::from_values(Some(" 5"), Some("10 "), Some(" 20 "), Some("\t7\n"));
        assert_eq!(c.flush_interval_secs, 5);
        assert_eq!(c.flush_max_series, 10);
        assert_eq!(c.channel_capacity, 20);
        assert_eq!(c.push_max_attempts, 7);
    }

    /// The interaction the trim creates: `"  "` trims to `""`, which parses as nothing and
    /// takes the default. That is the right outcome — an all-whitespace setting is not a
    /// number — but it is the case whose warning would otherwise read as `FLUSH_INTERVAL_SECS=
    /// is not a valid value`, so `parse_or` reports the untrimmed value quoted.
    #[test]
    fn an_all_whitespace_value_falls_back_to_the_default() {
        let _log = LogTail::start();
        let c = Config::from_values(Some("   "), Some("\t"), Some(" "), Some("\n"));
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
        assert_eq!(c.push_max_attempts, 3);
    }

    /// Trimming must not make an otherwise-invalid value parse. Internal whitespace is still
    /// an error, so `5 0` cannot quietly become `50`.
    #[test]
    fn internal_whitespace_is_still_unparseable() {
        let _log = LogTail::start();
        let c = Config::from_values(Some("5 0"), None, None, None);
        assert_eq!(c.flush_interval_secs, 1);
    }

    /// A trimmed value still meets the zero-guards: `" 0 "` must be rejected exactly as `"0"`
    /// is, not slip past because the guard runs on a differently-parsed value.
    #[test]
    fn a_padded_zero_still_hits_the_zero_guards() {
        let _log = LogTail::start();
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
        let _log = LogTail::start();
        const VARS: [(&str, &str); 4] = [
            ("FLUSH_INTERVAL_SECS", "11"),
            ("FLUSH_MAX_SERIES", "22"),
            ("CHANNEL_CAPACITY", "33"),
            ("PUSH_MAX_ATTEMPTS", "44"),
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
        assert_eq!(c.push_max_attempts, 44);

        // With the variables removed again, `from_env` must fall back to the defaults —
        // which also proves the removal above actually took effect and left no state behind
        // for another run.
        let c = Config::from_env();
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.flush_max_series, 2000);
        assert_eq!(c.channel_capacity, 1024);
        assert_eq!(c.push_max_attempts, 3);
    }

    /// Every fallback must be diagnosable from the logs, and must name the variable — a
    /// warning saying only "invalid value" leaves an operator with four candidates and no way
    /// to tell which of their settings was thrown away.
    ///
    /// Deleting this `warn!` killed nothing before the log harness moved to `crate::testlog`;
    /// `config::tests` could not reach the sink while it lived inside `series::tests`.
    #[test]
    fn an_unparseable_value_warns_and_names_the_variable() {
        let tail = LogTail::start();
        // The value is unique to this test, so a sibling logging into the shared buffer in
        // the same window cannot make these assertions pass on somebody else's line.
        Config::from_values(Some("cfg-unparseable-aa"), None, None, None);
        let logs = tail.tail();

        assert!(
            logs.contains("FLUSH_INTERVAL_SECS"),
            "the warning must name the rejected variable, got: {logs}"
        );
        assert!(
            logs.contains("cfg-unparseable-aa"),
            "the warning must quote the value that was rejected, got: {logs}"
        );
        assert!(
            logs.contains("using default 1"),
            "the warning must say what is being used instead, got: {logs}"
        );
    }

    /// The all-whitespace case is the one whose message is easiest to get wrong: `"  "` trims
    /// to `""`, so reporting the *trimmed* value would print `FLUSH_INTERVAL_SECS= is not a
    /// valid value`, which reads like a logger bug rather than a rejected setting. Pin that
    /// the raw value is quoted so the whitespace is visible.
    #[test]
    fn an_all_whitespace_value_warns_with_the_value_quoted() {
        let tail = LogTail::start();
        Config::from_values(None, None, Some(" \t "), None);
        let logs = tail.tail();

        assert!(
            logs.contains("CHANNEL_CAPACITY=\" \\t \""),
            "the raw value must be quoted so invisible input stays visible, got: {logs}"
        );
    }

    /// Each zero-guard warns for a different reason, so each needs its own assertion: a
    /// single shared "value was zero" line would not tell an operator why their setting was
    /// unusable. Deleting any one of these three `warn!`s killed nothing before this test.
    #[test]
    fn each_zero_guard_warns_and_explains_why_zero_is_unusable() {
        let tail = LogTail::start();
        Config::from_values(Some("0"), Some("0"), Some("0"), None);
        let logs = tail.tail();

        for (var, reason, default) in [
            (
                "FLUSH_INTERVAL_SECS",
                "spin the writer loop",
                "using default 1",
            ),
            (
                "FLUSH_MAX_SERIES",
                "flush on every series",
                "using default 2000",
            ),
            (
                "CHANNEL_CAPACITY",
                "panic tokio's mpsc channel",
                "using default 1024",
            ),
        ] {
            // Match on ONE line carrying all three facts, rather than finding the first line
            // mentioning `var` and asserting about it. The first draft did the latter and
            // matched a concurrent sibling's unrelated `CHANNEL_CAPACITY` warning.
            assert!(
                logs.lines()
                    .any(|l| l.contains(var) && l.contains(reason) && l.contains(default)),
                "expected one warning naming {var}, saying {reason:?} and {default:?}, got: {logs}"
            );
        }
    }

    /// The one I care about most: an UNSET variable is not a mistake and must stay silent.
    /// Warning on absence would put four lines in the log of every clean startup, which is
    /// how operators are trained to ignore logs — and it would defeat the entire point of
    /// adding these warnings, since a real fallback would be indistinguishable from noise.
    ///
    /// This is the only test watching the early return in `parse_or`.
    #[test]
    fn nothing_is_logged_when_variables_are_simply_unset() {
        let tail = LogTail::start();
        let c = Config::from_values(None, None, None, None);
        let logs = tail.tail();

        // Values are still the defaults — silence must come from taking the unset path, not
        // from the function having done nothing at all.
        assert_eq!(c.flush_interval_secs, 1);
        assert_eq!(c.push_max_attempts, 3);

        for var in [
            "FLUSH_INTERVAL_SECS",
            "FLUSH_MAX_SERIES",
            "CHANNEL_CAPACITY",
            "PUSH_MAX_ATTEMPTS",
        ] {
            assert!(
                !logs.contains(var),
                "an unset {var} must not warn, got: {logs}"
            );
        }
    }

    /// A valid value must also stay silent — the warning is for rejected input only. Without
    /// this, moving the `warn!` out of the `Err` arm would go unnoticed by the tests above,
    /// which only ever assert that a warning *did* appear.
    #[test]
    fn nothing_is_logged_when_values_are_valid() {
        let tail = LogTail::start();
        Config::from_values(Some("5"), Some("10"), Some("20"), Some("7"));
        let logs = tail.tail();

        for var in [
            "FLUSH_INTERVAL_SECS",
            "FLUSH_MAX_SERIES",
            "CHANNEL_CAPACITY",
            "PUSH_MAX_ATTEMPTS",
        ] {
            assert!(
                !logs.contains(var),
                "a valid {var} must not warn, got: {logs}"
            );
        }
    }
}
