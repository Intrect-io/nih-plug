# AU property notification regression

Run `bash scripts/au_notification_smoke.sh` on macOS. The script preserves each
run under `target/au_notification_smoke/run-*` and never installs an AU or
restarts AudioComponentRegistrar. Notification delivery requires the host's
main run loop to run; property/lifecycle entry points never dispatch inline.

The Rust example imports the production notification module and tests timer
delivery/coalescing, worker-thread deferral, listener removal, normal and
reentrant close, concurrent close/removal, and reclamation of retained state.
Its non-dereferenced instance handles are fixtures; listener records and bit
mapping come from the actual production module.

The Swift app registers the separately built fixture's actual AU factory,
renders a positive and a zero latency transition, and pumps the main run loop
without querying any AU property. Each transition must deliver a main-thread
latency notification. It then disposes the actual AudioComponent instance from
its notification callback, both directly and by waiting for a worker thread,
and checks the disposal status and absence of later notifications. A worker's
listener removal during that callback must promptly return
`kAudioUnitErr_CannotDoInCurrentContext`, keep the registration intact, and
succeed when retried after the callback returns. The JSON verdict, compiler
logs and signed image hashes are retained in the run directory.

Close cancels queued delivery without waiting for host code. The callback owns
notification state independently of the disposed Wrapper. A delivery already
in progress may finish: the host owns its callback's refcon and must keep it
valid until that callback returns. A failed removal does not transfer or end
that ownership. There is no wait for an in-flight callback on a Close or Remove
C-ABI path, so a main callback waiting for its worker cannot deadlock there.

The fixture reports 9112 or 28693 samples at 48kHz; it does not implement that
audio delay. These tests prove notification/lifetime contracts, not signal
latency, a DAW's PDC behavior, installed bundle discovery, or audible quality.
Product UAT must separately verify the rebuilt installed plugin.

The fixture lock uses the root lock's `time 0.3.37` family to support the CI
`nightly-2025-01-01` compiler (Rust 1.85). `time 0.3.55` requires Rust 1.88 and
failed the native fixture build before execution. The smoke script prints its
preserved compilation/result logs on failure while keeping the failure status.
