//! Heap-in-place construction ([`HeapInit`]) so large DSP processors, parameter
//! stores, and tables never materialize on the caller's stack.
//!
//! Host threads often have tight stack limits (Windows: 1 MiB, macOS
//! secondary/AU threads: 512 KiB, Linux musl: 128 KiB). In Rust,
//! `Box::new(T::default())` builds `T` on the stack first and relies on
//! optimizer copy-elision, which varies across build profiles and feature
//! unification. Implementing (or deriving `#[derive(HeapInit)]`) writes every
//! field directly into a heap `Box::new_uninit()` allocation via
//! `ptr::addr_of_mut!` and recurses into nested `HeapInit` fields.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::sync::atomic::{
    AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicU8, AtomicU16,
    AtomicU32, AtomicU64, AtomicUsize,
};
use std::sync::{Arc, Mutex, RwLock};

/// Construct a value directly inside heap memory without materializing `Self`
/// on the caller's stack.
///
/// Derive this automatically with `#[derive(HeapInit)]` on large DSP/editor
/// structs. Nested fields whose types implement `HeapInit` are initialized
/// recursively in place (zero stack temporary); other fields fall back to
/// `Default::default()` or a field-level `#[heap_init(with = expr)]` override.
pub trait HeapInit: Sized {
    /// Initialize `Self` directly into the uninitialized slot `dst`.
    ///
    /// # Safety
    ///
    /// - `dst` must be non-null, properly aligned for `Self`, and valid for
    ///   writes of `size_of::<Self>()` bytes.
    /// - On normal return, `*dst` is fully initialized.
    /// - If initialization panics partway through, every field already written
    ///   must have been dropped before unwinding so the caller can safely free
    ///   the raw backing storage without running `Self::drop`.
    unsafe fn heap_init_in_place(dst: *mut Self);

    /// Allocate `Self` on the heap and initialize it in place without ever
    /// placing `Self` on the caller's stack.
    #[must_use]
    fn heap_init() -> Box<Self> {
        let mut slot = Box::<Self>::new_uninit();
        // SAFETY: `slot` owns valid, aligned, writable storage for `Self`.
        // `heap_init_in_place` guarantees `*dst` is fully initialized on
        // return and unwind-safe on panic (leaving `Box<MaybeUninit<Self>>`
        // to deallocate without dropping uninitialized memory).
        unsafe {
            Self::heap_init_in_place(slot.as_mut_ptr());
            slot.assume_init()
        }
    }

    /// Allocate `Self` inside an `Arc` and initialize it in place without ever
    /// placing `Self` on the caller's stack.
    #[must_use]
    fn heap_init_arc() -> Arc<Self> {
        let mut slot = Arc::<Self>::new_uninit();
        let ptr = Arc::get_mut(&mut slot)
            .expect("freshly allocated Arc is uniquely owned")
            .as_mut_ptr();
        // SAFETY: `slot` is uniquely owned and provides valid, aligned,
        // writable storage for `Self`; `heap_init_in_place` initializes it or
        // cleans up partial writes before unwinding.
        unsafe {
            Self::heap_init_in_place(ptr);
            slot.assume_init()
        }
    }
}

/// Build a `Box<[T; N]>` by initializing each element directly in place on the
/// heap via [`HeapInit::heap_init_in_place`].
///
/// Never materializes even a single `T` on the stack, making large arrays of
/// `HeapInit` structs safe on constrained host stacks (and passable directly
/// into `moose_dsp::HeapArray::from_boxed_array`).
#[must_use]
pub fn heap_box_array_init<T: HeapInit, const N: usize>() -> Box<[T; N]> {
    let mut slot = Box::<[T; N]>::new_uninit();
    // SAFETY: `slot` owns valid, aligned, writable storage for `[T; N]`, and
    // `<[T; N] as HeapInit>::heap_init_in_place` initializes elements `0..N`
    // in place with a panic drop guard for `0..len`.
    unsafe {
        <[T; N] as HeapInit>::heap_init_in_place(slot.as_mut_ptr());
        slot.assume_init()
    }
}

/// Internal helpers consumed by `#[derive(HeapInit)]` generated code.
#[doc(hidden)]
pub mod __private {
    use super::HeapInit;
    use core::marker::PhantomData;

    /// Zero-sized dispatch tag enabling stable autoref specialization inside
    /// `#[derive(HeapInit)]`: prefers [`HeapInit::heap_init_in_place`] when
    /// implemented and falls back to `Default::default()` otherwise.
    pub struct Tag<T: ?Sized>(PhantomData<fn() -> *mut T>);

    impl<T: ?Sized> Tag<T> {
        pub const NEW: Self = Self(PhantomData);
    }

