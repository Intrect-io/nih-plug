//! Different ranges for numeric parameters.

use crate::util;

/// A distribution for a floating point parameter's range. All range endpoints are inclusive.
#[derive(Debug, Clone, Copy)]
pub enum FloatRange {
    /// The values are uniformly distributed between `min` and `max`.
    Linear { min: f32, max: f32 },
    /// The range is skewed by a factor. Values above 1.0 will make the end of the range wider,
    /// while values between 0 and 1 will skew the range towards the start. Use
    /// [`FloatRange::skew_factor()`] for a more intuitively way to calculate the skew factor where
    /// positive values skew the range towards the end while negative values skew the range toward
    /// the start.
    Skewed { min: f32, max: f32, factor: f32 },
    /// The same as [`FloatRange::Skewed`], but with the skewing happening from a central point.
    /// This central point is rescaled to be at 50% of the parameter's range for convenience of use.
    /// Git blame this comment to find a version that doesn't do this.
    SymmetricalSkewed {
        min: f32,
        max: f32,
        factor: f32,
        center: f32,
    },
    /// A reversed range that goes from high to low instead of from low to high.
    Reversed(&'static FloatRange),
}

/// A distribution for an integer parameter's range. All range endpoints are inclusive. Only linear
/// ranges are supported for integers since hosts expect discrete parameters to have a fixed step
/// size.
#[derive(Debug, Clone, Copy)]
pub enum IntRange {
    /// The values are uniformly distributed between `min` and `max`.
    Linear { min: i32, max: i32 },
    /// A reversed range that goes from high to low instead of from low to high.
    Reversed(&'static IntRange),
}

impl FloatRange {
    /// Calculate a skew factor for [`FloatRange::Skewed`] and [`FloatRange::SymmetricalSkewed`].
    /// Positive values make the end of the range wider while negative make the start of the range
    /// wider.
    pub fn skew_factor(factor: f32) -> f32 {
        2.0f32.powf(factor)
    }

    /// Calculate a skew factor for [`FloatRange::Skewed`] that makes a linear gain parameter range
    /// appear as if it was linear when formatted as decibels.
    pub fn gain_skew_factor(min_db: f32, max_db: f32) -> f32 {
        nih_debug_assert!(min_db < max_db);

        let min_gain = util::db_to_gain(min_db);
        let max_gain = util::db_to_gain(max_db);
        let middle_db = (max_db + min_db) / 2.0;
        let middle_gain = util::db_to_gain(middle_db);

        // Check the Skewed equation in the normalized function below, we need to solve the factor
        // such that the a normalized value of 0.5 resolves to the middle of the range
        0.5f32.log((middle_gain - min_gain) / (max_gain - min_gain))
    }

    /// Clamp a plain value to the range's bounds.
    ///
    /// [`f32::clamp()`] panics when `min > max` or when either bound is NaN. `FloatRange`'s fields
    /// are public, so a plugin can construct such a range. [`assert_validity()`][Self::
    /// assert_validity()] flags this during development; this keeps release builds from panicking
    /// during a parameter conversion.
    #[inline]
    fn clamp_to_bounds(plain: f32, min: f32, max: f32) -> f32 {
        if min <= max {
            plain.clamp(min, max)
        } else {
            min
        }
    }

    /// The range's lower bound, unwrapping any adapters. Used as a fallback for degenerate ranges.
    fn min_value(&self) -> f32 {
        match self {
            FloatRange::Linear { min, .. }
            | FloatRange::Skewed { min, .. }
            | FloatRange::SymmetricalSkewed { min, .. } => *min,
            FloatRange::Reversed(range) => range.min_value(),
        }
    }

