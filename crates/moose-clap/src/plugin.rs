//! `clap_plugin` lifecycle callbacks: init, activate, process start and stop, reset, main-thread work.

use crate::catalog_sync;
use crate::context_menu;
use crate::data::ClapPluginData;
use crate::data::HostOutPtr;
use crate::data_from_plugin;
use crate::gui::editor_context;
use crate::ports::apply_port_activation;
use crate::ports::host_rescan_audio_ports;
use crate::track_info;
use crate::voice_info;

use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency};
use clap_sys::ext::log::{CLAP_EXT_LOG, CLAP_LOG_ERROR, clap_host_log};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_host_params};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_host_tail};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::ext::track_info::{
    CLAP_EXT_TRACK_INFO, CLAP_EXT_TRACK_INFO_COMPAT, clap_host_track_info,
};
use clap_sys::plugin::clap_plugin;
use moose_core::bus_routing::{BusActivation, BusRouting};
use moose_core::config::{AudioConfig, ProcessMode};
use moose_core::export::PluginExport;
use moose_core::host_queue::{HostMessage, HostSignal};
use moose_core::main_thread::HostNotice;
use moose_core::plugin::{PreparationConfig, PreparationFrameBound};
use moose_core::rt::RtSection;
use moose_core::thread::MainThread;
use moose_core::wrapper::{apply_queued_state, enter_plugin, run_extern_callback_with};
use moose_params::Params;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Plugin callbacks
//
// SAFETY for all unsafe extern "C" fn in this file:
// - `plugin` is the clap_plugin pointer returned by create_plugin_instance().
// - `(*plugin).plugin_data` is a Box::into_raw'd ClapPluginData<P>,
//   valid for the plugin's lifetime. The host guarantees it is not
//   freed until after clap_plugin.destroy() returns.
// - Audio-thread callbacks (process, start/stop_processing) have
//   exclusive access - the host never calls them concurrently.
// - Main-thread callbacks (init, destroy, activate, deactivate,
//   gui_*, params on main thread) are serialized by the host.
// - params_flush may be called from the audio thread while process
//   is not active, or from the main thread - never concurrently
//   with process().
// - Audio buffer pointers (inputs/outputs in clap_process) are
//   valid for the declared channel count × frame count. The host
//   guarantees non-aliasing between input and output buffers.
// ---------------------------------------------------------------------------

pub(crate) unsafe extern "C" fn clap_plugin_init<P: PluginExport>(
    plugin: *const clap_plugin,
) -> bool {
    // Author `init` runs here; firewall it so a panic can't unwind across
    // the C ABI (false = "init failed", the host then discards the plugin).
    run_extern_callback_with::<P, bool>("CLAP", "init", false, || unsafe {
        // `init` runs once on the main thread before the host publishes the
        // instance to any audio thread, so an exclusive `&mut` to the whole
        // struct is sound here - the write-once fields below (`exposed`,
        // the host extension pointers) have no concurrent reader yet.
        let data = &mut *(*plugin).plugin_data.cast::<ClapPluginData<P>>();
        enter_plugin(&data.plugin).init();
        // Bind the signal first, then drop what `init` changed: the catalog
        // built next already holds it, and any later edit wakes us.
        data.catalog_signal = Some(catalog_sync::install_catalog_signal(data));
        let _ = data.params_arc.take_catalog_changes();
        let catalog = data.params_arc.param_catalog();
        data.audio.enter().catalog = Arc::clone(&catalog);
        data.exposed = Mutex::new(catalog_sync::ExposedParams::new(
            catalog,
            data.params_arc.as_ref(),
        ));
        // Query host extensions (`get_extension` is `[thread-safe]`).
        if let Some(ext) = data.host.extension::<clap_host_params>(CLAP_EXT_PARAMS) {
            data.host_params = ext;
        }
        if let Some(ext) = data.host.extension::<clap_host_state>(CLAP_EXT_STATE) {
            data.host_state = ext;
        }
        if let Some(ext) = data.host.extension::<clap_host_latency>(CLAP_EXT_LATENCY) {
            data.host_latency = ext;
        }
        if let Some(ext) = data.host.extension::<clap_host_tail>(CLAP_EXT_TAIL) {
            data.host_tail = ext;
        }
        if let Some(ext) = data
            .host
            .extension::<clap_host_track_info>(CLAP_EXT_TRACK_INFO)
            .or_else(|| {
                data.host
                    .extension::<clap_host_track_info>(CLAP_EXT_TRACK_INFO_COMPAT)
            })
        {
            data.host_track_info = ext;
        }
        if let Some(ext) = data.host.extension::<clap_host_log>(CLAP_EXT_LOG) {
            data.host_log = ext;
        }
        if let Some(ext) = data
            .host
            .extension::<clap_host_thread_check>(CLAP_EXT_THREAD_CHECK)
        {
            data.host_thread_check = ext;
        }
        data.host.debug_assert_main_thread(data.host_thread_check);
        if data.params_arc.wants_persist_notifications() {
            let queue = Arc::clone(&data.host_queue);
            let signal = Arc::new(moose_params::PersistChangeSignal::new(move || {
                queue.signal(HostSignal::PERSIST);
            }));
            data.params_arc
                .set_persist_change_signal(Arc::downgrade(&signal));
            data.persist_signal = Some(signal);
        }
        // SAFETY: `clap_plugin.init` is [main-thread] (plugin.h).
        let main = MainThread::assume();
        let data = &*data;
        let notifier = data.host_notifier.clone();
        data.with_hooks(&main, |hooks| hooks.connect(notifier));
        data.voice_info_supported
            .store(voice_info::query(data, &main).is_some(), Ordering::Relaxed);
        crate::context_menu::refresh_capability(data, &main);
        track_info::notify(data, &main);
        true
    })
}

