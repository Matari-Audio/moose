use crate::bus_routing::BusRouting;
use crate::config::ProcessMode;
use crate::events::{EventList, TransportInfo};
use crate::tasks::{AnyTaskSpawner, TaskSpawner};

/// Host-provided RT thread-pool executor (`num_tasks`, `task_fn`) -> `true` if
/// the host executed all `0..num_tasks` tasks before returning.
pub type ThreadPoolExec<'a> = &'a dyn Fn(u32, &(dyn Fn(u32) + Sync)) -> bool;

/// Per-block context handed to `process()`. Construct via
/// [`Self::new`] + the `with_*` builders. Marked `#[non_exhaustive]`
/// so adding host-populated fields in future (e.g. `host_latency`,
/// `bus_routing`) isn't a `SemVer` break for downstream pre-1.0 callers.
#[non_exhaustive]
pub struct ProcessContext<'a> {
    pub transport: &'a TransportInfo,
    /// How the host is driving audio this block. Tracks host toggles
    /// that don't force a re-prepare (VST3 `kRealtime` <-> `kPrefetch`,
    /// an LV2 freewheel port). A plugin that reallocates for offline
    /// keys off `AudioConfig::process_mode` at `reset` instead; this
    /// field is for "may I relax realtime discipline right now?".
    pub process_mode: ProcessMode,
    pub sample_rate: f64,
    pub block_size: usize,
    pub output_events: &'a mut EventList,
    /// Per-bus ranges and host activation state for this block. Channel
    /// ranges index the flattened `AudioBuffer` passed beside this context.
    pub bus_routing: BusRouting,
    params_fn: Option<&'a dyn Fn(u32) -> f64>,
    meters_fn: Option<&'a dyn Fn(u32, f32)>,
    latency_change_request: Option<&'a dyn Fn()>,
    thread_pool: Option<ThreadPoolExec<'a>>,
    /// Type-erased handle to this instance's background-task spawner,
    /// installed by the shell when the plugin wired `tasks:` on
    /// `plugin!`. Recover a typed spawner with [`Self::tasks`].
    tasks: Option<&'a AnyTaskSpawner>,
}

impl<'a> ProcessContext<'a> {
    pub fn new(
        transport: &'a TransportInfo,
        sample_rate: f64,
        block_size: usize,
        output_events: &'a mut EventList,
    ) -> Self {
        Self {
            transport,
            process_mode: ProcessMode::Realtime,
            sample_rate,
            block_size,
            output_events,
            bus_routing: BusRouting::new(),
            params_fn: None,
            meters_fn: None,
            latency_change_request: None,
            thread_pool: None,
            tasks: None,
        }
    }

    /// Stamp the current block's allocation-free bus routing snapshot.
    #[must_use]
    pub fn with_bus_routing(mut self, bus_routing: BusRouting) -> Self {
        self.bus_routing = bus_routing;
        self
    }

    /// Set the processing mode for this block. Defaults to
    /// [`ProcessMode::Realtime`]; wrappers stamp the live host mode.
    #[must_use]
    pub fn with_process_mode(mut self, mode: ProcessMode) -> Self {
        self.process_mode = mode;
        self
    }

    /// Require latency-affecting DSP changes to wait for host reactivation.
    /// The callback schedules reactivation without changing the active latency.
    #[must_use]
    pub fn with_deferred_latency_changes(mut self, request: &'a dyn Fn()) -> Self {
        self.latency_change_request = Some(request);
        self
    }

