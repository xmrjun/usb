use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::mem::{size_of, zeroed};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{null, null_mut};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{GetStockObject, UpdateWindow, COLOR_WINDOW, DEFAULT_GUI_FONT},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::{
            InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LVCF_SUBITEM,
            LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_STATE, LVIF_TEXT, LVIS_SELECTED, LVITEMW,
            LVM_DELETEALLITEMS, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETEXTENDEDLISTVIEWSTYLE,
            LVM_SETITEMSTATE, LVM_SETITEMTEXTW, LVN_ITEMCHANGED, LVS_EX_DOUBLEBUFFER,
            LVS_EX_FULLROWSELECT, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NMLISTVIEW,
            WC_LISTVIEWW,
        },
        Input::KeyboardAndMouse::EnableWindow,
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
            GetMessageW, GetWindowLongPtrW, KillTimer, LoadCursorW, MessageBoxW, MoveWindow,
            PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW, SetTimer,
            SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, BN_CLICKED,
            BS_PUSHBUTTON, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA,
            IDC_ARROW, IDYES, MB_ICONWARNING, MB_YESNO, MSG, SW_SHOWDEFAULT, WM_APP, WM_CLOSE,
            WM_COMMAND, WM_CREATE, WM_DESTROY, WM_DEVICECHANGE, WM_NCCREATE, WM_NCDESTROY,
            WM_NOTIFY, WM_SETFONT, WM_SIZE, WM_TIMER, WNDCLASSW, WS_CHILD, WS_EX_CLIENTEDGE,
            WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
        },
    },
};

use crate::{
    backend::{Backend, BackendError, Cancellation, EjectResult},
    gui::GuiState,
    model::Device,
    policy::ListedDevice,
    windows::WindowsBackend,
};

const ID_REFRESH: usize = 1001;
const ID_EJECT: usize = 1002;
const WM_WORKER_DONE: u32 = WM_APP + 1;
const TIMER_DEVICE_CHANGE: usize = 1;
const TIMER_AFTER_EJECT: usize = 2;

enum WorkerMessage {
    Refresh(u64, Result<Vec<ListedDevice>, BackendError>),
    Eject(Result<EjectResult, BackendError>),
}

struct App {
    state: RefCell<GuiState>,
    list: Cell<HWND>,
    refresh: Cell<HWND>,
    eject: Cell<HWND>,
    status: Cell<HWND>,
    eject_cancellation: RefCell<Option<Cancellation>>,
    alive: Arc<AtomicBool>,
    queue: Arc<Mutex<VecDeque<WorkerMessage>>>,
}

