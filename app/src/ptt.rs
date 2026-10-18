//! Global push-to-talk: a low-level keyboard hook on its own thread.
//!
//! The hook is only installed while push-to-talk is enabled (or a key is being captured in
//! the settings dialog) and only looks at one virtual-key code; keystrokes are never stored
//! or forwarded anywhere. The callback does atomics plus one channel send, nothing slow.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub enum PttMsg {
    Down,
    Up,
    /// A key was pressed while capturing (settings dialog).
    Captured {
        vk: u32,
        name: String,
    },
}

type Callback = Arc<dyn Fn(PttMsg) + Send + Sync>;

struct Shared {
    vk: AtomicU32,
    capture: AtomicBool,
    held: AtomicBool,
    cb: Mutex<Option<Callback>>,
}

static SHARED: Shared = Shared {
    vk: AtomicU32::new(0),
    capture: AtomicBool::new(false),
    held: AtomicBool::new(false),
    cb: Mutex::new(None),
};

/// Set the key that talks (0 = none).
pub fn set_key(vk: u32) {
    SHARED.vk.store(vk, Ordering::Relaxed);
    SHARED.held.store(false, Ordering::Relaxed);
}

/// The next key press is reported as [`PttMsg::Captured`] instead of acting as push-to-talk.
pub fn begin_capture() {
    SHARED.capture.store(true, Ordering::Relaxed);
}

pub fn cancel_capture() {
    SHARED.capture.store(false, Ordering::Relaxed);
}

#[cfg(windows)]
pub use imp::PttHook;

#[cfg(windows)]
mod imp {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyNameTextW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    use super::{PttMsg, SHARED};

    /// Owns the hook thread; dropping it removes the hook.
    pub struct PttHook {
        thread_id: u32,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl PttHook {
        pub fn start(cb: impl Fn(PttMsg) + Send + Sync + 'static) -> Option<PttHook> {
            *SHARED.cb.lock().ok()? = Some(Arc::new(cb));
            let (tx, rx) = std::sync::mpsc::channel::<Option<u32>>();
            let join = std::thread::Builder::new()
                .name("vox-ptt-hook".into())
                .spawn(move || {
                    // SAFETY: standard low-level hook install + message loop on this thread.
                    unsafe {
                        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0);
                        if hook.is_null() {
                            let _ = tx.send(None);
                            return;
                        }
                        let _ = tx.send(Some(GetCurrentThreadId()));
                        let mut msg: MSG = std::mem::zeroed();
                        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        UnhookWindowsHookEx(hook);
                    }
                })
                .ok()?;
            match rx.recv().ok().flatten() {
                Some(thread_id) => {
                    tracing::info!(thread_id, "push-to-talk keyboard hook installed");
                    Some(PttHook {
                        thread_id,
                        join: Some(join),
                    })
                }
                None => {
                    let _ = join.join();
                    None
                }
            }
        }
    }

    impl Drop for PttHook {
        fn drop(&mut self) {
            // SAFETY: posting WM_QUIT to our own hook thread.
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
            if let Ok(mut cb) = SHARED.cb.lock() {
                *cb = None;
            }
            SHARED.held.store(false, Ordering::Relaxed);
            SHARED.capture.store(false, Ordering::Relaxed);
        }
    }

    fn key_name(k: &KBDLLHOOKSTRUCT) -> String {
        let extended = (k.flags & 1) << 24;
        let lparam = ((k.scanCode << 16) | extended) as i32;
        let mut buf = [0u16; 64];
        // SAFETY: `buf` is a valid output buffer of the stated length.
        let n = unsafe { GetKeyNameTextW(lparam, buf.as_mut_ptr(), buf.len() as i32) };
        if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            format!("Tecla {}", k.vkCode)
        }
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == 0 {
            // SAFETY: for WH_KEYBOARD_LL with code HC_ACTION, lparam points at a KBDLLHOOKSTRUCT.
            let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let up = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
            let cb = SHARED.cb.try_lock().ok().and_then(|g| g.clone());
            if let Some(cb) = cb {
                if down && SHARED.capture.swap(false, Ordering::Relaxed) {
                    cb(PttMsg::Captured {
                        vk: k.vkCode,
                        name: key_name(k),
                    });
                } else {
                    let vk = SHARED.vk.load(Ordering::Relaxed);
                    if vk != 0 && k.vkCode == vk {
                        if down && !SHARED.held.swap(true, Ordering::Relaxed) {
                            cb(PttMsg::Down);
                        } else if up && SHARED.held.swap(false, Ordering::Relaxed) {
                            cb(PttMsg::Up);
                        }
                    }
                }
            }
        }
        // SAFETY: pass the event on unchanged.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

#[cfg(not(windows))]
pub struct PttHook;

#[cfg(not(windows))]
impl PttHook {
    pub fn start(_cb: impl Fn(PttMsg) + Send + Sync + 'static) -> Option<PttHook> {
        None
    }
}
