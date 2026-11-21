//! Driver-free regressions against the COM objects, with fake host
//! objects and a fake callback table per test (ported from the C++ shim
//! fixtures).

use std::ffi::{CStr, CString, c_char, c_void};
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::SeqCst};

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::com_scrape_types::Unknown;
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use super::component::{Component, Host};
use super::{Registration, build_units};
use crate::ffi::{
    ProcessFn, Vst3Callbacks, Vst3LearnSource, Vst3NativeEvent, Vst3ParamDescriptor,
    Vst3PluginDescriptor, Vst3TrackInfo, Vst3Transport,
};

// ---------------------------------------------------------------------------
// Fake callback table
// ---------------------------------------------------------------------------

/// A distinct, never-freed ctx per instance, so the instance registry never
/// confuses two of them (tests run in parallel).
unsafe extern "C" fn fresh_ctx() -> *mut c_void {
    Box::into_raw(Box::new(0u8)).cast()
}
unsafe extern "C" fn nop_ctx(_: *mut c_void) {}
unsafe extern "C" fn nop_lifecycle(_: *mut c_void, _: f64, _: u32, _: i32) {}
unsafe extern "C" fn ok_prepare(_: *mut c_void, _: f64, _: u32, _: i32) -> i32 {
    1
}
unsafe extern "C" fn ok_process(
    _: *mut c_void,
    _: *const *const c_void,
    _: *mut *mut c_void,
    _: u32,
    _: u32,
    _: *const u32,
    _: u32,
    _: u32,
    _: *const u32,
    _: u32,
    _: u32,
    _: u32,
    _: *const Vst3Transport,
    _: i32,
) -> u32 {
    1
}
unsafe extern "C" fn push_param(_: *mut c_void, _: u32, _: i32, _: f64, _: i32, _: f64) -> i32 {
    1
}
unsafe extern "C" fn zero_u32(_: *mut c_void) -> u32 {
    0
}
unsafe extern "C" fn zero_i32(_: *mut c_void) -> i32 {
    0
}
unsafe extern "C" fn param_value(_: *mut c_void, _: u32) -> f64 {
    0.0
}
unsafe extern "C" fn param_set(_: *mut c_void, _: u32, _: f64) {}
unsafe extern "C" fn identity(_: *mut c_void, _: u32, v: f64) -> f64 {
    v
}
unsafe extern "C" fn no_format(_: *mut c_void, _: u32, _: f64, _: *mut c_char, _: u32) -> u32 {
    0
}
unsafe extern "C" fn no_parse(_: *mut c_void, _: u32, _: *const c_char, _: *mut f64) -> i32 {
    0
}
unsafe extern "C" fn no_save(_: *mut c_void, _: *mut *mut u8, _: *mut u32) {}
unsafe extern "C" fn no_load(_: *mut c_void, _: *const u8, _: u32) -> i32 {
    0
}
unsafe extern "C" fn no_load_stream(
    _: *mut c_void,
    _: *mut c_void,
    _: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> i64,
) -> i32 {
    0
}
unsafe extern "C" fn no_save_stream(
    _: *mut c_void,
    _: *mut c_void,
    _: unsafe extern "C" fn(*mut c_void, *const u8, u32) -> i64,
) -> i32 {
    0
}
unsafe extern "C" fn no_free(_: *mut u8, _: u32) {}
unsafe extern "C" fn begin_input(_: *mut c_void, _: u32) {}
unsafe extern "C" fn push_input(_: *mut c_void, _: *const Vst3NativeEvent) -> u32 {
    0
}
unsafe extern "C" fn next_output(_: *mut c_void, _: *mut Vst3NativeEvent) -> u32 {
    0
}
unsafe extern "C" fn finish_output(_: *mut c_void, _: u32) {}
unsafe extern "C" fn get_size(_: *mut c_void, _: *mut u32, _: *mut u32) {}
unsafe extern "C" fn gui_open(_: *mut c_void, _: *mut c_void) -> i32 {
    1
}
unsafe extern "C" fn set_scale(_: *mut c_void, _: f64) {}
unsafe extern "C" fn set_size(_: *mut c_void, _: u32, _: u32) {}
unsafe extern "C" fn no_midi_map(_: *mut c_void, _: i32, _: i16, _: i16, _: *mut u32) -> i32 {
    0
}
unsafe extern "C" fn no_output_param(
    _: *mut c_void,
    _: u32,
    _: *mut u32,
    _: *mut i32,
    _: *mut f64,
) {
}
unsafe extern "C" fn set_active(_: *mut c_void, _: i32) {}
unsafe extern "C" fn match_layout(_: u32, _: u32) -> i32 {
    0
}
unsafe extern "C" fn no_bus_channels(_: u32, _: i32, _: u32) -> u32 {
    0
}
unsafe extern "C" fn match_perbus(_: *const u32, _: u32, _: *const u32, _: u32) -> i32 {
    0
}
unsafe extern "C" fn not_chunked(_: *mut c_void, _: u32) -> i32 {
    0
}
unsafe extern "C" fn no_presentation(_: *mut c_void, _: u32, _: *mut c_char, _: u32) -> i32 {
    -1
}
unsafe extern "C" fn revision(_: *mut c_void) -> u64 {
    0
}
unsafe extern "C" fn no_persist(_: *mut c_void) -> bool {
    false
}
unsafe extern "C" fn no_waker(
    _: *mut c_void,
    _: Option<unsafe extern "C" fn(*mut c_void)>,
    _: *mut c_void,
) {
}
unsafe extern "C" fn no_take(_: *mut c_void, _: *mut u32, _: *mut f64) -> bool {
    false
}
unsafe extern "C" fn no_processing(_: *mut c_void, _: i32, _: f64, _: u32, _: i32) {}
unsafe extern "C" fn never(_: *mut c_void, _: u32) -> bool {
    false
}
unsafe extern "C" fn no_function(_: *mut c_void, _: u32) -> *const c_char {
    std::ptr::null()
}
unsafe extern "C" fn no_changes(_: *mut c_void) -> u8 {
    0
}
unsafe extern "C" fn no_track(_: *mut c_void, _: *const Vst3TrackInfo) {}
unsafe extern "C" fn no_learn(_: *mut c_void, _: u32, _: u8, _: Vst3LearnSource) -> i32 {
    0
}
unsafe extern "C" fn no_remap(_: *mut c_void, _: *const u8, _: u32, _: *mut u32) -> i32 {
    0
}
unsafe extern "C" fn no_open_wayland(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> i32 {
    0
}
unsafe extern "C" fn no_fd(_: *mut c_void) -> i32 {
    -1
}
unsafe fn no_menu(_: *mut c_void, _: moose_core::main_thread::MenuTarget, _: u32) -> i32 {
    0
}

fn callbacks() -> Vst3Callbacks {
    let process: ProcessFn = ok_process;
    Vst3Callbacks {
        create: fresh_ctx,
        destroy: nop_ctx,
        reset: nop_lifecycle,
        prepare: ok_prepare,
        process,
        process_f64: process,
        push_param_change: push_param,
        param_count: zero_u32,
        param_get_value: param_value,
        param_set_value: param_set,
        param_normalize: identity,
        param_denormalize: identity,
        param_format: no_format,
        param_parse: no_parse,
        state_save: no_save,
        state_load: no_load,
        state_load_stream: no_load_stream,
        state_save_stream: no_save_stream,
        state_free: no_free,
        get_latency: zero_u32,
        get_tail: zero_u32,
        begin_input_events: begin_input,
        push_input_event: push_input,
        begin_output_events: nop_ctx,
        next_output_event: next_output,
        commit_output_event: nop_ctx,
        finish_output_events: finish_output,
        gui_has_editor: zero_i32,
        gui_get_size: get_size,
        gui_open,
        gui_close: nop_ctx,
        gui_set_content_scale: set_scale,
        gui_can_resize: zero_i32,
        gui_check_size_constraint: get_size,
        gui_set_size: set_size,
        midi_mapping_get_param_id: no_midi_map,
        get_output_param_count: zero_u32,
        get_output_param: no_output_param,
        set_active,
        match_bus_layout: match_layout,
        layout_bus_channels: no_bus_channels,
        match_bus_layout_perbus: match_perbus,
        param_is_chunked: not_chunked,
        param_presentation: no_presentation,
        param_presentation_revision: revision,
        wants_persist_notifications: no_persist,
        set_persist_waker: no_waker,
        take_persist_change: no_take,
        param_affects_presentation: never,
        param_function_name: no_function,
        set_catalog_waker: no_waker,
        take_catalog_changes: no_changes,
        track_info_changed: no_track,
        midi_learn: no_learn,
        remap_param_id: no_remap,
        set_processing: no_processing,
        gui_supports_wayland: zero_i32,
        gui_open_wayland: no_open_wayland,
        gui_event_fd: no_fd,
        gui_on_fd: nop_ctx,
        gui_on_timer: nop_ctx,
        gui_wayland_set_sync: {
            unsafe extern "C" fn nop_sync(_: *mut c_void, _: i32) {}
            nop_sync
        },
        bind_host_queue: {
            unsafe extern "C" fn nop_bind(_: *mut c_void, _: *mut c_void) {}
            nop_bind
        },
        drain_host_queue: nop_ctx,
        perform_menu: no_menu,
        gui_param_at: {
            unsafe extern "C" fn no_param_at(_: *mut c_void, _: i32, _: i32, _: *mut u32) -> i32 {
                0
            }
            no_param_at
        },
        set_audio_presentation_latency: {
            unsafe extern "C" fn no_pres_lat(_: *mut c_void, _: i32, _: u32, _: u32) {}
            no_pres_lat
        },
        param_step_count: {
            unsafe extern "C" fn static_step_count(_: *mut c_void, _: u32, _: *mut f64) -> i32 {
                -1
            }
            static_step_count
        },
        keyswitches: {
            unsafe fn no_keyswitches(
                _: *mut c_void,
                _: u32,
                _: u8,
            ) -> Vec<moose_core::main_thread::Keyswitch> {
                Vec::new()
            }
            no_keyswitches
        },
        program_list: {
            unsafe fn no_programs(_: *mut c_void) -> Vec<moose_core::main_thread::ProgramPreset> {
                Vec::new()
            }
            no_programs
        },
        set_program_data: {
            unsafe fn reject_program_data(
                _: *mut c_void,
                _: usize,
                _: *const u8,
                _: usize,
            ) -> bool {
                false
            }
            reject_program_data
        },
    }
}

fn descriptor() -> Vst3PluginDescriptor {
    // SAFETY: integers, a byte array and nullable C pointers.
    unsafe { std::mem::zeroed() }
}

fn registration(
    desc: Vst3PluginDescriptor,
    cb: Vst3Callbacks,
    params: Vec<Vst3ParamDescriptor>,
) -> &'static Registration {
    let params: &'static [Vst3ParamDescriptor] = Box::leak(params.into_boxed_slice());
    let (units, param_unit) = build_units(params);
    Box::leak(Box::new(Registration {
        desc: Box::leak(Box::new(desc)),
        cb: Box::leak(Box::new(cb)),
        params,
        units,
        param_unit,
    }))
}

fn component(reg: &'static Registration) -> ComWrapper<Component> {
    ComWrapper::new(Component {
        host: Host::new(reg, None).expect("create"),
    })
}

/// A component the test owns only through one raw `IComponent` reference,
/// as a host does.
fn raw_component(reg: &'static Registration) -> *mut IComponent {
    raw_component_ctx(reg).0
}

/// [`raw_component`] plus its plugin ctx.
fn raw_component_ctx(reg: &'static Registration) -> (*mut IComponent, *mut c_void) {
    let com = component(reg);
    (
        com.to_com_ptr::<IComponent>().unwrap().into_raw(),
        com.host.ctx,
    )
}

fn create_view(com: *mut IComponent) -> *mut IPlugView {
    let controller = unsafe { ComRef::from_raw(com) }
        .unwrap()
        .cast::<IEditController>()
        .unwrap();
    unsafe { controller.createView(c"editor".as_ptr()) }
}

fn release<I: Unknown>(p: *mut I) -> usize {
    unsafe { I::release(p) }
}

/// Current reference count of a COM object.
fn refs<I: Unknown>(p: *mut I) -> usize {
    unsafe {
        I::add_ref(p);
        I::release(p)
    }
}

#[cfg(target_os = "macos")]
const PLATFORM: &CStr = c"NSView";
#[cfg(target_os = "windows")]
const PLATFORM: &CStr = c"HWND";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PLATFORM: &CStr = c"X11EmbedWindowID";

// ---------------------------------------------------------------------------
// Fake host objects
// ---------------------------------------------------------------------------

/// Shared plug-frame behaviour: count resizes of the expected size.
struct FrameCore {
    expect: (i32, i32),
    resizes: AtomicU32,
    bad: AtomicBool,
    #[cfg(target_os = "linux")]
    threads: Option<std::sync::Arc<threads::State>>,
}

impl FrameCore {
    fn new(w: i32, h: i32) -> Self {
        Self {
            expect: (w, h),
            resizes: AtomicU32::new(0),
            bad: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            threads: None,
        }
    }

    fn resize(
        &self,
        #[allow(unused_variables)] view: *mut IPlugView,
        rect: *mut ViewRect,
    ) -> tresult {
        #[cfg(target_os = "linux")]
        if let Some(st) = &self.threads {
            st.record('r');
        }
        let r = unsafe { &*rect };
        if (r.right, r.bottom) != self.expect {
            self.bad.store(true, SeqCst);
        }
        self.resizes.fetch_add(1, SeqCst);
        #[cfg(target_os = "linux")]
        if let Some(st) = &self.threads
            && st.emit_on_resize.swap(false, SeqCst)
            && unsafe { ComRef::from_raw(view).unwrap().onSize(rect) } != kResultOk
        {
            self.bad.store(true, SeqCst);
        }
        kResultOk
    }
}

struct Frame(FrameCore);

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, rect: *mut ViewRect) -> tresult {
        self.0.resize(view, rect)
    }
}

/// A plug frame that also offers the host run loop.
#[cfg(target_os = "linux")]
struct LoopFrame {
    core: FrameCore,
    handler: std::sync::Mutex<Option<ComPtr<Linux::IEventHandler>>>,
    fd: AtomicI32,
    timers: AtomicI32,
}

#[cfg(target_os = "linux")]
impl LoopFrame {
    fn new(core: FrameCore) -> Self {
        Self {
            core,
            handler: std::sync::Mutex::new(None),
            fd: AtomicI32::new(-1),
            timers: AtomicI32::new(0),
        }
    }

    fn has_handler(&self) -> bool {
        self.handler.lock().unwrap().is_some()
    }

    /// What the host loop does when the fd is readable: call the handler
    /// on its own (this) thread.
    fn pump(&self) {
        let handler = self
            .handler
            .lock()
            .unwrap()
            .clone()
            .expect("event handler registered");
        unsafe { Linux::IEventHandlerTrait::onFDIsSet(&handler, self.fd.load(SeqCst)) };
    }
}

#[cfg(target_os = "linux")]
impl Class for LoopFrame {
    type Interfaces = (IPlugFrame, Linux::IRunLoop);
}

#[cfg(target_os = "linux")]
impl IPlugFrameTrait for LoopFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, rect: *mut ViewRect) -> tresult {
        self.core.resize(view, rect)
    }
}

#[cfg(target_os = "linux")]
impl Linux::IRunLoopTrait for LoopFrame {
    unsafe fn registerEventHandler(
        &self,
        handler: *mut Linux::IEventHandler,
        fd: Linux::FileDescriptor,
    ) -> tresult {
        *self.handler.lock().unwrap() = unsafe { super::retain(handler) };
        self.fd.store(fd, SeqCst);
        kResultOk
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut Linux::IEventHandler) -> tresult {
        let mut slot = self.handler.lock().unwrap();
        assert_eq!(slot.as_ref().map(ComPtr::as_ptr), Some(handler));
        *slot = None;
        self.fd.store(-1, SeqCst);
        kResultOk
    }

    unsafe fn registerTimer(
        &self,
        _: *mut Linux::ITimerHandler,
        _: Linux::TimerInterval,
    ) -> tresult {
        self.timers.fetch_add(1, SeqCst);
        kResultOk
    }

    unsafe fn unregisterTimer(&self, _: *mut Linux::ITimerHandler) -> tresult {
        self.timers.fetch_sub(1, SeqCst);
        kResultOk
    }
}

#[cfg(target_os = "linux")]
fn frame_ptr(frame: &ComWrapper<LoopFrame>) -> *mut IPlugFrame {
    frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr()
}

// ---------------------------------------------------------------------------
// lifecycle
// ---------------------------------------------------------------------------

mod lifecycle {
    use super::*;

    static RESETS: AtomicU32 = AtomicU32::new(0);
    static BAD: AtomicBool = AtomicBool::new(false);
    static PROCESSING_CALLS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn reset(_: *mut c_void, rate: f64, frames: u32, mode: i32) {
        if (rate - 44100.0).abs() > f64::EPSILON || frames != 1024 || mode != 0 {
            BAD.store(true, SeqCst);
        }
        RESETS.fetch_add(1, SeqCst);
    }
    unsafe extern "C" fn prepare(ctx: *mut c_void, rate: f64, frames: u32, mode: i32) -> i32 {
        unsafe { reset(ctx, rate, frames, mode) };
        1
    }
    unsafe extern "C" fn set_processing(
        ctx: *mut c_void,
        on: i32,
        rate: f64,
        frames: u32,
        mode: i32,
    ) {
        PROCESSING_CALLS.fetch_add(1, SeqCst);
        if on == 0 {
            unsafe { reset(ctx, rate, frames, mode) };
        }
    }

    #[test]
    fn lifecycle_resets() {
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                reset,
                prepare,
                set_processing,
                ..callbacks()
            },
            Vec::new(),
        );
        for processing in [false, true] {
            RESETS.store(0, SeqCst);
            PROCESSING_CALLS.store(0, SeqCst);
            let com = component(reg);
            unsafe {
                assert_eq!(com.setActive(1), kResultOk);
                assert_eq!(RESETS.load(SeqCst), 1);
                if processing {
                    com.setProcessing(1);
                    assert_eq!(PROCESSING_CALLS.load(SeqCst), 1);
                    assert_eq!(RESETS.load(SeqCst), 1);
                    com.setProcessing(0);
                    assert_eq!(PROCESSING_CALLS.load(SeqCst), 2);
                } else {
                    com.setActive(0);
                }
            }
            assert_eq!(RESETS.load(SeqCst), 2);
        }
        assert!(!BAD.load(SeqCst));
    }
}