impl App {
    fn new() -> Self {
        Self {
            state: RefCell::new(GuiState::new()),
            list: Cell::new(null_mut()),
            refresh: Cell::new(null_mut()),
            eject: Cell::new(null_mut()),
            status: Cell::new(null_mut()),
            eject_cancellation: RefCell::new(None),
            alive: Arc::new(AtomicBool::new(true)),
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn create_controls(&self, hwnd: HWND) -> Result<(), BackendError> {
        let list_style =
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS;
        // SAFETY: class names are valid, parent is a live window, and no dangling pointers persist.
        let list = unsafe {
            CreateWindowExW(
                WS_EX_CLIENTEDGE,
                WC_LISTVIEWW,
                wide("Devices").as_ptr(),
                list_style,
                0,
                0,
                0,
                0,
                hwnd,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        let refresh = create_button(hwnd, ID_REFRESH, "Refresh")?;
        let eject = create_button(hwnd, ID_EJECT, "Safely Eject")?;
        // SAFETY: class strings are NUL-terminated for the duration of the call.
        let status = unsafe {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("Ready").as_ptr(),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                0,
                0,
                hwnd,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        if list.is_null() || status.is_null() {
            return Err(super::super::windows::last_error("CreateWindowExW control"));
        }
        self.list.set(list);
        self.refresh.set(refresh);
        self.eject.set(eject);
        self.status.set(status);

        // SAFETY: all controls are live and the font is a process-lifetime stock object.
        unsafe {
            let font = GetStockObject(DEFAULT_GUI_FONT);
            for control in [list, refresh, eject, status] {
                SendMessageW(control, WM_SETFONT, font as usize, 1);
            }
            SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                0,
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize,
            );
        }
        for (index, title, width) in [
            (0, "Device", 230),
            (1, "Drive / mount paths", 230),
            (2, "Physical capacity", 120),
            (3, "Partitions", 80),
            (4, "Status", 320),
        ] {
            insert_column(list, index, title, width);
        }
        self.update_controls();
        Ok(())
    }

    fn start_refresh(&self, hwnd: HWND) {
        let generation = self.state.borrow_mut().begin_refresh();
        let Some(generation) = generation else {
            return;
        };
        self.update_controls();
        let alive = Arc::clone(&self.alive);
        let queue = Arc::clone(&self.queue);
        let hwnd_value = hwnd as usize;
        std::thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut backend = WindowsBackend::new();
                backend.list_devices()
            }))
            .unwrap_or_else(|_| Err(BackendError("inventory worker panicked".into())));
            post_worker_result(
                hwnd_value as HWND,
                &alive,
                &queue,
                WorkerMessage::Refresh(generation, result),
            );
        });
    }

    fn start_eject(&self, hwnd: HWND, device: Device) {
        self.update_controls();
        let cancellation = Cancellation::new();
        *self.eject_cancellation.borrow_mut() = Some(cancellation.clone());
        let alive = Arc::clone(&self.alive);
        let queue = Arc::clone(&self.queue);
        let hwnd_value = hwnd as usize;
        std::thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut backend = WindowsBackend::new();
                backend.request_eject(&device, &cancellation)
            }))
            .unwrap_or_else(|_| Err(BackendError("safe-removal worker panicked".into())));
            post_worker_result(
                hwnd_value as HWND,
                &alive,
                &queue,
                WorkerMessage::Eject(result),
            );
        });
    }

    fn drain_results(&self, hwnd: HWND) {
        let messages = match self.queue.lock() {
            Ok(mut queue) => queue.drain(..).collect::<Vec<_>>(),
            Err(_) => vec![WorkerMessage::Refresh(
                0,
                Err(BackendError("worker result queue was poisoned".into())),
            )],
        };
        for message in messages {
            match message {
                WorkerMessage::Refresh(generation, result) => {
                    self.state.borrow_mut().finish_refresh(generation, result);
                    self.populate_rows();
                }
                WorkerMessage::Eject(result) => {
                    let succeeded = matches!(result, Ok(EjectResult::Ejected));
                    self.state.borrow_mut().finish_eject(result);
                    self.eject_cancellation.borrow_mut().take();
                    if succeeded {
                        // SAFETY: hwnd is live while processing its message.
                        unsafe {
                            SetTimer(hwnd, TIMER_AFTER_EJECT, 700, None);
                        }
                    }
                }
            }
        }
        self.update_controls();
    }

    fn populate_rows(&self) {
        let list = self.list.get();
        // SAFETY: list is a live list-view control.
        unsafe {
            SendMessageW(list, LVM_DELETEALLITEMS, 0, 0);
        }
        let (rows, selected_id) = {
            let state = self.state.borrow();
            (state.rows(), state.selected_id().map(str::to_owned))
        };
        for (index, row) in rows.iter().enumerate() {
            insert_item(list, index as i32, 0, &row.name);
            set_item_text(list, index as i32, 1, &row.mounts);
            set_item_text(list, index as i32, 2, &row.capacity);
            set_item_text(list, index as i32, 3, &row.partitions);
            set_item_text(list, index as i32, 4, &row.status);
            if selected_id.as_deref() == Some(row.public_id.as_str()) {
                // SAFETY: item exists and LVITEMW is fully initialized for state update.
                unsafe {
                    let mut item: LVITEMW = zeroed();
                    item.stateMask = LVIS_SELECTED;
                    item.state = LVIS_SELECTED;
                    SendMessageW(
                        list,
                        LVM_SETITEMSTATE,
                        index,
                        (&mut item as *mut LVITEMW) as isize,
                    );
                }
            }
        }
    }

    fn update_controls(&self) {
        let (busy, can_eject, status_text) = {
            let state = self.state.borrow();
            (
                state.is_busy(),
                state.can_eject(),
                state.status().to_owned(),
            )
        };
        let refresh = self.refresh.get();
        let eject = self.eject.get();
        let status = self.status.get();
        // SAFETY: controls are live while App is attached to the window.
        unsafe {
            EnableWindow(refresh, (!busy) as i32);
            EnableWindow(eject, can_eject as i32);
            let status_text = wide(&status_text);
            SetWindowTextW(status, status_text.as_ptr());
        }
    }

    fn select_row(&self, index: i32) {
        if index < 0 {
            return;
        }
        let id = self
            .state
            .borrow()
            .rows()
            .get(index as usize)
            .map(|row| row.public_id.clone());
        if let Some(id) = id {
            self.state.borrow_mut().select(&id);
            self.update_controls();
        }
    }

    fn resize(&self, hwnd: HWND) {
        let list = self.list.get();
        let refresh = self.refresh.get();
        let eject = self.eject.get();
        let status = self.status.get();
        // SAFETY: hwnd and child controls are live, rect is writable.
        unsafe {
            let mut rect: RECT = zeroed();
            if GetClientRect(hwnd, &mut rect) == 0 {
                return;
            }
            let width = (rect.right - rect.left).max(400);
            let height = (rect.bottom - rect.top).max(240);
            MoveWindow(list, 10, 10, width - 20, height - 100, 1);
            MoveWindow(refresh, 10, height - 78, 110, 30, 1);
            MoveWindow(eject, 130, height - 78, 130, 30, 1);
            MoveWindow(status, 10, height - 38, width - 20, 24, 1);
        }
    }

    fn cancel_pending_eject(&self) {
        if let Some(cancellation) = self.eject_cancellation.borrow().as_ref() {
            cancellation.cancel();
        }
    }
}