pub(crate) unsafe extern "C" fn clap_plugin_destroy<P: PluginExport>(plugin: *const clap_plugin) {
    unsafe {
        // Wrap the drop in `catch_unwind`. Dropping the
        // `ClapPluginData` cascades into the editor's `Drop`,
        // which tears down the wgpu surface / `NSView` /
        // baseview / runloop timers. A panic anywhere in that
        // chain propagates across this `extern "C"` boundary as
        // UB - hosts catch it as an Obj-C exception,
        // `objc_exception_rethrow` can't recover, and
        // `std::terminate` aborts the host (the REAPER / Cubase
        // SIGABRT pattern). Hosts destroy instances routinely
        // mid-session - removing a plugin from a track, closing a
        // project - not just at quit, so catching here keeps a
        // live host alive; the instance is being torn down anyway.
        let plugin_ptr = plugin.cast_mut();
        let data_ptr = (*plugin).plugin_data.cast::<ClapPluginData<P>>();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Gate workers before any host/instance handle can be released.
            if let Some(signal) = &(*data_ptr).persist_signal {
                signal.close();
            }
            if let Some(signal) = &(*data_ptr).catalog_signal {
                signal.close();
            }
            (*data_ptr).host_queue.close();
            report_latched_panic(&*data_ptr);
            drop(Box::from_raw(data_ptr));
            drop(Box::from_raw(plugin_ptr));
        }));
    }
}

