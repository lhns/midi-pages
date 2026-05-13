//! Named-event IPC for headless shutdown of a running proxy.
//!
//! The proxy creates a Win32 auto-reset event named
//! `<namespace>\midi-pages-shutdown-<pid>` at startup. A watcher thread
//! blocks on `WaitForSingleObject` and runs the shutdown path when the
//! event is signalled. `midi-pages --stop` opens the event by name and
//! calls `SetEvent`. No console-signal handling involved.
//!
//! Namespace fallback:
//!
//! - Try `Global\` first with a permissive (NULL) DACL so any user in
//!   any session can open and signal. Interactive logons have
//!   `SeCreateGlobalPrivilege` by default, as do `LocalSystem` and
//!   service accounts, so this usually succeeds.
//! - Fall back to `Local\` (per-session, default DACL) if Global fails.
//!
//! The `--stop` helper mirrors: try Global then Local on Open. Auto-
//! detects which one the proxy used without explicit configuration.

#![cfg(target_os = "windows")]

use std::ffi::c_void;

/// Win32 `HANDLE`. We store as `isize` to match `windows-sys`'s convention
/// and the underlying C `HANDLE` typedef.
pub type EventHandle = isize;

const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
/// Big enough for `SECURITY_DESCRIPTOR` on every Windows ABI. The struct
/// is opaque; we just need a backing buffer.
const SECURITY_DESCRIPTOR_BUF_SIZE: usize = 64;
const EVENT_MODIFY_STATE: u32 = 0x0002;
/// `WaitForSingleObject` wait-forever sentinel.
pub const INFINITE: u32 = 0xFFFFFFFF;
pub const WAIT_OBJECT_0: u32 = 0;

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: i32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        lp_event_attributes: *const c_void,
        b_manual_reset: i32,
        b_initial_state: i32,
        lp_name: *const u16,
    ) -> EventHandle;
    fn OpenEventW(
        dw_desired_access: u32,
        b_inherit_handle: i32,
        lp_name: *const u16,
    ) -> EventHandle;
    fn SetEvent(h_event: EventHandle) -> i32;
    pub fn CloseHandle(h_event: EventHandle) -> i32;
    pub fn WaitForSingleObject(h_handle: EventHandle, dw_milliseconds: u32) -> u32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn InitializeSecurityDescriptor(p_descriptor: *mut c_void, revision: u32) -> i32;
    fn SetSecurityDescriptorDacl(
        p_descriptor: *mut c_void,
        b_dacl_present: i32,
        p_dacl: *mut c_void,
        b_dacl_defaulted: i32,
    ) -> i32;
}

/// Canonical name of the proxy's shutdown event for a given PID in a
/// given namespace.
pub fn event_name(namespace: Namespace, pid: u32) -> String {
    format!("{}\\midi-pages-shutdown-{pid}", namespace.prefix())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Global,
    Local,
}

impl Namespace {
    pub fn prefix(self) -> &'static str {
        match self {
            Namespace::Global => "Global",
            Namespace::Local => "Local",
        }
    }
}