    /// Priority-1 dispatch (matched on `&Tag<T>`): recursive in-place init.
    pub trait ViaHeapInit<T> {
        /// # Safety
        /// `dst` must be valid, aligned, and writable for `T`.
        unsafe fn __moose_heap_init(&self, dst: *mut T);
    }

    impl<T: HeapInit> ViaHeapInit<T> for &Tag<T> {
        #[inline]
        unsafe fn __moose_heap_init(&self, dst: *mut T) {
            // SAFETY: forwarded from caller's `dst` validity contract.
            unsafe { T::heap_init_in_place(dst) };
        }
    }

    /// Priority-2 dispatch (matched after auto-deref to `Tag<T>`): `Default`.
    pub trait ViaDefault<T> {
        /// # Safety
        /// `dst` must be valid, aligned, and writable for `T`.
        unsafe fn __moose_heap_init(&self, dst: *mut T);
    }

    impl<T: Default> ViaDefault<T> for Tag<T> {
        #[inline]
        unsafe fn __moose_heap_init(&self, dst: *mut T) {
            // SAFETY: forwarded from caller's `dst` validity contract.
            unsafe { dst.write(T::default()) };
        }
    }

    /// Panic-unwind drop guard for one already-initialized struct field.
    /// Disarmed via `core::mem::forget` once all fields finish initializing.
    pub struct FieldDropGuard<T> {
        ptr: *mut T,
    }

    impl<T> FieldDropGuard<T> {
        /// Arm a drop guard for an initialized field at `ptr`.
        ///
        /// # Safety
        /// `*ptr` must currently hold a valid, initialized `T` that will not be
        /// dropped elsewhere unless this guard is forgotten.
        #[inline]
        pub unsafe fn new(ptr: *mut T) -> Self {
            Self { ptr }
        }
    }

    impl<T> Drop for FieldDropGuard<T> {
        fn drop(&mut self) {
            // SAFETY: guard is only dropped if initialization panicked before
            // the caller called `mem::forget`, and `ptr` holds an initialized `T`.
            unsafe {
                self.ptr.drop_in_place();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in `HeapInit` implementations for primitives, std types, and arrays
// ---------------------------------------------------------------------------

macro_rules! impl_heap_init_default {
    ($($ty:ty),* $(,)?) => {
        $(
            impl HeapInit for $ty {
                #[inline]
                unsafe fn heap_init_in_place(dst: *mut Self) {
                    // SAFETY: caller guarantees `dst` is valid and aligned for `Self`.
                    unsafe { dst.write(Self::default()) };
                }
            }
        )*
    };
}

impl_heap_init_default!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    AtomicBool,
    AtomicU8,
    AtomicU16,
    AtomicU32,
    AtomicU64,
    AtomicUsize,
    AtomicI8,
    AtomicI16,
    AtomicI32,
    AtomicI64,
    AtomicIsize,
    String,
);

impl<T> HeapInit for Vec<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        // SAFETY: caller guarantees `dst` is valid and aligned for `Vec<T>`.
        unsafe { dst.write(Self::new()) };
    }
}

impl<T> HeapInit for Option<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        // SAFETY: caller guarantees `dst` is valid and aligned for `Option<T>`.
        unsafe { dst.write(None) };
    }
}

impl<T: ?Sized> HeapInit for PhantomData<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        // SAFETY: zero-sized type; valid write.
        unsafe { dst.write(Self) };
    }
}

impl<T: HeapInit> HeapInit for Box<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        // SAFETY: `T::heap_init()` builds `T` directly on the heap; writing
        // the `Box<T>` pointer into `dst` only moves one pointer on the stack.
        unsafe { dst.write(T::heap_init()) };
    }
}

impl<T: HeapInit> HeapInit for Arc<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        // SAFETY: `T::heap_init_arc()` initializes `T` directly inside `Arc`.
        unsafe { dst.write(T::heap_init_arc()) };
    }
}

impl<T: HeapInit> HeapInit for Cell<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        // `Cell<T>` is `#[repr(transparent)]` over `UnsafeCell<T>` (same layout as `T`).
        // SAFETY: `Cell<T>` has identical memory layout to `T`.
        unsafe { T::heap_init_in_place(dst.cast::<T>()) };
    }
}

impl<T: HeapInit> HeapInit for RefCell<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        let mut slot = MaybeUninit::<T>::uninit();
        // Note: `RefCell` fields are private, so initialize directly via `Box`
        // if large? No: `RefCell::new` takes `T` by value; keep `RefCell<Box<T>>`
        // for large values, and for small `T` initialize via `heap_init_in_place`.
        // SAFETY: `slot` is valid writable storage for `T`.
        unsafe {
            T::heap_init_in_place(slot.as_mut_ptr());
            dst.write(Self::new(slot.assume_init()));
        }
    }
}