    /// Retain the host's reactivation policy when forwarding this context.
    #[must_use]
    pub fn deferred_latency_change_request(&self) -> Option<&'a dyn Fn()> {
        self.latency_change_request
    }

    /// Whether the format requires the active latency contract until reactivation.
    #[must_use]
    pub fn defers_latency_changes(&self) -> bool {
        self.latency_change_request.is_some()
    }

    /// Request reactivation after staging new latency-affecting settings.
    /// Keep processing with the current settings until preparation runs again.
    pub fn request_latency_change(&self) {
        if let Some(request) = self.latency_change_request {
            request();
        }
    }

    /// Install a host RT thread-pool executor (e.g. CLAP `clap.thread-pool`).
    #[must_use]
    pub fn with_thread_pool(mut self, exec: ThreadPoolExec<'a>) -> Self {
        self.thread_pool = Some(exec);
        self
    }

    /// Execute `0..num_tasks` across the host's RT thread pool when available
    /// (CLAP `clap.thread-pool`), falling back to sequential execution on the
    /// calling audio thread when no pool is present or the host declines.
    ///
    /// Allocation-free: `f` is borrowed on the stack for the duration of the
    /// synchronous parallel region.
    pub fn parallel_for(&self, num_tasks: u32, f: impl Fn(u32) + Sync) {
        if num_tasks == 0 {
            return;
        }
        if num_tasks > 1
            && let Some(exec) = self.thread_pool
            && exec(num_tasks, &f)
        {
            return;
        }
        for i in 0..num_tasks {
            f(i);
        }
    }

    /// Set the parameter lookup callback.
    #[must_use]
    pub fn with_params(mut self, f: &'a dyn Fn(u32) -> f64) -> Self {
        self.params_fn = Some(f);
        self
    }

    /// Set the meter reporting callback.
    #[must_use]
    pub fn with_meters(mut self, f: &'a dyn Fn(u32, f32)) -> Self {
        self.meters_fn = Some(f);
        self
    }

    /// Install the background-task spawner for this block. The shell
    /// stamps it when the plugin wired `tasks:` on `plugin!`.
    #[must_use]
    pub fn with_tasks(mut self, tasks: &'a AnyTaskSpawner) -> Self {
        self.tasks = Some(tasks);
        self
    }

    /// The background-task spawner for task type `T`, or `None` if the
    /// plugin declared no `tasks:` lane of that type. Scheduling with the
    /// returned spawner is wait-free (see
    /// [`TaskSpawner::try_spawn`] / [`TaskSpawner::spawn_coalescing`]), so
    /// it is safe to call from the audio thread.
    #[must_use]
    pub fn tasks<T: Send + 'static>(&self) -> Option<TaskSpawner<T>> {
        self.tasks.and_then(AnyTaskSpawner::downcast::<T>)
    }

    /// Read a parameter's plain value by ID.
    ///
    /// Returns `None` when no params callback is wired up (e.g. when a
    /// plugin runs under the bare test driver without a `with_params`
    /// closure). Callers that always run inside a real format wrapper
    /// can `.unwrap_or_default()`. Distinguishing "no callback" from
    /// "value is zero" lets test harnesses notice when they forgot to
    /// wire up params rather than masking the misconfiguration as
    /// "host set the value to zero".
    #[must_use]
    pub fn param(&self, id: u32) -> Option<f64> {
        self.params_fn.map(|f| f(id))
    }

    /// Report a meter value (0.0 to 1.0).
    pub fn set_meter(&self, id: impl Into<u32>, value: f32) {
        let id = id.into();
        if let Some(f) = self.meters_fn {
            f(id, value);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProcessStatus {
    /// Plugin produced meaningful output.
    Normal,
    /// Plugin is producing tail. Value = remaining tail samples.
    Tail(u32),
    /// Keep alive even if input is silent.
    KeepAlive,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventList, TransportInfo};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn parallel_for_uses_host_pool_and_falls_back_sequentially() {
        let transport = TransportInfo::default();
        let mut out = EventList::with_capacity(4);
        let sum = AtomicU32::new(0);

        // Without a host pool: runs sequentially on the caller thread.
        let ctx = ProcessContext::new(&transport, 48_000.0, 64, &mut out);
        ctx.parallel_for(5, |i| {
            sum.fetch_add(i + 1, Ordering::Relaxed);
        });
        assert_eq!(sum.load(Ordering::Relaxed), 15);

        // With a host pool that executes tasks concurrently across threads.
        let pool_calls = AtomicU32::new(0);
        let pool = |n: u32, f: &(dyn Fn(u32) + Sync)| -> bool {
            pool_calls.fetch_add(1, Ordering::Relaxed);
            std::thread::scope(|s| {
                for i in 0..n {
                    s.spawn(move || f(i));
                }
            });
            true
        };
        let sum2 = AtomicU32::new(0);
        let ctx = ProcessContext::new(&transport, 48_000.0, 64, &mut out).with_thread_pool(&pool);
        ctx.parallel_for(6, |i| {
            sum2.fetch_add(i + 1, Ordering::Relaxed);
        });
        assert_eq!(pool_calls.load(Ordering::Relaxed), 1);
        assert_eq!(sum2.load(Ordering::Relaxed), 21);

        // When the host pool declines (`false`), falls back sequentially.
        let decline = |_: u32, _: &(dyn Fn(u32) + Sync)| false;
        let sum3 = AtomicU32::new(0);
        let ctx =
            ProcessContext::new(&transport, 48_000.0, 64, &mut out).with_thread_pool(&decline);
        ctx.parallel_for(4, |i| {
            sum3.fetch_add(i + 1, Ordering::Relaxed);
        });
        assert_eq!(sum3.load(Ordering::Relaxed), 10);
    }
}
