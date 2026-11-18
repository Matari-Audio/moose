use core::fmt::Debug;
use core::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// The sample types the primitives run at: `f32` and `f64`.
///
/// Sealed: every method forwards to the inherent `std` method of the same
/// name, which is what keeps results bit-identical to hand-written code.
pub trait Float:
    sealed::Sealed
    + Copy
    + Default
    + Debug
    + PartialOrd
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Neg<Output = Self>
    + AddAssign
    + SubAssign
    + MulAssign
{
    const ZERO: Self;
    const ONE: Self;
    const TWO: Self;
    const PI: Self;

    #[must_use]
    fn abs(self) -> Self;
    #[must_use]
    fn exp(self) -> Self;
    #[must_use]
    fn tan(self) -> Self;
    #[must_use]
    fn is_finite(self) -> bool;
}

macro_rules! float {
    ($t:ident) => {
        impl Float for $t {
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            const TWO: Self = 2.0;
            const PI: Self = core::$t::consts::PI;

            #[inline]
            fn abs(self) -> Self {
                $t::abs(self)
            }
            #[inline]
            fn exp(self) -> Self {
                $t::exp(self)
            }
            #[inline]
            fn tan(self) -> Self {
                $t::tan(self)
            }
            #[inline]
            fn is_finite(self) -> bool {
                $t::is_finite(self)
            }
        }
    };
}

float!(f32);
float!(f64);
