//! Helpers for safely constructing [`Buffer`]s from a plugin host's audio buffers.

use std::num::NonZeroU32;
use std::ptr::NonNull;

use crate::prelude::{AudioIOLayout, Buffer};

/// Buffers created using [`create_buffers`]. At some point the main `Plugin::process()` should
/// probably also take an argument like this instead of main+aux buffers if we also want to provide
/// access to overflowing input channels for e.g. stereo to mono plugins.
pub struct Buffers<'a, 'buffer: 'a> {
    pub main_buffer: &'a mut Buffer<'buffer>,

    // We can't use `AuxiliaryBuffers` here directly because we need different lifetimes for `'a`
    // and `'buffer` while `AuxiliaryBuffers` uses the same lifetime for both.
    pub aux_inputs: &'a mut [Buffer<'buffer>],
    pub aux_outputs: &'a mut [Buffer<'buffer>],
}

/// A helper for safely creating and initializing [`Buffer`]s based on the host's input and output
/// buffers.
pub struct BufferManager {
    // These are the storage backing the fields in `BufferSource`. The wrapper needs to set these
    // values to match the channel pointers provided by the host. If audio buffers are not provided
    // for a bus, then they should be set to `None`. This helper will then copy data to the buffers
    // or fill them with zeroes if there is no data, while also accounting for in-place main IO
    // buffers.
    main_input_channel_pointers: Option<ChannelPointers>,
    main_output_channel_pointers: Option<ChannelPointers>,
    aux_input_channel_pointers: Vec<Option<ChannelPointers>>,
    aux_output_channel_pointers: Vec<Option<ChannelPointers>>,

    /// The block size the backing buffers were preallocated for. `create_buffers()` must not
    /// allocate, so blocks larger than this are truncated instead of growing the storage.
    max_buffer_size: usize,

    /// The backing buffers that will be filled during `create_buffers`. This `'static` lifetime
    /// will be shortened when returning a reference to these buffers in `create_buffers` to match
    /// the function's lifetime.
    main_buffer: Buffer<'static>,

    aux_input_buffers: Vec<Buffer<'static>>,
    /// Stores the data to back `aux_input_buffers`. We need to copy the host's auxiliary input
    /// buffers to our own first because the `Buffer` API is designed around mutable buffers, and
    /// the host may reuse its input buffers between plugins.
    aux_input_storage: Vec<Vec<Vec<f32>>>,

    aux_output_buffers: Vec<Buffer<'static>>,
}

// SAFETY: The raw pointers in the `ChannelPointers` fields/vectors are only used as scratch storage
//         inside of the `create_buffers()` function.
unsafe impl Send for BufferManager {}
unsafe impl Sync for BufferManager {}

/// Host data that the plugin's [`Buffer`]s should be created from. Leave these fields as `None`
/// values
#[derive(Debug)]
pub struct BufferSource<'a> {
    pub main_input_channel_pointers: &'a mut Option<ChannelPointers>,
    pub main_output_channel_pointers: &'a mut Option<ChannelPointers>,
    pub aux_input_channel_pointers: &'a mut [Option<ChannelPointers>],
    pub aux_output_channel_pointers: &'a mut [Option<ChannelPointers>],
}

/// Pointers to raw multichannel audio data for this port.
#[derive(Debug, Clone, Copy)]
pub struct ChannelPointers {
    /// A raw pointer to an array of f32 arrays, containing one array for each channel. `ptrs` must
    /// contain (at least) `num_channel` `*const f32`s, and each of those inner arrays must contain
    /// (at least) `num_samples` `f32` values.
    pub ptrs: NonNull<*mut f32>,
    /// The number of audio channels used for this port.
    pub num_channels: usize,
}