// ---------------------------------------------------------------------------
// editor_attach
// ---------------------------------------------------------------------------

mod editor_attach {
    use super::*;

    static TOKEN: u8 = 0;
    static OPENS: AtomicU32 = AtomicU32::new(0);
    static CLOSES: AtomicU32 = AtomicU32::new(0);
    static RESTORES: AtomicU32 = AtomicU32::new(0);
    static RESETS: AtomicU32 = AtomicU32::new(0);
    static PARENT: AtomicUsize = AtomicUsize::new(0);
    static WIDTH: AtomicU32 = AtomicU32::new(1180);
    static HEIGHT: AtomicU32 = AtomicU32::new(760);
    static BAD: AtomicBool = AtomicBool::new(false);
    static FAIL_OPEN: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn create() -> *mut c_void {
        (&raw const TOKEN).cast_mut().cast()
    }
    unsafe extern "C" fn open(ctx: *mut c_void, parent: *mut c_void) -> i32 {
        if FAIL_OPEN.load(SeqCst) {
            return 0;
        }
        if ctx != unsafe { create() } {
            BAD.store(true, SeqCst);
        }
        OPENS.fetch_add(1, SeqCst);
        PARENT.store(parent as usize, SeqCst);
        1
    }
    unsafe extern "C" fn close(_: *mut c_void) {
        CLOSES.fetch_add(1, SeqCst);
    }
    unsafe extern "C" fn size(_: *mut c_void, w: *mut u32, h: *mut u32) {
        unsafe {
            *w = WIDTH.load(SeqCst);
            *h = HEIGHT.load(SeqCst);
        }
    }
    unsafe extern "C" fn resize(_: *mut c_void, w: u32, h: u32) {
        WIDTH.store(w, SeqCst);
        HEIGHT.store(h, SeqCst);
    }
    unsafe extern "C" fn load(
        _: *mut c_void,
        stream: *mut c_void,
        read: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> i64,
    ) -> i32 {
        let mut byte = 0u8;
        if unsafe { read(stream, &raw mut byte, 1) != 1 || read(stream, &raw mut byte, 1) != 0 } {
            return 0;
        }
        RESTORES.fetch_add(1, SeqCst);
        1
    }
    unsafe extern "C" fn reset(_: *mut c_void, _: f64, _: u32, _: i32) {
        RESETS.fetch_add(1, SeqCst);
    }
    unsafe extern "C" fn prepare(ctx: *mut c_void, rate: f64, frames: u32, mode: i32) -> i32 {
        unsafe { reset(ctx, rate, frames, mode) };
        1
    }
    unsafe extern "C" fn has_editor(_: *mut c_void) -> i32 {
        1
    }

    /// One state byte, then end of stream.
    struct OneByte(AtomicBool);

    impl Class for OneByte {
        type Interfaces = (IBStream,);
    }

    impl IBStreamTrait for OneByte {
        unsafe fn read(&self, buffer: *mut c_void, _: int32, count: *mut int32) -> tresult {
            let first = !self.0.swap(true, SeqCst);
            unsafe {
                *count = i32::from(first);
                if first {
                    *buffer.cast::<u8>() = 0;
                }
            }
            kResultOk
        }
        unsafe fn write(&self, _: *mut c_void, _: int32, _: *mut int32) -> tresult {
            kNotImplemented
        }
        unsafe fn seek(&self, _: int64, _: int32, _: *mut int64) -> tresult {
            kNotImplemented
        }
        unsafe fn tell(&self, _: *mut int64) -> tresult {
            kNotImplemented
        }
    }

    #[test]
    fn editor_attachment() {
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                create,
                gui_has_editor: has_editor,
                gui_open: open,
                gui_close: close,
                gui_get_size: size,
                gui_set_size: resize,
                state_load_stream: load,
                reset,
                prepare,
                ..callbacks()
            },
            Vec::new(),
        );
        let com = component(reg);
        let view = unsafe { ComPtr::from_raw(com.createView(c"editor".as_ptr())) }.expect("view");
        let mut parent_token = 0u8;
        let parent: *mut c_void = (&raw mut parent_token).cast();
        let counts = || (OPENS.load(SeqCst), CLOSES.load(SeqCst));
        unsafe {
            // Reject native representation mismatches before changing
            // attachment state or forwarding the pointer to the plugin.
            for kind in [c"unsupported", c"HIView", c"UIView", c"nsview"] {
                assert_eq!(view.attached(parent, kind.as_ptr()), kResultFalse);
                assert_eq!(counts(), (0, 0));
            }
            assert_eq!(view.isPlatformTypeSupported(std::ptr::null()), kResultFalse);
            assert_eq!(view.isPlatformTypeSupported(PLATFORM.as_ptr()), kResultOk);
            assert_eq!(
                view.attached(std::ptr::null_mut(), PLATFORM.as_ptr()),
                kResultFalse
            );
            assert_eq!(counts(), (0, 0));
            // An editor that fails to open leaves nothing attached.
            FAIL_OPEN.store(true, SeqCst);
            assert_eq!(view.attached(parent, PLATFORM.as_ptr()), kResultFalse);
            assert_eq!(counts(), (0, 0));
            FAIL_OPEN.store(false, SeqCst);
            assert_eq!(view.removed(), kResultOk);
            assert_eq!(counts(), (0, 0));

            // Fresh inactive instance: no state restore or activation
            // precedes attached; the editor must exist once it succeeds.
            assert_eq!(view.attached(parent, PLATFORM.as_ptr()), kResultOk);
            assert_eq!(counts(), (1, 0));
            assert_eq!(PARENT.load(SeqCst), parent as usize);
            // A failed reattachment keeps the attached editor.
            assert_eq!(view.attached(parent, c"unsupported".as_ptr()), kResultFalse);
            assert_eq!(counts(), (1, 0));

            let mut size = ViewRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            assert_eq!(view.getSize(&raw mut size), kResultOk);
            assert_eq!((size.right, size.bottom), (1180, 760));
            let mut size = ViewRect {
                left: 10,
                top: 20,
                right: 1310,
                bottom: 920,
            };
            assert_eq!(view.onSize(&raw mut size), kResultOk);
            assert_eq!((WIDTH.load(SeqCst), HEIGHT.load(SeqCst)), (1300, 900));

            let stream = ComWrapper::new(OneByte(AtomicBool::new(false)));
            let stream = stream.as_com_ref::<IBStream>().unwrap().as_ptr();
            assert_eq!(IComponentTrait::setState(&*com, stream), kResultOk);
            assert_eq!((RESTORES.load(SeqCst), OPENS.load(SeqCst)), (1, 1));
            assert_eq!(com.setActive(1), kResultOk);
            assert_eq!((RESETS.load(SeqCst), OPENS.load(SeqCst)), (1, 1));
            assert_eq!(view.removed(), kResultOk);
            assert_eq!(CLOSES.load(SeqCst), 1);
            assert_eq!(com.setActive(0), kResultOk);
            assert_eq!(OPENS.load(SeqCst), 1);

            // A reopened editor stays independent of processor activation.
            assert_eq!(view.attached(parent, PLATFORM.as_ptr()), kResultOk);
            assert_eq!(OPENS.load(SeqCst), 2);
            assert_eq!(view.removed(), kResultOk);
            assert_eq!(CLOSES.load(SeqCst), 2);
        }
        assert!(!BAD.load(SeqCst));
    }
}

// ---------------------------------------------------------------------------
// editor_lifetime
// ---------------------------------------------------------------------------

mod editor_lifetime {
    use super::*;

    static CONTEXT_ALIVE: AtomicBool = AtomicBool::new(false);
    static EDITOR_OPEN: AtomicBool = AtomicBool::new(false);
    static DESTROYS: AtomicU32 = AtomicU32::new(0);
    static OPENS: AtomicU32 = AtomicU32::new(0);
    static CLOSES: AtomicU32 = AtomicU32::new(0);
    static BAD: AtomicBool = AtomicBool::new(false);

    fn require(ok: bool) {
        if !ok {
            BAD.store(true, SeqCst);
        }
    }
    fn alive() -> bool {
        CONTEXT_ALIVE.load(SeqCst)
    }
    unsafe extern "C" fn create() -> *mut c_void {
        require(!CONTEXT_ALIVE.swap(true, SeqCst));
        (&raw const CONTEXT_ALIVE).cast_mut().cast()
    }
    unsafe extern "C" fn destroy(_: *mut c_void) {
        require(CONTEXT_ALIVE.swap(false, SeqCst));
        DESTROYS.fetch_add(1, SeqCst);
    }
    unsafe extern "C" fn has_editor(_: *mut c_void) -> i32 {
        require(alive());
        1
    }
    unsafe extern "C" fn open(_: *mut c_void, _: *mut c_void) -> i32 {
        require(alive());
        EDITOR_OPEN.store(true, SeqCst);
        OPENS.fetch_add(1, SeqCst);
        1
    }
    unsafe extern "C" fn close(_: *mut c_void) {
        require(alive() && EDITOR_OPEN.swap(false, SeqCst));
        CLOSES.fetch_add(1, SeqCst);
    }
    unsafe extern "C" fn size(_: *mut c_void, w: *mut u32, h: *mut u32) {
        require(alive());
        unsafe {
            *w = 1180;
            *h = 760;
        }
    }

    fn ctx() -> *mut c_void {
        (&raw const CONTEXT_ALIVE).cast_mut().cast()
    }

    fn view_call<R>(view: *mut IPlugView, f: impl FnOnce(&ComRef<'_, IPlugView>) -> R) -> R {
        f(&unsafe { ComRef::from_raw(view) }.unwrap())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn editor_ownership() {
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                create,
                destroy,
                gui_has_editor: has_editor,
                gui_open: open,
                gui_close: close,
                gui_get_size: size,
                ..callbacks()
            },
            Vec::new(),
        );
        let mut parent_token = 0u8;
        let parent: *mut c_void = (&raw mut parent_token).cast();
        let attach = |v| view_call(v, |v| unsafe { v.attached(parent, PLATFORM.as_ptr()) });
        let remove = |v| view_call(v, |v| unsafe { v.removed() });

        for iteration in 0..100 {
            let com = raw_component(reg);
            // Two outstanding views each keep the shared instance alive.
            let first = create_view(com);
            let current = create_view(com);
            assert!(!first.is_null() && !current.is_null());
            let scale = unsafe { ComRef::from_raw(current) }
                .unwrap()
                .cast::<IPlugViewContentScaleSupport>()
                .unwrap()
                .into_raw();
            // Host releases its last component interface before its views.
            assert_eq!(release(com), 0);
            assert!(alive() && DESTROYS.load(SeqCst) == iteration);
            assert_eq!(release(first), 0);
            assert!(
                super::super::pin(ctx())
                    .unwrap()
                    .is_current_view(current as usize)
            );
            assert_eq!(attach(current), kResultOk);
            assert_eq!(remove(current), kResultOk);
            assert_eq!(attach(current), kResultOk);
            assert_eq!(remove(current), kResultOk);
            assert_eq!(release(current), 1);
            // The secondary scale interface keeps the same view and owner.
            let view = unsafe { ComRef::from_raw(scale) }
                .unwrap()
                .cast::<IPlugView>()
                .unwrap();
            let mut size = ViewRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            assert_eq!(unsafe { view.getSize(&raw mut size) }, kResultOk);
            assert_eq!(size.right, 1180);
            drop(view);
            assert_eq!(release(scale), 0);
            assert!(!alive() && DESTROYS.load(SeqCst) == iteration + 1);
        }
        assert_eq!((OPENS.load(SeqCst), CLOSES.load(SeqCst)), (200, 200));

        // The common order: view released before component.
        let com = raw_component(reg);
        let view = create_view(com);
        assert!(release(view) == 0 && alive());
        assert!(release(com) == 0 && !alive() && DESTROYS.load(SeqCst) == 101);

        // IPlugFrame is another independent COM owner: kept from setFrame
        // until replacement, clear or teardown.
        let frames = [0; 3].map(|_| ComWrapper::new(Frame(FrameCore::new(1180, 760))));
        let fp = |i: usize| frames[i].as_com_ref::<IPlugFrame>().unwrap().as_ptr();
        let set_frame = |v, f| view_call(v, |v| unsafe { v.setFrame(f) });
        let base = refs(fp(0));
        let com = raw_component(reg);
        let view = create_view(com);
        assert!(set_frame(view, fp(0)) == kResultOk && refs(fp(0)) > base);
        assert!(set_frame(view, fp(0)) == kResultOk && refs(fp(0)) > base);
        assert_eq!(super::super::request_resize(ctx(), 1180, 760), 1);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(frames[0].0.resizes.load(SeqCst), 1);
        #[cfg(target_os = "macos")]
        {
            assert_eq!(frames[0].0.resizes.load(SeqCst), 0);
            assert_eq!(super::super::pin(ctx()).unwrap().edit_counts().0, 1);
        }
        assert!(set_frame(view, fp(1)) == kResultOk && refs(fp(0)) == base && refs(fp(1)) > base);
        assert!(set_frame(view, std::ptr::null_mut()) == kResultOk && refs(fp(1)) == base);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(super::super::request_resize(ctx(), 1180, 760), 0);
        assert!(set_frame(view, fp(2)) == kResultOk && refs(fp(2)) > base);
        assert!(release(view) == 0 && refs(fp(2)) == base && alive());
        assert!(release(com) == 0 && !alive() && DESTROYS.load(SeqCst) == 102);
        assert!(frames.iter().all(|f| !f.0.bad.load(SeqCst)));

        // Only the current view binds the instance's run loop.
        #[cfg(target_os = "linux")]
        {
            let loops = [0; 2].map(|_| ComWrapper::new(LoopFrame::new(FrameCore::new(1180, 760))));
            let (old_frame, current_frame) = (frame_ptr(&loops[0]), frame_ptr(&loops[1]));
            let timers = |i: usize| loops[i].timers.load(SeqCst);
            let base = refs(old_frame);
            let com = raw_component(reg);
            let old = create_view(com);
            assert!(
                set_frame(old, old_frame) == kResultOk && loops[0].has_handler() && timers(0) == 0
            );
            let view = create_view(com);
            assert!(
                set_frame(view, current_frame) == kResultOk
                    && loops[1].has_handler()
                    && timers(1) == 0
            );
            assert!(timers(0) == 0 && !loops[0].has_handler());
            assert!(
                set_frame(old, std::ptr::null_mut()) == kResultOk
                    && loops[1].has_handler()
                    && timers(1) == 0
            );
            assert_eq!(refs(old_frame), base);
            assert!(
                set_frame(old, old_frame) == kResultOk
                    && loops[1].has_handler()
                    && !loops[0].has_handler()
                    && timers(1) == 0
                    && timers(0) == 0
            );
            assert!(
                release(old) == 0
                    && loops[1].has_handler()
                    && timers(1) == 0
                    && refs(current_frame) > base
            );
            assert!(release(view) == 0 && timers(1) == 0 && !loops[1].has_handler());
            assert!(refs(old_frame) == base && refs(current_frame) == base);
            assert!(release(com) == 0 && !alive() && DESTROYS.load(SeqCst) == 103);
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Keep the destroy count in step with the Linux branch.
            let com = raw_component(reg);
            release(com);
        }

        // The view that attached owns the open editor.
        let (opens, closes) = (OPENS.load(SeqCst), CLOSES.load(SeqCst));
        let open = || EDITOR_OPEN.load(SeqCst);
        let com = raw_component(reg);
        let old = create_view(com);
        assert!(attach(old) == kResultOk && open());
        let view = create_view(com);
        // Merely creating a replacement must not disown the attached view.
        assert!(remove(old) == kResultOk && !open() && CLOSES.load(SeqCst) == closes + 1);
        assert!(attach(old) == kResultOk && open());
        assert!(attach(view) == kResultOk && open());
        // A newer attachment supersedes: the old view's removal and release
        // must not close the editor now owned by the new view.
        assert!(remove(old) == kResultOk && open() && CLOSES.load(SeqCst) == closes + 1);
        assert!(release(old) == 0 && open() && CLOSES.load(SeqCst) == closes + 1);
        assert!(remove(view) == kResultOk && !open() && CLOSES.load(SeqCst) == closes + 2);
        assert!(remove(view) == kResultOk && CLOSES.load(SeqCst) == closes + 2);
        assert!(attach(view) == kResultOk && open());
        // The view, not the component interface, keeps the instance.
        assert!(release(com) == 0 && alive());
        assert!(release(view) == 0 && !open() && !alive() && DESTROYS.load(SeqCst) == 104);
        assert_eq!(
            (OPENS.load(SeqCst), CLOSES.load(SeqCst)),
            (opens + 4, closes + 3)
        );
        assert!(!BAD.load(SeqCst));
    }
}

// ---------------------------------------------------------------------------
// parameter_units
// ---------------------------------------------------------------------------

#[test]
fn parameter_units() {
    let mut groups: Vec<String> = (1..=96).map(|i| format!("Oscillator {i}/Unison")).collect();
    let long_prefix = "x".repeat(180);
    groups.push(format!("{long_prefix}A/Leaf"));
    groups.push(format!("{long_prefix}B/Leaf"));
    groups.push(String::new());
    let groups: &'static [CString] = Box::leak(
        groups
            .into_iter()
            .map(|g| CString::new(g).unwrap())
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let params: Vec<Vst3ParamDescriptor> = (0..5004)
        .map(|index: usize| Vst3ParamDescriptor {
            id: 0x0100_0000 + index as u32,
            name: c"Voices".as_ptr(),
            short_name: c"Voices".as_ptr(),
            units: c"".as_ptr(),
            min: 1.0,
            max: 64.0,
            default_normalized: 0.0,
            step_count: 63,
            flags: 1,
            group: groups[if index < 5001 {
                index % 96
            } else {
                96 + (index - 5001)
            }]
            .as_ptr(),
        })
        .collect();
    let ids: Vec<u32> = params.iter().map(|p| p.id).collect();
    let reg = registration(descriptor(), callbacks(), params);
    let com = component(reg);
    let zero_info = || -> ParameterInfo { unsafe { std::mem::zeroed() } };
    let zero_unit = || -> UnitInfo { unsafe { std::mem::zeroed() } };
    unsafe {
        assert_eq!(com.getUnitCount(), 197); // root + 98 parents/children
        for index in [0, 4095, 4096, 5000] {
            let mut info = zero_info();
            assert_eq!(com.getParameterInfo(index, &raw mut info), kResultOk);
            assert_eq!(info.id, ids[index as usize]);
            assert_eq!(info.stepCount, 63);
            assert_ne!(info.unitId, 0);
            let (mut child, mut parent) = (zero_unit(), zero_unit());
            assert_eq!(com.getUnitInfo(info.unitId, &raw mut child), kResultOk);
            assert_eq!(
                com.getUnitInfo(child.parentUnitId, &raw mut parent),
                kResultOk
            );
            assert_eq!(parent.parentUnitId, 0);
            assert_eq!(child.programListId, -1);
        }
        let (mut a, mut b, mut root) = (zero_info(), zero_info(), zero_info());
        assert_eq!(com.getParameterInfo(5001, &raw mut a), kResultOk);
        assert_eq!(com.getParameterInfo(5002, &raw mut b), kResultOk);
        assert_ne!(a.unitId, b.unitId); // full paths stay distinct past 128 bytes
        assert_eq!(com.getParameterInfo(5003, &raw mut root), kResultOk);
        assert_eq!(root.unitId, 0);
        assert_eq!(com.getParameterInfo(5004, &raw mut root), kInvalidArgument);
        assert_eq!(com.getUnitInfo(197, std::ptr::null_mut()), kInvalidArgument);
    }
}

