//! Exclude lifecycle, audio and event-buffer mutation before borrowing wrapper state.
//! Audio uses try_lock only. Control may wait for an in-flight callback, but
//! synchronous reentry from that callback must fail instead of waiting on itself.
use parking_lot::{ReentrantMutex, ReentrantMutexGuard};

pub(crate) struct ProcessingGate(ReentrantMutex<()>);

impl ProcessingGate {
    pub fn new() -> Self {
        Self(ReentrantMutex::new(()))
    }

    pub fn try_lock(&self) -> Option<ReentrantMutexGuard<'_, ()>> {
        if self.0.is_owned_by_current_thread() {
            return None;
        }
        self.0.try_lock()
    }

    /// Only for non-audio control entrypoints. None means synchronous reentry.
    pub fn lock(&self) -> Option<ReentrantMutexGuard<'_, ()>> {
        if self.0.is_owned_by_current_thread() {
            return None;
        }
        Some(self.0.lock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{mpsc, Arc},
        thread,
        time::Duration,
    };
    #[test]
    fn audio_never_waits_control_waits_and_reentry_is_rejected() {
        let gate = Arc::new(ProcessingGate::new());
        let held = gate.lock().unwrap();
        assert!(gate.try_lock().is_none());
        assert!(gate.lock().is_none());
        let (audio_tx, audio_rx) = mpsc::channel();
        let audio_gate = gate.clone();
        let audio = thread::spawn(move || audio_tx.send(audio_gate.try_lock().is_none()).unwrap());
        let result = audio_rx.recv_timeout(Duration::from_secs(1));
        if result.is_err() {
            drop(held);
            audio.join().unwrap();
            panic!("audio waited on control");
        }
        assert!(result.unwrap());
        audio.join().unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let control_gate = gate.clone();
        let control = thread::spawn(move || {
            entered_tx.send(()).unwrap();
            let _held = control_gate.lock().unwrap();
            done_tx.send(()).unwrap();
        });
        entered_rx.recv().unwrap();
        assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        drop(held);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        control.join().unwrap();
    }
}