pub fn run() -> Result<(), BackendError> {
    // SAFETY: all Win32 structures are initialized and pointers remain valid through each call.
    unsafe {
        let controls = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        if InitCommonControlsEx(&controls) == 0 {
            return Err(super::super::windows::last_error("InitCommonControlsEx"));
        }
        let instance = GetModuleHandleW(null());
        let class_name = wide("UsbDiskEjectorRustWindow");
        let window_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as _,
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&window_class) == 0 {
            return Err(super::super::windows::last_error("RegisterClassW"));
        }
        let mut pending_app = Some(Box::new(App::new()));
        let title = wide("USB Disk Ejector — Safe Removal");
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1050,
            560,
            null_mut(),
            null_mut(),
            instance,
            (&mut pending_app as *mut Option<Box<App>>).cast(),
        );
        if hwnd.is_null() {
            return Err(super::super::windows::last_error(
                "CreateWindowExW main window",
            ));
        }
        ShowWindow(hwnd, SW_SHOWDEFAULT);
        UpdateWindow(hwnd);
        let mut message: MSG = zeroed();
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result == -1 {
                return Err(super::super::windows::last_error("GetMessageW"));
            }
            if result == 0 {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: Win32 invokes this function with message-appropriate parameters.
        unsafe { window_proc_inner(hwnd, message, wparam, lparam) }
    })) {
        Ok(result) => result,
        Err(_) => {
            // SAFETY: this is the standard fallback for an unhandled window message.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

unsafe fn window_proc_inner(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: lparam is CREATESTRUCTW for WM_NCCREATE and lpCreateParams points to the
        // pending Option<Box<App>> that remains live during synchronous CreateWindowExW.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        // SAFETY: the pending pointer is valid and uniquely borrowed during window creation.
        let pending = unsafe { &mut *(create.lpCreateParams as *mut Option<Box<App>>) };
        let Some(app) = pending.take() else {
            return 0;
        };
        let app_pointer = Box::into_raw(app);
        // SAFETY: the window now owns the Box and reclaims it during WM_NCDESTROY.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_pointer as isize);
        }
        return 1;
    }

    // SAFETY: the value was stored during WM_NCCREATE and remains valid until WM_NCDESTROY.
    let app_pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App };
    if app_pointer.is_null() {
        // SAFETY: no application state is attached, so default handling is required.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: the window owns the App Box until WM_NCDESTROY. App uses interior
    // mutability so no exclusive reference is held across reentrant Win32 calls.
    let app = unsafe { &*app_pointer };

    match message {
        WM_CREATE => {
            if app.create_controls(hwnd).is_err() {
                return -1;
            }
            app.resize(hwnd);
            app.start_refresh(hwnd);
            0
        }
        WM_SIZE => {
            app.resize(hwnd);
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xffff;
            let notification = (wparam >> 16) & 0xffff;
            if notification as u32 == BN_CLICKED {
                match id {
                    ID_REFRESH => app.start_refresh(hwnd),
                    ID_EJECT => {
                        let confirmation = app.state.borrow().confirmation_text();
                        if let Some(text) = confirmation {
                            let text = wide(&text);
                            let caption = wide("Confirm safe removal");
                            // SAFETY: window and NUL-terminated strings are valid.
                            let answer = unsafe {
                                MessageBoxW(
                                    hwnd,
                                    text.as_ptr(),
                                    caption.as_ptr(),
                                    MB_YESNO | MB_ICONWARNING,
                                )
                            };
                            if answer == IDYES {
                                let device = app.state.borrow_mut().begin_eject();
                                if let Some(device) = device {
                                    app.start_eject(hwnd, device);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            0
        }
        WM_NOTIFY => {
            if lparam != 0 {
                // SAFETY: all WM_NOTIFY payloads begin with NMHDR; only the header
                // is read until source and notification code are validated.
                let header = unsafe { &*(lparam as *const NMHDR) };
                if header.hwndFrom == app.list.get() && header.code == LVN_ITEMCHANGED {
                    // SAFETY: a list-view LVN_ITEMCHANGED payload is NMLISTVIEW.
                    let notification = unsafe { &*(lparam as *const NMLISTVIEW) };
                    if notification.uChanged & LVIF_STATE != 0
                        && notification.uNewState & LVIS_SELECTED != 0
                    {
                        app.select_row(notification.iItem);
                    }
                }
            }
            0
        }
        WM_DEVICECHANGE => {
            // SAFETY: resetting a window timer is valid for this live hwnd.
            unsafe {
                KillTimer(hwnd, TIMER_DEVICE_CHANGE);
                SetTimer(hwnd, TIMER_DEVICE_CHANGE, 400, None);
            }
            0
        }
        WM_TIMER => {
            let timer = wparam;
            // SAFETY: timer belongs to this window.
            unsafe {
                KillTimer(hwnd, timer);
            }
            if timer == TIMER_DEVICE_CHANGE || timer == TIMER_AFTER_EJECT {
                let busy = app.state.borrow().is_busy();
                if busy {
                    // SAFETY: defer until the current worker finishes.
                    unsafe {
                        SetTimer(hwnd, timer, 400, None);
                    }
                } else {
                    app.start_refresh(hwnd);
                }
            }
            0
        }
        WM_WORKER_DONE => {
            app.drain_results(hwnd);
            0
        }
        WM_CLOSE => {
            let can_close = app.state.borrow_mut().request_close();
            if can_close {
                // SAFETY: hwnd is the live top-level window.
                unsafe {
                    DestroyWindow(hwnd);
                }
            } else {
                app.update_controls();
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: this terminates only this thread's message loop.
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        WM_NCDESTROY => {
            app.cancel_pending_eject();
            app.alive.store(false, Ordering::Release);
            app.state.borrow_mut().close();
            // SAFETY: clear user data before reclaiming the unique App Box exactly once.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(app_pointer));
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
        }
        _ => {
            // SAFETY: standard handling for unprocessed messages.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn post_worker_result(
    hwnd: HWND,
    alive: &AtomicBool,
    queue: &Mutex<VecDeque<WorkerMessage>>,
    message: WorkerMessage,
) {
    if !alive.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut queue) = queue.lock() {
        queue.push_back(message);
    } else {
        return;
    }
    if !alive.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: posting does not dereference hwnd; failure is harmless because the queue is owned.
    unsafe {
        PostMessageW(hwnd, WM_WORKER_DONE, 0, 0);
    }
}

fn create_button(parent: HWND, id: usize, text: &str) -> Result<HWND, BackendError> {
    // SAFETY: class/text are NUL-terminated and parent is a live window.
    let button = unsafe {
        CreateWindowExW(
            0,
            wide("BUTTON").as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            0,
            0,
            0,
            parent,
            id as *mut _,
            GetModuleHandleW(null()),
            null(),
        )
    };
    if button.is_null() {
        Err(super::super::windows::last_error("CreateWindowExW button"))
    } else {
        Ok(button)
    }
}

fn insert_column(list: HWND, index: i32, title: &str, width: i32) {
    let mut title = wide(title);
    // SAFETY: column is initialized and title lives through synchronous SendMessageW.
    unsafe {
        let mut column: LVCOLUMNW = zeroed();
        column.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM;
        column.cx = width;
        column.pszText = title.as_mut_ptr();
        column.iSubItem = index;
        SendMessageW(
            list,
            LVM_INSERTCOLUMNW,
            index as usize,
            (&mut column as *mut LVCOLUMNW) as isize,
        );
    }
}

fn insert_item(list: HWND, row: i32, column: i32, text: &str) {
    let mut text = wide(text);
    // SAFETY: item is initialized and text lives through synchronous SendMessageW.
    unsafe {
        let mut item: LVITEMW = zeroed();
        item.mask = LVIF_TEXT;
        item.iItem = row;
        item.iSubItem = column;
        item.pszText = text.as_mut_ptr();
        SendMessageW(
            list,
            LVM_INSERTITEMW,
            0,
            (&mut item as *mut LVITEMW) as isize,
        );
    }
}

fn set_item_text(list: HWND, row: i32, column: i32, text: &str) {
    let mut text = wide(text);
    // SAFETY: item is initialized and text lives through synchronous SendMessageW.
    unsafe {
        let mut item: LVITEMW = zeroed();
        item.iSubItem = column;
        item.pszText = text.as_mut_ptr();
        SendMessageW(
            list,
            LVM_SETITEMTEXTW,
            row as usize,
            (&mut item as *mut LVITEMW) as isize,
        );
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