// ---------------------------------------------------------------------------
// output_silence
// ---------------------------------------------------------------------------

mod output_silence {
    use super::*;

    trait Sample: Copy + Into<f64> {
        const QUARTER: Self;
        const ZERO: Self;
    }
    impl Sample for f32 {
        const QUARTER: Self = 0.25;
        const ZERO: Self = 0.0;
    }
    impl Sample for f64 {
        const QUARTER: Self = 0.25;
        const ZERO: Self = 0.0;
    }

    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn synthesize<S: Sample>(
        _: *mut c_void,
        _: *const *const c_void,
        outputs: *mut *mut c_void,
        _: u32,
        num_out: u32,
        _: *const u32,
        _: u32,
        _: u32,
        _: *const u32,
        _: u32,
        _: u32,
        frames: u32,
        _: *const Vst3Transport,
        _: i32,
    ) -> u32 {
        for ch in 0..num_out as usize {
            let out = unsafe { *outputs.add(ch) }.cast::<S>();
            for i in 0..frames as usize {
                unsafe { *out.add(i) = S::QUARTER };
            }
        }
        1
    }

    unsafe extern "C" fn bus_channels(_: u32, output: i32, bus: u32) -> u32 {
        if output != 0 && bus == 0 { 2 } else { 0 }
    }

