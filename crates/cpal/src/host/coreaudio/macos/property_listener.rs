//! Helper code for registering audio object property listeners.
use std::{
    cell::UnsafeCell,
    os::raw::c_void,
    panic::{self, AssertUnwindSafe},
    ptr::{self, NonNull},
    sync::atomic::{AtomicU8, Ordering},
    thread,
};

use objc2_core_audio::{
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener,
};

use super::OSStatus;
use crate::Error;

/// Armed, with no notification inside the closure.
const IDLE: u8 = 0;
/// A notification is inside the closure.
const BUSY: u8 = 1;
/// Disarmed: the closure is gone, or about to be, and must never be called again.
const DEAD: u8 = 2;

/// The closure behind a registered property listener, plus the state that guards it.
///
/// `AudioObjectRemovePropertyListener` is not documented to be synchronized against a
/// notification the HAL has already begun dispatching, and it can fail outright, leaving
/// the listener registered. Either way a notification can reach the shim after the
/// [`AudioObjectPropertyListener`] is gone, so this cell is never freed: dereferencing
/// the client data is sound whenever the HAL calls. Removal disarms the cell and drops
/// the closure instead, which is what owns the captured state — for every listener here
/// a channel `Sender` whose disconnection is what stops a monitor thread. The leak is
/// this header, once per registration.
struct ListenerCell {
    state: AtomicU8,
    /// Reached only through `state`, which admits the notification that wins [`IDLE`] ->
    /// [`BUSY`], and then whoever establishes [`DEAD`].
    callback: UnsafeCell<Option<Box<dyn FnMut() + Send>>>,
}

// SAFETY: `callback` is only ever reached through `state`, which admits one accessor at a
// time, so the `UnsafeCell` is never aliased across threads.
unsafe impl Sync for ListenerCell {}

impl ListenerCell {
    fn new<F: FnMut() + Send + 'static>(callback: F) -> &'static Self {
        Box::leak(Box::new(Self {
            state: AtomicU8::new(IDLE),
            callback: UnsafeCell::new(Some(Box::new(callback))),
        }))
    }

    /// The pointer handed to CoreAudio as this listener's client data.
    fn as_client_data(&'static self) -> *mut c_void {
        ptr::from_ref(self).cast_mut().cast()
    }

    /// Runs the closure, unless the cell is disarmed or already dispatching.
    ///
    /// A notification arriving while another is inside the closure is dropped rather than
    /// queued: every listener here signals an edge that its monitor thread resolves by
    /// re-reading device state, so the dispatch in progress already covers it.
    fn dispatch(&self) {
        if self
            .state
            .compare_exchange(IDLE, BUSY, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        // SAFETY: BUSY is ours until we leave it, and it excludes every other accessor.
        let callback = unsafe { &mut *self.callback.get() };
        let dispatched = panic::catch_unwind(AssertUnwindSafe(|| {
            if let Some(callback) = callback {
                callback();
            }
        }));
        // Unwinding into the HAL's frames is not survivable, and leaving the cell BUSY
        // would hang `disarm`, so a listener that panicked retires itself instead.
        let state = if dispatched.is_ok() { IDLE } else { DEAD };
        self.state.store(state, Ordering::Release);
    }

    /// Disarms the cell and drops the closure, waiting out a dispatch in progress.
    ///
    /// The wait is bounded by the listener body, which is a channel send.
    fn disarm(&self) {
        while let Err(state) =
            self.state
                .compare_exchange_weak(IDLE, DEAD, Ordering::Acquire, Ordering::Acquire)
        {
            if state == DEAD {
                break;
            }
            thread::yield_now();
        }
        // SAFETY: DEAD is established, and no notification can be inside the closure: one
        // that held BUSY published DEAD only after leaving it, and none can enter now.
        unsafe { *self.callback.get() = None };
    }
}

/// Maintain an audio object property listener.
/// The listener will be removed when this type is dropped.
pub struct AudioObjectPropertyListener {
    cell: &'static ListenerCell,
    property_address: AudioObjectPropertyAddress,
    audio_object_id: AudioObjectID,
    /// Whether removal has been attempted, successfully or not.
    removed: bool,
}

impl AudioObjectPropertyListener {
    /// Attach the provided callback as a audio object property listener.
    ///
    /// `callback` runs on a HAL-owned notification thread. It must not call back into
    /// CoreAudio: the HAL holds internal locks across dispatch, so an `AudioObject*` call
    /// from there can deadlock, and removing this very listener reliably does. Signal a
    /// channel and let a thread this process owns do the work.
    pub fn new<F: FnMut() + Send + 'static>(
        audio_object_id: AudioObjectID,
        property_address: AudioObjectPropertyAddress,
        callback: F,
    ) -> Result<Self, Error> {
        let cell = ListenerCell::new(callback);
        // SAFETY: `cell` lives for the rest of the process, so the shim can dereference
        // the client data whenever the HAL calls it.
        let status = unsafe {
            AudioObjectAddPropertyListener(
                audio_object_id,
                NonNull::from(&property_address),
                Some(property_listener_handler_shim),
                cell.as_client_data(),
            )
        };
        if let Err(error) = coreaudio::Error::from_os_status(status) {
            cell.disarm();
            return Err(error.into());
        }
        Ok(Self {
            cell,
            audio_object_id,
            property_address,
            removed: false,
        })
    }

