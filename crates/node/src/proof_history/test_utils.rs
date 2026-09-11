//! Test-only metric capture shared by sidecar lifecycle and pin instrumentation tests.

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use std::sync::{Arc, Mutex, atomic::Ordering};

#[derive(Default)]
pub(super) struct TestMetrics {
    gauges: Mutex<Vec<(Key, Arc<metrics::atomics::AtomicU64>)>>,
}

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
            .map(|(_, value)| f64::from_bits(value.load(Ordering::Relaxed)))
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
        let value = Arc::new(metrics::atomics::AtomicU64::new(0));
        gauges.push((key.clone(), value.clone()));
        Gauge::from_arc(value)
    }
}