    fn audible_block<S: Sample>(com: &Component, sample_size: i32) -> bool {
        let mut setup = ProcessSetup {
            processMode: 0,
            symbolicSampleSize: sample_size,
            maxSamplesPerBlock: 16,
            sampleRate: 48000.0,
        };
        if unsafe { com.setupProcessing(&raw mut setup) } != kResultOk {
            return false;
        }
        let mut samples = [[S::ZERO; 16]; 2];
        let [a, b] = &mut samples;
        let mut channels = [a.as_mut_ptr(), b.as_mut_ptr()];
        let mut output: AudioBusBuffers = unsafe { std::mem::zeroed() };
        output.numChannels = 2;
        output.__field0.channelBuffers32 = channels.as_mut_ptr().cast();
        let mut data: ProcessData = unsafe { std::mem::zeroed() };
        data.symbolicSampleSize = sample_size;
        data.numSamples = 16;
        data.numOutputs = 1;
        data.outputs = &raw mut output;
        // Hosts may reuse a bus previously marked silent; every render
        // publishes its own silence state.
        for _ in 0..2 {
            output.silenceFlags = 3;
            if unsafe { com.process(&raw mut data) } != kResultOk {
                return false;
            }
            for (ch, buf) in samples.iter().enumerate() {
                let host = if output.silenceFlags & (1 << ch) != 0 {
                    0.0
                } else {
                    buf[0].into()
                };
                if (host - 0.25).abs() > f64::EPSILON {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn generated_audio_reaches_hosts_that_honor_silence_flags() {
        static MAIN: [u8; 1] = [0];
        let desc = Vst3PluginDescriptor {
            num_outputs: 2,
            max_outputs: 2,
            num_output_buses: 1,
            output_bus_kinds: MAIN.as_ptr(),
            supports_f64: 1,
            ..descriptor()
        };
        let reg = registration(
            desc,
            Vst3Callbacks {
                layout_bus_channels: bus_channels,
                process: synthesize::<f32>,
                process_f64: synthesize::<f64>,
                ..callbacks()
            },
            Vec::new(),
        );
        let com = component(reg);
        assert!(
            audible_block::<f32>(&com, SymbolicSampleSizes_::kSample32 as i32),
            "32-bit"
        );
        assert!(
            audible_block::<f64>(&com, SymbolicSampleSizes_::kSample64 as i32),
            "64-bit"
        );
    }
}

mod output_events {
    use super::*;

    static NEXT: AtomicU32 = AtomicU32::new(0);
    static RESULT: AtomicU32 = AtomicU32::new(99);
    unsafe extern "C" fn count(_: *mut c_void) -> u32 {
        1
    }
    unsafe extern "C" fn next(_: *mut c_void, event: *mut Vst3NativeEvent) -> u32 {
        if NEXT.fetch_add(1, SeqCst) == 0 {
            unsafe { (*event).kind = Event_::EventTypes_::kNoteOnEvent as u32 };
            1
        } else {
            0
        }
    }
    unsafe extern "C" fn finish(_: *mut c_void, result: u32) {
        RESULT.store(result, SeqCst);
    }
    struct Events(AtomicU32);
    impl Class for Events {
        type Interfaces = (IEventList,);
    }
    impl IEventListTrait for Events {
        unsafe fn getEventCount(&self) -> i32 {
            0
        }
        unsafe fn getEvent(&self, _: i32, _: *mut Event) -> tresult {
            kResultFalse
        }
        unsafe fn addEvent(&self, event: *mut Event) -> tresult {
            assert_eq!(
                unsafe { (*event).r#type },
                Event_::EventTypes_::kNoteOnEvent as u16
            );
            self.0.fetch_add(1, SeqCst);
            kResultOk
        }
    }
    #[test]
    fn note_is_delivered_without_parameter_output_queue() {
        NEXT.store(0, SeqCst);
        RESULT.store(99, SeqCst);
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                get_output_param_count: count,
                next_output_event: next,
                finish_output_events: finish,
                ..callbacks()
            },
            Vec::new(),
        );
        let com = component(reg);
        let events = ComWrapper::new(Events(AtomicU32::new(0)));
        let mut data: ProcessData = unsafe { std::mem::zeroed() };
        data.numSamples = 1;
        data.outputEvents = events.as_com_ref::<IEventList>().unwrap().as_ptr();
        assert_eq!(unsafe { com.process(&raw mut data) }, kResultOk);
        assert_eq!(events.0.load(SeqCst), 1);
        assert_eq!(RESULT.load(SeqCst), 2);
    }
}

mod arrangements {
    use super::*;
    unsafe extern "C" fn channels(_: u32, _: i32, _: u32) -> u32 {
        8
    }
    #[test]
    fn rejects_noncanonical_mask_without_changing_current_layout() {
        let reg = registration(
            Vst3PluginDescriptor {
                num_output_buses: 1,
                num_outputs: 8,
                max_outputs: 8,
                ..descriptor()
            },
            Vst3Callbacks {
                layout_bus_channels: channels,
                ..callbacks()
            },
            Vec::new(),
        );
        let com = component(reg);
        let mut cine: SpeakerArrangement = 0xff;
        assert_eq!(
            unsafe { com.setBusArrangements(std::ptr::null_mut(), 0, &raw mut cine, 1) },
            kResultFalse
        );
        let mut actual = 0;
        assert_eq!(
            unsafe { com.getBusArrangement(BusDirections_::kOutput as i32, 0, &raw mut actual) },
            kResultOk
        );
        assert_eq!(actual, 0x63f);
        assert_eq!(
            unsafe { com.setBusArrangements(std::ptr::null_mut(), 0, &raw mut actual, 1) },
            kResultOk
        );
    }
}

// ---------------------------------------------------------------------------
// editor_threads (Linux: the host IRunLoop wake)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod threads {
    //! Editor -> host calls made off the host UI thread (a Linux editor's
    //! own X11 thread) reach `IComponentHandler` and `IPlugFrame` only on the
    //! UI thread, in order, woken through the host `IRunLoop`'s event handler.

    use std::sync::{Arc, Mutex};
    use std::thread::ThreadId;

    use super::super::component::{DRAIN_EDITS, EditOp, TRANSITION_EDITS};
    use super::super::{begin_edit, end_edit, perform_edit, pin, request_resize};
    use super::*;

    pub(super) struct State {
        ui: ThreadId,
        calls: Mutex<String>,
        ctx: AtomicUsize,
        flood: AtomicBool,
        burst: AtomicI32,
        worker_on_begin: AtomicBool,
        pub emit_on_resize: AtomicBool,
    }

    impl State {
        pub fn record(&self, c: char) {
            // A host call off the UI thread shows up as '!'.
            let c = if std::thread::current().id() == self.ui {
                c
            } else {
                '!'
            };
            self.calls.lock().unwrap().push(c);
        }
        fn calls(&self) -> String {
            self.calls.lock().unwrap().clone()
        }
        fn clear(&self) {
            self.calls.lock().unwrap().clear();
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Normal,
        Replacement,
        Slow,
        /// Releases `victim` from inside performEdit.
        Drop,
    }

    struct Handler {
        st: Arc<State>,
        kind: Kind,
        victim: AtomicUsize,
    }

    impl Class for Handler {
        type Interfaces = (IComponentHandler,);
    }

    fn handler(st: &Arc<State>, kind: Kind) -> ComWrapper<Handler> {
        ComWrapper::new(Handler {
            st: st.clone(),
            kind,
            victim: AtomicUsize::new(0),
        })
    }

    fn hp(h: &ComWrapper<Handler>) -> *mut IComponentHandler {
        h.as_com_ref::<IComponentHandler>().unwrap().as_ptr()
    }

    fn off_ui(f: impl FnOnce() + Send + 'static) {
        std::thread::spawn(f).join().unwrap();
    }

    impl IComponentHandlerTrait for Handler {
        unsafe fn beginEdit(&self, _: ParamID) -> tresult {
            if self.kind == Kind::Slow {
                TEARDOWN_STAGE.store(1, SeqCst);
                while TEARDOWN_STAGE.load(SeqCst) != 2 {
                    std::thread::yield_now();
                }
                if DESTROYED.load(SeqCst) {
                    BAD.store(true, SeqCst);
                }
                return kResultOk;
            }
            self.st.record(if self.kind == Kind::Replacement {
                'B'
            } else {
                'b'
            });
            if self.st.worker_on_begin.swap(false, SeqCst) {
                let ctx = self.st.ctx.load(SeqCst);
                off_ui(move || end_edit(ctx as *mut c_void, 7));
            }
            kResultOk
        }

        unsafe fn performEdit(&self, _: ParamID, value: ParamValue) -> tresult {
            if self.kind == Kind::Drop {
                release(self.victim.load(SeqCst) as *mut IComponent);
                if DESTROYED.load(SeqCst) {
                    BAD.store(true, SeqCst); // our call is still inside it
                }
                return kResultOk;
            }
            let c = match self.kind {
                Kind::Replacement => 'P',
                _ if (value - 0.5).abs() < f64::EPSILON => 'p',
                _ => '?',
            };
            self.st.record(c);
            // A worker that submits again for every edit the host takes.
            let burst = self.st.burst.load(SeqCst);
            if self.st.flood.load(SeqCst) || burst > 0 {
                if burst > 0 {
                    self.st.burst.fetch_sub(1, SeqCst);
                }
                let ctx = self.st.ctx.load(SeqCst);
                off_ui(move || perform_edit(ctx as *mut c_void, 7, 0.5));
            }
            kResultOk
        }

        unsafe fn endEdit(&self, _: ParamID) -> tresult {
            self.st.record(if self.kind == Kind::Replacement {
                'E'
            } else {
                'e'
            });
            kResultOk
        }

        unsafe fn restartComponent(&self, _: int32) -> tresult {
            self.st.record('x');
            kResultOk
        }
    }

    static WORKER_ON_RESIZE: AtomicBool = AtomicBool::new(false);
    static WATCH: AtomicUsize = AtomicUsize::new(0);
    static DESTROYED: AtomicBool = AtomicBool::new(false);
    static TEARDOWN_STAGE: AtomicI32 = AtomicI32::new(0);
    static BAD: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn has_editor(_: *mut c_void) -> i32 {
        1
    }
    unsafe extern "C" fn set_size(ctx: *mut c_void, _: u32, _: u32) {
        if WORKER_ON_RESIZE.load(SeqCst) {
            let ctx = ctx as usize;
            off_ui(move || end_edit(ctx as *mut c_void, 7));
        } else {
            begin_edit(ctx, 8);
            end_edit(ctx, 8);
        }
    }
    unsafe extern "C" fn destroy(ctx: *mut c_void) {
        if ctx as usize == WATCH.load(SeqCst) {
            DESTROYED.store(true, SeqCst);
        }
    }

    /// The eventfd has a pending wake (left pending for the host loop).
    fn readable(fd: i32) -> bool {
        let mut v = 0u64;
        unsafe {
            libc::read(fd, (&raw mut v).cast(), 8) > 0 && {
                libc::write(fd, (&raw const v).cast(), 8);
                true
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn editor_calls_off_the_ui_thread() {
        let st = Arc::new(State {
            ui: std::thread::current().id(),
            calls: Mutex::new(String::new()),
            ctx: AtomicUsize::new(0),
            flood: AtomicBool::new(false),
            burst: AtomicI32::new(0),
            worker_on_begin: AtomicBool::new(false),
            emit_on_resize: AtomicBool::new(false),
        });
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                gui_has_editor: has_editor,
                gui_set_size: set_size,
                destroy,
                ..callbacks()
            },
            Vec::new(),
        );
        let normal = handler(&st, Kind::Normal);
        let next = handler(&st, Kind::Replacement);
        let frame = ComWrapper::new(LoopFrame::new(FrameCore {
            threads: Some(st.clone()),
            ..FrameCore::new(640, 480)
        }));
        let fp = frame_ptr(&frame);

        let com = component(reg);
        let c: &Component = &com;
        let ctx = com.host.ctx;
        st.ctx.store(ctx as usize, SeqCst);
        let ctx_u = ctx as usize;
        let set_handler = |h: *mut IComponentHandler| unsafe { c.setComponentHandler(h) };
        set_handler(hp(&normal));
        // No wake yet: worker edits wait for the next UI-thread operation.
        off_ui(move || begin_edit(ctx_u as *mut c_void, 7));
        assert_eq!(st.calls(), "");
        unsafe { c.getParamNormalized(7) };
        assert_eq!(st.calls(), "b");
        end_edit(ctx, 7);
        assert_eq!(st.calls(), "be");
        st.clear();
        // A gesture remains open even with an empty queue during a swap.
        begin_edit(ctx, 7);
        set_handler(hp(&next));
        end_edit(ctx, 7);
        assert_eq!(st.calls(), "beBE");
        set_handler(hp(&normal));
        st.clear();
        let view = unsafe { ComPtr::from_raw(c.createView(c"editor".as_ptr())) }.unwrap();
        let set_frame = |f: *mut IPlugFrame| unsafe { view.setFrame(f) };
        assert!(
            set_frame(fp) == kResultOk && frame.has_handler() && frame.timers.load(SeqCst) == 0
        );
        let edits = || com.host.edit_counts();
        let fd = || frame.fd.load(SeqCst);
        let resizes = || frame.core.resizes.load(SeqCst);
        let flood = |on: bool| st.flood.store(on, SeqCst);
        let p = |n: usize| "p".repeat(n);

        // A gesture and a resize from the editor's own thread wait for the loop.
        off_ui(move || {
            let ctx = ctx_u as *mut c_void;
            begin_edit(ctx, 7);
            perform_edit(ctx, 7, 0.5);
            end_edit(ctx, 7);
            assert_eq!(request_resize(ctx, 640, 480), 1);
        });
        assert!(st.calls().is_empty() && resizes() == 0 && readable(fd()));
        frame.pump();
        assert_eq!((st.calls().as_str(), resizes()), ("bper", 1));

        // A call on the UI thread goes first through whatever is queued.
        st.clear();
        off_ui(move || begin_edit(ctx_u as *mut c_void, 7));
        end_edit(ctx, 7);
        assert_eq!(st.calls(), "be");

        // A synchronous resize -> onSize -> editor callback must not jump
        // ahead of the rest of the batch already being delivered.
        st.clear();
        st.emit_on_resize.store(true, SeqCst);
        off_ui(move || {
            let ctx = ctx_u as *mut c_void;
            begin_edit(ctx, 7);
            request_resize(ctx, 640, 480);
            perform_edit(ctx, 7, 0.5);
            end_edit(ctx, 7);
        });
        frame.pump();
        assert_eq!(st.calls(), "brpebe");

        // A worker that never stops submitting cannot hold the UI thread:
        // one wake makes a bounded number of calls, then asks for another.
        st.clear();
        flood(true);
        off_ui(move || perform_edit(ctx_u as *mut c_void, 7, 0.5));
        frame.pump();
        assert!(st.calls() == p(DRAIN_EDITS) && readable(fd()));
        flood(false);
        frame.pump();
        assert_eq!(st.calls(), p(DRAIN_EDITS + 1));

        // A large backlog is delivered DRAIN_EDITS at a time, in order.
        st.clear();
        off_ui(move || {
            let ctx = ctx_u as *mut c_void;
            for _ in 0..3 * DRAIN_EDITS {
                perform_edit(ctx, 7, 0.5);
            }
            end_edit(ctx, 7);
        });
        frame.pump();
        assert!(st.calls() == p(DRAIN_EDITS) && readable(fd()));
        frame.pump();
        frame.pump();
        assert!(st.calls() == p(3 * DRAIN_EDITS) && readable(fd()));
        frame.pump();
        assert!(st.calls() == p(3 * DRAIN_EDITS) + "e" && edits().0 == 0);

        // A backlog far past both budgets with two gestures open: one wake
        // still delivers DRAIN_EDITS, and a handler change delivers its
        // budget, ends both gestures on the old handler and reopens them on
        // the new one, ahead of the rest, in order.
        st.clear();
        let big = 4 * TRANSITION_EDITS;
        off_ui(move || {
            let ctx = ctx_u as *mut c_void;
            begin_edit(ctx, 7);
            begin_edit(ctx, 9);
            for _ in 0..big {
                perform_edit(ctx, 7, 0.5);
            }
        });
        frame.pump();
        assert_eq!(st.calls(), "bb".to_owned() + &p(DRAIN_EDITS - 2));
        set_handler(hp(&next));
        let head = "bb".to_owned() + &p(DRAIN_EDITS + TRANSITION_EDITS - 2) + "ee";
        assert_eq!(st.calls(), head);
        while edits().0 != 0 {
            frame.pump();
        }
        end_edit(ctx, 7);
        end_edit(ctx, 9);
        let rest = "P".repeat(big + 2 - DRAIN_EDITS - TRANSITION_EDITS);
        assert_eq!(st.calls(), head + "BB" + &rest + "EE");
        assert_eq!(edits().1, 0);
        set_handler(hp(&normal));

        // Every UI operation stays behind the remainder of a saturated drain.
        for op in ['b', 'p', 'e', 'r'] {
            st.clear();
            flood(true);
            off_ui(move || perform_edit(ctx_u as *mut c_void, 7, 0.5));
            match op {
                'b' => begin_edit(ctx, 7),
                'p' => perform_edit(ctx, 7, 0.5),
                'e' => end_edit(ctx, 7),
                _ => assert_eq!(request_resize(ctx, 640, 480), 1),
            }
            assert!(st.calls() == p(DRAIN_EDITS) && readable(fd()), "{op}");
            flood(false);
            frame.pump();
            assert_eq!(st.calls(), p(DRAIN_EDITS + 1) + &op.to_string());
        }

        // A burst longer than one wake, under way when the handler changes
        // or the editor detaches: all of it still reaches the old handler on
        // the UI thread, and nothing is left queued.
        st.clear();
        st.burst.store(20, SeqCst);
        off_ui(move || perform_edit(ctx_u as *mut c_void, 7, 0.5));
        set_handler(hp(&next));
        assert!(st.calls() == p(21) && edits().0 == 0);
        set_handler(hp(&normal));
        st.clear();
        st.burst.store(20, SeqCst);
        off_ui(move || perform_edit(ctx_u as *mut c_void, 7, 0.5));
        assert_eq!(set_frame(std::ptr::null_mut()), kResultOk);
        assert!(st.calls() == p(21) && edits().0 == 0);
        assert_eq!(set_frame(fp), kResultOk);

        // A submitter that never stops cannot hold a handler change: past
        // the transition budget its gesture ends on the old handler and goes
        // on in a fresh one on the new handler.
        st.clear();
        flood(true);
        off_ui(move || {
            let ctx = ctx_u as *mut c_void;
            begin_edit(ctx, 7);
            perform_edit(ctx, 7, 0.5);
        });
        set_handler(hp(&next));
        flood(false);
        frame.pump();
        end_edit(ctx, 7);
        let calls = st.calls();
        let e = calls.find('e').unwrap();
        assert_eq!(e, TRANSITION_EDITS);
        assert_eq!(
            calls,
            "b".to_owned() + &p(e - 1) + "eB" + &"P".repeat(calls.len() - e - 3) + "E"
        );
        assert_eq!(edits(), (0, 0));
        set_handler(hp(&normal));

        // A detach under the same flood leaves the rest for the next call.
        st.clear();
        flood(true);
        off_ui(move || perform_edit(ctx_u as *mut c_void, 7, 0.5));
        assert_eq!(set_frame(std::ptr::null_mut()), kResultOk);
        assert!(st.calls() == p(TRANSITION_EDITS) && edits().0 != 0);
        flood(false);
        end_edit(ctx, 7); // no frame now: delivered in place, after the rest
        assert_eq!(st.calls(), p(TRANSITION_EDITS + 1) + "e");
        assert_eq!(set_frame(fp), kResultOk);

        // A worker holds the last reference when the host lets go, with an
        // edit queued for the still-bound UI thread: teardown runs on the
        // worker, drops the edit and makes no host call there.
        {
            DESTROYED.store(false, SeqCst);
            let (other, other_ctx) = raw_component_ctx(reg);
            let other_host = pin(other_ctx).unwrap();
            let other_ctx = other_ctx as usize;
            WATCH.store(other_ctx, SeqCst);
            let controller = unsafe { ComRef::from_raw(other) }
                .unwrap()
                .cast::<IEditController>()
                .unwrap();
            unsafe { controller.setComponentHandler(hp(&normal)) };
            drop(controller);
            let loop2 = ComWrapper::new(LoopFrame::new(FrameCore::new(640, 480)));
            // A bound wake, and no view to unbind it.
            other_host.update_run_loop_registration(Some(frame_ptr(&loop2).cast()));
            drop(other_host);
            assert!(loop2.has_handler());
            st.clear();
            let step = Arc::new(AtomicI32::new(0));
            let worker = {
                let step = step.clone();
                std::thread::spawn(move || {
                    let host = pin(other_ctx as *mut c_void).unwrap();
                    host.submit(EditOp::Perform(7, 0.5));
                    step.store(1, SeqCst);
                    while step.load(SeqCst) != 2 {
                        std::thread::yield_now();
                    }
                })
            };
            while step.load(SeqCst) != 1 {
                std::thread::yield_now();
            }
            release(other);
            assert!(!DESTROYED.load(SeqCst));
            step.store(2, SeqCst);
            worker.join().unwrap();
            assert!(DESTROYED.load(SeqCst) && st.calls().is_empty() && !loop2.has_handler());
        }

        // Enqueue from a worker while the old handler is draining. Its End
        // must finish on that handler, before replacing or clearing it.
        for target in [hp(&next), std::ptr::null_mut()] {
            st.clear();
            st.emit_on_resize.store(true, SeqCst);
            WORKER_ON_RESIZE.store(true, SeqCst);
            off_ui(move || {
                let ctx = ctx_u as *mut c_void;
                begin_edit(ctx, 7);
                request_resize(ctx, 640, 480);
                perform_edit(ctx, 7, 0.5);
            });
            set_handler(target);
            assert_eq!(st.calls(), "brpe");
            if !target.is_null() {
                begin_edit(ctx, 8);
                end_edit(ctx, 8);
                assert_eq!(st.calls(), "brpeBE");
            }
            set_handler(hp(&normal));
        }
        WORKER_ON_RESIZE.store(false, SeqCst);

        // A worker enqueue inside the detach drain: the run-loop binding
        // stays live until this batch also finishes.
        st.clear();
        st.worker_on_begin.store(true, SeqCst);
        off_ui(move || {
            let ctx = ctx_u as *mut c_void;
            begin_edit(ctx, 7);
            perform_edit(ctx, 7, 0.5);
        });
        assert_eq!(set_frame(std::ptr::null_mut()), kResultOk);
        assert!(st.calls() == "bpe" && !frame.has_handler() && frame.timers.load(SeqCst) == 0);
        assert_eq!(set_frame(fp), kResultOk);

        // Detaching delivers the rest: the host never stays mid-gesture.
        st.clear();
        off_ui(move || {
            let ctx = ctx_u as *mut c_void;
            begin_edit(ctx, 7);
            end_edit(ctx, 7);
        });
        assert_eq!(set_frame(std::ptr::null_mut()), kResultOk);
        assert!(st.calls() == "be" && !frame.has_handler() && frame.timers.load(SeqCst) == 0);

        // With no run loop bound, calls go straight through.
        st.clear();
        begin_edit(ctx, 7);
        assert_eq!(st.calls(), "b");
        drop(view);
        drop(com);

        // The UI callback pins its host while another thread releases it.
        {
            DESTROYED.store(false, SeqCst);
            TEARDOWN_STAGE.store(0, SeqCst);
            let slow = handler(&st, Kind::Slow);
            let (other, other_ctx) = raw_component_ctx(reg);
            WATCH.store(other_ctx as usize, SeqCst);
            let controller = unsafe { ComRef::from_raw(other) }
                .unwrap()
                .cast::<IEditController>()
                .unwrap();
            unsafe { controller.setComponentHandler(hp(&slow)) };
            drop(controller);
            let other_u = other as usize;
            let worker = std::thread::spawn(move || {
                while TEARDOWN_STAGE.load(SeqCst) != 1 {
                    std::thread::yield_now();
                }
                release(other_u as *mut IComponent);
                assert!(!DESTROYED.load(SeqCst));
                TEARDOWN_STAGE.store(2, SeqCst);
            });
            begin_edit(other_ctx, 3);
            worker.join().unwrap();
            assert!(DESTROYED.load(SeqCst));
            end_edit(other_ctx, 3);
        }

        // The host drops its last reference from inside performEdit: the
        // instance outlives the callback and goes when the editor call
        // returns.
        {
            DESTROYED.store(false, SeqCst);
            let dropper = handler(&st, Kind::Drop);
            let (victim, victim_ctx) = raw_component_ctx(reg);
            WATCH.store(victim_ctx as usize, SeqCst);
            dropper.victim.store(victim as usize, SeqCst);
            let controller = unsafe { ComRef::from_raw(victim) }
                .unwrap()
                .cast::<IEditController>()
                .unwrap();
            unsafe { controller.setComponentHandler(hp(&dropper)) };
            drop(controller);
            perform_edit(victim_ctx, 3, 0.5); // no frame: called in place
            assert!(DESTROYED.load(SeqCst));
        }
        assert!(!BAD.load(SeqCst) && !frame.core.bad.load(SeqCst));
    }

    #[test]
    fn factory_loop_delivers_restart_without_a_view() {
        let st = Arc::new(State {
            ui: std::thread::current().id(),
            calls: Mutex::new(String::new()),
            ctx: AtomicUsize::new(0),
            flood: AtomicBool::new(false),
            burst: AtomicI32::new(0),
            worker_on_begin: AtomicBool::new(false),
            emit_on_resize: AtomicBool::new(false),
        });
        let frame = ComWrapper::new(LoopFrame::new(FrameCore::new(640, 480)));
        let run_loop = frame.as_com_ref::<Linux::IRunLoop>().unwrap().to_com_ptr();
        let reg = registration(descriptor(), callbacks(), Vec::new());
        let com = ComWrapper::new(Component {
            host: Host::new(reg, Some(run_loop)).unwrap(),
        });
        let handler = handler(&st, Kind::Normal);
        assert_eq!(unsafe { com.setComponentHandler(hp(&handler)) }, kResultOk);
        let ctx = com.host.ctx as usize;
        off_ui(move || crate::com::mark_restart(ctx as *mut c_void, 4));
        assert_eq!(st.calls(), "");
        frame.pump();
        assert_eq!(st.calls(), "x");
    }
}

// ---------------------------------------------------------------------------
// host_features: runtime catalog, function names, info listener, MIDI learn,
// group edits
// ---------------------------------------------------------------------------

mod host_features {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU8, AtomicU64};

    // Only the group-edit tests use it, and they skip macOS.
    #[cfg(not(target_os = "macos"))]
    use moose_core::editor::EditGroup;
    use moose_params::CatalogChange;
    use vst3::Steinberg::Vst::ChannelContext::{
        IInfoListener, IInfoListenerTrait, kChannelColorKey, kChannelNameKey,
    };

    use super::super::component::DynamicSupport;
    use super::*;
    use crate::ffi::{PRESENTATION_HIDDEN, PRESENTATION_UNAVAILABLE};

    /// A component handler that logs every call it gets.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Recorder {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
        fn log(&self, entry: String) -> tresult {
            self.0.lock().unwrap().push(entry);
            kResultOk
        }
    }

    impl Class for Recorder {
        type Interfaces = (IComponentHandler, IComponentHandler2);
    }

    impl IComponentHandlerTrait for Recorder {
        unsafe fn beginEdit(&self, id: ParamID) -> tresult {
            self.log(format!("begin {id}"))
        }
        unsafe fn performEdit(&self, id: ParamID, _: ParamValue) -> tresult {
            self.log(format!("perform {id}"))
        }
        unsafe fn endEdit(&self, id: ParamID) -> tresult {
            self.log(format!("end {id}"))
        }
        unsafe fn restartComponent(&self, flags: int32) -> tresult {
            self.log(format!("restart {flags}"))
        }
    }

    impl IComponentHandler2Trait for Recorder {
        unsafe fn setDirty(&self, _: TBool) -> tresult {
            self.log("dirty".into())
        }
        unsafe fn requestOpenEditor(&self, _: FIDString) -> tresult {
            kNotImplemented
        }
        unsafe fn startGroupEdit(&self) -> tresult {
            self.log("group".into())
        }
        unsafe fn finishGroupEdit(&self) -> tresult {
            self.log("group end".into())
        }
    }

    fn attach(com: &ComWrapper<Component>) -> ComWrapper<Recorder> {
        let recorder = ComWrapper::new(Recorder::default());
        let handler = recorder.as_com_ref::<IComponentHandler>().unwrap().as_ptr();
        assert_eq!(unsafe { com.setComponentHandler(handler) }, kResultOk);
        recorder
    }

    fn param(id: u32) -> Vst3ParamDescriptor {
        Vst3ParamDescriptor {
            id,
            name: c"Param".as_ptr(),
            short_name: c"Param".as_ptr(),
            units: c"".as_ptr(),
            min: 0.0,
            max: 1.0,
            default_normalized: 0.0,
            step_count: 0,
            flags: ParameterInfo_::ParameterFlags_::kCanAutomate as i32,
            group: c"".as_ptr(),
        }
    }

    /// `IHostApplication` that reports a fixed name.
    struct App(&'static str);

    impl Class for App {
        type Interfaces = (IHostApplication,);
    }

    impl IHostApplicationTrait for App {
        unsafe fn getName(&self, name: *mut String128) -> tresult {
            super::super::utf16_into(unsafe { &mut *name }, self.0);
            kResultOk
        }
        unsafe fn createInstance(
            &self,
            _: *mut TUID,
            _: *mut TUID,
            _: *mut *mut c_void,
        ) -> tresult {
            kNotImplemented
        }
    }

    fn initialize_in(com: &ComWrapper<Component>, host: &'static str) {
        let app = ComWrapper::new(App(host));
        let context = app.as_com_ref::<FUnknown>().unwrap().as_ptr();
        assert_eq!(unsafe { com.initialize(context) }, kResultOk);
    }

    #[test]
    fn host_names_pick_the_dynamic_mode() {
        assert_eq!(
            DynamicSupport::for_host("Cubase Pro 14"),
            DynamicSupport::Reload
        );
        assert_eq!(DynamicSupport::for_host("Nuendo"), DynamicSupport::Reload);
        assert_eq!(DynamicSupport::for_host("REAPER"), DynamicSupport::SlotPool);
        let reg = registration(descriptor(), callbacks(), Vec::new());
        let com = component(reg);
        assert_eq!(
            com.host.dynamic_support(),
            DynamicSupport::SlotPool,
            "before initialize"
        );
        initialize_in(&com, "Nuendo 13");
        assert_eq!(com.host.dynamic_support(), DynamicSupport::Reload);
    }

    // --- catalog wake ---------------------------------------------------------

    type Wake = (unsafe extern "C" fn(*mut c_void), usize);
    static WAKE: Mutex<Option<Wake>> = Mutex::new(None);
    static CHANGES: AtomicU8 = AtomicU8::new(0);

    unsafe extern "C" fn store_waker(
        _: *mut c_void,
        wake: Option<unsafe extern "C" fn(*mut c_void)>,
        ctx: *mut c_void,
    ) {
        *WAKE.lock().unwrap() = wake.map(|w| (w, ctx as usize));
    }
    unsafe extern "C" fn take_changes(_: *mut c_void) -> u8 {
        CHANGES.swap(0, SeqCst)
    }

    /// What a committed `DynamicParams` edit does: set its pending bits and
    /// notify the signal, from a worker thread.
    fn commit(change: CatalogChange) {
        CHANGES.fetch_or(change.bits(), SeqCst);
        let (wake, ctx) = WAKE.lock().unwrap().expect("catalog waker bound");
        std::thread::spawn(move || unsafe { wake(ctx as *mut c_void) })
            .join()
            .unwrap();
    }

    #[test]
    fn catalog_wake_sends_restart_flags() {
        use RestartFlags_::*;
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                set_catalog_waker: store_waker,
                take_catalog_changes: take_changes,
                ..callbacks()
            },
            Vec::new(),
        );
        let com = component(reg);
        let recorder = attach(&com);
        let restart = |flags: u32| vec![format!("restart {flags}")];

        com.host.on_ui_tick();
        assert_eq!(
            recorder.take(),
            Vec::<String>::new(),
            "no wake, nothing to send"
        );
        #[cfg(target_os = "linux")]
        {
            let frame = ComWrapper::new(LoopFrame::new(FrameCore::new(640, 480)));
            commit(CatalogChange::INFO);
            assert_eq!(recorder.take(), Vec::<String>::new());
            com.host
                .update_run_loop_registration(Some(frame_ptr(&frame).cast()));
            let mut fd = libc::pollfd {
                fd: frame.fd.load(SeqCst),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&raw mut fd, 1, 0) }, 1);
            assert_eq!(fd.revents, libc::POLLIN);
            frame.pump();
            assert_eq!(recorder.take(), restart(kParamTitlesChanged as u32));
            com.host.update_run_loop_registration(None);
        }

        for (change, flags) in [
            (CatalogChange::VALUES, kParamValuesChanged),
            (CatalogChange::INFO, kParamTitlesChanged),
            (
                CatalogChange::STRUCTURE,
                kParamTitlesChanged | kParamValuesChanged,
            ),
        ] {
            commit(change);
            com.host.on_ui_tick();
            assert_eq!(
                recorder.take(),
                restart(flags as u32),
                "{change:?} in slot-pool mode"
            );
        }
        com.host.set_dynamic_support(DynamicSupport::Reload);
        commit(CatalogChange::STRUCTURE | CatalogChange::VALUES);
        com.host.on_ui_tick();
        assert_eq!(
            recorder.take(),
            restart((kReloadComponent | kParamValuesChanged) as u32)
        );

