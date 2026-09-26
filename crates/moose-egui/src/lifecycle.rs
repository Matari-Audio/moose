use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use moose_core::editor::{EditorBridge, PluginContext};

use crate::dialog::DialogRegistry;

#[derive(Clone)]
pub(crate) struct EditorLifecycle {
    closing: Arc<AtomicBool>,
    dialogs: DialogRegistry,
    gestures: GestureRegistry,
}

impl EditorLifecycle {
    pub(crate) fn new(context: &PluginContext) -> Self {
        Self {
            closing: Arc::new(AtomicBool::new(false)),
            dialogs: DialogRegistry::new(),
            gestures: GestureRegistry::new(Arc::clone(context.bridge())),
        }
    }

    pub(crate) fn install(&self, context: &egui::Context) {
        self.dialogs.install(context);
        self.gestures.install(context);
    }

    #[cfg(not(target_os = "ios"))]
    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    #[cfg(not(target_os = "ios"))]
    pub(crate) fn end_gestures(&self) {
        self.gestures.end_all();
    }

    pub(crate) fn close(&self) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        self.dialogs.close();
        self.gestures.close();
    }
}

#[derive(Clone)]
struct GestureRegistry {
    inner: Arc<(Mutex<GestureState>, Condvar)>,
    bridge: Arc<dyn EditorBridge>,
}

#[derive(Default)]
struct GestureState {
    closed: bool,
    suspended: bool,
    in_flight_sets: usize,
    in_flight_ends: usize,
    starting: Vec<u32>,
    active: Vec<u32>,
}

impl GestureRegistry {
    const CONTEXT_ID: &'static str = "moose-egui-gesture-registry";

    fn new(bridge: Arc<dyn EditorBridge>) -> Self {
        Self {
            inner: Arc::new((Mutex::new(GestureState::default()), Condvar::new())),
            bridge,
        }
    }

    fn install(&self, context: &egui::Context) {
        context.data_mut(|data| {
            data.insert_temp(egui::Id::new(Self::CONTEXT_ID), self.clone());
        });
    }

    fn from_context(context: &egui::Context) -> Option<Self> {
        context.data_mut(|data| data.get_temp(egui::Id::new(Self::CONTEXT_ID)))
    }

    fn begin(&self, id: u32) {
        {
            let mut state = self
                .inner
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed
                || state.suspended
                || state.active.contains(&id)
                || state.starting.contains(&id)
            {
                return;
            }
            state.starting.push(id);
        }

        let began = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.bridge.begin_edit(id);
        }))
        .is_ok();
        let should_end = {
            let mut state = self
                .inner
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(index) = state.starting.iter().position(|starting| *starting == id) {
                state.starting.swap_remove(index);
            }
            let should_end = began && (state.closed || state.suspended);
            if began && !state.closed && !state.suspended {
                state.active.push(id);
            } else if should_end {
                state.in_flight_ends += 1;
            }
            self.inner.1.notify_all();
            should_end
        };
        if !began {
            log::error!("egui host begin_edit panic swallowed");
        } else if should_end {
            self.end_bridge_edit(id);
            self.finish_end();
        }
    }

    fn end(&self, id: u32) {
        {
            let mut state = self
                .inner
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(index) = state.active.iter().position(|active| *active == id) else {
                return;
            };
            state.active.swap_remove(index);
            while state.in_flight_sets > 0 {
                state = self
                    .inner
                    .1
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            state.in_flight_ends += 1;
        }
        self.end_bridge_edit(id);
        self.finish_end();
    }

    fn set(&self, id: u32, normalized: f64) {
        {
            let mut state = self
                .inner
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed || state.suspended || !state.active.contains(&id) {
                return;
            }
            state.in_flight_sets += 1;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.bridge.set_param(id, normalized);
        }));
        {
            let mut state = self
                .inner
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.in_flight_sets = state.in_flight_sets.saturating_sub(1);
            self.inner.1.notify_all();
        }
        if result.is_err() {
            log::error!("egui host set_param panic swallowed");
        }
    }

    #[cfg(not(target_os = "ios"))]
    fn end_all(&self) {
        let active = {
            let mut state = self
                .inner
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed || state.suspended {
                return;
            }
            state.suspended = true;
            while !state.starting.is_empty() || state.in_flight_sets > 0 || state.in_flight_ends > 0
            {
                state = self
                    .inner
                    .1
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let active = std::mem::take(&mut state.active);
            state.in_flight_ends += active.len();
            active
        };
        for id in active {
            self.end_bridge_edit(id);
            self.finish_end();
        }
        let mut state = self
            .inner
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.closed {
            state.suspended = false;
        }
        self.inner.1.notify_all();
    }

    fn close(&self) {
        let active = {
            let mut state = self
                .inner
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            while !state.starting.is_empty() || state.in_flight_sets > 0 || state.in_flight_ends > 0
            {
                state = self
                    .inner
                    .1
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let active = std::mem::take(&mut state.active);
            state.in_flight_ends += active.len();
            active
        };
        for id in active {
            self.end_bridge_edit(id);
            self.finish_end();
        }
    }

    fn end_bridge_edit(&self, id: u32) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.bridge.end_edit(id);
        }));
        if result.is_err() {
            log::error!("egui host end_edit panic swallowed during lifecycle cleanup");
        }
    }

    fn finish_end(&self) {
        let mut state = self
            .inner
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight_ends = state.in_flight_ends.saturating_sub(1);
        self.inner.1.notify_all();
    }
}

pub(crate) fn begin_gesture<P: ?Sized>(context: &egui::Context, state: &PluginContext<P>, id: u32) {
    if let Some(registry) = GestureRegistry::from_context(context) {
        registry.begin(id);
    } else {
        state.begin_edit(id);
    }
}

pub(crate) fn end_gesture<P: ?Sized>(context: &egui::Context, state: &PluginContext<P>, id: u32) {
    if let Some(registry) = GestureRegistry::from_context(context) {
        registry.end(id);
    } else {
        state.end_edit(id);
    }
}

pub(crate) fn set_param<P: ?Sized>(
    context: &egui::Context,
    state: &PluginContext<P>,
    id: u32,
    normalized: f64,
) {
    if let Some(registry) = GestureRegistry::from_context(context) {
        registry.set(id, normalized);
    } else {
        state.set_param(id, normalized);
    }
}

pub(crate) fn automate<P: ?Sized>(
    context: &egui::Context,
    state: &PluginContext<P>,
    id: u32,
    normalized: f64,
) {
    begin_gesture(context, state, id);
    set_param(context, state, id, normalized);
    end_gesture(context, state, id);
}