/// Result of an event-creation attempt at proxy startup.
pub struct CreatedEvent {
    pub handle: EventHandle,
    pub namespace: Namespace,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Try to create the shutdown event in `Global\` (NULL DACL, anyone can
/// signal) first. If that fails, try `Local\` (default DACL, same-session
/// only). Returns `None` only if both fail.
pub fn create_shutdown_event(pid: u32) -> Option<CreatedEvent> {
    if let Some(h) = create_event_with_null_dacl(&event_name(Namespace::Global, pid)) {
        return Some(CreatedEvent {
            handle: h,
            namespace: Namespace::Global,
        });
    }
    if let Some(h) = create_event_default_dacl(&event_name(Namespace::Local, pid)) {
        return Some(CreatedEvent {
            handle: h,
            namespace: Namespace::Local,
        });
    }
    None
}

fn create_event_with_null_dacl(name: &str) -> Option<EventHandle> {
    let name_w = wide(name);
    let mut sd_buf = [0u8; SECURITY_DESCRIPTOR_BUF_SIZE];
    unsafe {
        if InitializeSecurityDescriptor(
            sd_buf.as_mut_ptr() as *mut c_void,
            SECURITY_DESCRIPTOR_REVISION,
        ) == 0
        {
            return None;
        }
        if SetSecurityDescriptorDacl(
            sd_buf.as_mut_ptr() as *mut c_void,
            1,                    // bDaclPresent = TRUE
            std::ptr::null_mut(), // pDacl = NULL  =>  no DACL  =>  unrestricted
            0,                    // bDaclDefaulted = FALSE
        ) == 0
        {
            return None;
        }
        let sa = SecurityAttributes {
            n_length: std::mem::size_of::<SecurityAttributes>() as u32,
            lp_security_descriptor: sd_buf.as_mut_ptr() as *mut c_void,
            b_inherit_handle: 0,
        };
        let h = CreateEventW(
            &sa as *const _ as *const c_void,
            0, // bManualReset = FALSE -> auto-reset
            0, // bInitialState = FALSE
            name_w.as_ptr(),
        );
        if h == 0 { None } else { Some(h) }
    }
}

fn create_event_default_dacl(name: &str) -> Option<EventHandle> {
    let name_w = wide(name);
    let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, name_w.as_ptr()) };
    if h == 0 { None } else { Some(h) }
}

/// Open the shutdown event for a given PID. Tries `Global\` first, then
/// `Local\`. Returns the namespace the open succeeded in.
pub fn open_shutdown_event(pid: u32) -> Option<(EventHandle, Namespace)> {
    if let Some(h) = open_event(&event_name(Namespace::Global, pid)) {
        return Some((h, Namespace::Global));
    }
    if let Some(h) = open_event(&event_name(Namespace::Local, pid)) {
        return Some((h, Namespace::Local));
    }
    None
}

fn open_event(name: &str) -> Option<EventHandle> {
    let name_w = wide(name);
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name_w.as_ptr()) };
    if h == 0 { None } else { Some(h) }
}

/// Signal an opened shutdown event. Closes the handle afterwards.
pub fn signal_event(handle: EventHandle) -> Result<(), std::io::Error> {
    let ok = unsafe { SetEvent(handle) };
    let err = if ok == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        CloseHandle(handle);
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn event_name_format_for_pid() {
        assert_eq!(
            event_name(Namespace::Global, 1234),
            "Global\\midi-pages-shutdown-1234"
        );
        assert_eq!(
            event_name(Namespace::Local, 1234),
            "Local\\midi-pages-shutdown-1234"
        );
    }

    #[test]
    fn create_then_open_then_signal_roundtrip() {
        // Use the current PID (each test run picks its own).
        let pid = std::process::id();
        let created = create_shutdown_event(pid).expect("create_shutdown_event");
        let signalled = Arc::new(AtomicBool::new(false));
        let signalled_clone = Arc::clone(&signalled);
        let waiter_handle = created.handle;
        let waiter = thread::spawn(move || {
            // Wait up to 5 seconds for the signal.
            let rc = unsafe { WaitForSingleObject(waiter_handle, 5_000) };
            if rc == WAIT_OBJECT_0 {
                signalled_clone.store(true, Ordering::SeqCst);
            }
        });

        // Give the watcher a moment to enter WaitForSingleObject before we open
        // and signal. Not strictly required (Wait is level-triggered for auto-
        // reset events: even if Set arrives first, the next Wait observes it
        // and resets) but it makes the test mirror real usage.
        thread::sleep(Duration::from_millis(50));

        let (open_handle, open_ns) = open_shutdown_event(pid).expect("open_shutdown_event");
        assert_eq!(open_ns, created.namespace);
        signal_event(open_handle).expect("signal_event");

        waiter.join().expect("waiter thread");
        assert!(
            signalled.load(Ordering::SeqCst),
            "watcher never saw the signal"
        );

        unsafe {
            CloseHandle(created.handle);
        }
    }

    #[test]
    fn create_succeeds_in_at_least_one_namespace() {
        // Sanity: on a fresh PID, create_shutdown_event must return Some.
        // Use a PID that's almost certainly free (our own PID + a large
        // offset, since name uniqueness is what matters, not validity as a
        // real PID).
        let fake_pid = std::process::id().wrapping_add(987_654);
        let created = create_shutdown_event(fake_pid).expect("must create somewhere");
        assert!(matches!(
            created.namespace,
            Namespace::Global | Namespace::Local
        ));
        unsafe {
            CloseHandle(created.handle);
        }
    }
}