        drop(com);
        assert!(WAKE.lock().unwrap().is_none(), "teardown unbinds the wake");
    }

    // --- parameter list ---------------------------------------------------------

    /// Slot 11 is free until `BOUND`; `REVISION` follows it.
    static BOUND: AtomicBool = AtomicBool::new(false);
    static REVISION: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn slot_presentation(
        _: *mut c_void,
        id: u32,
        name: *mut c_char,
        capacity: u32,
    ) -> i32 {
        if id != 11 || BOUND.load(SeqCst) {
            return -1;
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(name, capacity as usize) };
        super::super::c8_into(dst, "Slot 2");
        PRESENTATION_HIDDEN | PRESENTATION_UNAVAILABLE
    }
    unsafe extern "C" fn slot_revision(_: *mut c_void) -> u64 {
        REVISION.load(SeqCst)
    }
    unsafe extern "C" fn binder_param(_: *mut c_void, id: u32) -> bool {
        id == 10
    }

    fn listed(com: &Component) -> Vec<(u32, i32)> {
        (0..unsafe { com.getParameterCount() })
            .map(|i| {
                let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
                assert_eq!(unsafe { com.getParameterInfo(i, &raw mut info) }, kResultOk);
                (info.id, info.flags)
            })
            .collect()
    }

    #[test]
    fn slot_pool_hides_and_reload_omits_free_slots() {
        use ParameterInfo_::ParameterFlags_::*;
        use RestartFlags_::*;
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                param_presentation: slot_presentation,
                param_presentation_revision: slot_revision,
                param_affects_presentation: binder_param,
                ..callbacks()
            },
            vec![param(10), param(11), param(12)],
        );
        let automate = kCanAutomate as i32;
        let hidden = (kIsHidden | kIsReadOnly) as i32;

        let pool = component(reg);
        initialize_in(&pool, "REAPER");
        assert_eq!(
            listed(&pool),
            [(10, automate), (11, hidden), (12, automate)]
        );

        let reload = component(reg);
        initialize_in(&reload, "Cubase 14");
        assert_eq!(listed(&reload), [(10, automate), (12, automate)]);
        let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { reload.getParameterInfo(2, &raw mut info) },
            kInvalidArgument
        );
        let recorder = attach(&reload);

        // A revision bump with no event behind it is not polled.
        BOUND.store(true, SeqCst);
        REVISION.store(1, SeqCst);
        reload.host.on_ui_tick();
        assert_eq!(recorder.take(), Vec::<String>::new());
        // A host write to a parameter that renames others is the event.
        assert_eq!(unsafe { reload.setParamNormalized(10, 0.5) }, kResultOk);
        reload.host.on_ui_tick();
        assert_eq!(
            recorder.take(),
            [format!(
                "restart {}",
                (kParamTitlesChanged | kReloadComponent) as u32
            )]
        );
        assert_eq!(
            listed(&reload),
            [(10, automate), (11, automate), (12, automate)]
        );
    }

    // --- function names ---------------------------------------------------------

    unsafe extern "C" fn mix_is_11(_: *mut c_void, id: u32) -> *const c_char {
        if id == 11 {
            FunctionNameType::kDryWetMix
        } else {
            std::ptr::null()
        }
    }

    #[test]
    fn function_names_find_their_parameter() {
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                param_function_name: mix_is_11,
                ..callbacks()
            },
            vec![param(10), param(11)],
        );
        let com = component(reg);
        let finder = com
            .as_com_ref::<IParameterFunctionName>()
            .expect("IParameterFunctionName");
        let mut id = 0;
        unsafe {
            assert_eq!(
                finder.getParameterIDFromFunctionName(
                    kRootUnitId,
                    c"DryWetMix".as_ptr(),
                    &raw mut id
                ),
                kResultTrue
            );
            assert_eq!(id, 11);
            assert_eq!(
                finder.getParameterIDFromFunctionName(
                    kRootUnitId,
                    c"Randomize".as_ptr(),
                    &raw mut id
                ),
                kResultFalse
            );
            assert_eq!(id, kNoParamId);
            assert_eq!(
                finder.getParameterIDFromFunctionName(5, c"DryWetMix".as_ptr(), &raw mut id),
                kResultFalse
            );
        }
    }

    #[test]
    fn remap_param_id_forwards_old_cid_and_handles_nulls() {
        unsafe extern "C" fn remap_cb(
            _: *mut c_void,
            plugin_cid: *const u8,
            old_id: u32,
            out_new: *mut u32,
        ) -> i32 {
            let mut cid = [0u8; 16];
            unsafe { std::ptr::copy_nonoverlapping(plugin_cid, cid.as_mut_ptr(), 16) };
            if cid == [0xAB; 16] && old_id == 7 {
                unsafe { *out_new = 42 };
                1
            } else {
                0
            }
        }
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                remap_param_id: remap_cb,
                ..callbacks()
            },
            vec![param(42)],
        );
        let com = component(reg);
        let remap = com.as_com_ref::<IRemapParamID>().expect("IRemapParamID");
        let cid_match: TUID = [0xABu8 as std::ffi::c_char; 16];
        let cid_other: TUID = [0x01u8 as std::ffi::c_char; 16];
        let mut new_id = 0;
        unsafe {
            assert_eq!(
                remap.getCompatibleParamID(&raw const cid_match, 7, &raw mut new_id),
                kResultOk
            );
            assert_eq!(new_id, 42);
            assert_eq!(
                remap.getCompatibleParamID(&raw const cid_other, 7, &raw mut new_id),
                kResultFalse
            );
            assert_eq!(new_id, kNoParamId);
            assert_eq!(
                remap.getCompatibleParamID(std::ptr::null(), 7, &raw mut new_id),
                kInvalidArgument
            );
            assert_eq!(
                remap.getCompatibleParamID(&raw const cid_match, 7, std::ptr::null_mut()),
                kInvalidArgument
            );
        }
    }

    // --- info listener ----------------------------------------------------------

    /// The host's channel attributes: a name and a colour.
    struct Channel;

    impl Class for Channel {
        type Interfaces = (IAttributeList,);
    }

    fn key_is(id: *const c_char, key: *const c_char) -> bool {
        unsafe { CStr::from_ptr(id) == CStr::from_ptr(key) }
    }

    impl IAttributeListTrait for Channel {
        unsafe fn setInt(&self, _: IAttributeList_::AttrID, _: int64) -> tresult {
            kNotImplemented
        }
        unsafe fn getInt(&self, id: IAttributeList_::AttrID, value: *mut int64) -> tresult {
            if !key_is(id, kChannelColorKey) {
                return kResultFalse;
            }
            unsafe { *value = 0x80FF_2010 };
            kResultOk
        }
        unsafe fn setFloat(&self, _: IAttributeList_::AttrID, _: f64) -> tresult {
            kNotImplemented
        }
        unsafe fn getFloat(&self, _: IAttributeList_::AttrID, _: *mut f64) -> tresult {
            kResultFalse
        }
        unsafe fn setString(&self, _: IAttributeList_::AttrID, _: *const TChar) -> tresult {
            kNotImplemented
        }
        unsafe fn getString(
            &self,
            id: IAttributeList_::AttrID,
            string: *mut TChar,
            bytes: uint32,
        ) -> tresult {
            if !key_is(id, kChannelNameKey) {
                return kResultFalse;
            }
            let dst = unsafe { std::slice::from_raw_parts_mut(string, bytes as usize / 2) };
            super::super::utf16_into(dst, "Lead \u{b5}");
            kResultOk
        }
        unsafe fn setBinary(
            &self,
            _: IAttributeList_::AttrID,
            _: *const c_void,
            _: uint32,
        ) -> tresult {
            kNotImplemented
        }
        unsafe fn getBinary(
            &self,
            _: IAttributeList_::AttrID,
            _: *mut *const c_void,
            _: *mut uint32,
        ) -> tresult {
            kResultFalse
        }
    }

    static TRACK: Mutex<Option<(String, u32, bool)>> = Mutex::new(None);

    unsafe extern "C" fn record_track(_: *mut c_void, info: *const Vst3TrackInfo) {
        let info = unsafe { &*info };
        let name = unsafe { CStr::from_ptr(info.name) }
            .to_str()
            .unwrap()
            .to_owned();
        *TRACK.lock().unwrap() = Some((name, info.color_argb, info.has_color));
    }

    #[test]
    fn info_listener_passes_name_and_colour() {
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                track_info_changed: record_track,
                ..callbacks()
            },
            Vec::new(),
        );
        let com = component(reg);
        let listener = com.as_com_ref::<IInfoListener>().expect("IInfoListener");
        let channel = ComWrapper::new(Channel);
        let list = channel.as_com_ref::<IAttributeList>().unwrap().as_ptr();
        assert_eq!(
            unsafe { listener.setChannelContextInfos(list) },
            kResultTrue
        );
        assert_eq!(
            *TRACK.lock().unwrap(),
            Some(("Lead \u{b5}".to_owned(), 0x80FF_2010, true))
        );
        assert_eq!(
            unsafe { listener.setChannelContextInfos(std::ptr::null_mut()) },
            kInvalidArgument
        );
    }

    // --- MIDI learn -------------------------------------------------------------

    static LEARNED: Mutex<Vec<(u32, u8, Vst3LearnSource)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn learn_cc(
        _: *mut c_void,
        bus: u32,
        channel: u8,
        source: Vst3LearnSource,
    ) -> i32 {
        LEARNED.lock().unwrap().push((bus, channel, source));
        i32::from(!matches!(source, Vst3LearnSource::Midi1(129)))
    }

    /// A `Midi2Controller` as a host passes it: the SDK's two bytes land in
    /// the crate's first two fields (see `midi2_source`).
    fn midi2(registered: bool, bank: u8, index: u8) -> Midi2Controller {
        Midi2Controller {
            bank: bank | if registered { 0x80 } else { 0 },
            registered: index,
            index: 0xAA,
            reserved: 0xAA,
        }
    }

    #[test]
    fn midi_learn_reaches_the_plugin() {
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                midi_learn: learn_cc,
                ..callbacks()
            },
            Vec::new(),
        );
        let com = component(reg);
        let learn = com.as_com_ref::<IMidiLearn>().expect("IMidiLearn");
        let learn2 = com.as_com_ref::<IMidiLearn2>().expect("IMidiLearn2");
        unsafe {
            assert_eq!(learn.onLiveMIDIControllerInput(0, 3, 74), kResultTrue);
            assert_eq!(
                learn.onLiveMIDIControllerInput(1, 0, ControllerNumbers_::kPitchBend as i16),
                kResultFalse
            );
            assert_eq!(learn.onLiveMIDIControllerInput(0, 16, 1), kInvalidArgument);
            assert_eq!(learn2.onLiveMidi1ControllerInput(2, 15, 7), kResultTrue);
            let rpn = midi2(true, 3, 7);
            assert_eq!(learn2.onLiveMidi2ControllerInput(1, 4, rpn), kResultTrue);
            let nrpn = midi2(false, 127, 127);
            assert_eq!(learn2.onLiveMidi2ControllerInput(0, 15, nrpn), kResultTrue);
            let mut bad = midi2(false, 0, 0);
            bad.registered = 0x80;
            assert_eq!(
                learn2.onLiveMidi2ControllerInput(0, 0, bad),
                kInvalidArgument,
                "index past 7 bits"
            );
            assert_eq!(
                learn2.onLiveMidi2ControllerInput(0, 16, rpn),
                kInvalidArgument
            );
        }
        let midi1 = Vst3LearnSource::Midi1;
        assert_eq!(
            *LEARNED.lock().unwrap(),
            [
                (0, 3, midi1(74)),
                (1, 0, midi1(129)),
                (2, 15, midi1(7)),
                (
                    1,
                    4,
                    Vst3LearnSource::Midi2 {
                        registered: true,
                        bank: 3,
                        index: 7
                    }
                ),
                (
                    0,
                    15,
                    Vst3LearnSource::Midi2 {
                        registered: false,
                        bank: 127,
                        index: 127
                    }
                ),
            ]
        );
    }

    // --- group edits ------------------------------------------------------------

    // macOS: the test thread isn't the main queue, so the calls would queue.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn group_edits_bracket_the_edits_in_order() {
        let reg = registration(descriptor(), callbacks(), Vec::new());
        let com = component(reg);
        let recorder = attach(&com);
        let ctx = com.host.ctx;
        crate::com::group_edit(ctx, EditGroup::Begin);
        for id in [1, 2] {
            crate::com::begin_edit(ctx, id);
            crate::com::perform_edit(ctx, id, 0.5);
            crate::com::end_edit(ctx, id);
        }
        crate::com::group_edit(ctx, EditGroup::End);
        let log = recorder.take();
        assert_eq!(
            log,
            [
                "group",
                "begin 1",
                "perform 1",
                "end 1",
                "begin 2",
                "perform 2",
                "end 2",
                "group end"
            ],
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn handler_swap_moves_an_open_group_to_the_new_handler() {
        let reg = registration(descriptor(), callbacks(), Vec::new());
        let com = component(reg);
        let old = attach(&com);
        let ctx = com.host.ctx;
        crate::com::group_edit(ctx, EditGroup::Begin);
        crate::com::begin_edit(ctx, 1);
        let new = attach(&com);
        crate::com::perform_edit(ctx, 1, 0.5);
        crate::com::end_edit(ctx, 1);
        crate::com::group_edit(ctx, EditGroup::End);
        assert_eq!(old.take(), ["group", "begin 1", "end 1", "group end"]);
        assert_eq!(
            new.take(),
            ["group", "begin 1", "perform 1", "end 1", "group end"]
        );

        // Dropping the handler finishes the group and reopens nothing.
        crate::com::group_edit(ctx, EditGroup::Begin);
        assert_eq!(
            unsafe { com.setComponentHandler(std::ptr::null_mut()) },
            kResultOk
        );
        assert_eq!(new.take(), ["group", "group end"]);
        let last = attach(&com);
        crate::com::begin_edit(ctx, 2);
        assert_eq!(last.take(), ["begin 2"]);
    }
}