pub(crate) unsafe extern "C" fn clap_plugin_activate<P: PluginExport>(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min_frames_count: u32,
    max_frames_count: u32,
) -> bool {
    // Author `reset` runs here; firewall it (false = "activate failed").
    run_extern_callback_with::<P, bool>("CLAP", "activate", false, || unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.host.debug_assert_main_thread(data.host_thread_check);
        let mut audio = data.audio.enter();
        audio.sample_rate = sample_rate;
        let max_block = max_frames_count as usize;
        audio.max_block_size = max_block;
        let layouts = P::bus_layouts();
        let selected = (data.selected_config.load(Ordering::Relaxed) as usize)
            .min(layouts.len().saturating_sub(1));
        audio.bus_routing = BusRouting::new();
        if let Some(layout) = layouts.get(selected) {
            for bus in &layout.inputs {
                if !audio.bus_routing.push_input(
                    if bus.enabled {
                        bus.channels.channel_count()
                    } else {
                        0
                    },
                    BusActivation::Unknown,
                ) {
                    return false;
                }
            }
            for bus in &layout.outputs {
                if !audio.bus_routing.push_output(
                    if bus.enabled {
                        bus.channels.channel_count()
                    } else {
                        0
                    },
                    BusActivation::Unknown,
                ) {
                    return false;
                }
            }
            apply_port_activation(&mut audio.bus_routing, data, layout);
        }
        let mode = ProcessMode::from_u8(data.render_mode.load(Ordering::Relaxed));
        {
            let mut instance = enter_plugin(&data.plugin);
            let config = PreparationConfig::new(
                AudioConfig::new(sample_rate, max_block).with_process_mode(mode),
                PreparationFrameBound::NegotiatedHost,
            );
            if instance.prepare(&config).is_err() {
                return false;
            }
            data.panic_latch.clear_with(|msg| {
                if !data.host.log(data.host_log, CLAP_LOG_ERROR, msg) {
                    eprintln!("{msg}");
                }
            });
            // Publish what the freshly prepared processor reports, so the
            // host reads the real latency/tail before the first block - not
            // the placeholder-rate value from create. CLAP 1.2 permits
            // `clap_host_latency.changed` only `[main-thread & being-activated]`,
            // so notify right here when activation changes latency.
            let prev_latency = data.delay.latency();
            let new_latency = instance.latency();
            data.delay.publish(new_latency, instance.tail());
            if new_latency != prev_latency {
                // SAFETY: `clap_plugin.activate` is `[main-thread & being-activated]`.
                let main = MainThread::assume();
                data.host
                    .on_main_thread(&main)
                    .latency_changed(data.host_latency);
            }
        }

        // Pre-grow the widening / narrowing scratch so no host wire /
        // plugin precision combination allocates on the audio thread.
        // Without this, the first block after `activate` hits the global
        // allocator inside `clap_plugin_process` to grow the outer Vec and
        // each channel's inner Vec - a real RT hazard. The outer-Vec
        // capacity is already reserved in `create_plugin`; here we push the
        // inner per-channel `Vec<P::Sample>`s up to `max_block_size` frames
        // so the per-block `.clear() + .reserve()` path never allocates.
        //
        // Unconditional, both directions: an f64 plugin converts whenever a
        // host sends f32, and an f32 plugin converts if a (non-compliant)
        // host sends f64 to a port that never advertised
        // `SUPPORTS_64BITS`. Pre-growing for the f32 plugin keeps that
        // defensive path allocation-free too; its scratch just stays unused
        // on the common zero-copy path.
        {
            let scr = &mut *audio;
            let max_in = scr.input_widen.capacity();
            let max_out = scr.output_narrow.capacity();
            while scr.input_widen.len() < max_in {
                scr.input_widen.push(Vec::with_capacity(max_block));
            }
            for buf in &mut scr.input_widen {
                if buf.capacity() < max_block {
                    buf.reserve_exact(max_block - buf.len());
                }
            }
            while scr.output_narrow.len() < max_out {
                scr.output_narrow.push(Vec::with_capacity(max_block));
            }
            for buf in &mut scr.output_narrow {
                if buf.capacity() < max_block {
                    buf.reserve_exact(max_block - buf.len());
                }
            }
            while scr.host_out_ptrs.len() < max_out {
                scr.host_out_ptrs.push(HostOutPtr::Null);
            }
        }
        drop(audio);

        data.active.store(true, Ordering::Relaxed);
        true
    })
}

pub(crate) unsafe extern "C" fn clap_plugin_deactivate<P: PluginExport>(
    plugin: *const clap_plugin,
) {
    // Draining a stranded `pending_state` runs the author's `load_state`;
    // firewall it so a panic can't unwind across the C ABI.
    run_extern_callback_with::<P, ()>("CLAP", "deactivate", (), || unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.host.debug_assert_main_thread(data.host_thread_check);
        data.active.store(false, Ordering::Relaxed);
        // A `state_load` while active queues its blob for the audio thread
        // to drain at the top of the next block. If the host deactivates
        // before that block runs (sample-rate / buffer change, offline
        // reconfigure right after a recall), the blob is stranded: it would
        // resurrect stale state on the next activate, overwriting any newer
        // state applied while inactive. Drain it now and apply synchronously
        // (uncontended - no audio thread is processing) so nothing survives
        // deactivate.
        {
            let mut instance = enter_plugin(&data.plugin);
            if apply_queued_state(&mut *instance, &data.pending_state).is_some() {
                instance.republish_snapshot();
            }
        }
        clap_plugin_reset::<P>(plugin);
        // Inactive now: publish a structure change or list update held back
        // while active (restart + RESCAN_ALL, CLAP params.h).
        // SAFETY: `clap_plugin.deactivate` is [main-thread & active] (plugin.h).
        let main = MainThread::assume();
        let rescan = !data.host_queue.take(HostSignal::PARAM_RESCAN).is_empty();
        catalog_sync::sync_parameters(data, &main, rescan);
        if data.ports_rescan.swap(false, Ordering::Relaxed) {
            host_rescan_audio_ports(data, &main);
        }
    });
}

pub(crate) unsafe extern "C" fn clap_plugin_start_processing<P: PluginExport>(
    plugin: *const clap_plugin,
) -> bool {
    // SAFETY: CLAP supplies a live instance for this factory's plugin type.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    if !data.active.load(Ordering::Acquire) {
        return false;
    }
    data.processing.store(true, Ordering::Release);
    true
}

pub(crate) unsafe extern "C" fn clap_plugin_stop_processing<P: PluginExport>(
    plugin: *const clap_plugin,
) {
    // SAFETY: CLAP supplies a live instance for this factory's plugin type.
    let data = unsafe { data_from_plugin::<P>(plugin) };
    data.processing.store(false, Ordering::Release);
}

