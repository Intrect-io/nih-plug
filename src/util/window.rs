//! Windowing functions, useful in conjunction with [`StftHelper`][super::StftHelper].

use std::f32;

/// A Blackman window function with the 'standard' coefficients.
///
/// <https://en.wikipedia.org/wiki/Window_function#Blackman_window>
pub fn blackman(size: usize) -> Vec<f32> {
    let mut window = vec![0.0; size];
    blackman_in_place(&mut window);

    window
}

/// The same as [`blackman()`], but filling an existing slice instead.
///
/// An empty window is left untouched, and a single sample window is set to unity gain. Neither has
/// a shape to speak of, and the usual `size - 1` denominator is zero for both.
pub fn blackman_in_place(window: &mut [f32]) {
    let size = window.len();
    if size <= 1 {
        window.fill(1.0);
        return;
    }

    let scale_1 = (2.0 * f32::consts::PI) / (size - 1) as f32;
    let scale_2 = scale_1 * 2.0;
    for (i, sample) in window.iter_mut().enumerate() {
        let cos_1 = (scale_1 * i as f32).cos();
        let cos_2 = (scale_2 * i as f32).cos();
        *sample = 0.42 - (0.5 * cos_1) + (0.08 * cos_2);
    }
}

/// A Hann window function.
///
/// <https://en.wikipedia.org/wiki/Hann_function>
pub fn hann(size: usize) -> Vec<f32> {
    let mut window = vec![0.0; size];
    hann_in_place(&mut window);

    window
}

/// The same as [`hann()`], but filling an existing slice instead.
///
/// See [`blackman_in_place()`] for the zero and single sample behavior.
pub fn hann_in_place(window: &mut [f32]) {
    let size = window.len();
    if size <= 1 {
        window.fill(1.0);
        return;
    }

    // We want to scale `[0, size - 1]` to `[0, pi]`.
    // XXX: The `sin^2()` version results in weird rounding errors that cause spectral leakage
    let scale = (size as f32 - 1.0).recip() * f32::consts::TAU;
    for (i, sample) in window.iter_mut().enumerate() {
        let cos = (i as f32 * scale).cos();
        *sample = 0.5 - (0.5 * cos)
    }
}

/// Multiply a buffer with a window function.
///
/// The buffer and the window function need to have the same length. If they don't then only the
/// overlapping part is windowed, which silently leaves a tail of the buffer unprocessed or drops
/// window coefficients, so this is flagged during development.
#[inline]
pub fn multiply_with_window(buffer: &mut [f32], window_function: &[f32]) {
    nih_debug_assert_eq!(
        buffer.len(),
        window_function.len(),
        "The buffer and the window function need to have the same length"
    );

    // TODO: ALso use SIMD here if available
    for (sample, window_sample) in buffer.iter_mut().zip(window_function) {
        *sample *= window_sample;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `size - 1` is the denominator in both window functions, so zero and one sample windows used
    /// to underflow or produce NaN.
    #[test]
    fn degenerate_window_sizes() {
        assert!(blackman(0).is_empty());
        assert!(hann(0).is_empty());

        assert_eq!(blackman(1), vec![1.0]);
        assert_eq!(hann(1), vec![1.0]);

        for window in [blackman(2), hann(2), blackman(3), hann(3)] {
            assert!(
                window.iter().all(|sample| sample.is_finite()),
                "{window:?}"
            );
        }
    }
}