// ---------------------------------------------------------------------------
// wayland_embed (Linux: IWaylandHost + IRunLoop)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod wayland_embed {
    //! `WaylandSurfaceID` views: support follows the editor, `attached` opens
    //! on the host connection, the fd handler and 16 ms timer drive the
    //! editor, and teardown runs hooks → editor → connection.

    use std::sync::Mutex;

    use vst3::Steinberg::Linux::{
        FileDescriptor, IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler,
        ITimerHandlerTrait, TimerInterval,
    };
    use vst3::wl_display;

    use super::*;

    /// The editor's fd as the fake plugin reports it.
    const EDITOR_FD: i32 = 42;

    static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static SUPPORTS: AtomicBool = AtomicBool::new(false);
    static DISPLAY_TOKEN: u8 = 0;
    static SIZE: Mutex<(u32, u32)> = Mutex::new((0, 0));

    fn log(entry: impl Into<String>) {
        LOG.lock().unwrap().push(entry.into());
    }
    fn take_log() -> Vec<String> {
        std::mem::take(&mut *LOG.lock().unwrap())
    }
    fn display() -> *mut c_void {
        (&raw const DISPLAY_TOKEN).cast_mut().cast()
    }

    unsafe extern "C" fn has_editor(_: *mut c_void) -> i32 {
        1
    }
    unsafe extern "C" fn supports(_: *mut c_void) -> i32 {
        i32::from(SUPPORTS.load(SeqCst))
    }
    unsafe extern "C" fn native_open(_: *mut c_void, _: *mut c_void) -> i32 {
        log("native open");
        1
    }
    unsafe extern "C" fn open(_: *mut c_void, display: *mut c_void, surface: *mut c_void) -> i32 {
        log(format!("open {display:p} {surface:p}"));
        1
    }
    unsafe extern "C" fn event_fd(_: *mut c_void) -> i32 {
        EDITOR_FD
    }
    unsafe extern "C" fn close(_: *mut c_void) {
        log("editor close");
    }
    unsafe extern "C" fn on_fd(_: *mut c_void) {
        log("on_fd");
    }
    unsafe extern "C" fn on_timer(_: *mut c_void) {
        log("on_timer");
    }
    unsafe extern "C" fn set_size(_: *mut c_void, w: u32, h: u32) {
        *SIZE.lock().unwrap() = (w, h);
    }

    struct WaylandHost;

    impl Class for WaylandHost {
        type Interfaces = (IWaylandHost,);
    }

    impl IWaylandHostTrait for WaylandHost {
        unsafe fn openWaylandConnection(&self) -> *mut wl_display {
            log("connection open");
            display().cast()
        }
        unsafe fn closeWaylandConnection(&self, d: *mut wl_display) -> tresult {
            assert_eq!(d.cast(), display());
            log("connection close");
            kResultOk
        }
    }

    /// An `IHostApplication` that is not itself an `IWaylandHost`, so the
    /// view must go through `createInstance`.
    struct App;

    impl Class for App {
        type Interfaces = (IHostApplication,);
    }

    impl IHostApplicationTrait for App {
        unsafe fn getName(&self, _: *mut String128) -> tresult {
            kNotImplemented
        }
        unsafe fn createInstance(
            &self,
            cid: *mut TUID,
            iid: *mut TUID,
            obj: *mut *mut c_void,
        ) -> tresult {
            let want = IWaylandHost_iid;
            if unsafe { *cid != want || *iid != want } {
                return kNoInterface;
            }
            let host = ComWrapper::new(WaylandHost)
                .to_com_ptr::<IWaylandHost>()
                .unwrap();
            unsafe { *obj = host.into_raw().cast() };
            kResultOk
        }
    }

    /// A plug frame whose run loop names every registration, and keeps the
    /// Wayland ones so the test can fire them.
    #[derive(Default)]
    struct Frame {
        handlers: Mutex<Vec<(usize, String)>>,
        fd_handler: Mutex<Option<ComPtr<IEventHandler>>>,
        timer: Mutex<Option<ComPtr<ITimerHandler>>>,
    }

    impl Frame {
        fn unregister(&self, ptr: usize) {
            let mut handlers = self.handlers.lock().unwrap();
            let at = handlers
                .iter()
                .position(|(p, _)| *p == ptr)
                .expect("registered");
            let (_, name) = handlers.remove(at);
            log(format!("unregister {name}"));
        }
    }

    impl Class for Frame {
        type Interfaces = (IPlugFrame, IRunLoop);
    }

    impl IPlugFrameTrait for Frame {
        unsafe fn resizeView(&self, _: *mut IPlugView, _: *mut ViewRect) -> tresult {
            kResultOk
        }
    }

    impl IRunLoopTrait for Frame {
        unsafe fn registerEventHandler(
            &self,
            handler: *mut IEventHandler,
            fd: FileDescriptor,
        ) -> tresult {
            let name = format!("fd {}", if fd == EDITOR_FD { "editor" } else { "wake" });
            if fd == EDITOR_FD {
                *self.fd_handler.lock().unwrap() = unsafe { super::super::retain(handler) };
                log(format!("register {name}"));
            }
            self.handlers.lock().unwrap().push((handler as usize, name));
            kResultOk
        }
        unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
            self.unregister(handler as usize);
            kResultOk
        }
        unsafe fn registerTimer(&self, handler: *mut ITimerHandler, ms: TimerInterval) -> tresult {
            let name = format!("timer {ms}");
            if ms == 16 {
                *self.timer.lock().unwrap() = unsafe { super::super::retain(handler) };
                log(format!("register {name}"));
            }
            self.handlers.lock().unwrap().push((handler as usize, name));
            kResultOk
        }
        unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
            self.unregister(handler as usize);
            kResultOk
        }
    }

    struct WaylandFrame {
        inner: Frame,
        surface: *mut vst3::wl_surface,
    }
    unsafe impl Send for WaylandFrame {}
    unsafe impl Sync for WaylandFrame {}
    impl Class for WaylandFrame {
        type Interfaces = (IPlugFrame, IRunLoop, IWaylandFrame);
    }
    impl IPlugFrameTrait for WaylandFrame {
        unsafe fn resizeView(&self, view: *mut IPlugView, size: *mut ViewRect) -> tresult {
            unsafe { self.inner.resizeView(view, size) }
        }
    }
    impl IRunLoopTrait for WaylandFrame {
        unsafe fn registerEventHandler(
            &self,
            handler: *mut IEventHandler,
            fd: FileDescriptor,
        ) -> tresult {
            unsafe { self.inner.registerEventHandler(handler, fd) }
        }
        unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
            unsafe { self.inner.unregisterEventHandler(handler) }
        }
        unsafe fn registerTimer(&self, handler: *mut ITimerHandler, ms: TimerInterval) -> tresult {
            unsafe { self.inner.registerTimer(handler, ms) }
        }
        unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
            unsafe { self.inner.unregisterTimer(handler) }
        }
    }
    impl IWaylandFrameTrait for WaylandFrame {
        unsafe fn getWaylandSurface(&self, d: *mut wl_display) -> *mut vst3::wl_surface {
            assert_eq!(d.cast(), display());
            log("frame getWaylandSurface");
            self.surface
        }
        unsafe fn getParentSurface(
            &self,
            _: *mut ViewRect,
            _: *mut wl_display,
        ) -> *mut vst3::xdg_surface {
            std::ptr::null_mut()
        }
        unsafe fn getParentToplevel(&self, _: *mut wl_display) -> *mut vst3::xdg_toplevel {
            std::ptr::null_mut()
        }
    }

    /// Only the Wayland view's entries: the instance's own edit wake
    /// (`fd wake`, `timer 30`) shares the run loop.
    fn wayland_log() -> Vec<String> {
        take_log()
            .into_iter()
            .filter(|e| !e.ends_with("wake") && !e.ends_with("timer 30"))
            .collect()
    }

    unsafe extern "C" fn set_sync(_: *mut c_void, sync: i32) {
        log(format!("sync {sync}"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn wayland_surface_embedding() {
        let reg = registration(
            descriptor(),
            Vst3Callbacks {
                gui_has_editor: has_editor,
                gui_open: native_open,
                gui_close: close,
                gui_set_size: set_size,
                gui_supports_wayland: supports,
                gui_open_wayland: open,
                gui_event_fd: event_fd,
                gui_on_fd: on_fd,
                gui_on_timer: on_timer,
                gui_wayland_set_sync: set_sync,
                ..callbacks()
            },
            Vec::new(),
        );
        let com = component(reg);
        let app = ComWrapper::new(App);
        let context = app.as_com_ref::<FUnknown>().unwrap().as_ptr();
        assert_eq!(unsafe { com.initialize(context) }, kResultOk);
        let frame = ComWrapper::new(Frame::default());
        let frame_ptr = frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr();
        let mut surface_token = 0u8;
        let surface: *mut c_void = (&raw mut surface_token).cast();
        let wayland = unsafe { CStr::from_ptr(kPlatformTypeWaylandSurfaceID) };
        let opened = format!("open {:p} {surface:p}", display());
        let teardown = [
            "unregister fd editor",
            "unregister timer 16",
            "editor close",
            "connection close",
        ];

        let new_view = || {
            let view =
                unsafe { ComPtr::from_raw(com.createView(c"editor".as_ptr())) }.expect("view");
            assert_eq!(unsafe { view.setFrame(frame_ptr) }, kResultOk);
            view
        };
        let view = new_view();
        unsafe {
            // The editor decides; the native type is unaffected.
            SUPPORTS.store(false, SeqCst);
            assert_eq!(view.isPlatformTypeSupported(wayland.as_ptr()), kResultFalse);
            assert_eq!(view.attached(surface, wayland.as_ptr()), kResultFalse);
            assert_eq!(view.isPlatformTypeSupported(PLATFORM.as_ptr()), kResultOk);
            SUPPORTS.store(true, SeqCst);
            assert_eq!(view.isPlatformTypeSupported(wayland.as_ptr()), kResultOk);
            assert_eq!(
                view.attached(std::ptr::null_mut(), wayland.as_ptr()),
                kResultFalse
            );
            assert_eq!(wayland_log(), Vec::<String>::new());

            assert_eq!(view.attached(surface, wayland.as_ptr()), kResultOk);
            assert_eq!(
                wayland_log(),
                [
                    "connection open",
                    &opened,
                    "register timer 16",
                    "register fd editor"
                ],
            );
            // A second attach keeps the first embedding.
            assert_eq!(view.attached(surface, wayland.as_ptr()), kResultFalse);

            // The host's loop drives the editor through both hooks.
            let fd_handler = frame.fd_handler.lock().unwrap().clone().unwrap();
            fd_handler.onFDIsSet(EDITOR_FD);
            let timer = frame.timer.lock().unwrap().clone().unwrap();
            timer.onTimer();
            assert_eq!(wayland_log(), ["on_fd", "on_timer"]);

            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: 640,
                bottom: 480,
            };
            take_log();
            assert_eq!(view.onSize(&raw mut rect), kResultOk);
            assert_eq!(*SIZE.lock().unwrap(), (640, 480));
            assert_eq!(wayland_log(), ["sync 1", "on_fd", "sync 0"]);

            // Wayland HiDPI comes from the compositor, not VST3 `setContentScaleFactor`.
            let scale_ext = view
                .cast::<IPlugViewContentScaleSupport>()
                .expect("IPlugViewContentScaleSupport");
            assert_eq!(scale_ext.setContentScaleFactor(2.0), kResultFalse);

            assert_eq!(view.removed(), kResultOk);
            assert_eq!(wayland_log(), teardown);
            assert_eq!(view.removed(), kResultOk);
            assert_eq!(wayland_log(), Vec::<String>::new());
        }
        drop(view);
        take_log();

        // A view released while still attached tears down in the same order.
        let view = new_view();
        assert_eq!(
            unsafe { view.attached(surface, wayland.as_ptr()) },
            kResultOk
        );
        take_log();
        drop(view);
        assert_eq!(wayland_log(), teardown);
        assert!(frame.handlers.lock().unwrap().is_empty());

        // VST3 3.8 `IWaylandFrame::getWaylandSurface`: attached(nullptr, "WaylandSurfaceID")
        // resolves the parent surface from the plug frame, and two concurrent views on the
        // same host context share one `openWaylandConnection` / `closeWaylandConnection` pair.
        let mut frame_surface_token = 0u8;
        let frame_surface: *mut c_void = (&raw mut frame_surface_token).cast();
        let wframe = ComWrapper::new(WaylandFrame {
            inner: Frame::default(),
            surface: frame_surface.cast(),
        });
        let wframe_ptr = wframe.as_com_ref::<IPlugFrame>().unwrap().as_ptr();
        let opened_from_frame = format!("open {:p} {frame_surface:p}", display());

        let com2 = component(registration(
            descriptor(),
            Vst3Callbacks {
                gui_has_editor: has_editor,
                gui_open: native_open,
                gui_close: close,
                gui_set_size: set_size,
                gui_supports_wayland: supports,
                gui_open_wayland: open,
                gui_event_fd: event_fd,
                gui_on_fd: on_fd,
                gui_on_timer: on_timer,
                ..callbacks()
            },
            Vec::new(),
        ));
        assert_eq!(unsafe { com2.initialize(context) }, kResultOk);

        let v1 = unsafe { ComPtr::from_raw(com.createView(c"editor".as_ptr())) }.expect("v1");
        let v2 = unsafe { ComPtr::from_raw(com2.createView(c"editor".as_ptr())) }.expect("v2");
        assert_eq!(unsafe { v1.setFrame(wframe_ptr) }, kResultOk);
        assert_eq!(unsafe { v2.setFrame(wframe_ptr) }, kResultOk);
        take_log();

        assert_eq!(
            unsafe { v1.attached(std::ptr::null_mut(), wayland.as_ptr()) },
            kResultOk
        );
        assert_eq!(
            wayland_log(),
            [
                "connection open",
                "frame getWaylandSurface",
                &opened_from_frame,
                "register timer 16",
                "register fd editor",
            ]
        );
        // Second view on the same host context reuses the open connection.
        assert_eq!(
            unsafe { v2.attached(std::ptr::null_mut(), wayland.as_ptr()) },
            kResultOk
        );
        assert_eq!(
            wayland_log(),
            [
                "frame getWaylandSurface",
                &opened_from_frame,
                "register timer 16",
                "register fd editor",
            ]
        );
        // Closing the first view keeps the shared connection open.
        drop(v1);
        assert_eq!(
            wayland_log(),
            [
                "unregister fd editor",
                "unregister timer 16",
                "editor close"
            ]
        );
        // Closing the last view closes the shared host connection once.
        drop(v2);
        assert_eq!(wayland_log(), teardown);
        assert!(wframe.inner.handlers.lock().unwrap().is_empty());

        // Synchronous `onSize` during `gui_open_wayland` (permitted by VST3 `attached`)
        // must not deadlock on `View::wayland` and still brackets Wayland sync.
        {
            static REENTRANT_VIEW: AtomicUsize = AtomicUsize::new(0);
            unsafe extern "C" fn open_with_reentrant_resize(
                _ctx: *mut c_void,
                _d: *mut c_void,
                _s: *mut c_void,
            ) -> i32 {
                let ptr = REENTRANT_VIEW.load(SeqCst) as *mut IPlugView;
                if !ptr.is_null() {
                    let mut rect = ViewRect {
                        left: 0,
                        top: 0,
                        right: 512,
                        bottom: 384,
                    };
                    unsafe { ((*(*ptr).vtbl).onSize)(ptr, &raw mut rect) };
                }
                1
            }
            let com3 = component(registration(
                descriptor(),
                Vst3Callbacks {
                    gui_has_editor: has_editor,
                    gui_open: native_open,
                    gui_close: close,
                    gui_set_size: set_size,
                    gui_supports_wayland: supports,
                    gui_open_wayland: open_with_reentrant_resize,
                    gui_event_fd: event_fd,
                    gui_on_fd: on_fd,
                    gui_on_timer: on_timer,
                    gui_wayland_set_sync: set_sync,
                    ..callbacks()
                },
                Vec::new(),
            ));
            assert_eq!(unsafe { com3.initialize(context) }, kResultOk);
            let v3 = unsafe { ComPtr::from_raw(com3.createView(c"editor".as_ptr())) }.expect("v3");
            assert_eq!(unsafe { v3.setFrame(wframe_ptr) }, kResultOk);
            REENTRANT_VIEW.store(v3.as_ptr() as usize, SeqCst);
            take_log();
            assert_eq!(
                unsafe { v3.attached(std::ptr::null_mut(), wayland.as_ptr()) },
                kResultOk
            );
            REENTRANT_VIEW.store(0, SeqCst);
            assert_eq!(*SIZE.lock().unwrap(), (512, 384));
            assert!(wayland_log().contains(&"sync 1".to_string()));
            drop(v3);
            take_log();
        }
    }
}

// ---------------------------------------------------------------------------
// input_merge: notes + parameter queues reach the plugin in offset order
// ---------------------------------------------------------------------------

mod input_merge {
    use super::*;
    use crate::input_events::{
        cb_begin_input_events, cb_push_input_event, cb_push_param_change, input_event_typed_limit,
    };
    use crate::instance::Vst3Instance;
    use crate::lifecycle::{cb_create, cb_destroy, cb_prepare};
    use crate::params::{cb_param_denormalize, cb_param_get_value, cb_param_set_value};
    use moose_core::buffer::AudioBuffer;
    use moose_core::config::AudioConfig;
    use moose_core::events::{EventBody, EventList, PushError};
    use moose_core::export::PluginExport;
    use moose_core::info::PluginInfo;
    use moose_core::meters::MeterStore;
    use moose_core::plugin::PluginRuntime;
    use moose_core::process::{ProcessContext, ProcessStatus};
    use moose_core::snapshot::SnapshotSlot;
    use moose_test_fixtures::host_value_fixture::{TestParams, TestPlugin};
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    // -- Allocation counter: counts this thread's heap traffic while armed.

    struct CountingAlloc;
    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static HEAP_OPS: Cell<u32> = const { Cell::new(0) };
    }
    fn note_heap_op() {
        let _ = ARMED.try_with(|armed| {
            if armed.get() {
                let _ = HEAP_OPS.try_with(|n| n.set(n.get() + 1));
            }
        });
    }
    // SAFETY: forwards every call to `System` unchanged.
    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note_heap_op();
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            note_heap_op();
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note_heap_op();
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
    #[global_allocator]
    static ALLOC: CountingAlloc = CountingAlloc;

    /// Heap allocations + frees made on this thread inside `f`.
    fn heap_ops<R>(f: impl FnOnce() -> R) -> (R, u32) {
        HEAP_OPS.with(|n| n.set(0));
        ARMED.with(|a| a.set(true));
        let r = f();
        ARMED.with(|a| a.set(false));
        (r, HEAP_OPS.with(Cell::get))
    }

    // -- A MIDI-in plugin on the real VST3 callbacks.

    struct Probe {
        inner: TestPlugin,
    }
    impl PluginExport for Probe {
        type Params = TestParams;
        fn create() -> Self {
            Self {
                inner: TestPlugin::create(),
            }
        }
        fn params(&self) -> &TestParams {
            self.inner.params()
        }
        fn params_arc(&self) -> Arc<TestParams> {
            self.inner.params_arc()
        }
        fn meter_store(&self) -> Arc<MeterStore> {
            self.inner.meter_store()
        }
        fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
            self.inner.snapshot_slot()
        }
    }
    impl PluginRuntime for Probe {
        type Sample = f32;
        fn info() -> PluginInfo {
            let mut info = TestPlugin::info();
            info.accepts_midi_in = true;
            info.midi_input_ports = 1;
            info
        }
        fn reset(&mut self, _: &AudioConfig) {}
        fn process(
            &mut self,
            _: &mut AudioBuffer,
            events: &EventList,
            _: &mut ProcessContext,
        ) -> ProcessStatus {
            DELIVERED.with(|seen| {
                let mut seen = seen.borrow_mut();
                seen.clear();
                seen.extend(events.iter().map(|event| event.sample_offset));
            });
            ProcessStatus::Normal
        }
    }

    /// What the plugin's event list held when `process` began, before the
    /// adapter's `ensure_sorted_by_offset`.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Note(u32, u8),
        Param(u32, u32),
    }
    struct Snapshot {
        sorted: bool,
        overflow: Option<PushError>,
        capacity: usize,
        events: Vec<Seen>,
        exact_offsets: Vec<u32>,
    }
    thread_local! {
        static LAST_READ: Cell<i32> = const { Cell::new(-1) };
        static DELIVERED: RefCell<Vec<u32>> = RefCell::new(Vec::with_capacity(1024));
        static SNAPSHOT: RefCell<Snapshot> = RefCell::new(Snapshot {
            sorted: false,
            overflow: None,
            capacity: 0,
            events: Vec::with_capacity(1024),
            exact_offsets: Vec::with_capacity(1024),
        });
    }

    /// Records the ingested list without allocating (the snapshot vector is
    /// preallocated), so the heap counter sees only the adapter's work.
    #[allow(clippy::too_many_arguments)]
    unsafe extern "C" fn snapshot_process(
        ctx: *mut c_void,
        inputs: *const *const c_void,
        outputs: *mut *mut c_void,
        num_inputs: u32,
        num_outputs: u32,
        input_channels: *const u32,
        input_buses: u32,
        input_active: u32,
        output_channels: *const u32,
        output_buses: u32,
        output_active: u32,
        frames: u32,
        transport: *const Vst3Transport,
        mode: i32,
    ) -> u32 {
        let inst = unsafe { &*ctx.cast::<Vst3Instance<Probe>>() };
        let audio = inst.audio.enter();
        let list = &audio.event_list;
        SNAPSHOT.with(|s| {
            let mut s = s.borrow_mut();
            s.sorted = list.is_sorted_by_offset();
            s.overflow = list.overflow();
            s.capacity = list.capacity();
            s.exact_offsets.clear();
            s.exact_offsets.extend(
                list.exact_iter()
                    .map(moose_core::ExactEventRef::sample_offset),
            );
            s.events.clear();
            for event in list.iter() {
                s.events.push(match event.body {
                    EventBody::NoteOn { note, .. } => Seen::Note(event.sample_offset, note),
                    EventBody::ParamChange { id, .. } => Seen::Param(event.sample_offset, id),
                    ref other => panic!("unexpected event {other:?}"),
                });
            }
        });
        drop(audio);
        unsafe {
            crate::process::cb_process::<Probe>(
                ctx,
                inputs,
                outputs,
                num_inputs,
                num_outputs,
                input_channels,
                input_buses,
                input_active,
                output_channels,
                output_buses,
                output_active,
                frames,
                transport,
                mode,
            )
        }
    }

    fn plugin_registration() -> &'static Registration {
        let mut desc = descriptor();
        desc.midi_input_ports = 1;
        registration(
            desc,
            Vst3Callbacks {
                create: cb_create::<Probe>,
                destroy: cb_destroy::<Probe>,
                prepare: cb_prepare::<Probe>,
                process: snapshot_process,
                begin_input_events: cb_begin_input_events::<Probe>,
                push_input_event: cb_push_input_event::<Probe>,
                push_param_change: cb_push_param_change::<Probe>,
                param_get_value: cb_param_get_value::<Probe>,
                param_set_value: cb_param_set_value::<Probe>,
                param_denormalize: cb_param_denormalize::<Probe>,
                ..callbacks()
            },
            Vec::new(),
        )
    }

    // -- Fake host objects.

    struct Events(Vec<Event>);
    impl Class for Events {
        type Interfaces = (IEventList,);
    }
    impl IEventListTrait for Events {
        unsafe fn getEventCount(&self) -> i32 {
            self.0.len() as i32
        }
        unsafe fn getEvent(&self, index: i32, out: *mut Event) -> tresult {
            LAST_READ.with(|last| last.set(index));
            match self.0.get(index as usize) {
                Some(event) => {
                    unsafe { out.write(*event) };
                    kResultOk
                }
                None => kResultFalse,
            }
        }
        unsafe fn addEvent(&self, _: *mut Event) -> tresult {
            kResultFalse
        }
    }

    fn note_on(offset: i32, pitch: i16) -> Event {
        // SAFETY: a plain C struct/union; every field is set below.
        let mut event: Event = unsafe { std::mem::zeroed() };
        event.sampleOffset = offset;
        event.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
        event.__field0.noteOn = NoteOnEvent {
            channel: 0,
            pitch,
            tuning: 0.0,
            velocity: 1.0,
            length: 0,
            noteId: -1,
        };
        event
    }

    struct Queue {
        id: u32,
        points: Vec<(i32, f64)>,
    }
    impl Class for Queue {
        type Interfaces = (IParamValueQueue,);
    }
    impl IParamValueQueueTrait for Queue {
        unsafe fn getParameterId(&self) -> ParamID {
            self.id
        }
        unsafe fn getPointCount(&self) -> i32 {
            self.points.len() as i32
        }
        unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut f64) -> tresult {
            let Some(&(o, v)) = self.points.get(index as usize) else {
                return kResultFalse;
            };
            unsafe {
                offset.write(o);
                value.write(v);
            }
            kResultOk
        }
        unsafe fn addPoint(&self, _: i32, _: f64, _: *mut i32) -> tresult {
            kResultFalse
        }
    }

    struct Changes(Vec<ComWrapper<Queue>>);
    impl Class for Changes {
        type Interfaces = (IParameterChanges,);
    }
    impl IParameterChangesTrait for Changes {
        unsafe fn getParameterCount(&self) -> i32 {
            self.0.len() as i32
        }
        unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
            self.0
                .get(index as usize)
                .and_then(ComWrapper::as_com_ref::<IParamValueQueue>)
                .map_or(std::ptr::null_mut(), |q| q.as_ptr())
        }
        unsafe fn addParameterData(&self, _: *const ParamID, _: *mut i32) -> *mut IParamValueQueue {
            std::ptr::null_mut()
        }
    }

    fn changes(queues: &[(u32, &[(i32, f64)])]) -> ComWrapper<Changes> {
        ComWrapper::new(Changes(
            queues
                .iter()
                .map(|&(id, points)| {
                    ComWrapper::new(Queue {
                        id,
                        points: points.to_vec(),
                    })
                })
                .collect(),
        ))
    }

    /// Run one block through the COM `process`; returns the snapshot and
    /// the heap operations the adapter performed.
    fn run_block(
        events: Vec<Event>,
        queues: &[(u32, &[(i32, f64)])],
        frames: i32,
    ) -> (Snapshot, u32) {
        let com = component(plugin_registration());
        assert_eq!(unsafe { com.setActive(1) }, kResultOk);
        let events = ComWrapper::new(Events(events));
        let changes = changes(queues);
        let mut data: ProcessData = unsafe { std::mem::zeroed() };
        data.numSamples = frames;
        data.inputEvents = events.as_com_ref::<IEventList>().unwrap().as_ptr();
        data.inputParameterChanges = changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr();
        // Warm-up block: any lazy first-use initialisation happens here.
        assert_eq!(unsafe { com.process(&raw mut data) }, kResultOk);
        let (result, ops) = heap_ops(|| unsafe { com.process(&raw mut data) });
        assert_eq!(result, kResultOk);
        let snapshot = SNAPSHOT.with(|s| {
            let mut s = s.borrow_mut();
            Snapshot {
                sorted: s.sorted,
                overflow: s.overflow,
                capacity: s.capacity,
                events: std::mem::take(&mut s.events),
                exact_offsets: s.exact_offsets.clone(),
            }
        });
        SNAPSHOT.with(|s| s.borrow_mut().events.reserve(1024));
        (snapshot, ops)
    }

    #[test]
    fn notes_and_queues_interleave_on_the_sorted_fast_path() {
        let notes = vec![
            note_on(0, 60),
            note_on(10, 61),
            note_on(20, 62),
            note_on(40, 63),
            note_on(40, 64),
            note_on(63, 65),
        ];
        let gain: &[(i32, f64)] = &[(5, 0.1), (20, 0.5), (40, 0.9)];
        let slot: &[(i32, f64)] = &[(0, 0.2), (20, 0.3), (63, 0.4)];
        let (seen, heap) = run_block(notes, &[(0, gain), (1, slot)], 64);

        assert!(
            seen.sorted,
            "a merged block must not need EventList's sort pass"
        );
        // Ties: notes first, then queues in host order.
        assert_eq!(
            seen.events,
            vec![
                Seen::Note(0, 60),
                Seen::Param(0, 1),
                Seen::Param(5, 0),
                Seen::Note(10, 61),
                Seen::Note(20, 62),
                Seen::Param(20, 0),
                Seen::Param(20, 1),
                Seen::Note(40, 63),
                Seen::Note(40, 64),
                Seen::Param(40, 0),
                Seen::Note(63, 65),
                Seen::Param(63, 1),
            ]
        );
        assert_eq!(heap, 0, "ingestion must not touch the heap");
    }

    #[test]
    fn note_burst_cannot_starve_automation() {
        // One note per sample, far more than the event list holds.
        let notes: Vec<Event> = (0..400).map(|i| note_on(i, (i % 128) as i16)).collect();
        let gain: &[(i32, f64)] = &[(100, 0.1), (300, 0.5), (450, 0.9)];
        let slot: &[(i32, f64)] = &[(0, 0.2), (250, 0.3), (511, 0.4)];
        let (seen, heap) = run_block(notes, &[(0, gain), (1, slot)], 512);

        let params: Vec<&Seen> = seen
            .events
            .iter()
            .filter(|e| matches!(e, Seen::Param(..)))
            .collect();
        assert_eq!(
            params,
            [
                &Seen::Param(0, 1),
                &Seen::Param(100, 0),
                &Seen::Param(250, 1),
                &Seen::Param(300, 0),
                &Seen::Param(450, 0),
                &Seen::Param(511, 1),
            ],
            "every automation point survives the note burst"
        );
        // Notes stop once the typed lane reaches the input-event limit; the
        // two points before that cutoff (offsets 0 and 100) count toward it.
        let notes = seen.events.len() - params.len();
        assert_eq!(notes + 2, input_event_typed_limit(seen.capacity));
        assert_eq!(
            seen.overflow,
            Some(PushError::EventFull),
            "dropped notes are reported, not silent"
        );
        assert!(seen.sorted);
        assert_eq!(heap, 0, "ingestion must not touch the heap");
    }

    #[test]
    fn reserve_refusal_keeps_later_exact_only_note_offs() {
        let mut notes: Vec<Event> = (0..400).map(|i| note_on(i, 60)).collect();
        notes[0].__field0.noteOn.noteId = 7;
        let mut off = note_on(450, 60);
        off.r#type = Event_::EventTypes_::kNoteOffEvent as u16;
        off.__field0.noteOff = NoteOffEvent {
            channel: 0,
            pitch: 60,
            tuning: 0.0,
            velocity: 1.0,
            noteId: 7,
        };
        notes.push(off);
        let (seen, heap) = run_block(notes, &[], 512);
        assert_eq!(seen.overflow, Some(PushError::EventFull));
        assert_eq!(seen.events.len(), input_event_typed_limit(seen.capacity));
        assert_eq!(seen.exact_offsets.len(), seen.events.len() + 2);
        assert_eq!(seen.exact_offsets.first(), Some(&0));
        assert_eq!(seen.exact_offsets.last(), Some(&450));
        assert!(
            !seen.exact_offsets.contains(&399),
            "refused pairs stay atomic"
        );
        assert_eq!(heap, 0);
    }

    #[test]
    fn exact_exhaustion_stops_the_host_note_prefix() {
        let notes: Vec<Event> = (0..500)
            .map(|i| {
                let mut note = note_on(i, 60);
                note.__field0.noteOn.noteId = i;
                note
            })
            .collect();
        let (seen, heap) = run_block(notes, &[], 512);
        assert_eq!(seen.events, []);
        assert_eq!(seen.exact_offsets.len(), seen.capacity);
        assert_eq!(seen.overflow, Some(PushError::ExactEventFull));
        LAST_READ.with(|last| assert_eq!(last.get() as usize, seen.capacity));
        assert_eq!(heap, 0);
    }

    #[test]
    fn heap_counter_sees_allocations() {
        let (_, ops) = heap_ops(|| std::hint::black_box(Vec::<u8>::with_capacity(64)));
        assert!(
            ops >= 1,
            "the zero-allocation assertions above must be live"
        );
    }

    #[test]
    fn unsorted_host_notes_still_reach_the_plugin_in_order() {
        // A host that breaks the sorted-`IEventList` rule: the merge keeps
        // host order and the adapter's sort pass repairs it.
        let notes = vec![note_on(30, 60), note_on(10, 61)];
        let gain: &[(i32, f64)] = &[(20, 0.5)];
        let (seen, _) = run_block(notes, &[(0, gain)], 64);
        assert!(!seen.sorted);
        assert_eq!(seen.events.len(), 3);
        DELIVERED.with(|events| assert_eq!(*events.borrow(), [10, 20, 30]));
    }
}