    /// Explicitly remove the property listener.
    /// Use this method if you need to explicitly handle failure to remove
    /// the property listener.
    #[allow(dead_code)]
    pub fn remove(mut self) -> Result<(), Error> {
        self.remove_inner()
    }

    fn remove_inner(&mut self) -> Result<(), Error> {
        if self.removed {
            return Ok(());
        }
        self.removed = true;
        let status = unsafe {
            AudioObjectRemovePropertyListener(
                self.audio_object_id,
                NonNull::from(&self.property_address),
                Some(property_listener_handler_shim),
                self.cell.as_client_data(),
            )
        };
        // Disarm whether or not removal took: a listener left registered must stop
        // reaching a closure whose channel nobody is reading any more.
        self.cell.disarm();
        coreaudio::Error::from_os_status(status)?;
        Ok(())
    }
}

impl Drop for AudioObjectPropertyListener {
    fn drop(&mut self) {
        let _ = self.remove_inner();
    }
}

/// Callback used to call user-provided closure as a property listener.
///
/// Runs on a HAL-owned notification thread; see [`AudioObjectPropertyListener::new`].
unsafe extern "C-unwind" fn property_listener_handler_shim(
    _: AudioObjectID,
    _: u32,
    _: NonNull<AudioObjectPropertyAddress>,
    cell: *mut c_void,
) -> OSStatus {
    // SAFETY: `cell` is the client data given to `AudioObjectAddPropertyListener`: a
    // leaked `ListenerCell` that outlives every notification the HAL can produce for it.
    (*cell.cast::<ListenerCell>()).dispatch();
    0
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize},
        mpsc, Arc,
    };

    use super::*;

    #[test]
    fn dispatch_runs_the_callback_until_disarmed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cell = ListenerCell::new({
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::Relaxed);
            }
        });

        cell.dispatch();
        cell.dispatch();
        cell.disarm();
        cell.dispatch();

        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn disarm_drops_the_callback() {
        let (tx, rx) = mpsc::channel::<()>();
        let cell = ListenerCell::new(move || {
            let _ = tx.send(());
        });

        cell.dispatch();
        assert!(rx.try_recv().is_ok());

        cell.disarm();
        assert!(rx.recv().is_err());
    }

    #[test]
    fn disarm_waits_for_a_dispatch_in_progress() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let left = Arc::new(AtomicBool::new(false));
        let cell = ListenerCell::new({
            let left = left.clone();
            move || {
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
                left.store(true, Ordering::Release);
            }
        });

        let dispatching = thread::spawn(move || cell.dispatch());
        entered_rx.recv().expect("dispatch entered the callback");
        let disarming = thread::spawn(move || cell.disarm());
        assert!(!left.load(Ordering::Acquire));

        release_tx.send(()).expect("callback is waiting");
        disarming.join().expect("disarm completed");
        assert!(left.load(Ordering::Acquire));
        dispatching.join().expect("dispatch completed");
    }

    #[test]
    fn a_panicking_callback_retires_the_cell() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cell = ListenerCell::new({
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::Relaxed);
                panic!("listener");
            }
        });

        let hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        cell.dispatch();
        cell.dispatch();
        panic::set_hook(hook);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
