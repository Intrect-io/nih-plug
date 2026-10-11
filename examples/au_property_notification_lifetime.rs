// 실제 notification 모듈의 메인 run loop / 종료 경합 회귀 검사.
// listener 정의와 bit mapping도 같은 실제 모듈을 사용하며 오디오 출력은 판정하지 않는다.
#[cfg(target_os = "macos")]
use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoopRunInMode};
#[cfg(target_os = "macos")]
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
#[cfg(target_os = "macos")]
#[path = "../src/wrapper/au/property_notifications.rs"]
mod property_notifications;
#[cfg(target_os = "macos")]
use property_notifications::{Listener, PropertyNotifications, NOTIFY_LATENCY};
#[cfg(target_os = "macos")]
static CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(target_os = "macos")]
unsafe extern "C" fn listener(
    _: *mut std::ffi::c_void,
    _: au_sys::AudioUnit,
    _: u32,
    _: u32,
    _: u32,
) {
    assert_ne!(libc::pthread_main_np(), 0);
    CALLS.fetch_add(1, Ordering::SeqCst);
}
#[cfg(target_os = "macos")]
fn pump() {
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.06, 0);
    }
}
#[cfg(target_os = "macos")]
fn main() {
    let state = PropertyNotifications::new();
    let weak = Arc::downgrade(&state);
    state.listeners.lock().unwrap().push(Listener {
        property_id: 12,
        proc: listener,
        user_data: std::ptr::null_mut(),
    });
    assert!(state.start(1usize as au_sys::AudioUnit));
    assert!(
        state.start(1usize as au_sys::AudioUnit),
        "same instance open is idempotent"
    );
    assert!(
        !state.start(2usize as au_sys::AudioUnit),
        "different instance must not replace the live timer"
    );
    state.mark(NOTIFY_LATENCY);
    state.mark(NOTIFY_LATENCY);
    pump();
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "coalesced delivery without host query"
    );
    let worker = state.clone();
    std::thread::spawn(move || {
        worker.mark(NOTIFY_LATENCY);
        worker.drain();
    })
    .join()
    .unwrap();
    assert_eq!(CALLS.load(Ordering::SeqCst), 1, "no worker-thread callback");
    pump();
    assert_eq!(CALLS.load(Ordering::SeqCst), 2, "main-thread delivery");
    state.mark(NOTIFY_LATENCY);
    assert!(state.remove_listener(12, listener, std::ptr::null_mut()));
    pump();
    assert_eq!(CALLS.load(Ordering::SeqCst), 2, "removed listener");
    state.stop();
    state.mark(NOTIFY_LATENCY);
    pump();
    assert_eq!(CALLS.load(Ordering::SeqCst), 2, "no post-close callback");
    drop(state);
    assert!(weak.upgrade().is_none(), "timer context cycle reclaimed");
    let state = PropertyNotifications::new();
    let weak = Arc::downgrade(&state);
    unsafe extern "C" fn close(
        refcon: *mut std::ffi::c_void,
        _: au_sys::AudioUnit,
        _: u32,
        _: u32,
        _: u32,
    ) {
        let state = &*(refcon as *const PropertyNotifications);
        state.stop();
    }
    state.listeners.lock().unwrap().push(Listener {
        property_id: 12,
        proc: close,
        user_data: Arc::as_ptr(&state) as *mut std::ffi::c_void,
    });
    state.listeners.lock().unwrap().push(Listener {
        property_id: 12,
        proc: listener,
        user_data: std::ptr::null_mut(),
    });
    state.start(2usize as au_sys::AudioUnit);
    state.mark(NOTIFY_LATENCY);
    pump();
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        2,
        "reentrant close suppresses later callbacks"
    );
    drop(state);
    assert!(
        weak.upgrade().is_none(),
        "reentrant timer context reclaimed"
    );

    // The main callback synchronously awaits its worker. Close must cancel
    // without waiting; a busy removal must fail promptly and preserve the
    // registration. This fixture keeps refcon valid until its callback ends.
    static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static RETURNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    unsafe extern "C" fn slow(
        _: *mut std::ffi::c_void,
        _: au_sys::AudioUnit,
        _: u32,
        _: u32,
        _: u32,
    ) {
        ENTERED.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !STARTED.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "worker did not start");
            std::thread::yield_now();
        }
        while !RETURNED.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "closer/remover deadlocked against its callback"
            );
            std::thread::yield_now();
        }
    }
    for remove in [false, true] {
        ENTERED.store(false, Ordering::SeqCst);
        STARTED.store(false, Ordering::SeqCst);
        RETURNED.store(false, Ordering::SeqCst);
        let state = PropertyNotifications::new();
        let weak = Arc::downgrade(&state);
        state.listeners.lock().unwrap().push(Listener {
            property_id: 12,
            proc: slow,
            user_data: std::ptr::null_mut(),
        });
        state.start(3usize as au_sys::AudioUnit);
        let worker = state.clone();
        let thread = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !ENTERED.load(Ordering::SeqCst) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timer callback did not arrive"
                );
                std::thread::yield_now();
            }
            STARTED.store(true, Ordering::SeqCst);
            if remove {
                assert!(
                    !worker.remove_listener(12, slow, std::ptr::null_mut()),
                    "busy removal must fail without changing registration"
                );
            } else {
                worker.stop();
            }
            RETURNED.store(true, Ordering::SeqCst);
        });
        state.mark(NOTIFY_LATENCY);
        pump();
        thread.join().unwrap();
        assert!(RETURNED.load(Ordering::SeqCst));
        if remove {
            assert!(state.remove_listener(12, slow, std::ptr::null_mut()));
        }
        state.stop();
        drop(state);
        assert!(weak.upgrade().is_none());
    }
    println!("PASS: delivery/coalescing/main-thread/removal/close/reentrant-close/concurrent-close/concurrent-removal/Arc reclamation");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("AU notification lifetime regression requires macOS");
    std::process::exit(1);
}