/// Regression for the VST3 COM teardown wake hazard: a background thread
/// inside `HostQueue`'s wake must never pin `Arc<Host>`, drop the last `Host`,
/// and re-enter `Host::drop` -> `HostQueue::close()` (which would deadlock
/// waiting for itself).
#[test]
fn host_queue_wake_during_com_teardown_never_deadlocks_or_destroys_off_thread() {
    use moose_core::host_queue::{HostQueue, HostSignal};
    use std::sync::atomic::{AtomicPtr, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    struct Ctx {
        queue: Arc<HostQueue>,
        target: Arc<AtomicPtr<c_void>>,
    }

    static ACTIVE_QUEUE: Mutex<Option<Arc<HostQueue>>> = Mutex::new(None);
    static WAKE_ENTERED: Mutex<Option<Arc<Barrier>>> = Mutex::new(None);
    static DESTROY_THREAD: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);
    static WAKE_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn create_ctx() -> *mut c_void {
        let target = Arc::new(AtomicPtr::<c_void>::new(std::ptr::null_mut()));
        let target_wake = Arc::clone(&target);
        let queue = Arc::new(HostQueue::new(move || {
            let host = target_wake.load(Ordering::Acquire);
            if !host.is_null() {
                let first = WAKE_COUNT.fetch_add(1, Ordering::AcqRel) == 0;
                if first && let Some(barrier) = WAKE_ENTERED.lock().unwrap().clone() {
                    barrier.wait();
                    // Hold the wake open briefly while the main thread drops `Host`.
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                // SAFETY: `bind_host_queue(ctx, null)` closes the queue before
                // `Host::drop` frees `host`.
                unsafe { crate::ffi::moose_vst3_wake_host_queue(host) };
            }
        }));
        *ACTIVE_QUEUE.lock().unwrap() = Some(Arc::clone(&queue));
        Box::into_raw(Box::new(Ctx { queue, target })).cast()
    }

    unsafe extern "C" fn bind_ctx(ctx: *mut c_void, host: *mut c_void) {
        let state = unsafe { &*ctx.cast::<Ctx>() };
        state.target.store(host, Ordering::Release);
        if host.is_null() {
            state.queue.close();
        }
    }

    unsafe extern "C" fn destroy_ctx(ctx: *mut c_void) {
        *DESTROY_THREAD.lock().unwrap() = Some(std::thread::current().id());
        let state = unsafe { Box::from_raw(ctx.cast::<Ctx>()) };
        state.queue.close();
        drop(state);
    }

    let barrier = Arc::new(Barrier::new(2));
    *WAKE_ENTERED.lock().unwrap() = Some(Arc::clone(&barrier));
    *DESTROY_THREAD.lock().unwrap() = None;
    WAKE_COUNT.store(0, Ordering::Release);

    let mut cb = callbacks();
    cb.create = create_ctx;
    cb.bind_host_queue = bind_ctx;
    cb.destroy = destroy_ctx;
    let reg = registration(descriptor(), cb, Vec::new());
    let host = Host::new(reg, None).expect("host");
    let queue = ACTIVE_QUEUE
        .lock()
        .unwrap()
        .take()
        .expect("queue registered");

    let worker_queue = Arc::clone(&queue);
    let worker = std::thread::spawn(move || {
        worker_queue.signal(HostSignal::MARK_DIRTY);
        // After teardown completes, further signals on a retained queue must
        // stay harmless no-ops.
        for _ in 0..32 {
            worker_queue.signal(HostSignal::RESTART);
        }
    });

    // Wait until the worker is actively inside the `HostQueue` wake closure,
    // then drop the sole COM `Host` reference on the main thread.
    barrier.wait();
    let main_id = std::thread::current().id();
    drop(host);
    worker.join().expect("worker joined without deadlocking");

    assert!(WAKE_COUNT.load(Ordering::Acquire) >= 1);
    assert!(queue.is_closed());
    assert_eq!(
        *DESTROY_THREAD.lock().unwrap(),
        Some(main_id),
        "cb_destroy must run on the dropping thread, never inside the worker wake"
    );
}

/// Regression: a `MARK_DIRTY` drained during `Host::new` (before
/// `setComponentHandler` installs a handler) must remain pending and deliver
/// `IComponentHandler2::setDirty(1)` once a handler is attached (and only once).
#[test]
fn construction_time_mark_dirty_delivers_on_set_component_handler() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use vst3::Steinberg::Vst::{
        IComponentHandler, IComponentHandler2, IComponentHandler2Trait, IComponentHandlerTrait,
        IEditControllerTrait, ParamID, ParamValue,
    };

    struct DirtyHandler {
        dirty_calls: Arc<AtomicUsize>,
    }
    impl Class for DirtyHandler {
        type Interfaces = (IComponentHandler, IComponentHandler2);
    }
    impl IComponentHandlerTrait for DirtyHandler {
        unsafe fn beginEdit(&self, _: ParamID) -> tresult {
            kResultOk
        }
        unsafe fn performEdit(&self, _: ParamID, _: ParamValue) -> tresult {
            kResultOk
        }
        unsafe fn endEdit(&self, _: ParamID) -> tresult {
            kResultOk
        }
        unsafe fn restartComponent(&self, _: int32) -> tresult {
            kResultOk
        }
    }
    impl IComponentHandler2Trait for DirtyHandler {
        unsafe fn setDirty(&self, state: vst3::Steinberg::TBool) -> tresult {
            if state != 0 {
                self.dirty_calls.fetch_add(1, Ordering::AcqRel);
            }
            kResultOk
        }
        unsafe fn requestOpenEditor(&self, _: vst3::Steinberg::FIDString) -> tresult {
            kResultOk
        }
        unsafe fn startGroupEdit(&self) -> tresult {
            kResultOk
        }
        unsafe fn finishGroupEdit(&self) -> tresult {
            kResultOk
        }
    }

    static EMITTED: AtomicBool = AtomicBool::new(false);
    unsafe extern "C" fn drain_marking_dirty(ctx: *mut c_void) {
        if !EMITTED.swap(true, Ordering::AcqRel) {
            crate::ffi::moose_vst3_mark_dirty(ctx);
        }
    }

    EMITTED.store(false, Ordering::Release);
    let mut cb = callbacks();
    cb.drain_host_queue = drain_marking_dirty;
    let reg = registration(descriptor(), cb, Vec::new());
    let com = component(reg);
    let c: &Component = &com;

    // `Host::new` already drained `MARK_DIRTY` with no handler installed.
    assert!(EMITTED.load(Ordering::Acquire));

    let dirty_calls = Arc::new(AtomicUsize::new(0));
    let h = ComWrapper::new(DirtyHandler {
        dirty_calls: Arc::clone(&dirty_calls),
    })
    .to_com_ptr::<IComponentHandler>()
    .unwrap();
    assert_eq!(unsafe { c.setComponentHandler(h.as_ptr()) }, kResultOk);
    assert_eq!(
        dirty_calls.load(Ordering::Acquire),
        1,
        "construction-time MARK_DIRTY must reach setDirty(1) on setComponentHandler"
    );

    // Re-attaching a handler must not replay an already-delivered dirty bit.
    assert_eq!(unsafe { c.setComponentHandler(h.as_ptr()) }, kResultOk);
    assert_eq!(dirty_calls.load(Ordering::Acquire), 1);
}

#[test]
fn parameter_finder_and_audio_presentation_latency_interfaces() {
    use std::sync::Mutex;

    static LATENCY_CALLS: Mutex<Vec<(i32, u32, u32)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn has_editor(_: *mut c_void) -> i32 {
        1
    }
    unsafe extern "C" fn fake_param_at(_: *mut c_void, x: i32, y: i32, out_id: *mut u32) -> i32 {
        if x == 120 && y == 80 {
            unsafe { *out_id = 42 };
            1
        } else {
            0
        }
    }
    unsafe extern "C" fn record_pres_latency(
        _: *mut c_void,
        is_input: i32,
        bus_index: u32,
        latency: u32,
    ) {
        LATENCY_CALLS
            .lock()
            .unwrap()
            .push((is_input, bus_index, latency));
    }

    LATENCY_CALLS.lock().unwrap().clear();
    let mut cb = callbacks();
    cb.gui_has_editor = has_editor;
    cb.gui_param_at = fake_param_at;
    cb.set_audio_presentation_latency = record_pres_latency;
    let mut desc = descriptor();
    desc.num_input_buses = 2;
    desc.num_output_buses = 1;
    let reg = registration(desc, cb, Vec::new());
    let host = Host::new(reg, None).expect("host");

    // IAudioPresentationLatency on Component
    let comp = ComWrapper::new(Component { host: host.clone() });
    let pres = comp
        .to_com_ptr::<IAudioPresentationLatency>()
        .expect("Component exposes IAudioPresentationLatency");
    assert_eq!(
        unsafe { pres.setAudioPresentationLatencySamples(BusDirections_::kOutput as i32, 0, 512) },
        kResultOk
    );
    assert_eq!(
        unsafe { pres.setAudioPresentationLatencySamples(BusDirections_::kInput as i32, 1, 64) },
        kResultOk
    );
    assert_eq!(
        unsafe { pres.setAudioPresentationLatencySamples(BusDirections_::kOutput as i32, 1, 128) },
        kInvalidArgument
    );
    assert_eq!(
        unsafe { pres.setAudioPresentationLatencySamples(BusDirections_::kInput as i32, 2, 128) },
        kInvalidArgument
    );
    assert_eq!(
        unsafe { pres.setAudioPresentationLatencySamples(99, 0, 128) },
        kInvalidArgument
    );
    assert_eq!(
        unsafe { pres.setAudioPresentationLatencySamples(BusDirections_::kOutput as i32, -1, 128) },
        kInvalidArgument
    );
    assert_eq!(*LATENCY_CALLS.lock().unwrap(), [(0, 0, 512), (1, 1, 64)]);

    // IParameterFinder on View
    let raw_view = super::view::View::create(&host);
    assert!(!raw_view.is_null());
    let view = unsafe { ComPtr::from_raw(raw_view) }.expect("view");
    let finder = view
        .cast::<IParameterFinder>()
        .expect("View exposes IParameterFinder");
    let mut found: ParamID = 0;
    assert_eq!(
        unsafe { finder.findParameter(120, 80, &raw mut found) },
        kResultOk
    );
    assert_eq!(found, 42);
    assert_eq!(
        unsafe { finder.findParameter(10, 10, &raw mut found) },
        kResultFalse
    );
    assert_eq!(
        unsafe { finder.findParameter(120, 80, std::ptr::null_mut()) },
        kInvalidArgument
    );
}

