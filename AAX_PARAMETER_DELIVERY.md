# Private AAX parameter-only delivery

`io.intrect.aax-parameter-delivery/1` is separate from standard CLAP
`params.flush`. It requires an explicit
`ClapPlugin::CLAP_AAX_CONCURRENT_PARAMETER_DELIVERY` opt-in, no MIDI input/output,
no polyphonic modulation, and disabled sample-accurate automation.

The plugin must tolerate atomic parameter values changing during DSP and use
thread-safe parameter callbacks. The extension never borrows the mutable plugin
instance or its audio/note event buffers.

The host serializes incoming parameter batches with a nonblocking gate. Audio
tries once: when background owns the gate, audio/transport processing continues
without incoming parameter/modulation events. A later owning callback consumes
the queued batch. Lifecycle and state callbacks must not overlap delivery.

Background calls `try_begin` before consuming any host input. Zero means busy;
a nonzero token identifies that acquisition. The same thread supplies that token
to `flush` and calls `end` exactly once, including on failure. Failed acquisition,
stale tokens and wrong-thread calls cannot release another owner's gate.
Exhausted token generations refuse acquisition instead of reusing a token.

`flush` validates the entire stable input batch before applying any value. It
accepts only finite monophonic values in the advertised CLAP range for known parameter IDs, and emits only GUI
parameter values/gestures. A rejected input batch must be retained in order by the host. Rejected output is
retained ahead of later gestures/values and acknowledged events are not replayed.
Audio and background parameter-output drains share an atomic gate, preserving
gesture/value order. Audio defers this drain when busy and continues DSP.

The ABI is `repr(C)` with three CLAP C function pointers: `try_begin(plugin) ->
u64`, `flush(plugin, token, input, output) -> u32`, and `end(plugin, token) ->
bool`. Standard CLAP flush and lifecycle contracts remain unchanged. This is a
private Intrect host/plugin agreement, not permission to invoke standard flush
concurrently with processing.

Flush results are 0 (invalid/unowned input, retain batch), 1 (input consumed, output
still pending, discard batch and retry output), or 2 (complete delivery).
