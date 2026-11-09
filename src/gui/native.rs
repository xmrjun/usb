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
    Foundation::{GetLastError, SetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        CreateFontW, DeleteObject, GetStockObject, UpdateWindow, CLEARTYPE_QUALITY,
        CLIP_DEFAULT_PRECIS, COLOR_WINDOW, DEFAULT_CHARSET, DEFAULT_GUI_FONT, DEFAULT_PITCH,
        HFONT, OUT_DEFAULT_PRECIS,
    },
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
            BS_PUSHBUTTON, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
            DBT_DEVICEARRIVAL, DBT_DEVICEQUERYREMOVE, DBT_DEVICEQUERYREMOVEFAILED,
            DBT_DEVICEREMOVECOMPLETE, DBT_DEVICEREMOVEPENDING, DBT_DEVNODES_CHANGED, GWLP_USERDATA,
            IDC_ARROW, IDYES, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, MSG,
            SW_SHOWDEFAULT, WM_APP, WM_CLOSE,
            WM_COMMAND, WM_CREATE, WM_DESTROY, WM_DEVICECHANGE, WM_NCCREATE, WM_NCDESTROY,
            WM_NOTIFY, WM_SETFONT, WM_SIZE, WM_TIMER, WNDCLASSW, WS_CHILD, WS_EX_CLIENTEDGE,
            WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
        },
    },
};

use crate::{
    backend::{Backend, BackendError, Cancellation, EjectResult},
    gui::{window_title, GuiState},
    model::Device,
    policy::ListedDevice,
    windows::{elevation, WindowsBackend},
};

const ID_REFRESH: usize = 1001;
const ID_EJECT: usize = 1002;
const ID_ELEVATE: usize = 1003;
const ID_DETAILS: usize = 1004;
const WM_WORKER_DONE: u32 = WM_APP + 1;
const TIMER_DEVICE_CHANGE: usize = 1;
const TIMER_AFTER_EJECT: usize = 2;

enum WorkerMessage {
    Refresh(u64, Result<Vec<ListedDevice>, BackendError>),
    Eject(Result<EjectResult, BackendError>, Option<String>),
}

struct App {
    state: RefCell<GuiState>,
    list: Cell<HWND>,
    refresh: Cell<HWND>,
    eject: Cell<HWND>,
    elevate: Cell<HWND>,
    details: Cell<HWND>,
    elevated: bool,
    status: Cell<HWND>,
    font: Cell<HFONT>,
    eject_cancellation: RefCell<Option<Cancellation>>,
    alive: Arc<AtomicBool>,
    queue: Arc<Mutex<VecDeque<WorkerMessage>>>,
}

impl App {
    fn new(elevated: bool) -> Self {
        Self {
            state: RefCell::new(GuiState::new()),
            list: Cell::new(null_mut()),
            refresh: Cell::new(null_mut()),
            eject: Cell::new(null_mut()),
            elevate: Cell::new(null_mut()),
            details: Cell::new(null_mut()),
            elevated,
            status: Cell::new(null_mut()),
            font: Cell::new(null_mut()),
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
                wide("设备列表").as_ptr(),
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
        let refresh = create_button(hwnd, ID_REFRESH, "刷新列表")?;
        let eject = create_button(hwnd, ID_EJECT, "安全弹出")?;
        let elevate = create_button(hwnd, ID_ELEVATE, "管理员模式")?;
        let details = create_button(hwnd, ID_DETAILS, "详细信息")?;
        // SAFETY: class strings are NUL-terminated for the duration of the call.
        let status = unsafe {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                wide("就绪").as_ptr(),
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
            return Err(super::super::windows::last_error("创建界面控件"));
        }
        self.list.set(list);
        self.refresh.set(refresh);
        self.eject.set(eject);
        self.elevate.set(elevate);
        self.details.set(details);
        self.status.set(status);

        // SAFETY: the face name is NUL-terminated and CreateFontW copies it. App owns
        // the created font until WM_NCDESTROY, after the child controls are destroyed.
        unsafe {
            let created_font = CreateFontW(
                -16,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide("Microsoft YaHei UI").as_ptr(),
            );
            self.font.set(created_font);
            let font = if created_font.is_null() {
                GetStockObject(DEFAULT_GUI_FONT)
            } else {
                created_font
            };
            for control in [list, refresh, eject, elevate, details, status] {
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
            (0, "设备名称", 230),
            (1, "盘符 / 挂载路径", 260),
            (2, "物理容量", 120),
            (3, "分区数", 80),
            (4, "状态 / 原因", 340),
        ] {
            insert_column(list, index, title, width);
        }
        self.update_controls();
        Ok(())
    }

    fn start_refresh(&self, hwnd: HWND) {
        self.start_refresh_with_mode(hwnd, false);
    }

    fn start_background_refresh(&self, hwnd: HWND) {
        self.start_refresh_with_mode(hwnd, true);
    }

    fn start_refresh_with_mode(&self, hwnd: HWND, background: bool) {
        let generation = {
            let mut state = self.state.borrow_mut();
            if background {
                state.begin_background_refresh()
            } else {
                state.begin_refresh()
            }
        };
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
            .unwrap_or_else(|_| Err(BackendError("读取设备列表时发生内部错误".into())));
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
            let (result, details) = catch_unwind(AssertUnwindSafe(|| {
                let mut backend = WindowsBackend::new();
                let result = backend.request_eject(&device, &cancellation);
                (result, backend.diagnostics())
            }))
            .unwrap_or_else(|_| {
                (
                    Err(BackendError("安全弹出时发生内部错误".into())),
                    Some("安全弹出时发生内部错误，无法取得完整诊断。".into()),
                )
            });
            post_worker_result(
                hwnd_value as HWND,
                &alive,
                &queue,
                WorkerMessage::Eject(result, details),
            );
        });
    }

