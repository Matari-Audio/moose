//! Generated persistence coverage and cached-payload bypass, without a plugin.
use moose::core::custom_state::{PersistField, StateCursor};
use moose::params::{Params, PersistFingerprint, PersistFingerprintVisitor};
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
        Some(PersistFingerprint { byte_len: 8, digest })
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
    assert_eq!(events, vec![
        Event::Begin("payload".into()),
        Event::Cached(PersistFingerprint { byte_len: 8, digest: [0; 32] }),
        Event::End,
        Event::Begin("plain".into()),
        Event::Bytes(vec![0; 4]),
        Event::End,
        Event::Begin("nested".into()),
        Event::Begin("added-later".into()),
        Event::Bytes(vec![0]),
        Event::End,
        Event::End,
    ]);
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