#[test]
fn get_parameter_info_prefers_live_step_count_when_present() {
    use std::sync::atomic::{AtomicI32, Ordering};

    static LIVE_STEPS: AtomicI32 = AtomicI32::new(-1);

    unsafe extern "C" fn live_step(_: *mut c_void, id: u32, out_default_norm: *mut f64) -> i32 {
        if id == 7 {
            let steps = LIVE_STEPS.load(Ordering::Acquire);
            if steps > 0
                && let Some(out) = unsafe { out_default_norm.as_mut() }
            {
                *out = 1.0 / f64::from(steps);
            }
            steps
        } else {
            -1
        }
    }

    LIVE_STEPS.store(-1, Ordering::Release);
    let mut cb = callbacks();
    cb.param_step_count = live_step;
    let params = vec![
        Vst3ParamDescriptor {
            id: 7,
            name: c"Bank".as_ptr(),
            short_name: c"Bank".as_ptr(),
            units: c"".as_ptr(),
            min: 0.0,
            max: 2.0,
            default_normalized: 0.5,
            step_count: 2,
            flags: 0,
            group: c"".as_ptr(),
        },
        Vst3ParamDescriptor {
            id: 8,
            name: c"Gain".as_ptr(),
            short_name: c"Gain".as_ptr(),
            units: c"".as_ptr(),
            min: 0.0,
            max: 1.0,
            default_normalized: 0.5,
            step_count: 0,
            flags: 0,
            group: c"".as_ptr(),
        },
    ];
    let reg = registration(descriptor(), cb, params);
    let host = Host::new(reg, None).expect("host");
    let comp = ComWrapper::new(Component { host });
    let ctrl = comp.to_com_ptr::<IEditController>().expect("controller");

    let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { ctrl.getParameterInfo(0, &raw mut info) },
        kResultOk
    );
    assert_eq!(info.stepCount, 2, "-1 falls back to static step_count");
    assert!((info.defaultNormalizedValue - 0.5).abs() < 1e-12);

    LIVE_STEPS.store(4, Ordering::Release);
    assert_eq!(
        unsafe { ctrl.getParameterInfo(0, &raw mut info) },
        kResultOk
    );
    assert_eq!(info.stepCount, 4, "live step_count overrides static count");
    assert!(
        (info.defaultNormalizedValue - 0.25).abs() < 1e-12,
        "live defaultNormalizedValue updates alongside live step_count"
    );

    assert_eq!(
        unsafe { ctrl.getParameterInfo(1, &raw mut info) },
        kResultOk
    );
    assert_eq!(info.stepCount, 0, "unrelated param keeps static step_count");
    assert!((info.defaultNormalizedValue - 0.5).abs() < 1e-12);
}

struct MemStream {
    data: std::sync::Mutex<Vec<u8>>,
    pos: std::sync::Mutex<usize>,
}

impl MemStream {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            data: std::sync::Mutex::new(bytes),
            pos: std::sync::Mutex::new(0),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }
}

impl Class for MemStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let data = self.data.lock().unwrap();
        let mut pos = self.pos.lock().unwrap();
        let avail = data.len().saturating_sub(*pos);
        let n = (num_bytes as usize).min(avail);
        if n > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(data[*pos..].as_ptr(), buffer.cast::<u8>(), n);
            }
            *pos += n;
        }
        if !num_read.is_null() {
            unsafe { *num_read = n as int32 };
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_written: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let n = num_bytes as usize;
        let slice = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) };
        self.data.lock().unwrap().extend_from_slice(slice);
        if !num_written.is_null() {
            unsafe { *num_written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, _pos: int64, _mode: int32, _result: *mut int64) -> tresult {
        kNotImplemented
    }

    unsafe fn tell(&self, result: *mut int64) -> tresult {
        if !result.is_null() {
            unsafe { *result = *self.pos.lock().unwrap() as int64 };
        }
        kResultOk
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn physical_ui_keyswitch_program_list_and_compatibility_interfaces_round_trip() {
    static LOADED_PROGRAM: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());
    static BANK: std::sync::Mutex<Vec<moose_core::main_thread::ProgramPreset>> =
        std::sync::Mutex::new(Vec::new());

    unsafe fn test_keyswitches(
        _: *mut c_void,
        bus: u32,
        channel: u8,
    ) -> Vec<moose_core::main_thread::Keyswitch> {
        if bus == 0 && channel == 0 {
            vec![
                moose_core::main_thread::Keyswitch {
                    kind: moose_core::main_thread::KeyswitchKind::NoteOn,
                    title: "Legato".into(),
                    short_title: "Leg".into(),
                    min_key: 24,
                    max_key: 24,
                    remapped_key: -1,
                    unit_id: 0,
                },
                moose_core::main_thread::Keyswitch {
                    kind: moose_core::main_thread::KeyswitchKind::KeyRange,
                    title: "Staccato".into(),
                    short_title: "Stac".into(),
                    min_key: 25,
                    max_key: 27,
                    remapped_key: 36,
                    unit_id: 0,
                },
            ]
        } else {
            Vec::new()
        }
    }

    unsafe fn test_programs(_: *mut c_void) -> Vec<moose_core::main_thread::ProgramPreset> {
        BANK.lock().unwrap().clone()
    }

    unsafe fn test_set_program_data(
        _: *mut c_void,
        index: usize,
        data: *const u8,
        len: usize,
    ) -> bool {
        let mut bank = BANK.lock().unwrap();
        let Some(slot) = bank.get_mut(index) else {
            return false;
        };
        slot.state = if data.is_null() || len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
        };
        true
    }

    unsafe extern "C" fn capture_load_stream(
        _: *mut c_void,
        stream: *mut c_void,
        read: unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> i64,
    ) -> i32 {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { read(stream, buf.as_mut_ptr(), buf.len() as u32) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        if out == b"MOOSE_STATE_A" {
            return 0;
        }
        *LOADED_PROGRAM.lock().unwrap() = out;
        1
    }

    *BANK.lock().unwrap() = vec![
        moose_core::main_thread::ProgramPreset {
            name: "Init Pad".into(),
            category: "Pads".into(),
            state: b"MOOSE_STATE_A".to_vec(),
        },
        moose_core::main_thread::ProgramPreset {
            name: "Bright Lead".into(),
            category: "Leads".into(),
            state: b"MOOSE_STATE_B".to_vec(),
        },
    ];
    LOADED_PROGRAM.lock().unwrap().clear();

    let compat_json = c"{\"New\":[{\"ClassID\":\"00112233445566778899AABBCCDDEEFF\"}]}";
    let mut desc = descriptor();
    desc.name = c"ParityTest".as_ptr();
    desc.vendor = c"Matari".as_ptr();
    desc.category = c"Audio Module Class".as_ptr();
    desc.subcategories = c"Instrument|Synth".as_ptr();
    desc.version = c"1.0.0".as_ptr();
    desc.cid = [1; 16];
    desc.midi_input_ports = 1;
    desc.compatibility_json = compat_json.as_ptr();

    let reg = registration(
        desc,
        Vst3Callbacks {
            keyswitches: test_keyswitches,
            program_list: test_programs,
            set_program_data: test_set_program_data,
            state_load_stream: capture_load_stream,
            ..callbacks()
        },
        vec![],
    );

    let comp = component(reg);
    let comp_unk = comp.to_com_ptr::<IComponent>().expect("IComponent");

    // 1. INoteExpressionPhysicalUIMapping
    let pui = comp_unk
        .cast::<INoteExpressionPhysicalUIMapping>()
        .expect("INoteExpressionPhysicalUIMapping");
    let mut maps = [
        PhysicalUIMap {
            physicalUITypeID: PhysicalUITypeIDs_::kPUIXMovement as u32,
            noteExpressionTypeID: NoteExpressionTypeIDs_::kInvalidTypeID as u32,
        },
        PhysicalUIMap {
            physicalUITypeID: PhysicalUITypeIDs_::kPUIYMovement as u32,
            noteExpressionTypeID: NoteExpressionTypeIDs_::kInvalidTypeID as u32,
        },
        PhysicalUIMap {
            physicalUITypeID: PhysicalUITypeIDs_::kPUIPressure as u32,
            noteExpressionTypeID: NoteExpressionTypeIDs_::kInvalidTypeID as u32,
        },
        PhysicalUIMap {
            physicalUITypeID: 99,
            noteExpressionTypeID: 0,
        },
    ];
    let mut list = PhysicalUIMapList {
        count: maps.len() as u32,
        map: maps.as_mut_ptr(),
    };
    assert_eq!(
        unsafe { pui.getPhysicalUIMapping(0, 0, &raw mut list) },
        kResultOk
    );
    assert_eq!(
        maps[0].noteExpressionTypeID,
        NoteExpressionTypeIDs_::kTuningTypeID as u32
    );
    assert_eq!(
        maps[1].noteExpressionTypeID,
        NoteExpressionTypeIDs_::kBrightnessTypeID as u32
    );
    assert_eq!(
        maps[2].noteExpressionTypeID,
        NoteExpressionTypeIDs_::kExpressionTypeID as u32
    );
    assert_eq!(
        maps[3].noteExpressionTypeID,
        NoteExpressionTypeIDs_::kInvalidTypeID as u32
    );
    assert_eq!(
        unsafe { pui.getPhysicalUIMapping(1, 0, &raw mut list) },
        kInvalidArgument
    );

    // 2. IKeyswitchController
    let ks = comp_unk
        .cast::<IKeyswitchController>()
        .expect("IKeyswitchController");
    assert_eq!(unsafe { ks.getKeyswitchCount(0, 0) }, 2);
    assert_eq!(unsafe { ks.getKeyswitchCount(0, 1) }, 0);
    assert_eq!(unsafe { ks.getKeyswitchCount(1, 0) }, 0);
    let mut ks_info: KeyswitchInfo = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { ks.getKeyswitchInfo(0, 0, 1, &raw mut ks_info) },
        kResultOk
    );
    assert_eq!(ks_info.typeId, KeyswitchTypeIDs_::kKeyRangeTypeID as u32);
    assert_eq!(
        unsafe { super::utf16_from(ks_info.title.as_ptr(), 128) },
        "Staccato"
    );
    assert_eq!(ks_info.keyswitchMin, 25);
    assert_eq!(ks_info.keyswitchMax, 27);
    assert_eq!(ks_info.keyRemapped, 36);
    assert_eq!(
        unsafe { ks.getKeyswitchInfo(0, 0, 2, &raw mut ks_info) },
        kInvalidArgument
    );

    // 3. IUnitInfo + IProgramListData
    let units = comp_unk.cast::<IUnitInfo>().expect("IUnitInfo");
    assert_eq!(unsafe { units.getProgramListCount() }, 1);
    let mut unit_info: UnitInfo = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { units.getUnitInfo(0, &raw mut unit_info) },
        kResultOk
    );
    assert_eq!(unit_info.programListId, 0);
    let mut pl_info: ProgramListInfo = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { units.getProgramListInfo(0, &raw mut pl_info) },
        kResultOk
    );
    assert_eq!(pl_info.programCount, 2);
    let mut pname: String128 = [0; 128];
    assert_eq!(
        unsafe { units.getProgramName(0, 0, &raw mut pname) },
        kResultOk
    );
    assert_eq!(
        unsafe { super::utf16_from(pname.as_ptr(), 128) },
        "Init Pad"
    );
    let mut pcat: String128 = [0; 128];
    assert_eq!(
        unsafe { units.getProgramInfo(0, 0, c"Category".as_ptr(), &raw mut pcat) },
        kResultOk
    );
    assert_eq!(unsafe { super::utf16_from(pcat.as_ptr(), 128) }, "Pads");

    // Root unit must also export a `kIsProgramChange` selector parameter.
    let ctrl = comp_unk.cast::<IEditController>().expect("IEditController");
    assert_eq!(unsafe { ctrl.getParameterCount() }, 1);
    let mut pinfo: ParameterInfo = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { ctrl.getParameterInfo(0, &raw mut pinfo) },
        kResultOk
    );
    assert_eq!(pinfo.unitId, 0);
    assert_eq!(pinfo.stepCount, 1);
    assert_ne!(
        pinfo.flags & ParameterInfo_::ParameterFlags_::kIsProgramChange as i32,
        0
    );
    let mut label: String128 = [0; 128];
    assert_eq!(
        unsafe { ctrl.getParamStringByValue(pinfo.id, 1.0, &raw mut label) },
        kResultOk
    );
    assert_eq!(
        unsafe { super::utf16_from(label.as_ptr(), 128) },
        "Bright Lead"
    );
    assert_eq!(unsafe { ctrl.setParamNormalized(pinfo.id, 1.0) }, kResultOk);
    assert!((unsafe { ctrl.getParamNormalized(pinfo.id) } - 1.0).abs() < 1e-12);
    assert_eq!(
        unsafe { ctrl.setParamNormalized(pinfo.id, 0.0) },
        kResultFalse
    );
    assert!((unsafe { ctrl.getParamNormalized(pinfo.id) } - 1.0).abs() < 1e-12);
    assert_eq!(*LOADED_PROGRAM.lock().unwrap(), b"MOOSE_STATE_B");

    let pdata = comp_unk
        .cast::<IProgramListData>()
        .expect("IProgramListData");
    assert_eq!(unsafe { pdata.programDataSupported(0) }, kResultTrue);
    assert_eq!(unsafe { pdata.programDataSupported(1) }, kResultFalse);
    let out_stream = ComWrapper::new(MemStream::new(Vec::new()));
    let out_ptr = out_stream.to_com_ptr::<IBStream>().expect("IBStream");
    assert_eq!(
        unsafe { pdata.getProgramData(0, 0, out_ptr.as_ptr()) },
        kResultOk
    );
    assert_eq!(out_stream.bytes(), b"MOOSE_STATE_A");

    // `setProgramData` replaces the indexed bank entry without calling instance state load.
    LOADED_PROGRAM.lock().unwrap().clear();
    let in_stream = ComWrapper::new(MemStream::new(b"MOOSE_STATE_B_UPDATED".to_vec()));
    let in_ptr = in_stream.to_com_ptr::<IBStream>().expect("IBStream");
    assert_eq!(
        unsafe { pdata.setProgramData(0, 1, in_ptr.as_ptr()) },
        kResultOk
    );
    assert!(LOADED_PROGRAM.lock().unwrap().is_empty());
    let updated_stream = ComWrapper::new(MemStream::new(Vec::new()));
    let updated_ptr = updated_stream.to_com_ptr::<IBStream>().expect("IBStream");
    assert_eq!(
        unsafe { pdata.getProgramData(0, 1, updated_ptr.as_ptr()) },
        kResultOk
    );
    assert_eq!(updated_stream.bytes(), b"MOOSE_STATE_B_UPDATED");

    // Immutable banks (default `set_program_data` returning false) reject writes.
    let mut ro_desc = descriptor();
    ro_desc.name = c"ParityTest".as_ptr();
    ro_desc.cid = [2; 16];
    let ro_reg = registration(
        ro_desc,
        Vst3Callbacks {
            program_list: test_programs,
            ..callbacks()
        },
        vec![],
    );
    let ro_comp = component(ro_reg);
    let ro_pdata = ro_comp
        .to_com_ptr::<IProgramListData>()
        .expect("IProgramListData");
    let ro_in = ComWrapper::new(MemStream::new(b"NEW".to_vec()));
    let ro_in_ptr = ro_in.to_com_ptr::<IBStream>().expect("IBStream");
    assert_eq!(
        unsafe { ro_pdata.setProgramData(0, 0, ro_in_ptr.as_ptr()) },
        kResultFalse
    );

    // 4. IPluginCompatibility via Factory
    let factory = super::factory::new_test_factory(reg);
    let f2 = factory
        .to_com_ptr::<IPluginFactory2>()
        .expect("IPluginFactory2");
    assert_eq!(unsafe { f2.countClasses() }, 2);
    let mut cls: PClassInfo2 = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { f2.getClassInfo2(1, &raw mut cls) }, kResultOk);
    assert_eq!(
        unsafe { super::cstr(cls.category.as_ptr()) },
        "Plugin Compatibility Class"
    );
    let expected_cid = super::factory::compatibility_cid([1; 16]);
    assert_eq!(cls.cid.map(|b| b as u8), expected_cid);

    let mut raw_compat: *mut c_void = std::ptr::null_mut();
    let compat_iid = <IPluginCompatibility as vst3::com_scrape_types::Interface>::IID;
    assert_eq!(
        unsafe {
            f2.createInstance(
                expected_cid.as_ptr().cast(),
                compat_iid.as_ptr().cast(),
                &raw mut raw_compat,
            )
        },
        kResultOk
    );
    assert!(!raw_compat.is_null());
    let compat = unsafe { ComPtr::<IPluginCompatibility>::from_raw(raw_compat.cast()) }
        .expect("IPluginCompatibility");
    let json_stream = ComWrapper::new(MemStream::new(Vec::new()));
    let json_ptr = json_stream.to_com_ptr::<IBStream>().expect("IBStream");
    assert_eq!(
        unsafe { compat.getCompatibilityJSON(json_ptr.as_ptr()) },
        kResultOk
    );
    assert_eq!(json_stream.bytes(), compat_json.to_bytes());
}
