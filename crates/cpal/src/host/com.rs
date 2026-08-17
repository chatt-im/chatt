//! Handles COM initialization and cleanup.

use std::{io::Error as IoError, marker::PhantomData};

use windows::Win32::{
    Foundation::{CO_E_NOTINITIALIZED, RPC_E_CHANGED_MODE},
    System::Com::{
        CoGetApartmentType, CoInitializeEx, CoTaskMemFree, CoUninitialize, APTTYPE,
        APTTYPEQUALIFIER_NONE, APTTYPE_CURRENT, APTTYPE_MAINSTA, APTTYPE_MTA, APTTYPE_NA,
        APTTYPE_STA, COINIT_APARTMENTTHREADED,
    },
};

thread_local!(static COM_INITIALIZED: ComInitialized = {
    unsafe {
        // Try to initialize COM with STA by default to avoid compatibility issues with the ASIO
        // backend (where CoInitialize() is called by the ASIO SDK) or winit (where drag and drop
        // requires STA).
        // This call can fail with RPC_E_CHANGED_MODE if another library initialized COM with MTA.
        // That's OK though since COM ensures thread-safety/compatibility through marshalling when
        // necessary.
        let result = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if result.is_ok() || result == RPC_E_CHANGED_MODE {
            ComInitialized {
                result,
                _ptr: PhantomData,
            }
        } else {
            // COM initialization failed in another way, something is really wrong.
            panic!(
                "Failed to initialize COM: {}",
                IoError::from_raw_os_error(result.0)
            );
        }
    }
});

/// RAII object that guards the fact that COM is initialized.
///
// We store a raw pointer because it's the only way at the moment to remove `Send`/`Sync` from the
// object.
struct ComInitialized {
    result: windows::core::HRESULT,
    _ptr: PhantomData<*mut ()>,
}

impl Drop for ComInitialized {
    fn drop(&mut self) {
        // Need to avoid calling CoUninitialize() if CoInitializeEx failed since it may have
        // returned RPC_E_MODE_CHANGED - which is OK, see above.
        if self.result.is_ok() {
            unsafe { CoUninitialize() };
        }
    }
}

/// RAII wrapper for COM-allocated wide strings freed with `CoTaskMemFree`.
pub(super) struct ComString(pub windows::core::PWSTR);

impl Drop for ComString {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.as_ptr() as *mut _)) }
    }
}

/// Ensures that COM is initialized in this thread.
///
/// Never call this on a thread this process does not own, notably the MMDevAPI
/// thread that delivers `IMMNotificationClient` callbacks. `CoInitializeEx`
/// there is not undone until that thread exits, and an apartment we imposed on
/// a system worker breaks unrelated audio activations. Use [`Apartment::current`]
/// to observe instead.
pub fn com_initialized() {
    COM_INITIALIZED.with(|_| {});
}

/// The COM apartment a thread belongs to.
///
/// Diagnostics only. Nothing branches on this value. It exists because the
/// failure it explains leaves no other trace: `ActivateAudioInterfaceAsync`
/// returns `0x80010106` after something initialised COM on a thread this
/// process does not own, and the apartment of the thread that did it is the
/// whole answer. [`code`](Apartment::code) and [`from_code`](Apartment::from_code)
/// exist so it can ride along in an `AtomicU8` next to the device-change flag
/// rather than needing a lock on the callback path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Apartment {
    /// No apartment has been recorded yet.
    Unobserved,
    /// The thread has not initialized COM.
    Uninitialized,
    SingleThreaded,
    MultiThreaded,
    Neutral,
    MainSingleThreaded,
    Unknown,
}

impl Apartment {
    /// Observes the calling thread's apartment.
    ///
    /// Unlike [`com_initialized`] this only reports: `CoGetApartmentType` never
    /// initializes COM, so it is safe on foreign threads.
    pub(super) fn current() -> Self {
        let mut apartment = APTTYPE_CURRENT;
        let mut qualifier = APTTYPEQUALIFIER_NONE;
        match unsafe { CoGetApartmentType(&mut apartment, &mut qualifier) } {
            Ok(()) => match apartment {
                APTTYPE_STA => Self::SingleThreaded,
                APTTYPE_MTA => Self::MultiThreaded,
                APTTYPE_NA => Self::Neutral,
                APTTYPE_MAINSTA => Self::MainSingleThreaded,
                APTTYPE(_) => Self::Unknown,
            },
            Err(error) if error.code() == CO_E_NOTINITIALIZED => Self::Uninitialized,
            Err(_) => Self::Unknown,
        }
    }

    /// Short tag for error messages and logs.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Unobserved => "unobserved",
            Self::Uninitialized => "uninitialized",
            Self::SingleThreaded => "sta",
            Self::MultiThreaded => "mta",
            Self::Neutral => "neutral",
            Self::MainSingleThreaded => "main-sta",
            Self::Unknown => "unknown",
        }
    }

    /// Encoding for storage in an atomic, paired with [`Apartment::from_code`].
    pub(super) fn code(self) -> u8 {
        match self {
            Self::Unobserved => 0,
            Self::Uninitialized => 1,
            Self::SingleThreaded => 2,
            Self::MultiThreaded => 3,
            Self::Neutral => 4,
            Self::MainSingleThreaded => 5,
            Self::Unknown => 6,
        }
    }

    pub(super) fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Unobserved,
            1 => Self::Uninitialized,
            2 => Self::SingleThreaded,
            3 => Self::MultiThreaded,
            4 => Self::Neutral,
            5 => Self::MainSingleThreaded,
            _ => Self::Unknown,
        }
    }
}
