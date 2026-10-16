//! Generated persistence coverage and cached-payload bypass, without a plugin.
use moose::core::custom_state::{PersistField, StateCursor};
use moose::params::{
    Params, PersistFingerprint, PersistFingerprintVisitor, PreparedPersistWrite,
    write_prepared_persist,
};
use moose_derive::Params;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

#[derive(Default)]
struct CachedPayload {
    value: AtomicU64,
    writes: AtomicUsize,
}

impl PersistField for CachedPayload {
    fn persist_write(&self, bytes: &mut Vec<u8>) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        bytes.extend_from_slice(&self.value.load(Ordering::Relaxed).to_le_bytes());
    }

    fn persist_read(&self, _: &mut StateCursor) {}

    fn persist_fingerprint(&self) -> Option<PersistFingerprint> {
        // A test identity, not an example digest algorithm. The consumer owns
        // hashing policy; this fixture verifies the derive visits the override.
        let mut digest = [0; 32];
        digest[..8].copy_from_slice(&self.value.load(Ordering::Relaxed).to_le_bytes());
        Some(PersistFingerprint {
            byte_len: 8,
            digest,
        })
    }
}

#[derive(Params)]
struct Nested {
    #[persist = "added-later"]
    flag: Mutex<bool>,
}

#[derive(Params)]
#[params(pre_save = "prepare")]
struct Parent {
    #[persist = "payload"]
    payload: CachedPayload,
    #[persist = "plain"]
    plain: Mutex<u32>,
    #[nested]
    nested: Nested,
    #[skip]
    prepares: AtomicUsize,
}

