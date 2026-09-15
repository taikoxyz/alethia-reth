//! Test-only metric capture shared by sidecar lifecycle and pin instrumentation tests.
//!
//! Gauges read as absent until the code under test sets them, so an assertion on a value proves
//! a write happened rather than a registration.

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use std::sync::{Arc, Mutex, atomic::Ordering};

#[derive(Default)]
pub(super) struct TestMetrics {
    gauges: Mutex<Vec<(Key, Arc<metrics::atomics::AtomicU64>)>>,
}

/// Bit pattern a freshly registered gauge holds until its first `set`; never produced by `set`
/// in this crate, so it distinguishes "registered" from "written with 0.0".
const UNSET: u64 = f64::NAN.to_bits();

impl TestMetrics {
    pub(super) fn value(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let key = Key::from_parts(
            name.to_owned(),
            labels
                .iter()
                .map(|(k, v)| metrics::Label::new((*k).to_owned(), (*v).to_owned()))
                .collect::<Vec<_>>(),
        );
        self.gauges
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, value)| value.load(Ordering::Relaxed))
            .filter(|bits| *bits != UNSET)
            .map(f64::from_bits)
    }
}

impl Recorder for TestMetrics {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, _: &Key, _: &Metadata<'_>) -> Counter {
        Counter::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        let mut gauges = self.gauges.lock().unwrap();
        if let Some((_, value)) = gauges.iter().find(|(k, _)| k == key) {
            return Gauge::from_arc(value.clone());
        }
        let value = Arc::new(metrics::atomics::AtomicU64::new(UNSET));
        gauges.push((key.clone(), value.clone()));
        Gauge::from_arc(value)
    }
}

#[cfg(test)]
mod tests {
    use super::TestMetrics;

    #[test]
    fn registered_gauges_read_as_absent_until_set() {
        let recorder = TestMetrics::default();
        metrics::with_local_recorder(&recorder, || {
            let gauge = metrics::gauge!("taiko_test_gauge", "phase" => "unit");
            assert_eq!(recorder.value("taiko_test_gauge", &[("phase", "unit")]), None);
            gauge.set(0.0);
            assert_eq!(recorder.value("taiko_test_gauge", &[("phase", "unit")]), Some(0.0));
        });
    }
}