impl BufferManager {
    /// Initialize managed buffers for a specific audio IO layout. The actual buffers can be set up
    /// using channel pointer data using [`create_buffers()`][Self::create_buffers()].
    pub fn for_audio_io_layout(max_buffer_size: usize, audio_io_layout: AudioIOLayout) -> Self {
        nih_debug_assert!(
            audio_io_layout
                .main_input_channels
                .map(NonZeroU32::get)
                .unwrap_or(0)
                <= audio_io_layout
                    .main_output_channels
                    .map(NonZeroU32::get)
                    .unwrap_or(0),
            "Stereo-to-mono and other many-to-few audio channel configurations are currently not \
             supported"
        );

        // The buffers are preallocated so that `create_buffers()` can be called without having to
        // allocate
        let mut main_buffer = Buffer::default();
        unsafe {
            main_buffer.set_slices(0, |output_slices| {
                output_slices.resize_with(
                    audio_io_layout
                        .main_output_channels
                        .map(NonZeroU32::get)
                        .unwrap_or(0) as usize,
                    || &mut [],
                );
            })
        };

        let mut aux_input_buffers = Vec::with_capacity(audio_io_layout.aux_input_ports.len());
        let mut aux_input_storage = Vec::with_capacity(audio_io_layout.aux_input_ports.len());
        for num_channels in audio_io_layout.aux_input_ports {
            let mut buffer = Buffer::default();
            unsafe {
                buffer.set_slices(0, |slices| {
                    slices.resize_with(num_channels.get() as usize, || &mut []);
                })
            };

            aux_input_buffers.push(buffer);
            aux_input_storage.push(vec![
                vec![0.0; max_buffer_size];
                num_channels.get() as usize
            ]);
        }

        let mut aux_output_buffers = Vec::with_capacity(audio_io_layout.aux_output_ports.len());
        for num_channels in audio_io_layout.aux_output_ports {
            let mut buffer = Buffer::default();
            unsafe {
                buffer.set_slices(0, |slices| {
                    slices.resize_with(num_channels.get() as usize, || &mut []);
                })
            };

            aux_output_buffers.push(buffer);
        }

        Self {
            main_input_channel_pointers: None,
            main_output_channel_pointers: None,
            aux_input_channel_pointers: vec![None; audio_io_layout.aux_input_ports.len()],
            aux_output_channel_pointers: vec![None; audio_io_layout.aux_output_ports.len()],

            max_buffer_size,

            main_buffer,

            aux_input_buffers,
            aux_input_storage,

            aux_output_buffers,
        }
    }