impl Parent {
    fn prepare(&self) {
        self.prepares.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Event {
    Begin(String),
    End,
    Cached(PersistFingerprint),
    Bytes(Vec<u8>),
}

#[derive(Default)]
struct Recorder(Vec<Event>);

impl PersistFingerprintVisitor for Recorder {
    fn begin_field(&mut self, key: &str) {
        self.0.push(Event::Begin(key.to_owned()));
    }
    fn end_field(&mut self) {
        self.0.push(Event::End);
    }
    fn fingerprint(&mut self, value: PersistFingerprint) {
        self.0.push(Event::Cached(value));
    }
    fn serialized(&mut self, bytes: &[u8]) {
        self.0.push(Event::Bytes(bytes.to_vec()));
    }
}

fn record(params: &impl Params) -> Vec<Event> {
    let mut visitor = Recorder::default();
    params.visit_persist_fingerprints(&mut visitor);
    visitor.0
}

#[test]
fn cached_payload_bypasses_serialization_and_nested_fields_are_visited() {
    let params = Parent::default();
    let events = record(&params);
    assert_eq!(params.prepares.load(Ordering::Relaxed), 1);
    assert_eq!(params.payload.writes.load(Ordering::Relaxed), 0);
    assert_eq!(
        events,
        vec![
            Event::Begin("payload".into()),
            Event::Cached(PersistFingerprint {
                byte_len: 8,
                digest: [0; 32]
            }),
            Event::End,
            Event::Begin("plain".into()),
            Event::Bytes(vec![0; 4]),
            Event::End,
            Event::Begin("nested".into()),
            Event::Begin("added-later".into()),
            Event::Bytes(vec![0]),
            Event::End,
            Event::End,
        ]
    );
}

#[test]
fn unchanged_content_is_stable_and_each_leaf_changes_its_identity() {
    let params = Parent::default();
    let before = record(&params);
    assert_eq!(record(&params), before);
    *params.plain.lock().unwrap() = 4;
    let plain = record(&params);
    assert_ne!(plain, before);
    assert_eq!(record(&params), plain);
    *params.nested.flag.lock().unwrap() = true;
    let nested = record(&params);
    assert_ne!(nested, plain);
    params.payload.value.store(7, Ordering::Relaxed);
    assert_ne!(record(&params), nested);
    assert_eq!(params.payload.writes.load(Ordering::Relaxed), 0);
}

#[test]
fn fingerprint_visit_does_not_change_saved_bytes() {
    let params = Parent::default();
    let before = params.serialize_persist();
    let _ = record(&params);
    assert_eq!(params.payload.writes.load(Ordering::Relaxed), 1);
    assert_eq!(params.serialize_persist(), before);
}

#[test]
fn prepared_write_matches_existing_keyed_bytes_and_freezes_controls() {
    let params = Parent::default();
    *params.plain.lock().unwrap() = 7;
    *params.nested.flag.lock().unwrap() = true;
    let expected = params.serialize_persist();
    let before = params.prepares.load(Ordering::Relaxed);
    let snapshot = params.prepare_persist_write().unwrap();
    assert_eq!(params.prepares.load(Ordering::Relaxed), before + 1);
    *params.plain.lock().unwrap() = 8;
    *params.nested.flag.lock().unwrap() = false;
    let mut output = Vec::new();
    write_prepared_persist(snapshot.as_ref(), &mut output).unwrap();
    assert_eq!(snapshot.byte_len(), expected.len() as u64);
    assert_eq!(output, expected);
    assert_ne!(output, params.serialize_persist());
}

#[derive(Default)]
struct SharedPayload {
    bytes: Mutex<std::sync::Arc<Vec<u8>>>,
    writes: AtomicUsize,
}

struct SharedSnapshot(std::sync::Arc<Vec<u8>>);

impl PreparedPersistWrite for SharedSnapshot {
    fn byte_len(&self) -> u64 {
        self.0.len() as u64
    }
    fn write_to(&self, output: &mut dyn std::io::Write) -> std::io::Result<()> {
        output.write_all(&self.0)
    }
}

impl PersistField for SharedPayload {
    fn persist_write(&self, bytes: &mut Vec<u8>) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        bytes.extend_from_slice(&self.bytes.lock().unwrap());
    }
    fn persist_read(&self, _: &mut StateCursor) {}
    fn prepare_persist_write(&self) -> std::io::Result<Box<dyn PreparedPersistWrite>> {
        Ok(Box::new(SharedSnapshot(self.bytes.lock().unwrap().clone())))
    }
}

#[derive(Params)]
struct Streamed {
    #[persist]
    payload: SharedPayload,
    #[nested]
    empty: Empty,
}

#[derive(Params)]
#[params(pre_save = "prepare")]
struct Empty {
    #[skip]
    prepares: AtomicUsize,
}

impl Empty {
    fn prepare(&self) {
        self.prepares.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn large_override_retains_only_shared_payload_and_visits_empty_nested_hook() {
    let params = Streamed::default();
    let payload = std::sync::Arc::new(vec![17; 1024 * 1024]);
    *params.payload.bytes.lock().unwrap() = payload.clone();
    let expected = params.serialize_persist();
    assert_eq!(params.empty.prepares.load(Ordering::Relaxed), 1);
    let snapshot = params.prepare_persist_write().unwrap();
    assert_eq!(params.payload.writes.load(Ordering::Relaxed), 1);
    assert_eq!(std::sync::Arc::strong_count(&payload), 3);
    assert_eq!(params.empty.prepares.load(Ordering::Relaxed), 2);
    // Destroy the store before output: the writer must be owned, not borrowing
    // live locks or reading controls that could have changed in the meantime.
    drop(params);
    assert_eq!(std::sync::Arc::strong_count(&payload), 2);
    let mut output = Vec::new();
    write_prepared_persist(snapshot.as_ref(), &mut output).unwrap();
    assert_eq!(output, expected);
    drop(snapshot);
    assert_eq!(std::sync::Arc::strong_count(&payload), 1);
}

#[test]
fn write_snapshots_preserve_the_existing_generic_read_commit_api() {
    let params = Parent::default();
    let commit: moose::params::PreparedPersist<Parent> = Box::new(|params, _, _| {
        *params.plain.lock().unwrap() = 19;
    });
    commit(&params, &[], &[]);
    assert_eq!(*params.plain.lock().unwrap(), 19);
    assert!(Parent::prepare_persist_stream(&mut &[][..], 0).is_none());
    assert!(params.prepare_persist_write().is_ok());
}

#[derive(Default)]
struct Refused;
impl PersistField for Refused {
    fn persist_write(&self, _: &mut Vec<u8>) {
        panic!("failed prepared field fell back to serialization");
    }
    fn persist_read(&self, _: &mut StateCursor) {}
    fn prepare_persist_write(&self) -> std::io::Result<Box<dyn PreparedPersistWrite>> {
        Err(std::io::Error::other("field unavailable"))
    }
}
#[derive(Params)]
struct RefusedStore {
    #[persist]
    value: Refused,
}
#[test]
fn preparation_error_propagates_without_silent_field_omission() {
    assert!(RefusedStore::default().prepare_persist_write().is_err());
}

#[derive(Params)]
struct ZeroFields {}

#[test]
fn persistence_only_and_empty_stores_have_no_fake_host_parameters() {
    let mut nested = Nested::default();
    nested.offset_ids(100);
    assert_eq!(nested.count(), 0);
    assert!(nested.param_infos().is_empty());
    assert_eq!(nested.collect_values(), (vec![], vec![]));
    assert_eq!(nested.get_normalized(100), None);
    nested.set_normalized(100, 0.5);
    assert_eq!(*nested.flag.lock().unwrap(), false);
    let empty = ZeroFields::default();
    assert_eq!(empty.count(), 0);
    assert!(empty.serialize_persist().is_empty());
    assert_eq!(empty.prepare_persist_write().unwrap().byte_len(), 0);
    assert!(record(&empty).is_empty());
    let skipped = Empty::default();
    assert_eq!(skipped.count(), 0);
    assert!(record(&skipped).is_empty());
    assert_eq!(skipped.prepares.load(Ordering::Relaxed), 1);
}

#[derive(Default)]
struct NotifyingPayload {
    signal: Mutex<std::sync::Weak<moose::params::PersistChangeSignal>>,
    pending: std::sync::atomic::AtomicBool,
}
impl PersistField for NotifyingPayload {
    fn persist_write(&self, bytes: &mut Vec<u8>) {
        bytes.push(0);
    }
    fn persist_read(&self, _: &mut StateCursor) {}
    fn set_persist_change_signal(
        &self,
        signal: std::sync::Weak<moose::params::PersistChangeSignal>,
    ) {
        *self.signal.lock().unwrap() = signal;
    }
}
impl NotifyingPayload {
    fn complete(&self) {
        self.pending.store(true, Ordering::Release);
        let signal = self.signal.lock().unwrap().upgrade();
        if let Some(signal) = signal {
            signal.notify();
        }
    }
}
#[derive(Params)]
struct NotificationLeaf {
    #[persist]
    payload: NotifyingPayload,
}
#[derive(Params)]
#[params(persist_changed = "drain_completion")]
struct NotificationRoot {
    #[nested]
    child: NotificationLeaf,
}
impl NotificationRoot {
    fn drain_completion(&self) -> Option<(u32, f64)> {
        self.child
            .payload
            .pending
            .swap(false, Ordering::AcqRel)
            .then_some((7, 0.25))
    }
}
#[test]
fn completion_signal_traverses_nested_fields_without_an_editor_and_closes_safely() {
    let params = NotificationRoot::default();
    assert!(params.wants_persist_notifications());
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let signal = std::sync::Arc::new(moose::params::PersistChangeSignal::new(move || {
        observed.fetch_add(1, Ordering::Relaxed);
    }));
    params.set_persist_change_signal(std::sync::Arc::downgrade(&signal));
    std::thread::scope(|scope| {
        scope
            .spawn(|| params.child.payload.complete())
            .join()
            .unwrap();
    });
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(params.take_persist_change(), Some((7, 0.25)));
    assert_eq!(params.take_persist_change(), None);
    signal.close();
    params.child.payload.complete();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "no host access after teardown gate closes"
    );
    drop(signal);
    assert!(
        params
            .child
            .payload
            .signal
            .lock()
            .unwrap()
            .upgrade()
            .is_none()
    );
    assert!(!Nested::default().wants_persist_notifications());
    assert_eq!(Nested::default().take_persist_change(), None);
}
