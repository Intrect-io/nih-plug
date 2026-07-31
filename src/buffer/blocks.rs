//! Per-block per-channel per-sample iterators.

use std::marker::PhantomData;

#[cfg(feature = "simd")]
use std::simd::{LaneCount, Simd, SupportedLaneCount};

use super::SamplesIter;

/// An iterator over all samples in the buffer, slicing over the sample-dimension with a maximum
/// size of `max_block_size`. See [`Buffer::iter_blocks()`][super::Buffer::iter_blocks()]. Yields
/// both the block and the offset from the start of the buffer.
pub struct BlocksIter<'slice, 'sample: 'slice> {
    /// The raw output buffers.
    pub(super) buffers: *mut [&'sample mut [f32]],
    pub(super) max_block_size: usize,
    pub(super) current_block_start: usize,
    pub(super) _marker: PhantomData<&'slice mut [&'sample mut [f32]]>,
}

/// A block yielded by [`BlocksIter`]. Can be iterated over once or multiple times, and also
/// supports direct access to the block's samples if needed.
pub struct Block<'slice, 'sample: 'slice> {
    /// The raw output buffers.
    pub(self) buffers: *mut [&'sample mut [f32]],
    pub(self) current_block_start: usize,
    /// The index of the last sample in the block plus one.
    pub(self) current_block_end: usize,
    pub(self) _marker: PhantomData<&'slice mut [&'sample mut [f32]]>,
}

/// An iterator over all channels in a block yielded by [`Block`], returning an entire channel slice
/// at a time.
pub struct BlockChannelsIter<'slice, 'sample: 'slice> {
    /// The raw output buffers.
    pub(self) buffers: *mut [&'sample mut [f32]],
    pub(self) current_block_start: usize,
    pub(self) current_block_end: usize,
    pub(self) current_channel: usize,
    pub(self) _marker: PhantomData<&'slice mut [&'sample mut [f32]]>,
}

impl<'slice, 'sample> Iterator for BlocksIter<'slice, 'sample> {
    type Item = (usize, Block<'slice, 'sample>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // A zero block size would produce empty blocks without ever advancing `current_block_start`,
        // turning this into an infinite iterator. `Buffer::iter_blocks()` documents that the block
        // size must be positive and asserts this in debug builds, so yield nothing instead of
        // hanging the (potentially real-time) caller in release builds
        if self.max_block_size == 0 {
            return None;
        }

        let buffer_len = unsafe { (*self.buffers).first().map(|b| b.len()).unwrap_or(0) };
        if self.current_block_start < buffer_len {
            let current_block_start = self.current_block_start;
            let current_block_end =
                (self.current_block_start + self.max_block_size).min(buffer_len);
            let block = Block {
                buffers: self.buffers,
                current_block_start,
                current_block_end,
                _marker: self._marker,
            };

            self.current_block_start += self.max_block_size;

            Some((current_block_start, block))
        } else {
            None
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let buffer_len = unsafe { (*self.buffers).first().map(|b| b.len()).unwrap_or(0) };
        // This must count the blocks that are still left to yield, not the total number of blocks,
        // or the `ExactSizeIterator` implementation below would be incorrect after the first `next()`
        if self.max_block_size == 0 || self.current_block_start >= buffer_len {
            return (0, Some(0));
        }

        let remaining = (buffer_len - self.current_block_start).div_ceil(self.max_block_size);

        (remaining, Some(remaining))
    }
}

impl<'slice, 'sample> IntoIterator for Block<'slice, 'sample> {
    type Item = &'sample mut [f32];
    type IntoIter = BlockChannelsIter<'slice, 'sample>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        BlockChannelsIter {
            buffers: self.buffers,
            current_block_start: self.current_block_start,
            current_block_end: self.current_block_end,
            current_channel: 0,
            _marker: self._marker,
        }
    }
}

impl<'slice, 'sample> Iterator for BlockChannelsIter<'slice, 'sample> {
    type Item = &'sample mut [f32];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.current_channel < unsafe { (&(*self.buffers)).len() } {
            // SAFETY: These bounds have already been checked
            // SAFETY: It is also not possible to have multiple mutable references to the same
            //         sample at the same time
            let slice = unsafe {
                (&mut (*self.buffers))
                    .get_unchecked_mut(self.current_channel)
                    .get_unchecked_mut(self.current_block_start..self.current_block_end)
            };

            self.current_channel += 1;