    /// Initialize the buffers using the host provided buffer pointers and return a reference to the
    /// created buffers that can be passed to `Plugin::process()`. This accounts for in-place main
    /// IO, missing channel pointers, null pointers, and mismatching channel counts. All
    /// uninitialized buffer data (aux outputs, and main output channels with no matching input
    /// channel) are filled with zeroes.
    ///
    /// `sample_offset` and `num_samples` can be used to slice a set of host channel pointers for
    /// sample accurate automation. If any of the outputs are missing because the host hasn't
    /// provided enough channels or outputs, then they will be replaced by empty slices.
    ///
    /// # Panics
    ///
    /// May panic if one of the inner channel pointers is a null pointer.
    ///
    /// # Safety
    ///
    /// Any provided `ChannelPointers` must point to memory regions that remain valid to read from
    /// or write to for the lifetime of the returned [`Buffers`].
    pub unsafe fn create_buffers<'a, 'buffer: 'a>(
        &'a mut self,
        sample_offset: usize,
        num_samples: usize,
        set_buffer_sources: impl FnOnce(&mut BufferSource),
    ) -> Buffers<'a, 'buffer> {
        // The auxiliary input storage was preallocated for `max_buffer_size` samples, so a larger
        // block makes the copy below reallocate on the audio thread. Truncating the block instead
        // would silently drop the tail of the host's buffer — a correctness bug in exchange for a
        // latency spike — so this only flags the host's contract violation.
        nih_debug_assert!(
            num_samples <= self.max_buffer_size,
            "The host asked for a {} sample block, but the buffers were allocated for at most {} \
             samples. The auxiliary input storage has to grow to match.",
            num_samples,
            self.max_buffer_size
        );

        // Make sure the caller can't forget to unset previously set values
        self.main_input_channel_pointers = None;
        self.main_output_channel_pointers = None;
        self.aux_input_channel_pointers.fill(None);
        self.aux_output_channel_pointers.fill(None);
        set_buffer_sources(&mut BufferSource {
            main_input_channel_pointers: &mut self.main_input_channel_pointers,
            main_output_channel_pointers: &mut self.main_output_channel_pointers,
            aux_input_channel_pointers: &mut self.aux_input_channel_pointers,
            aux_output_channel_pointers: &mut self.aux_output_channel_pointers,
        });

        // The main buffer points directly to the main output pointers
        self.main_buffer.set_slices(num_samples, |output_slices| {
            match self.main_output_channel_pointers {
                Some(output_channel_pointers) => {
                    nih_debug_assert_eq!(output_slices.len(), output_channel_pointers.num_channels);
                    for (channel_idx, output_slice) in output_slices
                        .iter_mut()
                        .enumerate()
                        .take(output_channel_pointers.num_channels)
                    {
                        let output_channel_pointer =
                            output_channel_pointers.ptrs.as_ptr().add(channel_idx);

                        *output_slice = std::slice::from_raw_parts_mut(
                            (*output_channel_pointer).add(sample_offset),
                            num_samples,
                        );
                    }

                    // If the caller/host should have provided buffer pointers but didn't then we
                    // must get rid of any dangling slices. The host may also report more channels
                    // than this layout was configured for, in which case there is nothing left to
                    // clear.
                    let assigned_channels =
                        output_channel_pointers.num_channels.min(output_slices.len());
                    output_slices[assigned_channels..].fill_with(|| &mut [])
                }
                None => {
                    nih_debug_assert_eq!(output_slices.len(), 0);

                    // Same as above
                    output_slices.fill_with(|| &mut [])
                }
            }
        });

        // Since NIH-plug processes audio in-place, main input data needs to be copied to the main
        // output buffers
        if let (Some(input_channel_pointers), Some(output_channel_pointers)) = (
            self.main_input_channel_pointers,
            self.main_output_channel_pointers,
        ) {
            // Both pointer arrays are indexed with the same channel index below, so the copy needs
            // to stop at whichever of the two is shorter. A host reporting more input than output
            // channels would otherwise read past the end of the output pointer array.
            let copied_channels = input_channel_pointers
                .num_channels
                .min(output_channel_pointers.num_channels);
            nih_debug_assert_eq!(copied_channels, input_channel_pointers.num_channels);

            self.main_buffer.set_slices(num_samples, |output_slices| {
                for (channel_idx, output_slice) in
                    output_slices.iter_mut().enumerate().take(copied_channels)
                {
                    let input_channel_pointer =
                        *input_channel_pointers.ptrs.as_ptr().add(channel_idx);
                    let output_channel_pointer =
                        *output_channel_pointers.ptrs.as_ptr().add(channel_idx);

                    // If the host processes the main IO out of place then the inputs need to be
                    // copied to the output buffers. Otherwise the input should already be there.
                    if input_channel_pointer != output_channel_pointer {
                        output_slice.copy_from_slice(std::slice::from_raw_parts_mut(
                            input_channel_pointer.add(sample_offset),
                            num_samples,
                        ))
                    }
                }
            });

            // Any excess channels will need to be filled with zeroes since they'd otherwise point
            // to whatever was left in the buffer
            if copied_channels < output_channel_pointers.num_channels {
                self.main_buffer.set_slices(num_samples, |output_slices| {
                    // The host may report more output channels than this layout was configured for
                    let uncopied_channels = copied_channels.min(output_slices.len());
                    for slice in &mut output_slices[uncopied_channels..] {
                        slice.fill(0.0);
                    }
                });
            }
        }

        // Because NIH-plug's `Buffer` type is geared around in-place processing, auxiliary inputs
        // need to be copied to our own buffers first (backed by the 'storage' vectors on this
        // object). That way the plugin can modify those buffers like any other buffers.
        for (input_channel_pointers, (input_storage, input_buffer)) in
            self.aux_input_channel_pointers.iter().zip(
                self.aux_input_storage
                    .iter_mut()
                    .zip(self.aux_input_buffers.iter_mut()),
            )
        {
            // Since these buffers are backed by our own storage, we can fill them with zeroes if
            // the pointers are missing for whatever reason that might be
            nih_debug_assert!(input_channel_pointers.is_some());
            match input_channel_pointers {
                Some(input_channel_pointers) => {
                    nih_debug_assert_eq!(input_channel_pointers.num_channels, input_storage.len());
                    for (channel_idx, channel) in input_storage
                        .iter_mut()
                        .enumerate()
                        .take(input_channel_pointers.num_channels)
                    {
                        let input_channel_pointer =
                            input_channel_pointers.ptrs.as_ptr().add(channel_idx);

                        nih_debug_assert!(num_samples <= channel.capacity());
                        channel.resize(num_samples, 0.0);
                        channel.copy_from_slice(std::slice::from_raw_parts_mut(
                            (*input_channel_pointer).add(sample_offset),
                            num_samples,
                        ))
                    }

                    // In case we were provided too few channels we'll fill the rest with zeroes to
                    // avoid unexpected situations
                    for channel in input_storage
                        .iter_mut()
                        .skip(input_channel_pointers.num_channels)
                    {
                        channel.fill(0.0);
                    }
                }
                None => {
                    for channel in input_storage.iter_mut() {
                        channel.fill(0.0);
                    }
                }
            }

            input_buffer.set_slices(num_samples, |input_slices| {
                // Since we initialized both `input_buffer` and `input_storage` this invariant
                // should never fail unless we made an error ourselves
                debug_assert_eq!(input_slices.len(), input_storage.len());

                for (channel_slice, channel_storage) in
                    input_slices.iter_mut().zip(input_storage.iter_mut())
                {
                    // SAFETY: `channel_storage` is no longer used accessed directly after this
                    *channel_slice = &mut *(channel_storage.as_mut_slice() as *mut [f32]);
                }
            });
        }

        // The auxiliary output buffers can point directly to the host's buffers. This logic is the
        // same as the main outputs, minus the copying of input cdata
        for (output_channel_pointers, output_buffer) in self
            .aux_output_channel_pointers
            .iter()
            .zip(self.aux_output_buffers.iter_mut())
        {
            output_buffer.set_slices(num_samples, |output_slices| {
                match output_channel_pointers {
                    Some(output_channel_pointers) => {
                        nih_debug_assert_eq!(
                            output_slices.len(),
                            output_channel_pointers.num_channels
                        );
                        for (channel_idx, output_slice) in output_slices
                            .iter_mut()
                            .enumerate()
                            .take(output_channel_pointers.num_channels)
                        {
                            let output_channel_pointer =
                                output_channel_pointers.ptrs.as_ptr().add(channel_idx);

                            *output_slice = std::slice::from_raw_parts_mut(
                                (*output_channel_pointer).add(sample_offset),
                                num_samples,
                            );

                            // The host may not zero out the buffers, and assume the plugin always
                            // write something there
                            output_slice.fill(0.0);
                        }

                        // If the caller/host should have provided buffer pointers but didn't then
                        // we must get rid of any dangling slices. See the main output bus for why
                        // this is clamped.
                        let assigned_channels =
                            output_channel_pointers.num_channels.min(output_slices.len());
                        output_slices[assigned_channels..].fill_with(|| &mut [])
                    }
                    None => {
                        nih_debug_assert_eq!(output_slices.len(), 0);

                        // Same as above
                        output_slices.fill_with(|| &mut [])
                    }
                }
            });
        }

        // SAFETY: The 'static lifetimes on the objects are needed so we can store the buffers.
        //         Their actual lifetimes are `'a`, so we need to shrink them here. The contents are
        //         valid for as long as the returned object is borrowed.
        std::mem::transmute::<Buffers<'a, 'static>, Buffers<'a, 'buffer>>(Buffers {
            main_buffer: &mut self.main_buffer,
            aux_inputs: &mut self.aux_input_buffers,
            aux_outputs: &mut self.aux_output_buffers,
        })
    }
}