pub(crate) unsafe extern "C" fn clap_plugin_reset<P: PluginExport>(plugin: *const clap_plugin) {
    // Author `reset` runs here; firewall it so a panic can't unwind across
    // the C ABI.
    run_extern_callback_with::<P, ()>("CLAP", "reset", (), || unsafe {
        let data = data_from_plugin::<P>(plugin);
        let mut audio = data.audio.enter();
        audio.sounding_notes.clear_all();
        data.params_arc.clear_modulations();
        let mode = ProcessMode::from_u8(data.render_mode.load(Ordering::Relaxed));
        let mut instance = enter_plugin(&data.plugin);
        {
            // CLAP `reset` is [audio-thread]: the author's `reset` must not
            // allocate (rt-paranoid reports it). Sizing belongs in `prepare`.
            let _rt = RtSection::enter();
            instance.reset(
                &AudioConfig::new(audio.sample_rate, audio.max_block_size).with_process_mode(mode),
            );
        }
        // Latency stays as reported at `activate`: a latency change during
        // processing asks the host to restart, whereas tail changes notify
        // `[audio-thread]` `clap.tail.changed` directly without restarting.
    });
}

pub(crate) unsafe extern "C" fn clap_plugin_on_main_thread<P: PluginExport>(
    plugin: *const clap_plugin,
) {
    unsafe {
        let data = data_from_plugin::<P>(plugin);
        data.host.debug_assert_main_thread(data.host_thread_check);
        report_latched_panic(data);
        // SAFETY: `clap_plugin.on_main_thread` is [main-thread] (plugin.h).
        let main = MainThread::assume();
        let host = data.host.on_main_thread(&main);
        // Everything raised from here on wakes the host again.
        let drain = data.host_queue.drain(&main);
        let signals = drain.signals;
        if signals.contains(HostSignal::PERSIST)
            && let Some((id, value)) = data.params_arc.take_persist_change()
            && value.is_finite()
            && (0.0..=1.0).contains(&value)
            && data.params_arc.get_normalized(id).is_some()
        {
            // The ordinary gesture queue works independently of GUI creation.
            let context = editor_context::<P>(plugin);
            context.begin_edit(id);
            context.set_param(id, value);
            context.end_edit(id);
            // State dirtiness must also reach an inactive host that does not
            // implement params/request_flush.
            host.mark_dirty(data.host_state);
        }
        if signals.contains(HostSignal::MARK_DIRTY) {
            host.mark_dirty(data.host_state);
        }
        if signals.contains(HostSignal::FLOATING_CLOSED) {
            data.gui.enter().visible = false;
            host.gui_closed(false);
        }
        // Catalog changes, presentation revisions and value rescans. A
        // structure change requests a restart and is published inactive.
        catalog_sync::sync_parameters(data, &main, signals.contains(HostSignal::PARAM_RESCAN));
        let notices = signals.notices();
        if notices.contains(HostNotice::NOTE_NAMES) {
            host.note_name_changed();
        }
        if notices.contains(HostNotice::VOICE_INFO) {
            host.voice_info_changed();
        }
        let active = data.active.load(Ordering::Relaxed);
        let mut restart = signals.contains(HostSignal::RESTART);
        if notices.contains(HostNotice::AUDIO_PORTS) {
            data.ports_rescan.store(true, Ordering::Relaxed);
            restart |= active;
        }
        if !active && data.ports_rescan.swap(false, Ordering::Relaxed) {
            host_rescan_audio_ports(data, &main);
        }
        // CLAP 1.2 `clap_host_latency.changed` is `[main-thread & being-activated]`,
        // so an active latency change requests a restart and `activate` emits
        // `latency.changed` once the new latency is published.
        if signals.contains(HostSignal::LATENCY) && !host.is_null() && active {
            restart = true;
        }
        if restart {
            host.as_any().request_restart();
        }
        for message in drain {
            // Messages added to moose after this wrapper are dropped.
            if let HostMessage::ContextMenu { target, x, y } = message {
                context_menu::popup(data, &main, target, x, y);
            }
        }
    }
}

/// Forward any latched audio-thread panic message to the host's `clap.log`
/// extension, falling back to stderr when the host does not expose `clap.log`.
///
/// # Safety
/// `data` belongs to a live instance on a non-audio thread.
unsafe fn report_latched_panic<P: PluginExport>(data: &ClapPluginData<P>) {
    data.panic_latch.report_with(|msg| {
        // SAFETY: caller guarantees `data.host` and `data.host_log` are live.
        if !unsafe { data.host.log(data.host_log, CLAP_LOG_ERROR, msg) } {
            eprintln!("{msg}");
        }
    });
}
