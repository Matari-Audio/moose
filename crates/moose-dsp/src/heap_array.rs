//! Heap-allocated fixed-size arrays built element-by-element directly on the
//! heap, without ever materializing `[T; N]` on the caller's stack.
//!
//! Rust has no placement-new: `Box::new([0.0f32; 65536])` and `Box::default()`
//! construct the 256 KiB array on the caller's stack first and then `memcpy`
//! it to the heap. Whether LLVM elides that copy depends on optimization level
//! and inlining (which changes across feature sets), causing silent stack
//! overflows on constrained host threads (Windows 1 MiB, macOS AU/secondary
//! 512 KiB, Linux musl 128 KiB).
//!
//! [`HeapArray`] allocates via `Vec::with_capacity(N)` and converts the buffer
//! in place via `into_boxed_slice().try_into()`, keeping stack usage `O(size_of::<T>())`.

use core::fmt;
use core::ops::{Deref, DerefMut};

/// A fixed-size array `[T; N]` stored on the heap (`Box<[T; N]>`), constructed
/// element-by-element directly on the heap so `[T; N]` never touches the stack.
pub struct HeapArray<T, const N: usize>(Box<[T; N]>);

impl<T, const N: usize> HeapArray<T, N> {
    /// Build a `HeapArray` by calling `f(index)` for each index `0..N`,
    /// writing each element directly into a pre-allocated heap buffer.
    ///
    /// Stack usage is `O(size_of::<T>())`, never `O(N * size_of::<T>())`.
    #[must_use]
    pub fn from_fn(mut f: impl FnMut(usize) -> T) -> Self {
        let mut vec = Vec::with_capacity(N);
        for i in 0..N {
            vec.push(f(i));
        }
        let boxed: Box<[T; N]> = vec
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!("Vec length is exactly N"));
        Self(boxed)
    }

    /// Build a `HeapArray` filled with `N` clones of `value`, directly on the
    /// heap without materializing `[T; N]` on the stack.
    #[must_use]
    pub fn splat(value: T) -> Self
    where
        T: Clone,
    {
        let boxed: Box<[T; N]> = vec![value; N]
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!("vec![value; N] length is exactly N"));
        Self(boxed)
    }

    /// Wrap an existing `Box<[T; N]>` without copying or reallocating.
    #[inline]
    #[must_use]
    pub fn from_boxed_array(boxed: Box<[T; N]>) -> Self {
        Self(boxed)
    }

    /// Unwrap into the underlying `Box<[T; N]>` without copying or reallocating.
    #[inline]
    #[must_use]
    pub fn into_boxed_array(self) -> Box<[T; N]> {
        self.0
    }

    /// Convert into a `Box<[T]>` without copying or reallocating.
    #[inline]
    #[must_use]
    pub fn into_boxed_slice(self) -> Box<[T]> {
        self.0
    }

    /// Borrow the elements as a shared slice.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0[..]
    }

    /// Borrow the elements as a mutable slice.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.0[..]
    }

    /// Iterate over shared references to elements.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.0.iter()
    }

    /// Iterate over mutable references to elements.
    #[inline]
    pub fn iter_mut(&mut self) -> core::slice::IterMut<'_, T> {
        self.0.iter_mut()
    }
}

impl<T: Default, const N: usize> Default for HeapArray<T, N> {
    fn default() -> Self {
        Self::from_fn(|_| T::default())
    }
}

impl<T: Clone, const N: usize> Clone for HeapArray<T, N> {
    fn clone(&self) -> Self {
        // Clone element-by-element on the heap so cloning a large `HeapArray`
        // never materializes `[T; N]` on the stack.
        Self::from_fn(|i| self.0[i].clone())
    }
}

impl<T, const N: usize> Deref for HeapArray<T, N> {
    type Target = [T; N];

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T, const N: usize> DerefMut for HeapArray<T, N> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T, const N: usize> AsRef<[T]> for HeapArray<T, N> {
    #[inline]
    fn as_ref(&self) -> &[T] {
        &self.0[..]
    }
}

impl<T, const N: usize> AsMut<[T]> for HeapArray<T, N> {
    #[inline]
    fn as_mut(&mut self) -> &mut [T] {
        &mut self.0[..]
    }
}

impl<T, const N: usize> AsRef<[T; N]> for HeapArray<T, N> {
    #[inline]
    fn as_ref(&self) -> &[T; N] {
        &self.0
    }
}

impl<T, const N: usize> AsMut<[T; N]> for HeapArray<T, N> {
    #[inline]
    fn as_mut(&mut self) -> &mut [T; N] {
        &mut self.0
    }
}

impl<T: fmt::Debug, const N: usize> fmt::Debug for HeapArray<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0[..], f)
    }
}

impl<T: PartialEq, const N: usize> PartialEq for HeapArray<T, N> {
    fn eq(&self, other: &Self) -> bool {
        self.0[..] == other.0[..]
    }
}

impl<T: Eq, const N: usize> Eq for HeapArray<T, N> {}

impl<T, const N: usize> From<Box<[T; N]>> for HeapArray<T, N> {
    #[inline]
    fn from(boxed: Box<[T; N]>) -> Self {
        Self(boxed)
    }
}

impl<T, const N: usize> From<HeapArray<T, N>> for Box<[T; N]> {
    #[inline]
    fn from(array: HeapArray<T, N>) -> Self {
        array.0
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a HeapArray<T, N> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a mut HeapArray<T, N> {
    type Item = &'a mut T;
    type IntoIter = core::slice::IterMut<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}

impl<T, const N: usize> IntoIterator for HeapArray<T, N> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        let slice: Box<[T]> = self.0;
        Vec::from(slice).into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::HeapArray;

    #[test]
    fn from_fn_and_splat_and_deref_work() {
        let mut arr = HeapArray::<u32, 8>::from_fn(|i| u32::try_from(i * 3).unwrap());
        assert_eq!(&*arr, &[0, 3, 6, 9, 12, 15, 18, 21]);
        arr[2] = 99;
        assert_eq!(arr[2], 99);

        let splatted = HeapArray::<f32, 4>::splat(1.25);
        assert_eq!(&*splatted, &[1.25; 4]);
        assert_eq!(splatted.clone(), splatted);
    }

    #[test]
    fn huge_array_builds_and_clones_on_tiny_128k_stack() {
        // 2 MiB array built and cloned on a 128 KiB stack thread: would
        // immediately overflow the guard page if `[u8; 2_097_152]` ever landed
        // on the stack.
        let handle = std::thread::Builder::new()
            .name("heap-array-128k".into())
            .stack_size(128 * 1024)
            .spawn(|| {
                let mut a = HeapArray::<u8, 2_097_152>::splat(0x5A);
                assert_eq!(a[0], 0x5A);
                assert_eq!(a[2_097_151], 0x5A);
                a[123_456] = 0xA5;

                let b = a.clone();
                assert_eq!(b[123_456], 0xA5);

                let c = HeapArray::<u32, 262_144>::from_fn(|i| u32::try_from(i & 0xFF).unwrap());
                assert_eq!(c[262_143], 255);
            })
            .expect("spawn 128 KiB thread");
        handle.join().expect("128 KiB thread completed cleanly");
    }
}