#[cfg(any(miri, test))]
mod miri {
    use super::*;
    use crate::prelude::{new_nonzero_u32, PortNames};

    const BUFFER_SIZE: usize = 512;
    const NUM_MAIN_INPUT_CHANNELS: usize = 1;
    const NUM_MAIN_OUTPUT_CHANNELS: usize = 2;

    const NUM_AUX_CHANNELS: usize = 2;
    const NUM_AUX_PORTS: usize = 2;

    const AUDIO_IO_LAYOUT: AudioIOLayout = AudioIOLayout {
        main_input_channels: Some(new_nonzero_u32(NUM_MAIN_INPUT_CHANNELS as u32)),
        main_output_channels: Some(new_nonzero_u32(NUM_MAIN_OUTPUT_CHANNELS as u32)),
        aux_input_ports: &[new_nonzero_u32(NUM_AUX_CHANNELS as u32); NUM_AUX_PORTS],
        aux_output_ports: &[new_nonzero_u32(NUM_AUX_CHANNELS as u32); NUM_AUX_PORTS],
        names: PortNames::const_default(),
    };

    #[test]
    fn buffer_io() {
        // This works very similarly to the standalone CPAL and dummy backends
        let mut main_io_storage = vec![vec![0.0f32; BUFFER_SIZE]; NUM_MAIN_OUTPUT_CHANNELS];
        let mut aux_input_storage =
            vec![vec![vec![0.0f32; BUFFER_SIZE]; NUM_AUX_CHANNELS]; NUM_AUX_PORTS];
        let mut aux_output_storage =
            vec![vec![vec![0.0f32; BUFFER_SIZE]; NUM_AUX_CHANNELS]; NUM_AUX_PORTS];

        let mut main_io_channel_pointers: Vec<*mut f32> = main_io_storage
            .iter_mut()
            .map(|channel_slice| channel_slice.as_mut_ptr())
            .collect();
        let mut aux_input_channel_pointers: Vec<Vec<*mut f32>> = aux_input_storage
            .iter_mut()
            .map(|aux_input_storage| {
                aux_input_storage
                    .iter_mut()
                    .map(|channel_slice| channel_slice.as_mut_ptr())
                    .collect()
            })
            .collect();
        let mut aux_output_channel_pointers: Vec<Vec<*mut f32>> = aux_output_storage
            .iter_mut()
            .map(|aux_output_storage| {
                aux_output_storage
                    .iter_mut()
                    .map(|channel_slice| channel_slice.as_mut_ptr())
                    .collect()
            })
            .collect();

        // The actual buffer management here works the same as in the JACK backend. See that
        // implementation for more information.
        let mut buffer_manager = BufferManager::for_audio_io_layout(BUFFER_SIZE, AUDIO_IO_LAYOUT);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, BUFFER_SIZE, |buffer_sources| {
                *buffer_sources.main_output_channel_pointers = Some(ChannelPointers {
                    ptrs: NonNull::new(main_io_channel_pointers.as_mut_ptr()).unwrap(),
                    num_channels: main_io_channel_pointers.len(),
                });
                *buffer_sources.main_input_channel_pointers = Some(ChannelPointers {
                    ptrs: NonNull::new(main_io_channel_pointers.as_mut_ptr()).unwrap(),
                    num_channels: NUM_MAIN_INPUT_CHANNELS.min(main_io_channel_pointers.len()),
                });

                for (input_source_channel_pointers, input_channel_pointers) in buffer_sources
                    .aux_input_channel_pointers
                    .iter_mut()
                    .zip(aux_input_channel_pointers.iter_mut())
                {
                    *input_source_channel_pointers = Some(ChannelPointers {
                        ptrs: NonNull::new(input_channel_pointers.as_mut_ptr()).unwrap(),
                        num_channels: input_channel_pointers.len(),
                    });
                }

                for (output_source_channel_pointers, output_channel_pointers) in buffer_sources
                    .aux_output_channel_pointers
                    .iter_mut()
                    .zip(aux_output_channel_pointers.iter_mut())
                {
                    *output_source_channel_pointers = Some(ChannelPointers {
                        ptrs: NonNull::new(output_channel_pointers.as_mut_ptr()).unwrap(),
                        num_channels: output_channel_pointers.len(),
                    });
                }
            })
        };

        for channel_samples in buffers
            .main_buffer
            .iter_samples()
            .chain(
                buffers
                    .aux_inputs
                    .iter_mut()
                    .flat_map(|buffer| buffer.iter_samples()),
            )
            .chain(
                buffers
                    .aux_outputs
                    .iter_mut()
                    .flat_map(|buffer| buffer.iter_samples()),
            )
        {
            for sample in channel_samples {
                *sample += 1.0;
            }
        }

        // These checks are fine due to stacked borrows even without explicitly dropping `buffers`.
        // If we were to access `buffers` again after this miri would trigger an error.
        for channel in main_io_storage
            .iter()
            .chain(aux_output_storage.iter().flat_map(|storage| storage.iter()))
        {
            for sample in channel {
                assert!(*sample == 1.0);
            }
        }

        for channel in aux_input_storage.iter().flat_map(|storage| storage.iter()) {
            for sample in channel {
                assert!(*sample == 0.0);
            }
        }
    }

    /// The contract violations below are caught by debug assertions, which `nih_debug_assert!()`
    /// upgrades to panicking assertions during tests. That leaves the release behavior—which is what
    /// actually needs to not be undefined—only reachable through `cargo test --release`.
    #[cfg(not(debug_assertions))]
    mod release_only {
        use super::*;

        /// A layout without auxiliary ports, so these tests only have to set up the main IO.
        const MAIN_ONLY_AUDIO_IO_LAYOUT: AudioIOLayout = AudioIOLayout {
            main_input_channels: Some(new_nonzero_u32(NUM_MAIN_INPUT_CHANNELS as u32)),
            main_output_channels: Some(new_nonzero_u32(NUM_MAIN_OUTPUT_CHANNELS as u32)),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        };

        /// Set up the main IO from separate input and output storage, reporting
        /// `num_input_channels`/`num_output_channels` to the buffer manager. The pointer arrays are
        /// sized to match what is reported, so indexing past a reported count is a genuine
        /// out of bounds access that miri catches. Returns the number of samples in the resulting
        /// main buffer.
        fn create_main_buffers(
            buffer_manager: &mut BufferManager,
            input_storage: &mut [Vec<f32>],
            output_storage: &mut [Vec<f32>],
            num_samples: usize,
        ) -> usize {
            let num_input_channels = input_storage.len();
            let num_output_channels = output_storage.len();

            let mut input_pointers: Vec<*mut f32> = input_storage
                .iter_mut()
                .map(|channel_slice| channel_slice.as_mut_ptr())
                .collect();
            let mut output_pointers: Vec<*mut f32> = output_storage
                .iter_mut()
                .map(|channel_slice| channel_slice.as_mut_ptr())
                .collect();

            let buffers = unsafe {
                buffer_manager.create_buffers(0, num_samples, |buffer_sources| {
                    *buffer_sources.main_output_channel_pointers = Some(ChannelPointers {
                        ptrs: NonNull::new(output_pointers.as_mut_ptr()).unwrap(),
                        num_channels: num_output_channels,
                    });
                    *buffer_sources.main_input_channel_pointers = Some(ChannelPointers {
                        ptrs: NonNull::new(input_pointers.as_mut_ptr()).unwrap(),
                        num_channels: num_input_channels,
                    });
                })
            };

            buffers.main_buffer.samples()
        }

        /// A block larger than `max_buffer_size` violates the host's own contract, but the audio
        /// still has to come out intact: truncating it here would silently drop the tail of the
        /// host's buffer. Only the auxiliary input storage has to grow to match.
        #[test]
        fn oversized_block_is_not_truncated() {
            let mut input_storage = vec![vec![0.0f32; BUFFER_SIZE * 2]; NUM_MAIN_INPUT_CHANNELS];
            let mut output_storage = vec![vec![0.0f32; BUFFER_SIZE * 2]; NUM_MAIN_OUTPUT_CHANNELS];
            let mut buffer_manager =
                BufferManager::for_audio_io_layout(BUFFER_SIZE, MAIN_ONLY_AUDIO_IO_LAYOUT);

            let samples = create_main_buffers(
                &mut buffer_manager,
                &mut input_storage,
                &mut output_storage,
                BUFFER_SIZE * 2,
            );
            assert_eq!(samples, BUFFER_SIZE * 2);
        }

        /// A host reporting more output channels than the layout was configured for used to index
        /// past the end of the output slices.
        #[test]
        fn excess_host_output_channels_are_ignored() {
            let mut input_storage = vec![vec![0.0f32; BUFFER_SIZE]; NUM_MAIN_INPUT_CHANNELS];
            let mut output_storage =
                vec![vec![0.0f32; BUFFER_SIZE]; NUM_MAIN_OUTPUT_CHANNELS + 2];
            let mut buffer_manager =
                BufferManager::for_audio_io_layout(BUFFER_SIZE, MAIN_ONLY_AUDIO_IO_LAYOUT);

            let samples = create_main_buffers(
                &mut buffer_manager,
                &mut input_storage,
                &mut output_storage,
                BUFFER_SIZE,
            );
            assert_eq!(samples, BUFFER_SIZE);
        }

        /// A host reporting more input than output channels used to read past the end of the output
        /// pointer array while copying the main input to the main output. The output pointer array
        /// here is exactly one entry long, so the old code's second read was out of bounds.
        #[test]
        fn excess_host_input_channels_are_ignored() {
            let mut input_storage = vec![vec![1.0f32; BUFFER_SIZE]; NUM_MAIN_OUTPUT_CHANNELS + 2];
            let mut output_storage = vec![vec![0.0f32; BUFFER_SIZE]; 1];
            let mut buffer_manager =
                BufferManager::for_audio_io_layout(BUFFER_SIZE, MAIN_ONLY_AUDIO_IO_LAYOUT);

            let samples = create_main_buffers(
                &mut buffer_manager,
                &mut input_storage,
                &mut output_storage,
                BUFFER_SIZE,
            );
            assert_eq!(samples, BUFFER_SIZE);

            // Only the single channel the host actually provided may have been written to
            assert!(output_storage[0].iter().all(|sample| *sample == 1.0));
        }
    }
}