impl<T: HeapInit> HeapInit for Mutex<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        let mut slot = MaybeUninit::<T>::uninit();
        // SAFETY: `slot` is valid writable storage for `T`.
        unsafe {
            T::heap_init_in_place(slot.as_mut_ptr());
            dst.write(Self::new(slot.assume_init()));
        }
    }
}

impl<T: HeapInit> HeapInit for RwLock<T> {
    #[inline]
    unsafe fn heap_init_in_place(dst: *mut Self) {
        let mut slot = MaybeUninit::<T>::uninit();
        // SAFETY: `slot` is valid writable storage for `T`.
        unsafe {
            T::heap_init_in_place(slot.as_mut_ptr());
            dst.write(Self::new(slot.assume_init()));
        }
    }
}

impl<T: HeapInit, const N: usize> HeapInit for [T; N] {
    unsafe fn heap_init_in_place(dst: *mut Self) {
        struct SliceInitGuard<T> {
            base: *mut T,
            len: usize,
        }
        impl<T> Drop for SliceInitGuard<T> {
            fn drop(&mut self) {
                for i in (0..self.len).rev() {
                    // SAFETY: indices `0..self.len` were fully initialized before unwinding.
                    unsafe { self.base.add(i).drop_in_place() };
                }
            }
        }

        let base = dst.cast::<T>();
        let mut guard = SliceInitGuard { base, len: 0 };
        while guard.len < N {
            // SAFETY: `guard.len < N`, so `base.add(guard.len)` points inside `[T; N]`.
            unsafe { T::heap_init_in_place(base.add(guard.len)) };
            guard.len += 1;
        }
        core::mem::forget(guard);
    }
}

macro_rules! impl_heap_init_tuple {
    ($($idx:tt : $T:ident),+) => {
        impl<$($T: HeapInit),+> HeapInit for ($($T,)+) {
            #[inline]
            unsafe fn heap_init_in_place(dst: *mut Self) {
                $(
                    #[allow(non_snake_case)]
                    let $T = {
                        // SAFETY: field projection within valid `dst`.
                        let p = unsafe { core::ptr::addr_of_mut!((*dst).$idx) };
                        unsafe { $T::heap_init_in_place(p) };
                        unsafe { __private::FieldDropGuard::new(p) }
                    };
                )+
                $(core::mem::forget($T);)+
            }
        }
    };
}

impl_heap_init_tuple!(0: A);
impl_heap_init_tuple!(0: A, 1: B);
impl_heap_init_tuple!(0: A, 1: B, 2: C);
impl_heap_init_tuple!(0: A, 1: B, 2: C, 3: D);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn large_array_and_heap_array_init_fit_in_128k_stack() {
        let handle = std::thread::Builder::new()
            .name("heap-init-128k".into())
            .stack_size(128 * 1024)
            .spawn(|| {
                let raw: Box<[u8; 524_288]> = <[u8; 524_288]>::heap_init();
                assert_eq!(raw[0], 0);
                assert_eq!(raw[524_287], 0);

                let nested: Box<[[u8; 1024]; 512]> = heap_box_array_init::<[u8; 1024], 512>();
                assert_eq!(nested[0][0], 0);
                assert_eq!(nested[511][1023], 0);

                let arc_big: Arc<[f32; 131_072]> = <[f32; 131_072]>::heap_init_arc();
                assert_eq!(arc_big[131_071].to_bits(), 0.0f32.to_bits());
            })
            .expect("spawn 128 KiB thread");
        handle.join().expect("128 KiB thread completed cleanly");
    }

    #[test]
    fn array_heap_init_drops_initialized_prefix_on_panic() {
        static INIT_COUNT: AtomicUsize = AtomicUsize::new(0);
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct PanicOnThird(#[allow(dead_code)] usize);
        impl HeapInit for PanicOnThird {
            unsafe fn heap_init_in_place(dst: *mut Self) {
                let idx = INIT_COUNT.fetch_add(1, Ordering::SeqCst);
                assert!(idx != 2, "intentional panic at element 2");
                // SAFETY: `dst` is valid and aligned for `PanicOnThird`.
                unsafe { dst.write(Self(idx)) };
            }
        }
        impl Drop for PanicOnThird {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        INIT_COUNT.store(0, Ordering::SeqCst);
        DROP_COUNT.store(0, Ordering::SeqCst);
        let res = std::panic::catch_unwind(|| {
            let _ = <[PanicOnThird; 5]>::heap_init();
        });
        assert!(res.is_err());
        assert_eq!(INIT_COUNT.load(Ordering::SeqCst), 3);
        assert_eq!(
            DROP_COUNT.load(Ordering::SeqCst),
            2,
            "elements 0 and 1 must drop cleanly on panic at element 2"
        );
    }
}