    /// Normalize a plain, unnormalized value. Will be clamped to the bounds of the range if the
    /// normalized value exceeds `[0, 1]`.
    pub fn normalize(&self, plain: f32) -> f32 {
        let normalized = match self {
            FloatRange::Linear { min, max } => {
                (Self::clamp_to_bounds(plain, *min, *max) - min) / (max - min)
            }
            FloatRange::Skewed { min, max, factor } => {
                ((Self::clamp_to_bounds(plain, *min, *max) - min) / (max - min)).powf(*factor)
            }
            FloatRange::SymmetricalSkewed {
                min,
                max,
                factor,
                center,
            } => {
                // There's probably a much faster equivalent way to write this. Also, I have no clue
                // how I managed to implement this correctly on the first try.
                let unscaled_proportion = (Self::clamp_to_bounds(plain, *min, *max) - min)
                    / (max - min);
                let center_proportion = (center - min) / (max - min);
                if unscaled_proportion > center_proportion {
                    // The part above the center gets normalized to a [0, 1] range, skewed, and then
                    // unnormalized and scaled back to the original [center_proportion, 1] range
                    let scaled_proportion = (unscaled_proportion - center_proportion)
                        * (1.0 - center_proportion).recip();
                    (scaled_proportion.powf(*factor) * 0.5) + 0.5
                } else {
                    // The part below the center gets scaled, inverted (so the range is [0, 1] where
                    // 0 corresponds to the center proportion and 1 corresponds to the original
                    // normalized 0 value), skewed, inverted back again, and then scaled back to the
                    // original range
                    let inverted_scaled_proportion =
                        (center_proportion - unscaled_proportion) * (center_proportion).recip();
                    (1.0 - inverted_scaled_proportion.powf(*factor)) * 0.5
                }
            }
            FloatRange::Reversed(range) => 1.0 - range.normalize(plain),
        };

        // A zero width range, non-finite bounds, or an invalid skew factor would produce NaN or
        // infinity above. `assert_validity()` flags those during development, but hosts must never
        // be handed a non-normalized value.
        sanitize_normalized(normalized)
    }

    /// Unnormalize a normalized value. Will be clamped to `[0, 1]` if the plain, unnormalized value
    /// would exceed that range.
    pub fn unnormalize(&self, normalized: f32) -> f32 {
        let normalized = sanitize_normalized(normalized);
        let plain = match self {
            FloatRange::Linear { min, max } => (normalized * (max - min)) + min,
            FloatRange::Skewed { min, max, factor } => {
                (normalized.powf(factor.recip()) * (max - min)) + min
            }
            FloatRange::SymmetricalSkewed {
                min,
                max,
                factor,
                center,
            } => {
                // Reconstructing the subranges works the same as with the normal skewed ranges
                let center_proportion = (center - min) / (max - min);
                let skewed_proportion = if normalized > 0.5 {
                    let scaled_proportion = (normalized - 0.5) * 2.0;
                    (scaled_proportion.powf(factor.recip()) * (1.0 - center_proportion))
                        + center_proportion
                } else {
                    let inverted_scaled_proportion = (0.5 - normalized) * 2.0;
                    (1.0 - inverted_scaled_proportion.powf(factor.recip())) * center_proportion
                };

                (skewed_proportion * (max - min)) + min
            }
            FloatRange::Reversed(range) => range.unnormalize(1.0 - normalized),
        };

        // See `normalize()`
        if plain.is_finite() {
            plain
        } else {
            let min = self.min_value();
            if min.is_finite() {
                min
            } else {
                0.0
            }
        }
    }

    /// The range's previous discrete step from a certain value with a certain step size. If the
    /// step size is not set, then the normalized range is split into 50 segments instead. If
    /// `finer` is true, then this is upped to 200 segments.
    pub fn previous_step(&self, from: f32, step_size: Option<f32>, finer: bool) -> f32 {
        // This one's slightly more involved than the integer version. We'll split the normalized
        // range up into 50 segments, but if `self.step_size` would cause the range to be devided
        // into less than 50 segments then we'll use that.
        match self {
            FloatRange::Linear { min, max }
            | FloatRange::Skewed { min, max, .. }
            | FloatRange::SymmetricalSkewed { min, max, .. } => {
                let normalized_naive_step_size = if finer { 0.005 } else { 0.02 };
                let naive_step =
                    self.unnormalize(self.normalize(from) - normalized_naive_step_size);

                let stepped = match step_size {
                    // Use the naive step size if it is larger than the configured step size
                    Some(step_size) if (naive_step - from).abs() > step_size => {
                        self.snap_to_step(naive_step, step_size)
                    }
                    Some(step_size) => from - step_size,
                    None => naive_step,
                };

                Self::clamp_to_bounds(stepped, *min, *max)
            }
            FloatRange::Reversed(range) => range.next_step(from, step_size, finer),
        }
    }

