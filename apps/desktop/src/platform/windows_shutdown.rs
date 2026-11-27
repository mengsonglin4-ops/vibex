use std::cell::Cell;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

type WindowHandle = *mut c_void;
type WindowCallback = unsafe extern "system" fn(WindowHandle, isize) -> i32;
type SubclassCallback =
    unsafe extern "system" fn(WindowHandle, u32, usize, isize, usize, usize) -> isize;

const WM_GETOBJECT: u32 = 0x003d;
const WM_CLOSE: u32 = 0x0010;
const WM_NCDESTROY: u32 = 0x0082;
const SUBCLASS_ID: usize = 0x56425841;
const RPC_E_CANTCALLOUT_ININPUTSYNCCALL: i32 = 0x8001010d_u32 as i32;
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static PENDING_WINDOWS: Mutex<Vec<isize>> = Mutex::new(Vec::new());
thread_local! {
    static DISCONNECT_RESULT: Cell<Option<Result<(), i32>>> = const { Cell::new(None) };
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumThreadWindows(thread_id: u32, callback: Option<WindowCallback>, parameter: isize)
    -> i32;
    fn EnumChildWindows(
        parent: WindowHandle,
        callback: Option<WindowCallback>,
        parameter: isize,
    ) -> i32;
    fn InSendMessageEx(reserved: *const c_void) -> u32;
    fn ShowWindowAsync(window: WindowHandle, command: i32) -> i32;
    fn GetClassNameW(window: WindowHandle, name: *mut u16, capacity: i32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetLastError() -> u32;
}

#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(
        window: WindowHandle,
        callback: Option<SubclassCallback>,
        id: usize,
        data: usize,
    ) -> i32;
    fn RemoveWindowSubclass(
        window: WindowHandle,
        callback: Option<SubclassCallback>,
        id: usize,
    ) -> i32;
    fn DefSubclassProc(window: WindowHandle, message: u32, wparam: usize, lparam: isize) -> isize;
}

#[link(name = "uiautomationcore")]
unsafe extern "system" {
    fn UiaDisconnectAllProviders() -> i32;
}

unsafe extern "system" fn stopping_window(
    window: WindowHandle,
    message: u32,
    wparam: usize,
    lparam: isize,
    _id: usize,
    _data: usize,
) -> isize {
    if message == WM_GETOBJECT || message == WM_CLOSE {
        return 0;
    }
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(window, Some(stopping_window), SUBCLASS_ID) };
        let result = unsafe { DefSubclassProc(window, message, wparam, lparam) };
        if let Ok(mut pending) = PENDING_WINDOWS.lock() {
            pending.retain(|handle| *handle != window as isize);
        }
        return result;
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

unsafe extern "system" fn guard_window(window: WindowHandle, parameter: isize) -> i32 {
    unsafe { (*(parameter as *mut Vec<WindowHandle>)).push(window) };
    1
}

unsafe extern "system" fn guard_window_tree(window: WindowHandle, parameter: isize) -> i32 {
    unsafe {
        guard_window(window, parameter);
        EnumChildWindows(window, Some(guard_window), parameter);
    }
    1
}

pub(crate) fn disconnect_before_window_destruction() -> Result<(), i32> {
    if let Some(result) = DISCONNECT_RESULT.get() {
        return result;
    }
    if unsafe { InSendMessageEx(std::ptr::null()) } & 1 != 0 {
        return Err(RPC_E_CANTCALLOUT_ININPUTSYNCCALL);
    }
    let mut windows = Vec::<WindowHandle>::new();
    let enumerated = unsafe {
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(guard_window_tree),
            std::ptr::from_mut(&mut windows) as isize,
        )
    };
    if enumerated == 0 {
        return Err((0x80070000 | unsafe { GetLastError() } & 0xffff) as i32);
    }
    DISCONNECT_RESULT.set(Some(Err(0x8000000a_u32 as i32)));
    let pending = windows
        .iter()
        .filter(|window| is_gpui_window(**window))
        .map(|window| *window as isize)
        .collect::<Vec<_>>();
    let result = guarded_disconnect(
        &windows,
        |window| unsafe { SetWindowSubclass(*window, Some(stopping_window), SUBCLASS_ID, 0) != 0 },
        || {
            expect_window_destruction(pending);
            let code = unsafe { UiaDisconnectAllProviders() };
            if code < 0 { Err(code) } else { Ok(()) }
        },
        |window| unsafe {
            RemoveWindowSubclass(*window, Some(stopping_window), SUBCLASS_ID);
        },
    );
    if result.is_err() {
        expect_window_destruction(Vec::new());
    }
    DISCONNECT_RESULT.set(result.is_ok().then_some(result));
    result
}

fn is_gpui_window(window: WindowHandle) -> bool {
    let mut name = [0u16; 128];
    let length = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) };
    length > 0
        && name[..length as usize]
            .iter()
            .copied()
            .eq("GpuiFast::Window".encode_utf16())
}

fn guarded_disconnect<Handle>(
    windows: &[Handle],
    mut install: impl FnMut(&Handle) -> bool,
    disconnect: impl FnOnce() -> Result<(), i32>,
    mut remove: impl FnMut(&Handle),
) -> Result<(), i32> {
    for (index, window) in windows.iter().enumerate() {
        if !install(window) {
            for installed in &windows[..index] {
                remove(installed);
            }
            return Err(0x80004005_u32 as i32);
        }
    }
    let result = disconnect();
    if result.is_err() {
        for window in windows {
            remove(window);
        }
    }
    result
}

pub(super) fn set_visibility(handle: isize, visible: bool) -> bool {
    unsafe { ShowWindowAsync(handle as WindowHandle, if visible { 5 } else { 0 }) != 0 }
}

pub(super) fn begin_exit() -> bool {
    !EXIT_REQUESTED.swap(true, Ordering::SeqCst)
}

pub(super) fn exit_requested() -> bool {
    EXIT_REQUESTED.load(Ordering::SeqCst)
}

pub(super) fn cancel_exit() {
    EXIT_REQUESTED.store(false, Ordering::SeqCst);
}

pub(super) fn expect_window_destruction(handles: Vec<isize>) {
    *PENDING_WINDOWS.lock().unwrap() = handles;
}

pub(super) fn pending_window_count() -> usize {
    PENDING_WINDOWS
        .lock()
        .map_or(usize::MAX, |pending| pending.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn model_review_windows_shutdown_rolls_back_failed_subclass() {
        let removed = RefCell::new(Vec::new());
        let result = guarded_disconnect(
            &[1, 2, 3],
            |window| *window != 2,
            || panic!("must not disconnect without all guards"),
            |window| removed.borrow_mut().push(*window),
        );
        assert!(result.is_err());
        assert_eq!(*removed.borrow(), vec![1]);
    }

    #[test]
    fn model_review_windows_shutdown_rolls_back_failed_disconnect() {
        let removed = RefCell::new(Vec::new());
        let result = guarded_disconnect(
            &[1, 2],
            |_| true,
            || Err(-1),
            |window| removed.borrow_mut().push(*window),
        );
        assert_eq!(result, Err(-1));
        assert_eq!(*removed.borrow(), vec![1, 2]);
    }

    #[test]
    fn model_review_windows_shutdown_keeps_guards_after_success() {
        assert_eq!(
            guarded_disconnect(
                &[1, 2],
                |_| true,
                || Ok(()),
                |_| panic!("guard must remain")
            ),
            Ok(())
        );
    }
}
