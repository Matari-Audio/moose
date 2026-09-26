//! Block-rate audio ops at `f64`. Mirror of [`crate::ops`] (f32);
//! see that module for the per-op contracts. The wider lane width
//! means `f64x4` SIMD lanes for the same `wide` backend; chunk
//! granularity is 4 instead of 8.

#[inline]
pub fn gain_block_scalar(buf: &mut [f64], gain: f64) {
    for s in buf {
        *s *= gain;
    }
}

#[inline]
pub fn gain_block(buf: &mut [f64], gain: f64) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let g = f64x4::splat(gain);
        let n = buf.len();
        let n4 = n / 4 * 4;
        let (head, tail) = buf.split_at_mut(n4);
        for chunk in head.chunks_exact_mut(4) {
            let v = f64x4::from(<[f64; 4]>::try_from(&chunk[..]).unwrap_or_default());
            chunk.copy_from_slice((v * g).as_array_ref());
        }
        gain_block_scalar(tail, gain);
    }
    #[cfg(not(feature = "wide-backend"))]
    gain_block_scalar(buf, gain);
}

#[inline]
pub fn scale_block_scalar(out: &mut [f64], src: &[f64], scale: f64) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] = src[i] * scale;
    }
}

#[inline]
pub fn scale_block(out: &mut [f64], src: &[f64], scale: f64) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let n4 = n / 4 * 4;
        let g = f64x4::splat(scale);
        let (out_v, out_tail) = out[..n].split_at_mut(n4);
        let src_v = &src[..n4];
        let src_tail = &src[n4..n];
        for (out_chunk, src_chunk) in out_v.chunks_exact_mut(4).zip(src_v.chunks_exact(4)) {
            let v = f64x4::from(<[f64; 4]>::try_from(src_chunk).unwrap_or_default());
            out_chunk.copy_from_slice((v * g).as_array_ref());
        }
        scale_block_scalar(out_tail, src_tail, scale);
    }
    #[cfg(not(feature = "wide-backend"))]
    scale_block_scalar(out, src, scale);
}

#[inline]
pub fn mul_block_scalar(out: &mut [f64], a: &[f64], b: &[f64]) {
    let n = out.len().min(a.len()).min(b.len());
    for i in 0..n {
        out[i] = a[i] * b[i];
    }
}

#[inline]
pub fn mul_block(out: &mut [f64], a: &[f64], b: &[f64]) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(a.len()).min(b.len());
        let n4 = n / 4 * 4;
        let (out_v, out_tail) = out[..n].split_at_mut(n4);
        let a_v = &a[..n4];
        let b_v = &b[..n4];
        let a_tail = &a[n4..n];
        let b_tail = &b[n4..n];
        for ((out_chunk, a_chunk), b_chunk) in out_v
            .chunks_exact_mut(4)
            .zip(a_v.chunks_exact(4))
            .zip(b_v.chunks_exact(4))
        {
            let av = f64x4::from(<[f64; 4]>::try_from(a_chunk).unwrap_or_default());
            let bv = f64x4::from(<[f64; 4]>::try_from(b_chunk).unwrap_or_default());
            out_chunk.copy_from_slice((av * bv).as_array_ref());
        }
        mul_block_scalar(out_tail, a_tail, b_tail);
    }
    #[cfg(not(feature = "wide-backend"))]
    mul_block_scalar(out, a, b);
}

#[inline]
pub fn mac_block_scalar(out: &mut [f64], src: &[f64], scale: f64) {
    let n = out.len().min(src.len());
    for i in 0..n {
        out[i] += src[i] * scale;
    }
}

#[inline]
pub fn mac_block(out: &mut [f64], src: &[f64], scale: f64) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(src.len());
        let n4 = n / 4 * 4;
        let (out_v, out_tail) = out[..n].split_at_mut(n4);
        let src_v = &src[..n4];
        let src_tail = &src[n4..n];
        let s = f64x4::splat(scale);
        for (out_chunk, src_chunk) in out_v.chunks_exact_mut(4).zip(src_v.chunks_exact(4)) {
            let ov = f64x4::from(<[f64; 4]>::try_from(&out_chunk[..]).unwrap_or_default());
            let sv = f64x4::from(<[f64; 4]>::try_from(src_chunk).unwrap_or_default());
            out_chunk.copy_from_slice((ov + sv * s).as_array_ref());
        }
        mac_block_scalar(out_tail, src_tail, scale);
    }
    #[cfg(not(feature = "wide-backend"))]
    mac_block_scalar(out, src, scale);
}

#[inline]
pub fn mix_block_scalar(out: &mut [f64], a: &[f64], gain_a: f64, b: &[f64], gain_b: f64) {
    let n = out.len().min(a.len()).min(b.len());
    for i in 0..n {
        out[i] = a[i] * gain_a + b[i] * gain_b;
    }
}

#[inline]
pub fn mix_block(out: &mut [f64], a: &[f64], gain_a: f64, b: &[f64], gain_b: f64) {
    #[cfg(feature = "wide-backend")]
    {
        use wide::f64x4;
        let n = out.len().min(a.len()).min(b.len());
        let n4 = n / 4 * 4;
        let (out_v, out_tail) = out[..n].split_at_mut(n4);
        let a_v = &a[..n4];
        let b_v = &b[..n4];
        let a_tail = &a[n4..n];
        let b_tail = &b[n4..n];
        let ga = f64x4::splat(gain_a);
        let gb = f64x4::splat(gain_b);
        for ((out_chunk, a_chunk), b_chunk) in out_v
            .chunks_exact_mut(4)
            .zip(a_v.chunks_exact(4))
            .zip(b_v.chunks_exact(4))
        {
            let av = f64x4::from(<[f64; 4]>::try_from(a_chunk).unwrap_or_default());
            let bv = f64x4::from(<[f64; 4]>::try_from(b_chunk).unwrap_or_default());
            out_chunk.copy_from_slice((av * ga + bv * gb).as_array_ref());
        }
        mix_block_scalar(out_tail, a_tail, gain_a, b_tail, gain_b);
    }
    #[cfg(not(feature = "wide-backend"))]
    mix_block_scalar(out, a, gain_a, b, gain_b);
}

#[inline]
pub fn copy_block(out: &mut [f64], src: &[f64]) {
    let n = out.len().min(src.len());
    out[..n].copy_from_slice(&src[..n]);
}

#[inline]
pub fn zero_block(buf: &mut [f64]) {
    buf.fill(0.0);
}

#[inline]
#[must_use]
pub fn abs_max_block(buf: &[f64]) -> f64 {
    let mut peak = 0.0_f64;
    for &v in buf {
        if v.is_nan() {
            return f64::NAN;
        }
        let a = v.abs();
        if a > peak {
            peak = a;
        }
    }
    peak
}
