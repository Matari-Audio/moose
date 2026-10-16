//! Factory/COM/IRunLoop proof: completion without ever creating a plug view.
#![cfg(target_os = "linux")]
use super::host_value_fixture::{CREATED, TestPlugin};
use super::*;
use std::{cell::Cell, ffi::c_void};
const CONTROLLER: [u8; 16] = [
    0xDC, 0xD7, 0xBB, 0xE3, 0x77, 0x42, 0x44, 0x8D, 0xA8, 0x74, 0xAA, 0xCC, 0x97, 0x9C, 0x75, 0x9E,
];
#[repr(C)]
struct Loop {
    vtbl: *const *const c_void,
    refs: Cell<u32>,
    event: Cell<*mut c_void>,
    fd: Cell<i32>,
    thread: std::thread::ThreadId,
}
unsafe extern "C" fn qi(raw: *mut c_void, _: *const u8, out: *mut *mut c_void) -> i32 {
    unsafe {
        *out = raw;
        add(raw);
    }
    0
}
unsafe extern "C" fn add(raw: *mut c_void) -> u32 {
    let host = unsafe { &*raw.cast::<Loop>() };
    host.refs.set(host.refs.get() + 1);
    host.refs.get()
}
unsafe extern "C" fn release(raw: *mut c_void) -> u32 {
    let host = unsafe { &*raw.cast::<Loop>() };
    host.refs.set(host.refs.get() - 1);
    host.refs.get()
}
unsafe extern "C" fn register(raw: *mut c_void, event: *mut c_void, fd: i32) -> i32 {
    let host = unsafe { &*raw.cast::<Loop>() };
    assert_eq!(std::thread::current().id(), host.thread);
    assert!(host.event.get().is_null());
    host.event.set(event);
    host.fd.set(fd);
    0
}
unsafe extern "C" fn unregister(raw: *mut c_void, event: *mut c_void) -> i32 {
    let host = unsafe { &*raw.cast::<Loop>() };
    assert_eq!(host.event.replace(std::ptr::null_mut()), event);
    host.fd.set(-1);
    0
}
unsafe extern "C" fn no_timer(_: *mut c_void, _: *mut c_void, _: u64) -> i32 {
    panic!("closed editor completion must not register a polling timer");
}
unsafe extern "C" fn no_unreg_timer(_: *mut c_void, _: *mut c_void) -> i32 {
    0
}
#[repr(C)]
struct Handler {
    vtbl: *const *const c_void,
    thread: std::thread::ThreadId,
    events: Vec<(u8, u32, f64)>,
}
unsafe extern "C" fn handler_add(_: *mut c_void) -> u32 {
    1
}
unsafe extern "C" fn begin(raw: *mut c_void, id: u32) -> i32 {
    let handler = unsafe { &mut *raw.cast::<Handler>() };
    assert_eq!(std::thread::current().id(), handler.thread);
    handler.events.push((0, id, 0.));
    0
}
unsafe extern "C" fn perform(raw: *mut c_void, id: u32, value: f64) -> i32 {
    let handler = unsafe { &mut *raw.cast::<Handler>() };
    assert_eq!(std::thread::current().id(), handler.thread);
    handler.events.push((1, id, value));
    0
}
unsafe extern "C" fn end(raw: *mut c_void, id: u32) -> i32 {
    let handler = unsafe { &mut *raw.cast::<Handler>() };
    assert_eq!(std::thread::current().id(), handler.thread);
    handler.events.push((2, id, 0.));
    0
}
unsafe extern "C" fn restart(_: *mut c_void, _: i32) -> i32 {
    0
}
unsafe fn slot(raw: *mut c_void, index: usize) -> *const c_void {
    unsafe { *(*raw.cast::<*const *const c_void>()).add(index) }
}
#[repr(C)]
struct Poll {
    fd: i32,
    events: i16,
    revents: i16,
}
unsafe extern "C" {
    fn poll(fds: *mut Poll, count: usize, millis: i32) -> i32;
}
unsafe fn dispatch(host: &Loop) {
    let mut fd = Poll {
        fd: host.fd.get(),
        events: 1,
        revents: 0,
    };
    assert_eq!(
        unsafe { poll(&raw mut fd, 1, 0) },
        1,
        "worker must wake host fd"
    );
    let event = host.event.get();
    let callback: unsafe extern "C" fn(*mut c_void, i32) =
        unsafe { std::mem::transmute(slot(event, 3)) };
    unsafe {
        callback(event, host.fd.get());
    }
    fd.revents = 0;
    assert_eq!(
        unsafe { poll(&raw mut fd, 1, 0) },
        0,
        "idle fd must be drained"
    );
}
#[test]
fn factory_host_runloop_delivers_closed_editor_completion_without_polling_or_uaf() {
    // This is the only test registering the global shim factory. Ordinary FFI
    // tests use cb_create directly and do not access its global callback table.
    unsafe {
        register_vst3::<TestPlugin>();
        let factory = ffi::moose_vst3_get_factory();
        let loop_vtbl = [
            qi as *const c_void,
            add as _,
            release as _,
            register as _,
            unregister as _,
            no_timer as _,
            no_unreg_timer as _,
        ];
        let host = Loop {
            vtbl: loop_vtbl.as_ptr(),
            refs: Cell::new(1),
            event: Cell::new(std::ptr::null_mut()),
            fd: Cell::new(-1),
            thread: std::thread::current().id(),
        };
        let host_ptr = (&raw const host).cast_mut().cast();
        let bind: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
            std::mem::transmute(slot(factory, 9));
        assert_eq!(bind(factory, host_ptr), 0);
        let info: unsafe extern "C" fn(*mut c_void, i32, *mut c_void) -> i32 =
            std::mem::transmute(slot(factory, 5));
        let mut class = [0_u64; 32];
        assert_eq!(info(factory, 0, class.as_mut_ptr().cast()), 0);
        let create: unsafe extern "C" fn(
            *mut c_void,
            *const u8,
            *const u8,
            *mut *mut c_void,
        ) -> i32 = std::mem::transmute(slot(factory, 6));
        let mut controller = std::ptr::null_mut();
        assert_eq!(
            create(
                factory,
                class.as_ptr().cast(),
                CONTROLLER.as_ptr(),
                &raw mut controller
            ),
            0
        );
        let params = CREATED.with(|created| created.borrow().upgrade().unwrap());
        assert!(!host.event.get().is_null());
        let handler_vtbl = [
            std::ptr::null(),
            handler_add as *const c_void,
            handler_add as _,
            begin as _,
            perform as _,
            end as _,
            restart as _,
        ];
        let mut handler = Handler {
            vtbl: handler_vtbl.as_ptr(),
            thread: host.thread,
            events: vec![],
        };
        let set_handler: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
            std::mem::transmute(slot(controller, 16));
        assert_eq!(set_handler(controller, (&raw mut handler).cast()), 0);
        assert_eq!(params.get_normalized(1), Some(0.5));
        std::thread::scope(|scope| {
            scope
                .spawn(|| params.completion.complete(9))
                .join()
                .unwrap();
        });
        assert!(
            handler.events.is_empty(),
            "worker cannot invoke host gesture callbacks"
        );
        dispatch(&host);
        assert_eq!(handler.events, vec![(0, 1, 0.), (1, 1, 0.25), (2, 1, 0.)]);
        params.completion.complete(9);
        dispatch(&host);
        assert_eq!(handler.events.len(), 3, "identical content is a no-op");
        let signal = params.completion.signal().unwrap();
        let drop_controller: unsafe extern "C" fn(*mut c_void) -> u32 =
            std::mem::transmute(slot(controller, 2));
        assert_eq!(drop_controller(controller), 0);
        assert!(host.event.get().is_null());
        signal.notify(); // retained worker handle is closed before native free
        params.completion.complete(10);
        assert_eq!(bind(factory, std::ptr::null_mut()), 0);
        let drop_factory: unsafe extern "C" fn(*mut c_void) -> u32 =
            std::mem::transmute(slot(factory, 2));
        assert_eq!(drop_factory(factory), 0);
        assert_eq!(
            host.refs.get(),
            1,
            "no runloop reference retained after teardown"
        );
    }
}
