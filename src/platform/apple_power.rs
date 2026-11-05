//! IOKit power assertion that keeps the Mac out of idle sleep (DEV-1156).
//!
//! `kIOPMAssertionTypePreventUserIdleSystemSleep` holds off *system* sleep
//! only. The display still sleeps on its normal schedule, which is the point:
//! the PTYs stay alive, the screen does not stay lit. `pmset -g assertions`
//! lists the assertion under the Allele process with [`ASSERTION_NAME`].
//!
//! The kernel drops a process's assertions when it exits, so a crash cannot
//! leave the machine pinned awake. Releasing on quit is still done explicitly
//! (see `session::keep_awake`) so the release does not depend on that.

use std::ffi::{c_char, c_void, CString};
use std::sync::Mutex;

use crate::errors::{AlleleError, Result};

/// What `pmset -g assertions` shows next to the process name.
pub(crate) const ASSERTION_NAME: &str = "Allele: sessions are working";

type CFStringRef = *const c_void;
type IOPMAssertionID = u32;
type IOReturn = i32;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
const K_IO_RETURN_SUCCESS: IOReturn = 0;
/// `kIOPMAssertionTypePreventUserIdleSystemSleep`. Not the display variant.
const PREVENT_USER_IDLE_SYSTEM_SLEEP: &str = "PreventUserIdleSystemSleep";

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        level: u32,
        name: CFStringRef,
        id: *mut IOPMAssertionID,
    ) -> IOReturn;
    fn IOPMAssertionRelease(id: IOPMAssertionID) -> IOReturn;
}

/// An owned `CFStringRef`, released on drop.
struct CfString(CFStringRef);

impl CfString {
    fn new(s: &str) -> Result<Self> {
        let c = CString::new(s).map_err(|e| AlleleError::Other(e.to_string()))?;
        // SAFETY: `c` is a valid NUL-terminated UTF-8 string for the call's
        // duration; a null allocator selects the default allocator.
        let r = unsafe {
            CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        };
        if r.is_null() {
            return Err(AlleleError::Other(format!(
                "CFStringCreateWithCString({s})"
            )));
        }
        Ok(Self(r))
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        // SAFETY: we own the +1 reference returned by the Create call.
        unsafe { CFRelease(self.0) }
    }
}

/// Holds at most one assertion. The mutex makes "already held?" and the
/// IOKit call one step, so two callers can never create two assertions.
#[derive(Default)]
pub(crate) struct ApplePower {
    held: Mutex<Option<IOPMAssertionID>>,
}

impl ApplePower {
    fn create() -> Result<IOPMAssertionID> {
        let kind = CfString::new(PREVENT_USER_IDLE_SYSTEM_SLEEP)?;
        let name = CfString::new(ASSERTION_NAME)?;
        let mut id: IOPMAssertionID = 0;
        // SAFETY: both CFStrings are live for the call; `id` is a valid out-pointer.
        let rc = unsafe {
            IOPMAssertionCreateWithName(kind.0, K_IOPM_ASSERTION_LEVEL_ON, name.0, &mut id)
        };
        if rc != K_IO_RETURN_SUCCESS {
            return Err(AlleleError::Other(format!(
                "IOPMAssertionCreateWithName failed: {rc:#x}"
            )));
        }
        Ok(id)
    }

    fn release(id: IOPMAssertionID) -> Result<()> {
        // SAFETY: `id` came from a successful IOPMAssertionCreateWithName.
        let rc = unsafe { IOPMAssertionRelease(id) };
        if rc != K_IO_RETURN_SUCCESS {
            return Err(AlleleError::Other(format!(
                "IOPMAssertionRelease({id}) failed: {rc:#x}"
            )));
        }
        Ok(())
    }
}

impl super::PowerManagement for ApplePower {
    fn set_prevent_idle_sleep(&self, prevent: bool) -> Result<()> {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        match (prevent, *held) {
            (true, None) => {
                let id = Self::create()?;
                tracing::info!("keep-awake: holding idle-sleep assertion {id}");
                *held = Some(id);
            }
            (false, Some(id)) => {
                // Forget the id even if the release errors: a failed release
                // means IOKit no longer knows it, and retrying would fail too.
                *held = None;
                Self::release(id)?;
                tracing::info!("keep-awake: released idle-sleep assertion {id}");
            }
            _ => {}
        }
        Ok(())
    }

    fn is_preventing_idle_sleep(&self) -> bool {
        self.held.lock().map(|h| h.is_some()).unwrap_or(false)
    }
}

impl Drop for ApplePower {
    fn drop(&mut self) {
        if let Ok(Some(id)) = self.held.get_mut().map(|h| h.take()) {
            let _ = Self::release(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::PowerManagement;
    use std::process::Command;

    /// `PreventUserIdleSystemSleep` assertions `pmset` lists for this test
    /// process under our name.
    fn listed_by_pmset() -> usize {
        let out = Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset runs");
        let pid = format!("pid {}(", std::process::id());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| {
                l.contains(&pid)
                    && l.contains(PREVENT_USER_IDLE_SYSTEM_SLEEP)
                    && l.contains(ASSERTION_NAME)
            })
            .count()
    }

    /// The real IOKit round trip, checked from outside the process the way
    /// the acceptance criteria are: `pmset -g assertions`. One test, not two:
    /// tests share a process, so parallel tests would count each other's
    /// assertions under the same pid.
    #[test]
    fn holds_one_assertion_releases_it_and_drop_releases_too() {
        let power = ApplePower::default();
        power.set_prevent_idle_sleep(true).unwrap();
        power.set_prevent_idle_sleep(true).unwrap(); // idempotent
        assert!(power.is_preventing_idle_sleep());
        assert_eq!(listed_by_pmset(), 1, "enable twice holds exactly one");

        power.set_prevent_idle_sleep(false).unwrap();
        assert!(!power.is_preventing_idle_sleep());
        assert_eq!(listed_by_pmset(), 0, "released assertion is gone");

        power.set_prevent_idle_sleep(true).unwrap();
        assert_eq!(listed_by_pmset(), 1);
        drop(power);
        assert_eq!(listed_by_pmset(), 0, "drop releases a held assertion");
    }
}
