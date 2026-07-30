use prometheus_remote_write::{Label, Sample, TimeSeries, WriteRequest};
use std::collections::{BTreeMap, HashMap};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