    fn drain_results(&self, hwnd: HWND) {
        let messages = match self.queue.lock() {
            Ok(mut queue) => queue.drain(..).collect::<Vec<_>>(),
            Err(_) => vec![WorkerMessage::Refresh(
                0,
                Err(BackendError("无法读取设备操作的处理结果".into())),
            )],
        };
        for message in messages {
            match message {
                WorkerMessage::Refresh(generation, result) => {
                    self.state.borrow_mut().finish_refresh(generation, result);
                    self.populate_rows();
                }
                WorkerMessage::Eject(result, details) => {
                    {
                        let mut state = self.state.borrow_mut();
                        state.set_operation_details(details);
                        state.finish_eject(result);
                    }
                    self.eject_cancellation.borrow_mut().take();
                    // Coalesce notifications from this operation only after it
                    // finishes. A failed preparation may also change the list;
                    // the background refresh preserves its failure and details.
                    // SAFETY: hwnd is live while processing its message.
                    unsafe {
                        KillTimer(hwnd, TIMER_DEVICE_CHANGE);
                        SetTimer(hwnd, TIMER_AFTER_EJECT, 700, None);
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
        let (can_refresh, can_eject, has_details, status_text) = {
            let state = self.state.borrow();
            (
                state.can_refresh(),
                state.can_eject(),
                state.operation_details().is_some(),
                state.status().to_owned(),
            )
        };
        let refresh = self.refresh.get();
        let eject = self.eject.get();
        let elevate = self.elevate.get();
        let details = self.details.get();
        let status = self.status.get();
        let status_text = if has_details && status_text.chars().count() > 100 {
            format!(
                "{}…（完整过程见“详细信息”）",
                status_text.chars().take(85).collect::<String>()
            )
        } else {
            status_text
        };
        // SAFETY: controls are live while App is attached to the window.
        unsafe {
            EnableWindow(refresh, can_refresh as i32);
            EnableWindow(eject, can_eject as i32);
            EnableWindow(elevate, (can_refresh && !self.elevated) as i32);
            EnableWindow(details, (can_refresh && has_details) as i32);
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
        let elevate = self.elevate.get();
        let details = self.details.get();
        let status = self.status.get();
        // SAFETY: hwnd and child controls are live, rect is writable.
        unsafe {
            let mut rect: RECT = zeroed();
            if GetClientRect(hwnd, &mut rect) == 0 {
                return;
            }
            let width = (rect.right - rect.left).max(400);
            let height = (rect.bottom - rect.top).max(240);
            let wrapped = width < 540;
            let controls_top = height - if wrapped { 145 } else { 103 };
            MoveWindow(list, 10, 10, width - 20, controls_top - 22, 1);
            MoveWindow(refresh, 10, controls_top, 110, 32, 1);
            MoveWindow(eject, 130, controls_top, 130, 32, 1);
            let extra_top = controls_top + if wrapped { 42 } else { 0 };
            let extra_left = if wrapped { 10 } else { 270 };
            MoveWindow(elevate, extra_left, extra_top, 130, 32, 1);
            MoveWindow(details, extra_left + 140, extra_top, 110, 32, 1);
            MoveWindow(status, 10, height - 61, width - 20, 50, 1);
        }
    }

    fn cancel_pending_eject(&self) {
        if let Some(cancellation) = self.eject_cancellation.borrow().as_ref() {
            cancellation.cancel();
        }
    }

    fn start_elevation(&self, hwnd: HWND) {
        if self.elevated || !self.state.borrow_mut().begin_elevation() {
            return;
        }
        self.update_controls();
        // ShellExecuteW may pump messages while UAC is shown. The state remains
        // reserved and no RefCell borrow crosses that call.
        let result = elevation::relaunch_elevated(hwnd);
        let relaunched = self.state.borrow_mut().finish_elevation(result);
        self.update_controls();
        if relaunched {
            // SAFETY: hwnd is live; the closed state prevents queued scans from
            // starting before this deferred close is dispatched.
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
    }

    fn show_details(&self, hwnd: HWND) {
        let details = self.state.borrow_mut().begin_details();
        let Some(details) = details else {
            return;
        };
        self.update_controls();
        let text = format!(
            "上次安全弹出操作的详细信息\n按 Ctrl+C 可复制此窗口内容。\n\n{details}"
        );
        // SAFETY: hwnd and NUL-terminated strings are valid for the modal call.
        // No state borrow is retained while MessageBoxW pumps window messages.
        unsafe {
            MessageBoxW(
                hwnd,
                wide(&text).as_ptr(),
                wide("详细信息").as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        self.state.borrow_mut().finish_details();
        self.update_controls();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let font = self.font.get();
        if !font.is_null() {
            // SAFETY: this font was created by App and the window's child controls
            // have been destroyed before App is reclaimed during WM_NCDESTROY.
            unsafe {
                DeleteObject(font);
            }
        }
    }
}

pub fn run() -> Result<(), BackendError> {
    let elevated = elevation::is_elevated()?;
    // SAFETY: all Win32 structures are initialized and pointers remain valid through each call.
    unsafe {
        let controls = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        if InitCommonControlsEx(&controls) == 0 {
            return Err(super::super::windows::last_error("初始化界面控件"));
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
            return Err(super::super::windows::last_error("注册应用窗口"));
        }
        let mut pending_app = Some(Box::new(App::new(elevated)));
        let title = wide(&window_title(elevated));
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1120,
            600,
            null_mut(),
            null_mut(),
            instance,
            (&mut pending_app as *mut Option<Box<App>>).cast(),
        );
        if hwnd.is_null() {
            return Err(super::super::windows::last_error(
                "创建应用主窗口",
            ));
        }
        ShowWindow(hwnd, SW_SHOWDEFAULT);
        UpdateWindow(hwnd);
        let mut message: MSG = zeroed();
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result == -1 {
                return Err(super::super::windows::last_error("读取窗口消息"));
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
        let pending_pointer = create.lpCreateParams as *mut Option<Box<App>>;
        if pending_pointer.is_null() {
            return 0;
        }
        // SAFETY: preserve default non-client initialization, including the
        // window title. No App reference or pending borrow crosses this call.
        let accepted = unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        if accepted == 0 {
            return 0;
        }
        // SAFETY: the caller's pending Box is live throughout CreateWindowExW.
        let pending = unsafe { &mut *pending_pointer };
        let Some(app) = pending.take() else {
            return 0;
        };
        let app_pointer = Box::into_raw(app);
        // SAFETY: attach the unique Box only after default creation succeeds.
        // A zero return is valid when replacing initially empty user data.
        unsafe {
            SetLastError(0);
            let previous = SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_pointer as isize);
            if previous == 0 && GetLastError() != 0 {
                *pending = Some(Box::from_raw(app_pointer));
                return 0;
            }
        }
        // WM_NCDESTROY now owns the only remaining Box reclamation path.
        return accepted;
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
                    ID_ELEVATE => app.start_elevation(hwnd),
                    ID_DETAILS => app.show_details(hwnd),
                    ID_EJECT => {
                        let confirmation = app.state.borrow_mut().begin_confirmation();
                        if let Some(text) = confirmation {
                            app.update_controls();
                            let text = wide(&text);
                            let caption = wide("确认安全弹出");
                            // SAFETY: window and NUL-terminated strings are valid.
                            let answer = unsafe {
                                MessageBoxW(
                                    hwnd,
                                    text.as_ptr(),
                                    caption.as_ptr(),
                                    MB_YESNO | MB_ICONWARNING,
                                )
                            };
                            let device = app
                                .state
                                .borrow_mut()
                                .finish_confirmation(answer == IDYES);
                            if let Some(device) = device {
                                app.start_eject(hwnd, device);
                            } else {
                                app.update_controls();
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
            match wparam as u32 {
                DBT_DEVICEQUERYREMOVE | DBT_DEVICEREMOVEPENDING => {
                    app.state.borrow_mut().removal_started();
                    // SAFETY: cancel this window's scans instead of reopening
                    // devices while Windows is querying or performing removal.
                    unsafe {
                        KillTimer(hwnd, TIMER_DEVICE_CHANGE);
                        KillTimer(hwnd, TIMER_AFTER_EJECT);
                    }
                    app.update_controls();
                }
                DBT_DEVICEQUERYREMOVEFAILED => {
                    let refresh_now = {
                        let mut state = app.state.borrow_mut();
                        state.removal_finished();
                        !state.is_ejecting()
                    };
                    // The running eject owns its completion refresh. Do not
                    // queue a competing timer after each rejected PnP attempt.
                    // SAFETY: both timer operations target this live window.
                    unsafe {
                        if refresh_now {
                            SetTimer(hwnd, TIMER_DEVICE_CHANGE, 400, None);
                        } else {
                            KillTimer(hwnd, TIMER_DEVICE_CHANGE);
                        }
                    }
                    app.update_controls();
                }
                DBT_DEVICEREMOVECOMPLETE => {
                    app.state.borrow_mut().removal_finished();
                    // SAFETY: removal ended; a deferred scan may update rows.
                    unsafe {
                        SetTimer(hwnd, TIMER_DEVICE_CHANGE, 400, None);
                    }
                    app.update_controls();
                }
                DBT_DEVICEARRIVAL | DBT_DEVNODES_CHANGED => {
                    if !app.state.borrow().removal_pending() {
                        // SAFETY: coalesce notifications only outside removal.
                        unsafe {
                            SetTimer(hwnd, TIMER_DEVICE_CHANGE, 400, None);
                        }
                    }
                }
                _ => {}
            }
            // Grant query-remove requests; this application never vetoes them.
            1
        }
        WM_TIMER => {
            let timer = wparam;
            // SAFETY: timer belongs to this window.
            unsafe {
                KillTimer(hwnd, timer);
            }
            if timer == TIMER_DEVICE_CHANGE || timer == TIMER_AFTER_EJECT {
                if app.state.borrow().removal_pending() {
                    return 0;
                }
                let busy = app.state.borrow().is_busy();
                if busy {
                    // SAFETY: defer until the current worker finishes.
                    unsafe {
                        SetTimer(hwnd, timer, 400, None);
                    }
                } else {
                    app.start_background_refresh(hwnd);
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
        Err(super::super::windows::last_error("创建操作按钮"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowTextW, UnregisterClassW};

    unsafe extern "system" fn test_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if matches!(
            message,
            WM_NCCREATE | WM_NCDESTROY | WM_DEVICECHANGE | WM_TIMER
        ) {
            // SAFETY: dispatch genuine Windows messages to the production
            // handler, excluding WM_CREATE so this test never scans disks.
            unsafe { window_proc_inner(hwnd, message, wparam, lparam) }
        } else {
            // SAFETY: default handling for this test window's other messages.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }

    fn test_state(hwnd: HWND) -> (bool, bool) {
        // SAFETY: this test reads state only while its created window is live;
        // no App reference is retained across another window API call.
        let app = unsafe { &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App) };
        let state = app.state.borrow();
        (state.removal_pending(), state.is_busy())
    }

    #[test]
    fn native_window_preserves_title_and_suspends_scans_during_removal() {
        let class_name = wide("UsbEjectorNativeRegressionWindow");
        let expected_title = window_title(false);
        let title = wide(&expected_title);
        let mut pending = Some(Box::new(App::new(false)));
        // SAFETY: buffers and the pending Box outlive synchronous creation;
        // the hidden test window is destroyed before its class is unregistered.
        unsafe {
            let instance = GetModuleHandleW(null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(test_window_proc),
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                ..zeroed()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                400,
                200,
                null_mut(),
                null_mut(),
                instance,
                (&mut pending as *mut Option<Box<App>>).cast(),
            );
            assert!(!hwnd.is_null());
            let transferred = pending.is_none();
            let mut text = [0_u16; 128];
            let text_length = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32);
            let query_result =
                SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVICEQUERYREMOVE as usize, 0);
            let after_query = test_state(hwnd);
            SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVICEARRIVAL as usize, 0);
            SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVNODES_CHANGED as usize, 0);
            SendMessageW(hwnd, WM_TIMER, TIMER_DEVICE_CHANGE, 0);
            let after_unrelated_events = test_state(hwnd);
            SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVICEQUERYREMOVEFAILED as usize, 0);
            let after_failed = test_state(hwnd);
            SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVICEREMOVEPENDING as usize, 0);
            let after_pending = test_state(hwnd);
            SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVICEREMOVECOMPLETE as usize, 0);
            let after_complete = test_state(hwnd);
            let removal_refresh_scheduled = KillTimer(hwnd, TIMER_DEVICE_CHANGE);

            // Enter the GUI eject state without starting a backend worker. This
            // verifies real timer scheduling without opening any disk or volume.
            {
                let app = &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App);
                let mut state = app.state.borrow_mut();
                let device = Device::new(
                    "USB\\TEST".into(),
                    "test-container".into(),
                    "USBSTOR".into(),
                    "test-device".into(),
                    Vec::new(),
                    Vec::new(),
                );
                let id = device.public_id.clone();
                let generation = state.begin_refresh().unwrap();
                state.finish_refresh(
                    generation,
                    Ok(vec![ListedDevice {
                        device,
                        eligibility: Ok(()),
                    }]),
                );
                assert!(state.select(&id));
                assert!(state.begin_eject().is_some());
            }
            SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVICEQUERYREMOVE as usize, 0);
            SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVICEQUERYREMOVEFAILED as usize, 0);
            let own_failed_refresh_scheduled = KillTimer(hwnd, TIMER_DEVICE_CHANGE);
            let during_own_failed_query = test_state(hwnd);
            {
                let app = &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App);
                app.queue.lock().unwrap().push_back(WorkerMessage::Eject(
                    Err(BackendError("test refusal".into())),
                    Some("test diagnostics".into()),
                ));
                app.drain_results(hwnd);
            }
            let completion_refresh_scheduled = KillTimer(hwnd, TIMER_AFTER_EJECT);
            SendMessageW(hwnd, WM_DEVICECHANGE, DBT_DEVICEARRIVAL as usize, 0);
            let arrival_refresh_scheduled = KillTimer(hwnd, TIMER_DEVICE_CHANGE);
            let destroyed = DestroyWindow(hwnd);
            let unregistered = UnregisterClassW(class_name.as_ptr(), instance);

            assert!(transferred);
            assert_eq!(
                String::from_utf16_lossy(&text[..text_length as usize]),
                expected_title
            );
            assert_eq!(query_result, 1);
            assert_eq!(after_query, (true, false));
            assert_eq!(after_unrelated_events, (true, false));
            assert_eq!(after_failed, (false, false));
            assert_eq!(after_pending, (true, false));
            assert_eq!(after_complete, (false, false));
            assert_ne!(removal_refresh_scheduled, 0);
            assert_eq!(own_failed_refresh_scheduled, 0);
            assert_eq!(during_own_failed_query, (false, true));
            assert_ne!(completion_refresh_scheduled, 0);
            assert_ne!(arrival_refresh_scheduled, 0);
            assert_ne!(destroyed, 0);
            assert_ne!(unregistered, 0);
        }
    }
}