    /// The range's next discrete step from a certain value with a certain step size. If the step
    /// size is not set, then the normalized range is split into 100 segments instead.
    pub fn next_step(&self, from: f32, step_size: Option<f32>, finer: bool) -> f32 {
        // See above
        match self {
            FloatRange::Linear { min, max }
            | FloatRange::Skewed { min, max, .. }
            | FloatRange::SymmetricalSkewed { min, max, .. } => {
                let normalized_naive_step_size = if finer { 0.005 } else { 0.02 };
                let naive_step =
                    self.unnormalize(self.normalize(from) + normalized_naive_step_size);

                let stepped = match step_size {
                    Some(step_size) if (naive_step - from).abs() > step_size => {
                        self.snap_to_step(naive_step, step_size)
                    }
                    Some(step_size) => from + step_size,
                    None => naive_step,
                };

                Self::clamp_to_bounds(stepped, *min, *max)
            }
            FloatRange::Reversed(range) => range.previous_step(from, step_size, finer),
        }
    }

    /// Snap a value to a step size, clamping to the minimum and maximum value of the range.
    pub fn snap_to_step(&self, value: f32, step_size: f32) -> f32 {
        match self {
            FloatRange::Linear { min, max }
            | FloatRange::Skewed { min, max, .. }
            | FloatRange::SymmetricalSkewed { min, max, .. } => {
                // There is nothing to snap to for a zero, negative, or non-finite step size. The
                // parameter types assert on this, this keeps the division from producing NaN.
                let snapped = if step_size > 0.0 && step_size.is_finite() {
                    (value / step_size).round() * step_size
                } else {
                    value
                };

                Self::clamp_to_bounds(snapped, *min, *max)
            }
            FloatRange::Reversed(range) => range.snap_to_step(value, step_size),
        }
    }

    /// Emits debug assertions to make sure that the range is usable: the bounds need to be finite
    /// and ordered, skew factors need to be positive and finite, and a symmetrical skew's center
    /// needs to lie strictly between the bounds.
    pub(super) fn assert_validity(&self) {
        match self {
            FloatRange::Linear { min, max } => Self::assert_bounds_validity(*min, *max),
            FloatRange::Skewed { min, max, factor } => {
                Self::assert_bounds_validity(*min, *max);
                Self::assert_skew_factor_validity(*factor);
            }
            FloatRange::SymmetricalSkewed {
                min,
                max,
                factor,
                center,
            } => {
                Self::assert_bounds_validity(*min, *max);
                Self::assert_skew_factor_validity(*factor);

                // The center is normalized against the range's bounds, and the proportions on
                // either side of it are divided by. A center on one of the bounds would make one of
                // those divisors zero.
                nih_debug_assert!(
                    *min < *center && *center < *max,
                    "The symmetrical skew's center ({}) needs to lie strictly between the range \
                     minimum ({}) and maximum ({})",
                    center,
                    min,
                    max
                );
            }
            FloatRange::Reversed(range) => range.assert_validity(),
        }
    }

    fn assert_bounds_validity(min: f32, max: f32) {
        nih_debug_assert!(
            min.is_finite() && max.is_finite(),
            "The range bounds ({}, {}) need to be finite numbers",
            min,
            max
        );
        nih_debug_assert!(
            min < max,
            "The range minimum ({}) needs to be less than the range maximum ({}) and they \
             cannot be equal",
            min,
            max
        );
    }