            Some(slice)
        } else {
            None
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = unsafe { (&(*self.buffers)).len() } - self.current_channel;

        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for BlocksIter<'_, '_> {}
impl ExactSizeIterator for BlockChannelsIter<'_, '_> {}

impl<'slice, 'sample> Block<'slice, 'sample> {
    /// Get the number of samples per channel in the block.
    #[inline]
    pub fn samples(&self) -> usize {
        self.current_block_end - self.current_block_start
    }

    /// Returns the number of channels in this buffer.
    #[inline]
    pub fn channels(&self) -> usize {
        unsafe { (&(*self.buffers)).len() }
    }

    /// A resetting iterator. This lets you iterate over the same block multiple times. Otherwise
    /// you don't need to use this function as [`Block`] already implements [`Iterator`]. You can
    /// also use the direct accessor functions on this block instead.
    #[inline]
    pub fn iter_mut(&mut self) -> BlockChannelsIter<'slice, 'sample> {
        BlockChannelsIter {
            buffers: self.buffers,
            current_block_start: self.current_block_start,
            current_block_end: self.current_block_end,
            current_channel: 0,
            _marker: self._marker,
        }
    }

    /// Iterate over this block on a per-sample per-channel basis. This is identical to
    /// [`Buffer::iter_samples()`][super::Buffer::iter_samples()] but for a smaller block instead of
    /// the entire buffer
    #[inline]
    pub fn iter_samples(&mut self) -> SamplesIter<'slice, 'sample> {
        SamplesIter {
            buffers: self.buffers,
            current_sample: self.current_block_start,
            samples_end: self.current_block_end,
            _marker: self._marker,
        }
    }

    /// Access a channel by index. Useful when you would otherwise iterate over this [`Block`]
    /// multiple times.
    #[inline]
    pub fn get(&self, channel_index: usize) -> Option<&[f32]> {
        // SAFETY: The block bound has already been checked
        unsafe {
            Some(
                (&(*self.buffers))
                    .get(channel_index)?
                    .get_unchecked(self.current_block_start..self.current_block_end),
            )
        }
    }

    /// The same as [`get()`][Self::get], but without any bounds checking.
    ///
    /// # Safety
    ///
    /// `channel_index` must be in the range `0..Self::len()`.
    #[inline]
    pub unsafe fn get_unchecked(&self, channel_index: usize) -> &[f32] {
        (&(*self.buffers))
            .get_unchecked(channel_index)
            .get_unchecked(self.current_block_start..self.current_block_end)
    }

    /// Access a mutable channel by index. Useful when you would otherwise iterate over this
    /// [`Block`] multiple times.
    #[inline]
    pub fn get_mut(&mut self, channel_index: usize) -> Option<&mut [f32]> {
        // SAFETY: The block bound has already been checked
        unsafe {
            Some(
                (&mut (*self.buffers))
                    .get_mut(channel_index)?
                    .get_unchecked_mut(self.current_block_start..self.current_block_end),
            )
        }
    }

    /// The same as [`get_mut()`][Self::get_mut], but without any bounds checking.
    ///
    /// # Safety
    ///
    /// `channel_index` must be in the range `0..Self::len()`.
    #[inline]
    pub unsafe fn get_unchecked_mut(&mut self, channel_index: usize) -> &mut [f32] {
        (&mut (*self.buffers))
            .get_unchecked_mut(channel_index)
            .get_unchecked_mut(self.current_block_start..self.current_block_end)
    }

    /// Get a SIMD vector containing the channel data for a specific sample in this block. If `LANES
    /// > channels.len()` then this will be padded with zeroes. If `LANES < channels.len()` then
    /// this won't contain all values.
    ///
    /// Returns a `None` value if `sample_index` is out of bounds.
    #[cfg(feature = "simd")]
    #[inline]
    pub fn to_channel_simd<const LANES: usize>(
        &self,
        sample_index: usize,
    ) -> Option<Simd<f32, LANES>>
    where
        LaneCount<LANES>: SupportedLaneCount,
    {
        if sample_index >= self.samples() {
            return None;
        }

        // The vector is `LANES` channels wide, but the buffer may contain fewer channels than that.
        // Any remaining lanes stay zeroed, as documented above.
        let used_lanes = self.channels().min(LANES);
        let mut values = [0.0; LANES];
        for (channel_idx, value) in values.iter_mut().enumerate().take(used_lanes) {
            *value = unsafe {
                *(&(*self.buffers))
                    .get_unchecked(channel_idx)
                    .get_unchecked(self.current_block_start + sample_index)
            };
        }

        Some(Simd::from_array(values))
    }

    /// Get a SIMD vector containing the channel data for a specific sample in this block. Will
    /// always read exactly `LANES` channels, and does not perform bounds checks on `sample_index`.
    ///
    /// # Safety
    ///
    /// Undefined behavior if `LANES > block.len()` or if `sample_index > block.len()`.
    #[cfg(feature = "simd")]
    #[inline]
    pub unsafe fn to_channel_simd_unchecked<const LANES: usize>(
        &self,
        sample_index: usize,
    ) -> Simd<f32, LANES>
    where
        LaneCount<LANES>: SupportedLaneCount,
    {
        let mut values = [0.0; LANES];
        for (channel_idx, value) in values.iter_mut().enumerate() {
            *value = *(&(*self.buffers))
                .get_unchecked(channel_idx)
                .get_unchecked(self.current_block_start + sample_index);
        }

        Simd::from_array(values)
    }

    /// Write data from a SIMD vector to this sample's channel data for a specific sample in this
    /// block. This takes the padding added by [`to_channel_simd()`][Self::to_channel_simd()] into
    /// account.
    ///
    /// Returns `false` if `sample_index` is out of bounds.
    #[cfg(feature = "simd")]
    #[allow(clippy::wrong_self_convention)]
    #[inline]
    pub fn from_channel_simd<const LANES: usize>(
        &mut self,
        sample_index: usize,
        vector: Simd<f32, LANES>,
    ) -> bool
    where
        LaneCount<LANES>: SupportedLaneCount,
    {
        if sample_index >= self.samples() {
            return false;
        }

        // See `to_channel_simd()`, lanes past the buffer's channel count are dropped
        let used_lanes = self.channels().min(LANES);
        let values = vector.to_array();
        for (channel_idx, value) in values.into_iter().enumerate().take(used_lanes) {
            *unsafe {
                (&mut (*self.buffers))
                    .get_unchecked_mut(channel_idx)
                    .get_unchecked_mut(self.current_block_start + sample_index)
            } = value;
        }

        true
    }

    /// Write data from a SIMD vector to this sample's channel data for a specific sample in this
    /// block.. This assumes `LANES` matches exactly with the number of channels in the buffer, and
    /// does not perform bounds checks on `sample_index`.
    ///
    /// # Safety
    ///
    /// Undefined behavior if `LANES > block.len()` or if `sample_index > block.len()`.
    #[cfg(feature = "simd")]
    #[allow(clippy::wrong_self_convention)]
    #[inline]
    pub unsafe fn from_channel_simd_unchecked<const LANES: usize>(
        &mut self,
        sample_index: usize,
        vector: Simd<f32, LANES>,
    ) where
        LaneCount<LANES>: SupportedLaneCount,
    {
        let values = vector.to_array();
        for (channel_idx, value) in values.into_iter().enumerate() {
            *(&mut (*self.buffers))
                .get_unchecked_mut(channel_idx)
                .get_unchecked_mut(self.current_block_start + sample_index) = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Buffer::iter_blocks()` debug-asserts that the block size is positive, so this constructs the
    /// iterator directly to check the release-build behavior: yielding nothing instead of looping
    /// forever.
    #[test]
    fn zero_block_size_yields_nothing() {
        let mut real_buffers = vec![vec![0.0f32; 32]; 2];
        let (first_channel, other_channels) = real_buffers.split_at_mut(1);
        let mut slices: Vec<&mut [f32]> =
            vec![first_channel[0].as_mut_slice(), other_channels[0].as_mut_slice()];

        let mut blocks = BlocksIter {
            buffers: slices.as_mut_slice(),
            max_block_size: 0,
            current_block_start: 0,
            _marker: PhantomData,
        };

        assert_eq!(blocks.size_hint(), (0, Some(0)));
        assert!(blocks.next().is_none());
    }

    #[cfg(feature = "simd")]
    mod simd {
        use super::*;

        fn with_block(f: impl FnOnce(&mut Block)) {
            let mut real_buffers = vec![vec![0.0f32; 8]; 2];
            let (first_channel, other_channels) = real_buffers.split_at_mut(1);
            let mut slices: Vec<&mut [f32]> = vec![
                first_channel[0].as_mut_slice(),
                other_channels[0].as_mut_slice(),
            ];

            let mut block = Block {
                buffers: slices.as_mut_slice(),
                current_block_start: 0,
                current_block_end: 4,
                _marker: PhantomData,
            };
            f(&mut block);
        }

        /// The last valid sample index is `samples() - 1`, an index equal to the block length used
        /// to read one sample past the end of the block.
        #[test]
        fn channel_simd_rejects_out_of_bounds_sample_indices() {
            with_block(|block| {
                assert!(block.to_channel_simd::<2>(block.samples() - 1).is_some());
                assert!(block.to_channel_simd::<2>(block.samples()).is_none());
                assert!(!block.from_channel_simd::<2>(block.samples(), Simd::splat(1.0)));
            });
        }

        /// Vectors wider than the block's channel count must be zero padded, not read from channels
        /// that don't exist.
        #[test]
        fn channel_simd_is_padded_to_the_channel_count() {
            with_block(|block| {
                assert_eq!(block.channels(), 2);

                let vector = block.to_channel_simd::<4>(0).unwrap();
                assert_eq!(vector.to_array(), [0.0; 4]);

                // The two lanes past the channel count must be dropped instead of writing out of
                // bounds
                assert!(block.from_channel_simd::<4>(0, Simd::splat(1.0)));
                assert_eq!(block.to_channel_simd::<4>(0).unwrap().to_array(), [1.0, 1.0, 0.0, 0.0]);
            });
        }
    }
}
