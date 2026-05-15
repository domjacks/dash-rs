/// Extension trait that makes float intrinsics available in `no_std` builds.
///
/// In stable Rust the `core_float_math` feature is not yet stable, so methods
/// such as `f64::floor`, `f64::exp`, `f64::log2`, etc. are unavailable when
/// compiling without `std`.  This trait fills the gap by delegating to `libm`.
///
/// The trait is only defined (and needs importing) for `no_std` builds; `std`
/// builds use the inherent `f64` methods directly.
#[cfg(not(feature = "std"))]
pub(crate) trait FloatExt {
    fn floor(self) -> Self;
    fn ceil(self) -> Self;
    fn round(self) -> Self;
    fn sqrt(self) -> Self;
    fn powi(self, n: i32) -> Self;
    fn powf(self, n: Self) -> Self;
    fn exp(self) -> Self;
    fn ln(self) -> Self;
    fn log2(self) -> Self;
    fn log10(self) -> Self;
}

#[cfg(not(feature = "std"))]
impl FloatExt for f64 {
    #[inline]
    fn floor(self) -> f64 {
        libm::floor(self)
    }
    #[inline]
    fn ceil(self) -> f64 {
        libm::ceil(self)
    }
    #[inline]
    fn round(self) -> f64 {
        libm::round(self)
    }
    #[inline]
    fn sqrt(self) -> f64 {
        libm::sqrt(self)
    }
    #[inline]
    fn powi(self, n: i32) -> f64 {
        libm::pow(self, n as f64)
    }
    #[inline]
    fn powf(self, n: f64) -> f64 {
        libm::pow(self, n)
    }
    #[inline]
    fn exp(self) -> f64 {
        libm::exp(self)
    }
    #[inline]
    fn ln(self) -> f64 {
        libm::log(self)
    }
    #[inline]
    fn log2(self) -> f64 {
        libm::log2(self)
    }
    #[inline]
    fn log10(self) -> f64 {
        libm::log10(self)
    }
}