    fn assert_skew_factor_validity(factor: f32) {
        // The factor is used as an exponent on a `[0, 1]` proportion and its reciprocal is taken
        // when unnormalizing, so zero and negative factors do not describe a usable curve
        nih_debug_assert!(
            factor > 0.0 && factor.is_finite(),
            "The skew factor ({}) needs to be a positive, finite number",
            factor
        );
    }
}

/// Constrain a normalized value to `[0, 1]`, mapping non-finite values to `0.0`. [`f32::clamp()`]
/// propagates NaN, and hosts do not expect to receive it.
#[inline]
fn sanitize_normalized(normalized: f32) -> f32 {
    if normalized.is_finite() {
        normalized.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

impl IntRange {
    /// Clamp a plain value to the range's bounds. See
    /// [`FloatRange::clamp_to_bounds()`][FloatRange::clamp_to_bounds()] for why this doesn't use
    /// [`Ord::clamp()`] directly.
    #[inline]
    fn clamp_to_bounds(plain: i32, min: i32, max: i32) -> i32 {
        if min <= max {
            plain.clamp(min, max)
        } else {
            min
        }
    }

    /// Normalize a plain, unnormalized value. Will be clamped to the bounds of the range if the
    /// normalized value exceeds `[0, 1]`.
    pub fn normalize(&self, plain: i32) -> f32 {
        let normalized = match self {
            // These are widened to `i64` because the difference between two `i32`s does not
            // necessarily fit in an `i32`
            IntRange::Linear { min, max } => {
                (plain as i64 - *min as i64) as f32 / (*max as i64 - *min as i64) as f32
            }
            IntRange::Reversed(range) => 1.0 - range.normalize(plain),
        };

        // A zero width range would produce NaN above. `assert_validity()` flags that during
        // development, but hosts must never be handed a non-normalized value.
        sanitize_normalized(normalized)
    }

    /// Unnormalize a normalized value. Will be clamped to `[0, 1]` if the plain, unnormalized value
    /// would exceed that range.
    pub fn unnormalize(&self, normalized: f32) -> i32 {
        let normalized = sanitize_normalized(normalized);
        match self {
            IntRange::Linear { min, max } => {
                // See `normalize()` for why the range width is widened to `i64`. The multiplication
                // itself stays in `f32`: rounding a half-step lands on a different side in `f64`,
                // which would silently move every host written value that sits exactly between two
                // steps (e.g. 0.35 in a `0..=10` range).
                let plain = (normalized * (*max as i64 - *min as i64) as f32).round() as i64
                    + *min as i64;

                Self::clamp_to_bounds(
                    plain.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                    *min,
                    *max,
                )
            }
            IntRange::Reversed(range) => range.unnormalize(1.0 - normalized),
        }
    }

    /// The range's previous discrete step from a certain value.
    pub fn previous_step(&self, from: i32) -> i32 {
        match self {
            IntRange::Linear { min, max } => Self::clamp_to_bounds(from.saturating_sub(1), *min, *max),
            IntRange::Reversed(range) => range.next_step(from),
        }
    }

    /// The range's next discrete step from a certain value.
    pub fn next_step(&self, from: i32) -> i32 {
        match self {
            IntRange::Linear { min, max } => Self::clamp_to_bounds(from.saturating_add(1), *min, *max),
            IntRange::Reversed(range) => range.previous_step(from),
        }
    }

    /// The number of steps in this range. Used for the host's generic UI.
    pub fn step_count(&self) -> usize {
        match self {
            // A reversed range would otherwise wrap around to an enormous step count
            IntRange::Linear { min, max } => (*max as i64 - *min as i64).max(0) as usize,
            IntRange::Reversed(range) => range.step_count(),
        }
    }

    /// If this range is wrapped in an adapter, like `Reversed`, then return the wrapped range.
    pub fn inner_range(&self) -> Self {
        match self {
            IntRange::Linear { .. } => *self,
            IntRange::Reversed(range) => range.inner_range(),
        }
    }

    /// Emits debug assertions to make sure that range minima are always less than the maxima and
    /// that they are not equal.
    pub(super) fn assert_validity(&self) {
        match self {
            IntRange::Linear { min, max } => {
                nih_debug_assert!(
                    min < max,
                    "The range minimum ({}) needs to be less than the range maximum ({}) and they \
                     cannot be equal",
                    min,
                    max
                );
            }
            IntRange::Reversed(range) => range.assert_validity(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn make_linear_float_range() -> FloatRange {
        FloatRange::Linear {
            min: 10.0,
            max: 20.0,
        }
    }

    const fn make_linear_int_range() -> IntRange {
        IntRange::Linear { min: -10, max: 10 }
    }

    const fn make_skewed_float_range(factor: f32) -> FloatRange {
        FloatRange::Skewed {
            min: 10.0,
            max: 20.0,
            factor,
        }
    }

    const fn make_symmetrical_skewed_float_range(factor: f32) -> FloatRange {
        FloatRange::SymmetricalSkewed {
            min: 10.0,
            max: 20.0,
            factor,
            center: 12.5,
        }
    }

    #[test]
    fn step_size() {
        // These are weird step sizes, but if it works here then it will work for anything
        let range = make_linear_float_range();
        // XXX: We round to decimal places when outputting, but not when snapping to steps
        assert_eq!(range.snap_to_step(13.0, 4.73), 14.190001);
    }

    #[test]
    fn step_size_clamping() {
        let range = make_linear_float_range();
        assert_eq!(range.snap_to_step(10.0, 4.73), 10.0);
        assert_eq!(range.snap_to_step(20.0, 6.73), 20.0);
    }

    mod linear {
        use super::*;

        #[test]
        fn range_normalize_float() {
            let range = make_linear_float_range();
            assert_eq!(range.normalize(17.5), 0.75);
        }

        #[test]
        fn range_normalize_int() {
            let range = make_linear_int_range();
            assert_eq!(range.normalize(-5), 0.25);
        }

        #[test]
        fn range_unnormalize_float() {
            let range = make_linear_float_range();
            assert_eq!(range.unnormalize(0.25), 12.5);
        }

        #[test]
        fn range_unnormalize_int() {
            let range = make_linear_int_range();
            assert_eq!(range.unnormalize(0.75), 5);
        }

        #[test]
        fn range_unnormalize_int_rounding() {
            let range = make_linear_int_range();
            assert_eq!(range.unnormalize(0.73), 5);
        }

        /// Values that land exactly between two steps have to keep rounding the way they always
        /// have. Widening the multiplication to `f64` moves these to the next step down, which
        /// silently changes where every host written value snaps to.
        #[test]
        fn range_unnormalize_int_half_step_rounding() {
            let range = IntRange::Linear { min: 0, max: 10 };
            assert_eq!(range.unnormalize(0.35), 4);
            assert_eq!(range.unnormalize(0.45), 5);
            assert_eq!(range.unnormalize(0.65), 7);
            assert_eq!(range.unnormalize(0.95), 10);

            let range = IntRange::Linear { min: 0, max: 100 };
            assert_eq!(range.unnormalize(0.005), 1);
        }
    }

    mod skewed {
        use super::*;

        #[test]
        fn range_normalize_float() {
            let range = make_skewed_float_range(FloatRange::skew_factor(-2.0));
            assert_eq!(range.normalize(17.5), 0.9306049);
        }

        #[test]
        fn range_unnormalize_float() {
            let range = make_skewed_float_range(FloatRange::skew_factor(-2.0));
            assert_eq!(range.unnormalize(0.9306049), 17.5);
        }

        #[test]
        fn range_normalize_linear_equiv_float() {
            let linear_range = make_linear_float_range();
            let skewed_range = make_skewed_float_range(1.0);
            assert_eq!(linear_range.normalize(17.5), skewed_range.normalize(17.5));
        }

        #[test]
        fn range_unnormalize_linear_equiv_float() {
            let linear_range = make_linear_float_range();
            let skewed_range = make_skewed_float_range(1.0);
            assert_eq!(
                linear_range.unnormalize(0.25),
                skewed_range.unnormalize(0.25)
            );
        }
    }

    mod symmetrical_skewed {
        use super::*;

        #[test]
        fn range_normalize_float() {
            let range = make_symmetrical_skewed_float_range(FloatRange::skew_factor(-2.0));
            assert_eq!(range.normalize(17.5), 0.951801);
        }

        #[test]
        fn range_unnormalize_float() {
            let range = make_symmetrical_skewed_float_range(FloatRange::skew_factor(-2.0));
            assert_eq!(range.unnormalize(0.951801), 17.5);
        }
    }

    mod reversed_linear {
        use super::*;

        #[test]
        fn range_normalize_int() {
            const WRAPPED_RANGE: IntRange = make_linear_int_range();
            let range = IntRange::Reversed(&WRAPPED_RANGE);
            assert_eq!(range.normalize(-5), 1.0 - 0.25);
        }

        #[test]
        fn range_unnormalize_int() {
            const WRAPPED_RANGE: IntRange = make_linear_int_range();
            let range = IntRange::Reversed(&WRAPPED_RANGE);
            assert_eq!(range.unnormalize(1.0 - 0.75), 5);
        }

        #[test]
        fn range_unnormalize_int_rounding() {
            const WRAPPED_RANGE: IntRange = make_linear_int_range();
            let range = IntRange::Reversed(&WRAPPED_RANGE);
            assert_eq!(range.unnormalize(1.0 - 0.73), 5);
        }
    }

    /// `assert_validity()` flags these configurations during development, but since the range types
    /// have public fields a release build can still end up with them. None of the conversions may
    /// panic or produce NaN when that happens.
    mod invalid_configurations {
        use super::*;

        fn assert_is_normalized(normalized: f32, context: &str) {
            assert!(
                normalized.is_finite() && (0.0..=1.0).contains(&normalized),
                "{context}: {normalized}"
            );
        }

        #[test]
        fn zero_width_float_range() {
            let range = FloatRange::Linear {
                min: 5.0,
                max: 5.0,
            };

            assert_is_normalized(range.normalize(5.0), "zero width");
            assert!(range.unnormalize(0.5).is_finite());
        }

        #[test]
        fn reversed_float_bounds() {
            let range = FloatRange::Linear {
                min: 20.0,
                max: 10.0,
            };

            assert_is_normalized(range.normalize(15.0), "reversed bounds");
            assert!(range.unnormalize(0.5).is_finite());
            assert!(range.snap_to_step(15.0, 1.0).is_finite());
            assert!(range.next_step(15.0, Some(1.0), false).is_finite());
            assert!(range.previous_step(15.0, Some(1.0), false).is_finite());
        }

        #[test]
        fn non_finite_float_bounds() {
            for (min, max) in [(f32::NAN, 10.0), (0.0, f32::NAN), (0.0, f32::INFINITY)] {
                let range = FloatRange::Linear { min, max };

                assert_is_normalized(range.normalize(5.0), "non-finite bounds");
                assert!(range.unnormalize(0.5).is_finite(), "{min} - {max}");
            }
        }

        #[test]
        fn invalid_skew_factors() {
            for factor in [0.0, -1.0, f32::NAN, f32::INFINITY] {
                let range = make_skewed_float_range(factor);

                assert_is_normalized(range.normalize(15.0), &format!("factor {factor}"));
                assert!(range.unnormalize(0.5).is_finite(), "factor {factor}");
            }
        }

        #[test]
        fn symmetrical_skew_center_outside_of_the_range() {
            // 10.0 and 20.0 are the range's own bounds, where the proportion on one side of the
            // center collapses to zero and gets divided by
            for center in [5.0, 10.0, 20.0, 25.0] {
                let range = FloatRange::SymmetricalSkewed {
                    min: 10.0,
                    max: 20.0,
                    factor: 1.0,
                    center,
                };

                for plain in [10.0, 15.0, 20.0] {
                    assert_is_normalized(
                        range.normalize(plain),
                        &format!("center {center}, plain {plain}"),
                    );
                }
                for normalized in [0.0, 0.5, 1.0] {
                    assert!(
                        range.unnormalize(normalized).is_finite(),
                        "center {center}, normalized {normalized}"
                    );
                }
            }
        }

        #[test]
        fn invalid_step_sizes() {
            let range = make_linear_float_range();
            for step_size in [0.0, -1.0, f32::NAN, f32::INFINITY] {
                assert!(
                    range.snap_to_step(15.0, step_size).is_finite(),
                    "step size {step_size}"
                );
            }
        }

        #[test]
        fn zero_width_int_range() {
            let range = IntRange::Linear { min: 5, max: 5 };

            assert_is_normalized(range.normalize(5), "zero width");
            assert_eq!(range.unnormalize(0.5), 5);
            assert_eq!(range.step_count(), 0);
        }

        #[test]
        fn reversed_int_bounds() {
            let range = IntRange::Linear { min: 10, max: 0 };

            assert_is_normalized(range.normalize(5), "reversed bounds");
            assert_eq!(range.unnormalize(0.5), 10);
            assert_eq!(range.previous_step(5), 10);
            assert_eq!(range.next_step(5), 10);
            assert_eq!(range.step_count(), 0);
        }

        /// The difference between two `i32`s does not fit in an `i32`.
        #[test]
        fn full_width_int_range() {
            let range = IntRange::Linear {
                min: i32::MIN,
                max: i32::MAX,
            };

            assert_eq!(range.normalize(i32::MIN), 0.0);
            assert_eq!(range.normalize(i32::MAX), 1.0);
            assert_eq!(range.unnormalize(0.0), i32::MIN);
            assert_eq!(range.unnormalize(1.0), i32::MAX);
            assert_eq!(range.next_step(i32::MAX), i32::MAX);
            assert_eq!(range.previous_step(i32::MIN), i32::MIN);
            assert_eq!(range.step_count(), u32::MAX as usize);
        }
    }

    mod reversed_skewed {
        use super::*;

        #[test]
        fn range_normalize_float() {
            const WRAPPED_RANGE: FloatRange = make_skewed_float_range(0.25);
            let range = FloatRange::Reversed(&WRAPPED_RANGE);
            assert_eq!(range.normalize(17.5), 1.0 - 0.9306049);
        }

        #[test]
        fn range_unnormalize_float() {
            const WRAPPED_RANGE: FloatRange = make_skewed_float_range(0.25);
            let range = FloatRange::Reversed(&WRAPPED_RANGE);
            assert_eq!(range.unnormalize(1.0 - 0.9306049), 17.5);
        }

        #[test]
        fn range_normalize_linear_equiv_float() {
            const WRAPPED_LINEAR_RANGE: FloatRange = make_linear_float_range();
            const WRAPPED_SKEWED_RANGE: FloatRange = make_skewed_float_range(1.0);
            let linear_range = FloatRange::Reversed(&WRAPPED_LINEAR_RANGE);
            let skewed_range = FloatRange::Reversed(&WRAPPED_SKEWED_RANGE);
            assert_eq!(linear_range.normalize(17.5), skewed_range.normalize(17.5));
        }

        #[test]
        fn range_unnormalize_linear_equiv_float() {
            const WRAPPED_LINEAR_RANGE: FloatRange = make_linear_float_range();
            const WRAPPED_SKEWED_RANGE: FloatRange = make_skewed_float_range(1.0);
            let linear_range = FloatRange::Reversed(&WRAPPED_LINEAR_RANGE);
            let skewed_range = FloatRange::Reversed(&WRAPPED_SKEWED_RANGE);
            assert_eq!(
                linear_range.unnormalize(0.25),
                skewed_range.unnormalize(0.25)
            );
        }
    }
}
