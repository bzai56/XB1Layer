//! WinRT shell layer: our own `CoreApplication`, `CoreApplicationView`, `CoreWindow`
//! and dispatcher, so a UWP-style title's `IFrameworkView` callbacks run under our control.
//!
//! The title is a C++/CX app. It reaches this layer through `GetActivationFactoryByPCWSTR`
//! (see `try_activation_factory`), calls `ICoreApplication::Run(source)`, and from then on
//! talks to the view, window and dispatcher objects created here.
//!
//! `ICoreWindow` and the activation-args interfaces are implemented with the `windows`
//! crate. `ICoreApplication`, `ICoreApplicationView` and `ICoreDispatcher` have no
//! implementable trait there, so they are small hand-written COM objects built on the
//! crate's vtable structs (which gives the correct slot order).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::ApplicationModel::Activation::*;
use windows::ApplicationModel::Core::*;
use windows::Foundation::Collections::*;
use windows::Foundation::*;
use windows::UI::Core::*;
use windows::Win32::Foundation::{E_NOINTERFACE, E_NOTIMPL, E_POINTER};
use windows::core::*;

macro_rules! wlog {
    ($($t:tt)*) => { crate::raw_log(format_args!($($t)*)) };
}

/// Like `wlog!`, but only the first 20 calls from this site and then each power of two, so calls
/// a title makes every frame do not flood the log.
macro_rules! wlog_rare {
    ($($t:tt)*) => {{
        static RARE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = RARE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 20 || n.is_power_of_two() {
            crate::raw_log(format_args!($($t)*))
        }
    }};
}

unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

unsafe extern "system" {
    fn VirtualQuery(addr: *mut c_void, info: *mut c_void, len: usize) -> usize;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn PeekMessageW(msg: *mut [u8; 48], hwnd: *mut c_void, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(msg: *const [u8; 48]) -> i32;
    fn DispatchMessageW(msg: *const [u8; 48]) -> isize;
    fn MsgWaitForMultipleObjects(n: u32, handles: *const c_void, all: i32, ms: u32, mask: u32) -> u32;
}

/// Delegates the title registers on the console window through its event-add slots: `(slot, handler)`.
static WINDOW_HANDLERS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Event-args object for the window events raised by `raise_window_events`: every getter writes
/// `WINDOW_EVENT_VALUE` (a small integer: `bool Visible`, an activation-state enum, ...) through its
/// out-pointer. What each of the console window's events is, is unknown, so this is an experiment.
#[repr(C)]
struct WindowArgs {
    vtbl: *const usize,
    refs: AtomicU32,
}

unsafe extern "system" fn wargs_qi(this: *mut c_void, _iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    unsafe { *out = this };
    HRESULT(0)
}
unsafe extern "system" fn wargs_ref(_this: *mut c_void) -> u32 {
    2
}
unsafe extern "system" fn wargs_get(_this: *mut c_void, out: *mut u32) -> HRESULT {
    let v = std::env::var("XB1_WINDOW_EVENT_VALUE").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    if !out.is_null() && (out as usize) % 4 == 0 {
        unsafe { *out = v };
    }
    HRESULT(0)
}

fn window_args() -> *mut c_void {
    static OBJ: OnceLock<usize> = OnceLock::new();
    *OBJ.get_or_init(|| {
        let mut v = vec![wargs_get as usize; 32];
        v[0] = wargs_qi as usize;
        v[1] = wargs_ref as usize;
        v[2] = wargs_ref as usize;
        let vtbl = Vec::leak(v).as_ptr();
        Box::into_raw(Box::new(WindowArgs { vtbl, refs: AtomicU32::new(2) })) as usize
    }) as *mut c_void
}

/// Raises every delegate the title registered on the console window: `Invoke(sender, args)` is slot 3
/// of a delegate. Only done when `XB1_WINDOW_EVENTS` is set (an experiment).
pub fn raise_window_events() {
    let handlers: Vec<(usize, usize)> = WINDOW_HANDLERS.lock().unwrap().clone();
    wlog!("[winrt] raising {} window event handler(s)", handlers.len());
    let sender = shell().console_window;
    for (slot, h) in handlers {
        let vtbl = unsafe { *(h as *const *const usize) };
        let invoke: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT = unsafe { std::mem::transmute(*vtbl.add(3)) };
        let hr = unsafe { invoke(h as *mut c_void, sender, window_args()) };
        wlog!("[winrt]   window event (add slot {slot}): Invoke -> {:#x}", hr.0 as u32);
    }
}

/// Delegates the title registers on `Windows.Xbox.System.User` statics: `(slot, handler)`.
static USER_HANDLERS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Raises every user-event delegate (`Invoke(sender, args)`): `args` is a generic object that hands
/// out stand-ins (the signed-in user element for any getter). On by default; `XB1_NO_USER_EVENTS=1` turns it off.
pub fn raise_user_events() {
    // The title registers these during `Run`, so a helper thread waits for them to appear and raises
    // each handler once, a moment after it is registered.
    std::thread::spawn(|| {
        crate::ensure_tls_on_current_thread();
        let mut raised = 0usize;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let (total, pending): (usize, Vec<(usize, usize)>) = {
                let all = USER_HANDLERS.lock().unwrap();
                (all.len(), all[raised..].to_vec())
            };
            if pending.is_empty() {
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(1000));
            raise_user_handlers(&pending);
            raised = total;
        }
        crate::restore_loader_tls();
    });
}

fn raise_user_handlers(handlers: &[(usize, usize)]) {
    wlog!("[winrt] raising {} user event handler(s)", handlers.len());
    for &(slot, h) in handlers {
        let vtbl = unsafe { *(h as *const *const usize) };
        let invoke: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT = unsafe { std::mem::transmute(*vtbl.add(3)) };
        let iid = GUID::from_u128(0xB4F23A93_7FC6_4A50_B153_950016389E50);
        let sender = create_generic("Windows.Xbox.System.User.Users (element 0)", &iid);
        let args = create_generic("UserEventArgs", &iid);
        let hr = unsafe { invoke(h as *mut c_void, sender, args) };
        wlog!("[winrt]   user event (add slot {slot}): Invoke -> {:#x}", hr.0 as u32);
    }
}

/// Delegates the title registers on the `Windows.Xbox.Input` statics: `(class, slot, handler)`.
static INPUT_HANDLERS: Mutex<Vec<(&'static str, usize, usize)>> = Mutex::new(Vec::new());

/// The controller as the title holds it (`IGamepad^`): the controller list hands it out and the input events use it as
/// their sender, so the title sees the same object everywhere. On the older SDK it has its own vtable layout: slot 8
/// is `GetCurrentReading(GamepadReading*)`. The `IController` view of it (`9776FF56`, see `controller_view`) answers
/// the same slot number with the user.
fn controller_element() -> *mut c_void {
    static ELEMENT: OnceLock<usize> = OnceLock::new();
    *ELEMENT.get_or_init(|| {
        let iid = GUID::from_u128(0x8C4C1BCD_FDAB_4BF8_BDE9_ABB2B1D6FDD3);
        create_generic("Windows.Xbox.Input.Gamepad (element)", &iid) as usize
    }) as *mut c_void
}

/// The `IController` (`9776FF56`) view of the controller: slot 6 is `get_Id`, 7 `get_Type`, 8 `get_User`.
fn controller_view() -> *mut c_void {
    static VIEW: OnceLock<usize> = OnceLock::new();
    *VIEW.get_or_init(|| {
        let iid = GUID::from_u128(0x9776FF56_9BFD_4581_AD45_B645BBA526D6);
        create_generic("Windows.Xbox.Input.Controller.Controllers (element 0)", &iid) as usize
    }) as *mut c_void
}

/// Raises the registered `Gamepad` / `Controller` events once (a controller was added): `Invoke(sender, args)` with the
/// controller element as the sender and as the answer of any getter on the arguments. `XB1_NO_INPUT_EVENTS=1` turns it off.
pub fn raise_input_events() {
    if std::env::var_os("XB1_NO_INPUT_EVENTS").is_some() || !crate::VCCORLIB110.load(Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        // The handlers are the title's code and use its thread-local storage.
        crate::ensure_tls_on_current_thread();
        let mut raised = 0usize;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let (total, pending): (usize, Vec<(&'static str, usize, usize)>) = {
                let all = INPUT_HANDLERS.lock().unwrap();
                (all.len(), all[raised..].to_vec())
            };
            if pending.is_empty() {
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
            wlog!("[winrt] raising {} input event handler(s)", pending.len());
            for (class, slot, h) in pending {
                let vtbl = unsafe { *(h as *const *const usize) };
                let invoke: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT = unsafe { std::mem::transmute(*vtbl.add(3)) };
                let iid = GUID::from_u128(0x8C4C1BCD_FDAB_4BF8_BDE9_ABB2B1D6FDD3);
                // `ControllerPairingChangedEventArgs.Controller` is an `IController^`, `GamepadAddedEventArgs.Gamepad` an `IGamepad^`.
                let args = create_generic(if class == "Controller" { "ControllerEventArgs" } else { "GamepadEventArgs" }, &iid);
                let hr = unsafe { invoke(h as *mut c_void, controller_element(), args) };
                wlog!("[winrt]   input event ({class} add slot {slot}): Invoke -> {:#x}", hr.0 as u32);
            }
            raised = total;
        }
        crate::restore_loader_tls();
    });
}

static NEXT_TOKEN: AtomicI64 = AtomicI64::new(1);

fn next_token() -> i64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// Process-wide shell state. The title may call in from several threads.
struct Shell {
    view: *mut c_void,
    window: ICoreWindow,
    /// The window object handed to the title: a generic object with the console's layout.
    console_window: *mut c_void,
    dispatcher: *mut c_void,
    activated: Mutex<Vec<(i64, TypedEventHandler<CoreApplicationView, IActivatedEventArgs>)>>,
    queue: Mutex<VecDeque<DispatchedHandler>>,
    quit: AtomicBool,
    ui_thread: AtomicU32,
}

unsafe impl Send for Shell {}
unsafe impl Sync for Shell {}

static SHELL: OnceLock<Shell> = OnceLock::new();

fn shell() -> &'static Shell {
    SHELL.get_or_init(|| Shell {
        view: Com::<AppViewTag>::create(()),
        window: Window.into(),
        console_window: {
            let o = create_generic("Windows.UI.Core.CoreWindow (console layout)", &ICoreWindow::IID);
            unsafe {
                (*(o as *mut GenericObj)).kind = 2;
                (*(o as *mut GenericObj)).vtbl = console_window_vtbl();
            }
            o
        },
        dispatcher: Com::<DispatcherTag>::create(()),
        activated: Mutex::new(Vec::new()),
        queue: Mutex::new(VecDeque::new()),
        quit: AtomicBool::new(false),
        ui_thread: AtomicU32::new(0),
    })
}

// ---------------------------------------------------------------------------------------
// Minimal hand-written COM objects
// ---------------------------------------------------------------------------------------

trait Iface: 'static {
    const IID: GUID;
    const NAME: &'static str;
    type Vtbl: 'static;
    type State: Send + Sync + 'static;
    const VTBL: &'static Self::Vtbl;
}

#[repr(C)]
struct Com<T: Iface> {
    vtbl: *const T::Vtbl,
    refs: AtomicU32,
    #[allow(dead_code)]
    state: T::State,
    _p: PhantomData<T>,
}

impl<T: Iface> Com<T> {
    /// Returns a new object with one reference owned by the caller.
    fn create(state: T::State) -> *mut c_void {
        Box::into_raw(Box::new(Com::<T> { vtbl: T::VTBL, refs: AtomicU32::new(1), state, _p: PhantomData })) as *mut c_void
    }
}

unsafe extern "system" fn com_query_interface<T: Iface>(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    let iid = unsafe { *iid };
    if iid == IUnknown::IID || iid == IInspectable::IID || iid == T::IID {
        unsafe {
            com_add_ref::<T>(this);
            *out = this;
        }
        HRESULT(0)
    } else {
        unsafe { *out = ptr::null_mut() };
        wlog!("[winrt] {}::QueryInterface({iid:?}) -> E_NOINTERFACE", T::NAME);
        E_NOINTERFACE
    }
}

unsafe extern "system" fn com_add_ref<T: Iface>(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const Com<T>)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn com_release<T: Iface>(this: *mut c_void) -> u32 {
    let left = unsafe { (*(this as *const Com<T>)).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        drop(unsafe { Box::from_raw(this as *mut Com<T>) });
    }
    left
}

unsafe extern "system" fn com_get_iids(_this: *mut c_void, count: *mut u32, iids: *mut *mut GUID) -> HRESULT {
    // The title also reaches this slot through `GetWeakReference` (an IWeakReferenceSource query
    // answered with this object), passing one out-pointer and garbage; only well-formed arguments
    // are written.
    unsafe {
        if (count as usize) % 4 == 0 && !count.is_null() {
            ptr::write_unaligned(count, 0);
        }
        if (iids as usize) % 8 == 0 && !iids.is_null() {
            ptr::write_unaligned(iids, ptr::null_mut());
        }
    }
    HRESULT(0)
}

unsafe extern "system" fn com_get_runtime_class_name<T: Iface>(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { *out = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from(T::NAME)) };
    HRESULT(0)
}

unsafe extern "system" fn com_get_trust_level(_this: *mut c_void, level: *mut i32) -> HRESULT {
    unsafe { *level = 0 };
    HRESULT(0)
}

const fn inspectable<T: Iface>() -> IInspectable_Vtbl {
    IInspectable_Vtbl {
        base: IUnknown_Vtbl { QueryInterface: com_query_interface::<T>, AddRef: com_add_ref::<T>, Release: com_release::<T> },
        GetIids: com_get_iids,
        GetRuntimeClassName: com_get_runtime_class_name::<T>,
        GetTrustLevel: com_get_trust_level,
    }
}

fn to_hr(r: Result<()>) -> HRESULT {
    match r {
        Ok(()) => HRESULT(0),
        Err(e) => {
            wlog!("[winrt] call failed: {e}");
            e.code()
        }
    }
}

// ---------------------------------------------------------------------------------------
// ICoreApplication (the CoreApplication statics)
// ---------------------------------------------------------------------------------------

struct CoreAppTag;

/// `Windows.UI.Core.CoreWindow` statics (`ICoreWindowStatic`): `GetForCurrentThread` returns the
/// shell's window, since the real runtime has none for this process.
struct CoreWindowStaticsTag;

impl Iface for CoreWindowStaticsTag {
    const IID: GUID = ICoreWindowStatic::IID;
    const NAME: &'static str = "Windows.UI.Core.CoreWindow";
    type Vtbl = ICoreWindowStatic_Vtbl;
    type State = ();
    const VTBL: &'static Self::Vtbl = &ICoreWindowStatic_Vtbl {
        base__: inspectable::<CoreWindowStaticsTag>(),
        GetForCurrentThread: core_window_for_current_thread,
    };
}

unsafe extern "system" fn core_window_for_current_thread(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    wlog_rare!("[winrt] CoreWindow::GetForCurrentThread");
    // The console-layout window: titles call slot 7 expecting the dispatcher (the desktop layout has
    // Bounds there).
    unsafe { *out = generic_add_ref_ret(shell().console_window) };
    HRESULT(0)
}

impl Iface for CoreAppTag {
    const IID: GUID = ICoreApplication::IID;
    const NAME: &'static str = "Windows.ApplicationModel.Core.CoreApplication";
    type Vtbl = ICoreApplication_Vtbl;
    type State = ();
    const VTBL: &'static Self::Vtbl = &ICoreApplication_Vtbl {
        base__: inspectable::<CoreAppTag>(),
        Id: app_id,
        Suspending: app_add_event,
        RemoveSuspending: app_remove_event,
        Resuming: app_add_event,
        RemoveResuming: app_remove_event,
        Properties: app_properties,
        GetCurrentView: app_get_current_view,
        Run: app_run,
        RunWithActivationFactories: app_run_with_factories,
    };
}

unsafe extern "system" fn app_id(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    wlog!("[winrt] CoreApplication::Id");
    unsafe { *out = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from("App")) };
    HRESULT(0)
}

unsafe extern "system" fn app_add_event(_this: *mut c_void, _handler: *mut c_void, token: *mut i64) -> HRESULT {
    wlog!("[winrt] CoreApplication::add_Suspending/Resuming");
    unsafe { *token = next_token() };
    HRESULT(0)
}

unsafe extern "system" fn app_remove_event(_this: *mut c_void, _token: i64) -> HRESULT {
    HRESULT(0)
}

unsafe extern "system" fn app_properties(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    wlog!("[winrt] CoreApplication::Properties");
    match PropertySet::new().and_then(|p| p.cast::<IPropertySet>()) {
        Ok(p) => {
            unsafe { *out = p.into_raw() };
            HRESULT(0)
        }
        Err(e) => e.code(),
    }
}

unsafe extern "system" fn app_get_current_view(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    wlog!("[winrt] CoreApplication::GetCurrentView");
    unsafe { *out = add_ref_raw::<AppViewTag>(shell().view) };
    HRESULT(0)
}

unsafe extern "system" fn app_run_with_factories(_this: *mut c_void, _f: *mut c_void) -> HRESULT {
    wlog!("[winrt] CoreApplication::RunWithActivationFactories (not implemented)");
    E_NOTIMPL
}

/// `CoreApplication::Run(IFrameworkViewSource)`: drives the title's `IFrameworkView`
/// through its lifecycle.
unsafe extern "system" fn app_run(_this: *mut c_void, source: *mut c_void) -> HRESULT {
    wlog!("[winrt] CoreApplication::Run");
    let Some(source) = (unsafe { IFrameworkViewSource::from_raw_borrowed(&source) }) else {
        return E_POINTER;
    };
    to_hr(run_view(source))
}

fn run_view(source: &IFrameworkViewSource) -> Result<()> {
    let sh = shell();
    if crate::TRACE_WINDOW.load(Ordering::Relaxed) {
        instrument_window();
    }
    sh.ui_thread.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);

    let view = source.CreateView()?;
    wlog!("[winrt] IFrameworkViewSource::CreateView ok");
    let app_view: CoreApplicationView = unsafe { ICoreApplicationView::from_raw_borrowed(&sh.view) }.unwrap().cast()?;

    {
        use windows_core::Interface;
        let vtbl = unsafe { *(view.as_raw() as *const *const usize) };
        let names = ["Initialize", "SetWindow", "Load", "Run", "Uninitialize"];
        for (i, n) in names.iter().enumerate() {
            wlog!("[winrt] title IFrameworkView::{n} = {:#x}", unsafe { *vtbl.add(6 + i) });
        }
    }
    wlog!("[winrt] IFrameworkView::Initialize");
    view.Initialize(&app_view)?;
    let window: CoreWindow = unsafe { CoreWindow::from_raw_borrowed(&sh.console_window) }.unwrap().clone();
    wlog!("[winrt] IFrameworkView::SetWindow");
    view.SetWindow(&window)?;
    wlog!("[winrt] IFrameworkView::Load");
    view.Load(&HSTRING::new())?;

    // The real shell raises Activated once the view is loaded.
    let args: IActivatedEventArgs = LaunchArgs.into();
    let handlers: Vec<_> = sh.activated.lock().unwrap().iter().map(|(_, h)| h.clone()).collect();
    wlog!("[winrt] raising CoreApplicationView::Activated to {} handler(s)", handlers.len());
    for h in handlers {
        {
            use windows_core::Interface;
            let vtbl = unsafe { *(h.as_raw() as *const *const usize) };
            wlog!("[winrt] Activated handler Invoke = {:#x}", unsafe { *vtbl.add(3) });
        }
        h.Invoke(&app_view, &args)?;
    }

    if std::env::var_os("XB1_NO_USER_EVENTS").is_none() {
        raise_user_events();
        raise_input_events();
    }
    if std::env::var_os("XB1_WINDOW_EVENTS").is_some() {
        raise_window_events();
    }
    wlog!("[winrt] IFrameworkView::Run");
    view.Run()?;
    wlog!("[winrt] IFrameworkView::Uninitialize");
    view.Uninitialize()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// ICoreApplicationView
// ---------------------------------------------------------------------------------------

struct AppViewTag;

impl Iface for AppViewTag {
    const IID: GUID = ICoreApplicationView::IID;
    const NAME: &'static str = "Windows.ApplicationModel.Core.CoreApplicationView";
    type Vtbl = ICoreApplicationView_Vtbl;
    type State = ();
    const VTBL: &'static Self::Vtbl = &ICoreApplicationView_Vtbl {
        base__: inspectable::<AppViewTag>(),
        CoreWindow: view_core_window,
        Activated: view_add_activated,
        RemoveActivated: view_remove_activated,
        IsMain: view_is_main,
        IsHosted: view_is_hosted,
    };
}

unsafe fn add_ref_raw<T: Iface>(p: *mut c_void) -> *mut c_void {
    unsafe { com_add_ref::<T>(p) };
    p
}

unsafe extern "system" fn view_core_window(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    wlog!("[winrt] CoreApplicationView::CoreWindow");
    unsafe { *out = generic_add_ref_ret(shell().console_window) };
    HRESULT(0)
}

unsafe extern "system" fn view_add_activated(_this: *mut c_void, handler: *mut c_void, token: *mut i64) -> HRESULT {
    let h = unsafe { TypedEventHandler::<CoreApplicationView, IActivatedEventArgs>::from_raw_borrowed(&handler) };
    let Some(h) = h else { return E_POINTER };
    let t = next_token();
    shell().activated.lock().unwrap().push((t, h.clone()));
    wlog!("[winrt] CoreApplicationView::add_Activated");
    unsafe { *token = t };
    HRESULT(0)
}

unsafe extern "system" fn view_remove_activated(_this: *mut c_void, token: i64) -> HRESULT {
    shell().activated.lock().unwrap().retain(|(t, _)| *t != token);
    HRESULT(0)
}

unsafe extern "system" fn view_is_main(_this: *mut c_void, out: *mut bool) -> HRESULT {
    unsafe { *out = true };
    HRESULT(0)
}

unsafe extern "system" fn view_is_hosted(_this: *mut c_void, out: *mut bool) -> HRESULT {
    unsafe { *out = false };
    HRESULT(0)
}

// ---------------------------------------------------------------------------------------
// ICoreDispatcher
// ---------------------------------------------------------------------------------------

struct DispatcherTag;

impl Iface for DispatcherTag {
    const IID: GUID = ICoreDispatcher::IID;
    const NAME: &'static str = "Windows.UI.Core.CoreDispatcher";
    type Vtbl = ICoreDispatcher_Vtbl;
    type State = ();
    const VTBL: &'static Self::Vtbl = &ICoreDispatcher_Vtbl {
        base__: inspectable::<DispatcherTag>(),
        HasThreadAccess: disp_has_thread_access,
        ProcessEvents: disp_process_events,
        RunAsync: disp_run_async,
        RunIdleAsync: disp_run_idle_async,
    };
}

unsafe extern "system" fn disp_has_thread_access(_this: *mut c_void, out: *mut bool) -> HRESULT {
    let ui = shell().ui_thread.load(Ordering::Relaxed);
    unsafe { *out = ui == 0 || ui == GetCurrentThreadId() };
    HRESULT(0)
}

/// Runs queued work items, then pumps Win32 messages.
fn pump_once() -> bool {
    let mut did = false;
    loop {
        let item = shell().queue.lock().unwrap().pop_front();
        let Some(item) = item else { break };
        did = true;
        if let Err(e) = item.Invoke() {
            wlog!("[winrt] dispatched work item failed: {e}");
        }
    }
    let mut msg = [0u8; 48];
    // PM_REMOVE
    while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, 1) } != 0 {
        did = true;
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    did
}

unsafe extern "system" fn disp_process_events(_this: *mut c_void, option: CoreProcessEventsOption) -> HRESULT {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        wlog!("[winrt] CoreDispatcher::ProcessEvents(option={})", option.0);
    }
    if option == CoreProcessEventsOption::ProcessUntilQuit {
        while !shell().quit.load(Ordering::Relaxed) {
            if !pump_once() {
                // QS_ALLINPUT; wake on input or after a short timeout to re-check the queue.
                unsafe { MsgWaitForMultipleObjects(0, ptr::null(), 0, 10, 0x4FF) };
            }
        }
    } else {
        pump_once();
    }
    HRESULT(0)
}

unsafe extern "system" fn disp_run_async(_this: *mut c_void, _priority: CoreDispatcherPriority, handler: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let Some(h) = (unsafe { DispatchedHandler::from_raw_borrowed(&handler) }) else { return E_POINTER };
    shell().queue.lock().unwrap().push_back(h.clone());
    match windows_future::IAsyncAction::ready(Ok(())) {
        action => {
            unsafe { *out = action.into_raw() };
            HRESULT(0)
        }
    }
}

unsafe extern "system" fn disp_run_idle_async(_this: *mut c_void, _handler: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    wlog!("[winrt] CoreDispatcher::RunIdleAsync (not implemented)");
    unsafe { *out = ptr::null_mut() };
    E_NOTIMPL
}

// ---------------------------------------------------------------------------------------
// ICoreWindow
// ---------------------------------------------------------------------------------------

#[implement(ICoreWindow)]
struct Window;

macro_rules! window_events {
    ($(($add:ident, $remove:ident, $arg:ty)),* $(,)?) => {
        $(
            fn $add(&self, _handler: Ref<TypedEventHandler<CoreWindow, $arg>>) -> Result<i64> {
                wlog!(concat!("[winrt] CoreWindow::", stringify!($add)));
                Ok(next_token())
            }
            fn $remove(&self, _cookie: i64) -> Result<()> {
                Ok(())
            }
        )*
    };
}

impl ICoreWindow_Impl for Window_Impl {
    fn AutomationHostProvider(&self) -> Result<IInspectable> {
        wlog!("[winrt] CoreWindow::AutomationHostProvider");
        Err(E_NOTIMPL.into())
    }
    fn Bounds(&self) -> Result<Rect> {
        wlog!("[winrt] CoreWindow::Bounds");
        Ok(Rect { X: 0.0, Y: 0.0, Width: 1920.0, Height: 1080.0 })
    }
    fn CustomProperties(&self) -> Result<IPropertySet> {
        wlog!("[winrt] CoreWindow::CustomProperties");
        PropertySet::new()?.cast()
    }
    fn Dispatcher(&self) -> Result<CoreDispatcher> {
        wlog!("[winrt] CoreWindow::Dispatcher");
        unsafe { ICoreDispatcher::from_raw_borrowed(&shell().dispatcher) }.unwrap().cast()
    }
    fn FlowDirection(&self) -> Result<CoreWindowFlowDirection> {
        Ok(CoreWindowFlowDirection::LeftToRight)
    }
    fn SetFlowDirection(&self, _value: CoreWindowFlowDirection) -> Result<()> {
        Ok(())
    }
    fn IsInputEnabled(&self) -> Result<bool> {
        Ok(true)
    }
    fn SetIsInputEnabled(&self, _value: bool) -> Result<()> {
        Ok(())
    }
    fn PointerCursor(&self) -> Result<CoreCursor> {
        wlog!("[winrt] CoreWindow::PointerCursor");
        Err(E_NOTIMPL.into())
    }
    fn SetPointerCursor(&self, _value: Ref<CoreCursor>) -> Result<()> {
        wlog!("[winrt] CoreWindow::SetPointerCursor");
        Ok(())
    }
    fn PointerPosition(&self) -> Result<Point> {
        Ok(Point { X: 0.0, Y: 0.0 })
    }
    fn Visible(&self) -> Result<bool> {
        Ok(true)
    }
    fn Activate(&self) -> Result<()> {
        wlog!("[winrt] CoreWindow::Activate");
        Ok(())
    }
    fn Close(&self) -> Result<()> {
        wlog!("[winrt] CoreWindow::Close");
        shell().quit.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn GetAsyncKeyState(&self, _key: windows::System::VirtualKey) -> Result<CoreVirtualKeyStates> {
        Ok(CoreVirtualKeyStates::None)
    }
    fn GetKeyState(&self, _key: windows::System::VirtualKey) -> Result<CoreVirtualKeyStates> {
        Ok(CoreVirtualKeyStates::None)
    }
    fn ReleasePointerCapture(&self) -> Result<()> {
        Ok(())
    }
    fn SetPointerCapture(&self) -> Result<()> {
        Ok(())
    }

    window_events!(
        (Activated, RemoveActivated, WindowActivatedEventArgs),
        (AutomationProviderRequested, RemoveAutomationProviderRequested, AutomationProviderRequestedEventArgs),
        (CharacterReceived, RemoveCharacterReceived, CharacterReceivedEventArgs),
        (Closed, RemoveClosed, CoreWindowEventArgs),
        (InputEnabled, RemoveInputEnabled, InputEnabledEventArgs),
        (KeyDown, RemoveKeyDown, KeyEventArgs),
        (KeyUp, RemoveKeyUp, KeyEventArgs),
        (PointerCaptureLost, RemovePointerCaptureLost, PointerEventArgs),
        (PointerEntered, RemovePointerEntered, PointerEventArgs),
        (PointerExited, RemovePointerExited, PointerEventArgs),
        (PointerMoved, RemovePointerMoved, PointerEventArgs),
        (PointerPressed, RemovePointerPressed, PointerEventArgs),
        (PointerReleased, RemovePointerReleased, PointerEventArgs),
        (TouchHitTesting, RemoveTouchHitTesting, TouchHitTestingEventArgs),
        (PointerWheelChanged, RemovePointerWheelChanged, PointerEventArgs),
        (SizeChanged, RemoveSizeChanged, WindowSizeChangedEventArgs),
        (VisibilityChanged, RemoveVisibilityChanged, VisibilityChangedEventArgs),
    );
}

// ---------------------------------------------------------------------------------------
// Activation arguments
// ---------------------------------------------------------------------------------------

#[implement(ILaunchActivatedEventArgs, IActivatedEventArgs)]
struct LaunchArgs;

impl IActivatedEventArgs_Impl for LaunchArgs_Impl {
    fn Kind(&self) -> Result<ActivationKind> {
        Ok(ActivationKind::Launch)
    }
    fn PreviousExecutionState(&self) -> Result<ApplicationExecutionState> {
        Ok(ApplicationExecutionState::NotRunning)
    }
    fn SplashScreen(&self) -> Result<SplashScreen> {
        wlog!("[winrt] LaunchActivatedEventArgs::SplashScreen");
        Err(E_NOTIMPL.into())
    }
}

impl ILaunchActivatedEventArgs_Impl for LaunchArgs_Impl {
    fn Arguments(&self) -> Result<HSTRING> {
        // XB1_LAUNCH_ARGS: experiment with what the title is launched with (it parses "restart" and
        // "ms-xbl-%X://" tokens from its activation).
        Ok(std::env::var("XB1_LAUNCH_ARGS").map(|v| HSTRING::from(v.as_str())).unwrap_or_default())
    }
    fn TileId(&self) -> Result<HSTRING> {
        Ok(HSTRING::new())
    }
}

// ---------------------------------------------------------------------------------------
// Generic stand-in for console-only WinRT classes (Windows.Xbox.*)
// ---------------------------------------------------------------------------------------
//
// There is no metadata for these classes here, so their interfaces are unknown. This object
// answers QueryInterface for any IID and logs every method call by vtable slot. Methods
// report success without writing any out-parameter (or hand back another stand-in when the
// first argument looks like one), so this is for discovery only.

const GENERIC_SLOTS: usize = 64;

#[repr(C)]
struct GenericObj {
    vtbl: *const usize,
    refs: AtomicU32,
    class: String,
    iid: GUID,
    /// 0 = fully generic; 1 = the console audio extension of the MMDevice enumerator.
    kind: u8,
}

unsafe extern "system" fn generic_qi(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    let o = unsafe { &*(this as *const GenericObj) };
    if o.class.contains("Controllers (element") {
        static ELEM_QI: AtomicU32 = AtomicU32::new(0);
        if ELEM_QI.fetch_add(1, Ordering::Relaxed) < 200 {
            wlog!("[winrt] element QueryInterface({:?})", unsafe { *iid });
        }
    }
    if o.class == "Windows.Xbox.Input.Gamepad (element)" || o.class == "Windows.Xbox.Input.Controller.Controllers (element 0)" && crate::VCCORLIB110.load(Ordering::Relaxed) {
        let want = unsafe { *iid };
        let target = if want == GUID::from_u128(0x9776FF56_9BFD_4581_AD45_B645BBA526D6) { Some(controller_view()) } else if want == GUID::from_u128(0x082E402C_07DF_45E1_A5AB_A3127AF197B5) { Some(controller_element()) } else { None };
        if let Some(t) = target {
            unsafe { generic_add_ref(t) };
            unsafe { *out = t };
            return HRESULT(0);
        }
    }
    if o.class.starts_with("DownloadableContentPackageManager.Packages") && unsafe { (*iid).data1 } == 0x356E4A78 {
        // The package list as `IIterable`: slot 6 is `First(IIterator** out)`.
        let it = create_generic("DownloadableContentPackageManager.Packages (iterable)", unsafe { &*iid });
        unsafe { (*(it as *mut GenericObj)).vtbl = iterable_vtbl() };
        unsafe { *out = it };
        return HRESULT(0);
    }
    wlog_rare!("[winrt] {}::QueryInterface({:?}) -> self", o.class, unsafe { *iid });
    o.refs.fetch_add(1, Ordering::AcqRel);
    unsafe { *out = this };
    HRESULT(0)
}

unsafe extern "system" fn generic_add_ref(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const GenericObj)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe fn generic_add_ref_ret(p: *mut c_void) -> *mut c_void {
    unsafe { generic_add_ref(p) };
    p
}

unsafe extern "system" fn generic_release(this: *mut c_void) -> u32 {
    let left = unsafe { (*(this as *const GenericObj)).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        // Stand-ins stand for runtime-owned singletons (the title balances its own references on the
        // assumption that the runtime keeps the object alive), so they are never freed.
        unsafe { (*(this as *const GenericObj)).refs.store(1, Ordering::Release) };
    }
    left
}

unsafe extern "system" fn generic_class_name(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let o = unsafe { &*(this as *const GenericObj) };
    // The title compares a controller's runtime class name with "Windows.Xbox.Input.Gamepad" to pick its type.
    let name = if o.class.starts_with("Windows.Xbox.Input.Controller.Controllers (element") || o.class == "Windows.Xbox.Input.Gamepad (element)" { "Windows.Xbox.Input.Gamepad" } else { o.class.as_str() };
    wlog_rare!("[winrt] GetRuntimeClassName on {} -> {name}", o.class);
    unsafe { *out = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from(name)) };
    HRESULT(0)
}

/// True if `p` points into committed, writable memory (so it can plausibly be an out-parameter).
fn is_writable(p: *mut c_void) -> bool {
    if (p as usize) < 0x10000 || in_layer_image(p) {
        return false; // never an out-parameter: the layer's own vtables and statics live in its image
    }
    let mut mbi = [0u64; 8];
    if unsafe { VirtualQuery(p, mbi.as_mut_ptr() as *mut c_void, 48) } == 0 {
        return false;
    }
    let (state, protect) = (mbi[4] & 0xFFFF_FFFF, mbi[4] >> 32);
    // MEM_COMMIT; PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    state == 0x1000 && protect & 0xCC != 0
}

/// True if `p` looks like a COM object: its first word points at a vtable whose first entry is
/// executable code. A delegate passed to an `add_Event` method looks like this; an out-parameter
/// slot does not.
fn looks_like_com_object(p: *mut c_void) -> bool {
    if !is_readable_ptr(p) {
        return false;
    }
    let vtbl = unsafe { *(p as *const usize) };
    if !is_readable_ptr(vtbl as *mut c_void) {
        return false;
    }
    let entry = unsafe { *(vtbl as *const usize) };
    let mut mbi = [0u64; 8];
    if unsafe { VirtualQuery(entry as *mut c_void, mbi.as_mut_ptr() as *mut c_void, 48) } == 0 {
        return false;
    }
    let (state, protect) = (mbi[4] & 0xFFFF_FFFF, mbi[4] >> 32);
    // MEM_COMMIT; PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    state == 0x1000 && protect & 0xF0 != 0
}

fn is_readable_ptr(p: *mut c_void) -> bool {
    if (p as usize) < 0x10000 || (p as usize) % 8 != 0 {
        return false;
    }
    let mut mbi = [0u64; 8];
    if unsafe { VirtualQuery(p, mbi.as_mut_ptr() as *mut c_void, 48) } == 0 {
        return false;
    }
    let (state, protect) = (mbi[4] & 0xFFFF_FFFF, mbi[4] >> 32);
    state == 0x1000 && protect & 0xFF != 0 && protect & 0x101 == 0
}

/// True if `p` looks like a fast-pass `HSTRING_HEADER` (flags = 1 for a string reference, a small
/// length, a readable data pointer at +16): a string argument, not an out-parameter slot.
fn looks_like_hstring_header(p: *mut c_void) -> bool {
    if !is_readable_ptr(p) {
        return false;
    }
    let (flags, len, data) = unsafe { (*(p as *const u32), *(p as *const u32).add(1), *((p as *const usize).add(2))) };
    flags == 1 && len > 0 && len < 4096 && is_readable_ptr((data & !7) as *mut c_void)
}

/// Completes an asynchronous operation after `put_Completed` has returned, on its own thread, as the
/// runtime does (the title sets up its state around the call and does not expect the handler to run
/// before it returns). The handler and operation are kept referenced.
fn complete_later(handler: *mut c_void, op: *mut c_void) {
    if std::env::var_os("XB1_ASYNC_INLINE").is_some() {
        unsafe {
            let vt = *(handler as *const *const usize);
            let invoke: unsafe extern "system" fn(*mut c_void, *mut c_void, i32) -> HRESULT = std::mem::transmute(*vt.add(3));
            invoke(handler, op, 1);
        }
        return;
    }
    unsafe {
        let add_ref = |p: *mut c_void| {
            let vt = *(p as *const *const usize);
            let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(1));
            f(p);
        };
        add_ref(handler);
    }
    let (h, o) = (handler as usize, op as usize);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(std::env::var("XB1_ASYNC_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(5)));
        crate::ensure_tls_on_current_thread();
        unsafe {
            let handler = h as *mut c_void;
            let vt = *(handler as *const *const usize);
            let invoke: unsafe extern "system" fn(*mut c_void, *mut c_void, i32) -> HRESULT = std::mem::transmute(*vt.add(3));
            invoke(handler, o as *mut c_void, 1);
        }
        crate::restore_loader_tls();
    });
}

/// Any method of a stand-in object. The signature is unknown, so the first argument after
/// `this` is treated as a possible out-parameter: if it points at writable memory, a new
/// stand-in object is stored there and the call reports success (the common shape of a
/// property getter or factory method). Otherwise the call fails with E_NOTIMPL.
unsafe extern "system" fn generic_slot<const N: usize>(this: *mut c_void, arg1: *mut c_void, arg2: *mut c_void, a3: *mut c_void, _a4: usize, a5: *mut c_void, _a6: usize) -> HRESULT {
    let o = unsafe { &*(this as *const GenericObj) };
    {
        static LAST: Mutex<String> = Mutex::new(String::new());
        static REPORTED: AtomicBool = AtomicBool::new(false);
        let v0 = unsafe { *generic_vtbl() };
        if v0 != generic_qi as usize && !REPORTED.swap(true, Ordering::Relaxed) {
            eprintln!("[winrt] generic vtable slot 0 is corrupt ({v0:#x}), vtable at {:p}; it was fine before the previous call: {}; now {} slot {N}", generic_vtbl(), LAST.lock().unwrap(), o.class);
        }
        *LAST.lock().unwrap() = format!("{} slot {N} arg1 {arg1:p} arg2 {arg2:p} a3 {a3:p}", o.class);
    }
    // The gamepad as `IGamepad^` (older SDK): slot 8 is `GetCurrentReading(GamepadReading*)`, polled every frame into a
    // 40-byte buffer; same reading as Sonic Mania's newer SDK (36 bytes: time, buttons, triggers, sticks).
    if o.class == "Windows.Xbox.Input.Gamepad (element)" && N == 8 && is_writable(arg1) {
        unsafe { fill_gamepad_reading(arg1 as *mut u8) };
        return HRESULT(0);
    }
    // Older SDK: slot 6 of the controller interface is `get_Id` (a UInt64 the title compares to find the record).
    if o.class.starts_with("Windows.Xbox.Input.Controller.Controllers (element") && !o.class.contains('#') && N == 6 && is_writable(arg1) && crate::VCCORLIB110.load(Ordering::Relaxed) {
        unsafe { ptr::write_unaligned(arg1 as *mut u64, 1) };
        wlog_rare!("[winrt] controller slot 6 (older SDK) -> id 1");
        return HRESULT(0);
    }
    // Older SDK: slot 7 is `get_Type`, an HSTRING the title compares with "Windows.Xbox.Input.Gamepad".
    if o.class.starts_with("Windows.Xbox.Input.Controller.Controllers (element") && !o.class.contains('#') && N == 7 && is_writable(arg1) && crate::VCCORLIB110.load(Ordering::Relaxed) {
        unsafe { *(arg1 as *mut *mut c_void) = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from("Windows.Xbox.Input.Gamepad")) };
        wlog_rare!("[winrt] controller slot 7 (older SDK) -> type \"Windows.Xbox.Input.Gamepad\"");
        return HRESULT(0);
    }
    // The arguments of the input events answer every getter with the controller element.
    if (o.class == "ControllerEventArgs" || o.class == "GamepadEventArgs") && is_writable(arg1) {
        unsafe { *(arg1 as *mut *mut c_void) = controller_view() };
        wlog_rare!("[winrt] {} slot {N} -> the controller", o.class);
        return HRESULT(0);
    }
    // Older SDK: `IController::get_User` is slot 8 (Sonic Mania's newer SDK has it at slot 6). The title
    // compares the returned user with its user list, so it must be the shared user.
    if o.class.starts_with("Windows.Xbox.Input.Controller.Controllers (element") && !o.class.contains('#') && N == 8 && is_writable(arg1) && crate::VCCORLIB110.load(Ordering::Relaxed) {
        unsafe { *(arg1 as *mut *mut c_void) = shared_user() };
        wlog_rare!("[winrt] controller slot 8 (older SDK) -> the shared user element");
        return HRESULT(0);
    }
    // `CurrentApp.LicenseInformation` (`ILicenseInformation`): slot 7 is `IsActive` and slot 8 `IsTrial`,
    // both `boolean*`. The generic stand-in wrote an object pointer, which made the game a trial, so the
    // answers are a full, active license.
    if o.class == "Windows.ApplicationModel.Store.CurrentApp#slot6" && (N == 7 || N == 8) && is_writable(arg1) {
        unsafe { *(arg1 as *mut u8) = if N == 7 { 1 } else { 0 } };
        wlog_rare!("[winrt] LicenseInformation slot {N} -> {}", if N == 7 { "IsActive = true" } else { "IsTrial = false" });
        return HRESULT(0);
    }
    // Xbox Live services operation start `(a, b, c, int, Operation** out)` (title call site 0x30e9d4): the out pointer is the
    // fifth argument (on the stack), and the returned object is an async operation the title hooks a completion handler on.
    if o.class.starts_with("Microsoft.Xbox.Services.XboxLiveContext#slot6#slot") && N == 9 && (_a4 as usize) % 8 == 0 && is_writable(_a4 as *mut c_void) {
        let child = create_generic(&format!("{}#slot9", o.class), &o.iid);
        unsafe { *(_a4 as *mut *mut c_void) = child };
        wlog!("[winrt] {} slot 9 -> an async operation through the fifth argument", o.class);
        return HRESULT(0);
    }
    // The object `SystemUI` slot 10 returns is an async operation: slot 6 is `put_Completed(handler)`
    // (one argument; the second register holds the object's own vtable pointer, so the event-add path
    // would write a token over vtable slot 0, which crashed with "execute of 0xb").
    if (o.class == "Windows.Xbox.UI.SystemUI#slot10" || o.class == "Windows.Xbox.UI.SystemUI#slot11" || o.class == "Windows.Xbox.Storage.ConnectedStorageSpace#slot8" || o.class == "Windows.Xbox.Storage.ConnectedStorageSpace#slot6" || o.class.starts_with("Microsoft.Xbox.Services.XboxLiveContext#slot6#slot")) && N == 6 {
        // Complete at once: the game waits for the handler (AsyncStatus::Completed = 1).
        if looks_like_com_object(arg1) {
            let vt = unsafe { *(arg1 as *const *const usize) };
            let invoke: unsafe extern "system" fn(*mut c_void, *mut c_void, i32) -> HRESULT = unsafe { std::mem::transmute(*vt.add(3)) };
            let hr = unsafe { invoke(arg1, this, 1) };
            wlog!("[winrt] SystemUI async operation: put_Completed -> handler Invoke(Completed) = {:#x}", hr.0 as u32);
        }
        return HRESULT(0);
    }
    // The operation `SystemUI` slot 11 started: its result (slot 8, `GetResults(HSTRING*)`) is the text entered in the
    // system keyboard (read as a string by the title right after Create New World; a stand-in object there crashed
    // in the C runtime). The default world name stands in for what a player would type.
    if o.class == "Windows.Xbox.UI.SystemUI#slot11" && N == 8 && (arg1 as usize) % 8 == 0 && is_writable(arg1) && !in_layer_image(arg1) {
        unsafe { *(arg1 as *mut HSTRING) = HSTRING::from("New World") };
        wlog!("[winrt] SystemUI keyboard result -> \"New World\"");
        return HRESULT(0);
    }
    // `SystemUI` statics slot 11 (title call site 0x539859, reached from Create New World) takes its out pointer
    // as the fifth argument (on the stack) and returns an asynchronous operation like slot 10.
    if o.class == "Windows.Xbox.UI.SystemUI" && N == 11 && (a5 as usize) % 8 == 0 && is_writable(a5) && !in_layer_image(a5) {
        unsafe { *(a5 as *mut *mut c_void) = create_generic("Windows.Xbox.UI.SystemUI#slot11", &o.iid) };
        wlog!("[winrt] SystemUI slot 11 -> an async operation through the fifth argument");
        return HRESULT(0);
    }
    // `SystemUI` statics slot 10 is `(user, 0, Object** out)` (title call site 0x26ac25, reached when a
    // game starts): it returns an object, not an event token; leaving the out-pointer NULL crashed
    // the title at 0x26660c.
    if o.class == "Windows.Xbox.UI.SystemUI" && N == 10 && (a3 as usize) % 8 == 0 && is_writable(a3) {
        unsafe { *(a3 as *mut *mut c_void) = create_generic("Windows.Xbox.UI.SystemUI#slot10", &o.iid) };
        wlog!("[winrt] SystemUI slot 10 (user, 0, out) -> a stand-in object");
        return HRESULT(0);
    }
    // `Store.Product` statics slot 11 is `(user, id, flag, ptr, ptr, Product** out)`: the object comes
    // back through the fifth argument after `this` (stack; title call site 0x1948e55).
    if o.class == "Windows.Xbox.ApplicationModel.Store.Product" && N == 11 && (a5 as usize) % 8 == 0 && is_writable(a5) {
        let child = create_generic("Windows.Xbox.ApplicationModel.Store.Product#slot11", &o.iid);
        unsafe { *(a5 as *mut *mut c_void) = child };
        wlog!("[winrt] {} slot 11 -> a stand-in object through the 5th argument after this", o.class);
        return HRESULT(0);
    }
    if o.kind == 2 {
        // Console CoreWindow. The title's vtable layout differs from desktop ICoreWindow:
        // observed calls are slots 7, 11, 15, 23, 25 and 35, and slot 7 is followed by
        // ProcessEvents on its result, so slot 7 is taken to be the Dispatcher getter. Slot 6
        // is assumed to be Bounds. Everything else logs and succeeds; where a second argument
        // is writable it receives an event-registration token (the shape of `add_*Event`).
        match N {
            6 if is_writable(arg1) => unsafe {
                ptr::write_unaligned(arg1 as *mut Rect, Rect { X: 0.0, Y: 0.0, Width: 1920.0, Height: 1080.0 });
            },
            7 if is_writable(arg1) => unsafe {
                ptr::write_unaligned(arg1 as *mut *mut c_void, add_ref_raw::<DispatcherTag>(shell().dispatcher));
            },
            // Looks like add_Event(handler, &token): both arguments are 8-byte-aligned pointers.
            _ if N > 7 && (arg1 as usize) % 8 == 0 && (arg2 as usize) % 8 == 0 && is_writable(arg1) && is_writable(arg2) => unsafe {
                if looks_like_com_object(arg1) {
                    // Keep the delegate (AddRef) so it can be raised later.
                    let vt = *(arg1 as *const *const usize);
                    let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(1));
                    add_ref(arg1);
                    WINDOW_HANDLERS.lock().unwrap().push((N, arg1 as usize));
                    ptr::write_unaligned(arg2 as *mut i64, next_token());
                }
                // Not a delegate: arg2 may be a small value (a bool or int), and an 8-byte token written
                // through it overflowed into the heap (Sonic Mania, slot 11).
            },
            _ => {}
        }
        wlog_rare!("[winrt] console CoreWindow slot {N} called (arg1={arg1:p}, arg2={arg2:p}) -> S_OK");
        return HRESULT(0);
    }
    if o.kind == 1 {
        // Console audio extension: the title calls slot 9 with a pointer to its own object
        // (looks like registering a callback) and slot 8 with two out-pointers that look
        // like 32-bit values. Meaning unknown; report zeros and success.
        if N == 8 && is_writable(arg1) && is_writable(arg2) {
            unsafe {
                *(arg1 as *mut u32) = 0;
                *(arg2 as *mut u32) = 0;
            }
        }
        wlog!("[winrt] {} slot {N} called (arg1={arg1:p}, arg2={arg2:p}) -> S_OK", o.class);
        return HRESULT(0);
    }
    // `Windows.Xbox.System.User.Users` (statics slot 9) is an IVectorView<User>. The title reads
    // its size and then each element, and waits until the list has a signed-in user, so it gets
    // a vector view with one user (a console always has one).
    if o.class == "Windows.Xbox.System.User" && N == 9 && is_writable(arg1) {
        let list = create_vector_view("Windows.Xbox.System.User.Users", &o.iid, 1);
        unsafe { *(arg1 as *mut *mut c_void) = list };
        wlog!("[winrt] {} slot 9 (Users) -> a vector view with 1 user", o.class);
        return HRESULT(0);
    }
    // `XboxLiveConfiguration` slot 8 is read as a string by the title (wstring constructor at
    // 0x16f2420); taken to be the service configuration id, with a placeholder value.
    if o.class == "Windows.Xbox.Services.XboxLiveConfiguration" && N >= 6 && (arg1 as usize) % 8 == 0 && is_writable(arg1) {
        unsafe { *(arg1 as *mut *mut c_void) = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from("1d220100-a389-45d9-ab7a-fd1a3bc8d4d5")) };
        wlog!("[winrt] {} slot {N} -> placeholder HSTRING", o.class);
        return HRESULT(0);
    }
    // The result of an Xbox Live asynchronous operation is a collection (title loop at 0x30ec99:
    if o.class == "Windows.Xbox.UI.SystemUI" {
        unsafe extern "system" { fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16; }
        let mut frames = [ptr::null_mut::<c_void>(); 1];
        unsafe { RtlCaptureStackBackTrace(1, 1, frames.as_mut_ptr(), ptr::null_mut()) };
        let (base, _) = crate::image_range();
        wlog!("[winrt] SystemUI slot {N} from title+{:#x} args {arg1:p} {arg2:p} {a3:p} {:#x} {a5:p}", (frames[0] as usize).wrapping_sub(base), _a4);
    }
    if o.class.starts_with("Windows.Xbox.Storage.") && std::env::var_os("XB1_STORAGE_TRACE").is_some() {
        unsafe extern "system" { fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16; }
        let mut frames = [ptr::null_mut::<c_void>(); 2];
        unsafe { RtlCaptureStackBackTrace(1, 2, frames.as_mut_ptr(), ptr::null_mut()) };
        let (base, _) = crate::image_range();
        eprintln!("[storage] {} slot {N} arg1={arg1:p} arg2={arg2:p} a3={a3:p} from title+{:#x} / {:#x}", o.class, (frames[0] as usize).wrapping_sub(base), (frames[1] as usize).wrapping_sub(base));
    }
    // slot 7 is `get_Size(uint*)`, slot 6 `GetAt`); there is no Xbox Live data, so it is empty.
    if o.class.starts_with("Microsoft.Xbox.Services.XboxLiveContext#") && o.class.ends_with("#slot8") && N == 7 && (arg1 as usize) % 4 == 0 && is_writable(arg1) {
        unsafe { *(arg1 as *mut u32) = 0 };
        wlog!("[winrt] {} slot 7 (get_Size) -> 0", o.class);
        return HRESULT(0);
    }
    // `DownloadableContentPackageManager` slot 6 is `(filter, IVectorView** out)` (title call site
    // 0xcef5d4): the list of installed DLC packages, empty here.
    if o.class.starts_with("Windows.Xbox.Management.Deployment.DownloadableContentPackageManager") && N == 6 && (arg1 as usize) < 0x10000 && (arg2 as usize) % 8 == 0 && is_writable(arg2) {
        let list = create_vector_view("DownloadableContentPackageManager.Packages", &o.iid, 0);
        unsafe { *(arg2 as *mut *mut c_void) = list };
        wlog!("[winrt] {} slot 6 -> an empty package list through the second argument", o.class);
        return HRESULT(0);
    }
    // The user-event args object: its first getter (slot 6) returns the signed-in user, which the
    // title then reads like a `Users` list element.
    if o.class == "UserEventArgs" && N == 6 && (arg1 as usize) % 8 == 0 && is_writable(arg1) {
        unsafe { *(arg1 as *mut *mut c_void) = create_generic("Windows.Xbox.System.User.Users (element 0)", &o.iid) };
        wlog!("[winrt] UserEventArgs slot 6 -> the user element");
        return HRESULT(0);
    }
    // `SpeechRecognizerFactory` slot 6 is `(locale id, Recognizer** out)`: a scalar first argument,
    // the object comes back through the second.
    if o.class == "Windows.Xbox.Speech.Recognition.SpeechRecognizerFactory" && N == 6 && (arg1 as usize) < 0x10000 && (arg2 as usize) % 8 == 0 && is_writable(arg2) {
        unsafe { *(arg2 as *mut *mut c_void) = create_generic("SpeechRecognizer", &o.iid) };
        wlog!("[winrt] {} slot 6 (locale {:#x}) -> a stand-in recognizer", o.class, arg1 as usize);
        return HRESULT(0);
    }
    // `SpeechRecognizer` slot 15 returns an object whose slot 8 the title reads as a string
    // (wstring constructor at 0x16f2420), probably the recognizer's language tag.
    if o.class == "SpeechRecognizer#slot15" && N == 8 && (arg1 as usize) % 8 == 0 && is_writable(arg1) {
        unsafe { *(arg1 as *mut *mut c_void) = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from("en-US")) };
        wlog!("[winrt] {} slot 8 -> HSTRING \"en-US\"", o.class);
        return HRESULT(0);
    }
    // `Package.Current` -> slot 7 -> slot 7: the title reads a string here (a wrong guess that
    // this was an IPackageId made a non-string crash `combase`). Taken to be the installed
    // location, so it gets the title folder (the process working directory).
    if o.class == "Windows.ApplicationModel.Package#slot6#slot7" && (6..=13).contains(&N) && (arg1 as usize) % 8 == 0 && is_writable(arg1) {
        let path = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        unsafe { *(arg1 as *mut *mut c_void) = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from(path.as_str())) };
        wlog!("[winrt] {} slot {N} -> HSTRING (title folder)", o.class);
        return HRESULT(0);
    }
    // `SpeechRecognizer.slot 9 object` slot 7 is `(x, y, z, Object** out)`: the object comes back through
    // the third argument (title call site 0x212b209), which the generic rule never wrote.
    if o.class == "SpeechRecognizer#slot9" && N == 7 && (a3 as usize) % 8 == 0 && is_writable(a3) {
        unsafe { *(a3 as *mut *mut c_void) = create_generic("SpeechRecognizer#slot9#slot7", &o.iid) };
        wlog!("[winrt] {} slot 7 -> a stand-in object through the third argument", o.class);
        return HRESULT(0);
    }
    // `Controller` statics slot 6 is the controller list, an IVectorView<IController> (the title
    // calls `get_Size` and `GetAt(index, &out)` on it). Empty for now: no controllers connected.
    if (o.class == "Windows.Xbox.Input.Controller" || o.class == "Windows.Xbox.Input.Gamepad") && N == 6 && is_writable(arg1) {
        let count = if std::env::var_os("XB1_NO_GAMEPAD").is_some() { 0 } else { 1 };
        let list = create_vector_view("Windows.Xbox.Input.Controller.Controllers", &o.iid, count);
        unsafe { *(arg1 as *mut *mut c_void) = list };
        wlog!("[winrt] {} slot 6 (Controllers) -> an empty vector view", o.class);
        return HRESULT(0);
    }
    // Statics slot 10 is an event subscription, `add_Event(handler, &token)`: arg1 is the title's
    // own delegate object, so it must not be treated as an out-parameter (that overwrote its vtable).
    if o.class == "Windows.Xbox.System.User" && N >= 10 {
        if N % 2 == 0 && is_writable(arg2) {
            if looks_like_com_object(arg1) {
                let vt = unsafe { *(arg1 as *const *const usize) };
                let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*vt.add(1)) };
                unsafe { add_ref(arg1) };
                USER_HANDLERS.lock().unwrap().push((N, arg1 as usize));
            }
            unsafe { ptr::write_unaligned(arg2 as *mut i64, next_token()) };
        }
        wlog!("[winrt] {} slot 10 (event add) -> token", o.class);
        return HRESULT(0);
    }
    // The title reads a string (a Platform::String it hands to a wstring constructor) from slot 6
    // of the object that slot 9 of a signed-in user returns; a stand-in object there faults in
    // `Data()`. Taken to be the gamertag, so it gets a real HSTRING.
    if o.class.starts_with("Windows.Xbox.System.User.Users (element") && o.class.ends_with("#slot9") && N >= 6 && is_writable(arg1) {
        unsafe { *(arg1 as *mut *mut c_void) = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from("XB1Layer Player")) };
        wlog!("[winrt] {}: slot {N} -> HSTRING \"XB1Layer Player\"", o.class);
        return HRESULT(0);
    }
    // Older SDK, `IUser` booleans (polled every frame once Play Game is chosen; the generic object pointer made
    // the title treat the user as a guest): slot 10 `IsGuest` = false, slot 11 `IsSignedIn` = true (settled from the "Guest Xbox profile" and "You are not signed in" dialogs that the other order produced). A guess from
    // the "Guest Xbox profile" dialog that appeared.
    if o.class.starts_with("Windows.Xbox.System.User.Users (element") && !o.class.contains('#') && (N == 10 || N == 11) && crate::VCCORLIB110.load(Ordering::Relaxed) && is_writable(arg1) {
        unsafe { *(arg1 as *mut u8) = if N == 11 { 1 } else { 0 } };
        wlog_rare!("[winrt] user slot {N} (older SDK) -> {}", if N == 11 { "true" } else { "false" });
        return HRESULT(0);
    }
    // Slot 15 of a user is parsed by the title with base-10 `wcstoull`: the Xbox user id as a string.
    if o.class.starts_with("Windows.Xbox.System.User.Users (element") && !o.class.contains('#') && N == 15 && is_writable(arg1) {
        unsafe { *(arg1 as *mut *mut c_void) = std::mem::transmute::<HSTRING, *mut c_void>(HSTRING::from("2535400000000001")) };
        wlog!("[winrt] {}: slot 15 -> HSTRING user id", o.class);
        return HRESULT(0);
    }
    // `PackageTransferWatcher` slot 7 is `(object, Object** out)`: the second argument is an object
    // out-parameter even though the first is a COM object (title call site 0x18c2a02).
    if o.class == "Windows.Xbox.Management.Deployment.PackageTransferWatcher" && N == 7 && (arg2 as usize) % 8 == 0 && is_writable(arg2) {
        unsafe { *(arg2 as *mut *mut c_void) = create_generic("PackageTransferWatcher#slot7", &o.iid) };
        wlog!("[winrt] {} slot 7 -> a stand-in object through the second argument", o.class);
        return HRESULT(0);
    }
    // Xbox Live services classes are factories: `(object, Object** out)` creates something from
    // the object (e.g. an XboxLiveContext from a user; title call site 0x1947b12), not an event add.
    // `ConnectedStorageSpace.GetForUser(user, ConnectedStorageSpace** out)` (statics slot 6) is a factory too.
    if (o.class.starts_with("Microsoft.Xbox.") || (o.class == "Windows.Xbox.Storage.ConnectedStorageSpace" && N == 6)) && looks_like_com_object(arg1) && (arg2 as usize) % 8 == 0 && is_writable(arg2) {
        unsafe { *(arg2 as *mut *mut c_void) = create_generic(&format!("{}#slot{N}", o.class), &o.iid) };
        wlog!("[winrt] {} slot {N}: (object, out) -> a stand-in object", o.class);
        return HRESULT(0);
    }
    // `ChatManager` session slot 23 returns a collection the title walks with `get_Size` and `GetAt` (loop at
    // `0x2cc790`); without chat users it is an empty vector view. The generic stand-in left the size unwritten and the
    // loop ran on garbage until the C runtime crashed.
    if o.class == "Microsoft.Xbox.GameChat.ChatManager#slot6" && N == 23 && (arg1 as usize) % 8 == 0 && is_writable(arg1) && !in_layer_image(arg1) {
        unsafe { *(arg1 as *mut *mut c_void) = create_vector_view("Microsoft.Xbox.GameChat.ChatManager#slot6#slot23", &o.iid, 0) };
        wlog!("[winrt] {} slot 23 -> an empty vector view", o.class);
        return HRESULT(0);
    }
    // The result of a storage query (a `GetResults` output other than the space itself) is a collection whose
    // slot 7 is `get_Size(uint*)` (title loop at 0x594386); there is nothing stored, so it is empty.
    if o.class.starts_with("Windows.Xbox.Storage.ConnectedStorageSpace#") && o.class.ends_with("#slot8") && o.class != "Windows.Xbox.Storage.ConnectedStorageSpace#slot6#slot8" && N == 7 && (arg1 as usize) % 4 == 0 && is_writable(arg1) && !in_layer_image(arg1) {
        unsafe { *(arg1 as *mut u32) = 0 };
        wlog!("[winrt] {} slot 7 (get_Size) -> 0", o.class);
        return HRESULT(0);
    }
    // Storage operations started with `(object, 0, Operation** out)` (title call site 0x5a8eee): the out pointer
    // is the third argument, and the result is an asynchronous operation (its `put_Completed` is handled above).
    if o.class.starts_with("Windows.Xbox.Storage.ConnectedStorageSpace#") && N >= 7 && looks_like_com_object(arg1) && arg2.is_null() && (a3 as usize) % 8 == 0 && is_writable(a3) && !in_layer_image(a3) {
        unsafe { *(a3 as *mut *mut c_void) = create_generic(&format!("{}#slot{N}", o.class), &o.iid) };
        wlog!("[winrt] {} slot {N}: (object, 0, out) -> an async operation", o.class);
        return HRESULT(0);
    }
    // Storage objects: `Get/Create(name, Object** out)` where the name is a string whose pointer is
    // not always recognizable as an HSTRING header (title call site 0x599190): the object goes to the
    // second argument, and the first is never written.
    if o.class.starts_with("Windows.Xbox.Storage.ConnectedStorageSpace#") && (N == 9 || (N >= 7 && (arg1.is_null() || looks_like_hstring_header(arg1)))) && (arg2 as usize) % 8 == 0 && is_writable(arg2) && !in_layer_image(arg2) {
        unsafe { *(arg2 as *mut *mut c_void) = create_generic(&format!("{}#slot{N}", o.class), &o.iid) };
        wlog!("[winrt] {} slot {N}: (name, out) -> a stand-in object", o.class);
        return HRESULT(0);
    }
    // A string in arg1 (`GetXxx(name, Object** out)`): the object goes to the second argument.
    if looks_like_hstring_header(arg1) && (arg2 as usize) % 8 == 0 && is_writable(arg2) {
        unsafe { *(arg2 as *mut *mut c_void) = create_generic(&format!("{}#slot{N}", o.class), &o.iid) };
        wlog!("[winrt] {} slot {N}: (string, out) -> a stand-in object", o.class);
        return HRESULT(0);
    }
    // A delegate in arg1 means `add_Event(handler, &token)`: leave the handler alone.
    if looks_like_com_object(arg1) {
        if o.class == "Windows.Xbox.Input.Gamepad" || o.class == "Windows.Xbox.Input.Controller" {
            let vt = unsafe { *(arg1 as *const *const usize) };
            let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { std::mem::transmute(*vt.add(1)) };
            unsafe { add_ref(arg1) };
            INPUT_HANDLERS.lock().unwrap().push((if o.class == "Windows.Xbox.Input.Gamepad" { "Gamepad" } else { "Controller" }, N, arg1 as usize));
        }
        if is_writable(arg2) && !in_layer_image(arg2) {
            unsafe { ptr::write_unaligned(arg2 as *mut i64, next_token()) };
        }
        // `put_Completed(handler)` of a storage operation: one argument, so the second register still
        // holds the object's own vtable pointer (in the layer's image); an event add would have a token pointer.
        if o.class.starts_with("Windows.Xbox.Storage.") && N == 6 && in_layer_image(arg2) {
            complete_later(arg1, this);
            wlog!("[winrt] {} slot 6: put_Completed -> handler Invoke(Completed) queued", o.class);
            return HRESULT(0);
        }
        if o.class.starts_with("Windows.Xbox.Storage.") {
            unsafe extern "system" { fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16; }
            let mut frames = [ptr::null_mut::<c_void>(); 2];
            unsafe { RtlCaptureStackBackTrace(0, 2, frames.as_mut_ptr(), ptr::null_mut()) };
            let (base, _) = crate::image_range();
            wlog!("[winrt] {} slot {N} called from title+{:#x} / {:#x} (arg1={:p} arg2={:p})", o.class, (frames[0] as usize).wrapping_sub(base), (frames[1] as usize).wrapping_sub(base), arg1, arg2);
        }
        wlog!("[winrt] {} slot {N}: arg1 is a COM object -> treated as an event subscription", o.class);
        return HRESULT(0);
    }
    // `IController::get_User` (slot 6 on a controller element): the shared user.
    if o.class.starts_with("Windows.Xbox.Input.Controller.Controllers (element") && !o.class.contains('#') && N == 6 && is_writable(arg1) {
        unsafe { *(arg1 as *mut *mut c_void) = shared_user() };
        wlog!("[winrt] controller slot 6 -> the shared user element");
        return HRESULT(0);
    }
    // Slot 8 on the gamepad interface is `GetCurrentReading(GamepadReading* out)`: a 36-byte struct
    // (timestamp u64, buttons u32, left and right trigger f32, left and right stick x/y f32), decoded
    // from the title's poll at title+0x254e30 (it zeroes the struct, calls slot 8, then reads it).
    if o.class.starts_with("Windows.Xbox.Input.Controller.Controllers (element") && !o.class.contains('#') && N == 8 && is_writable(arg1) && !crate::VCCORLIB110.load(Ordering::Relaxed) {
        unsafe { fill_gamepad_reading(arg1 as *mut u8) };
        return HRESULT(0);
    }
    // Experiment (XB1_NO_HEAP_OUT=1): an out-parameter outside this thread's stack may be a smaller field of a heap object.
    let on_stack = {
        let (lo, hi): (usize, usize);
        unsafe {
            let mut a: usize;
            let mut b: usize;
            std::arch::asm!("mov {}, gs:[8]", out(reg) a, options(nostack, readonly, preserves_flags));
            std::arch::asm!("mov {}, gs:[16]", out(reg) b, options(nostack, readonly, preserves_flags));
            hi = a;
            lo = b;
        }
        (arg1 as usize) >= lo && (arg1 as usize) < hi
    };
    if (std::env::var_os("XB1_NO_HEAP_OUT").is_some() || o.class == "Microsoft.Xbox.Services.Presence.PresenceData") && !on_stack && is_writable(arg1) {
        wlog!("[winrt] {} slot {N}: out-parameter {arg1:p} is not on the stack, left alone", o.class);
        return HRESULT(0);
    }
    // A pointer that is not 8-aligned cannot be an `Object**`: it is a value getter (`uint*`, `bool*`), so zero is
    // written (a pointer write there panicked on the misalignment).
    if is_writable(arg1) && (arg1 as usize) % 8 != 0 && !in_layer_image(arg1) {
        unsafe {
            if (arg1 as usize) % 4 == 0 { *(arg1 as *mut u32) = 0 } else { *(arg1 as *mut u8) = 0 }
        }
        wlog!("[winrt] {} slot {N}: out-parameter {arg1:p} is not pointer-aligned -> value 0", o.class);
        return HRESULT(0);
    }
    if is_writable(arg1) {
        let child = create_generic(&format!("{}#slot{N}", o.class), &o.iid);
        unsafe { *(arg1 as *mut *mut c_void) = child };
        wlog!("[winrt] {} (iid {:?}): slot {N} called with out-param {arg1:p} -> returned a new stand-in object", o.class, o.iid);
        HRESULT(0)
    } else {
        // `(NULL name, Object** out)` (title call site 0x2ca99a): the object goes to the second argument.
        if o.class.starts_with("Windows.Xbox.System.User") && arg1.is_null() && (arg2 as usize) % 8 == 0 && is_writable(arg2) && !in_layer_image(arg2) {
            unsafe { *(arg2 as *mut *mut c_void) = create_generic(&format!("{}#slot{N}", o.class), &o.iid) };
            wlog!("[winrt] {} slot {N}: (NULL, out) -> a stand-in object", o.class);
            return HRESULT(0);
        }
        if o.class.starts_with("Windows.Xbox.System.User") || std::env::var_os("XB1_NW_TRACE").is_some() {
            unsafe extern "system" { fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16; }
            let mut frames = [ptr::null_mut::<c_void>(); 1];
            unsafe { RtlCaptureStackBackTrace(1, 1, frames.as_mut_ptr(), ptr::null_mut()) };
            let (base, _) = crate::image_range();
            wlog!("[winrt] {} slot {N} (not writable) from title+{:#x} args {arg1:p} {arg2:p} {a3:p}", o.class, (frames[0] as usize).wrapping_sub(base));
        }
        wlog!("[winrt] {} (iid {:?}): slot {N} called (arg1={arg1:p}, not writable) -> S_OK, nothing written", o.class, o.iid);
        HRESULT(0)
    }
}

macro_rules! generic_slots {
    ($($n:literal),* $(,)?) => { [$(generic_slot::<$n> as usize),*] };
}

fn generic_vtbl() -> *const usize {
    static VTBL: OnceLock<[usize; GENERIC_SLOTS]> = OnceLock::new();
    VTBL.get_or_init(|| {
        let mut v = [0usize; GENERIC_SLOTS];
        v[0] = generic_qi as usize;
        v[1] = generic_add_ref as usize;
        v[2] = generic_release as usize;
        v[3] = com_get_iids as usize;
        v[4] = generic_class_name as usize;
        v[5] = com_get_trust_level as usize;
        let slots = generic_slots!(
            6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34,
            35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63
        );
        v[6..].copy_from_slice(&slots);
        v
    })
    .as_ptr()
}

/// Vtable for the console CoreWindow stand-in. Unlike the generic one, slots 3 to 5 are plain
/// logging slots too: the title's window interface does not follow IInspectable's layout, and
/// it calls slot 3 with arguments that are not `GetIids`' pointers.
fn console_window_vtbl() -> *const usize {
    static VTBL: OnceLock<[usize; GENERIC_SLOTS]> = OnceLock::new();
    VTBL.get_or_init(|| {
        let mut v = [0usize; GENERIC_SLOTS];
        v[0] = generic_qi as usize;
        v[1] = generic_add_ref as usize;
        v[2] = generic_release as usize;
        v[3] = generic_slot::<3> as usize;
        v[4] = generic_slot::<4> as usize;
        v[5] = generic_slot::<5> as usize;
        let slots = generic_slots!(
            6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34,
            35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63
        );
        v[6..].copy_from_slice(&slots);
        v
    })
    .as_ptr()
}

/// `IVectorView<T>::GetAt(index, T** out)`. The element type does not matter to the layout.
unsafe extern "system" fn vector_view_get_at(this: *mut c_void, index: u32, out: *mut *mut c_void) -> HRESULT {
    let o = unsafe { &*(this as *const GenericObj) };
    let size = o.kind as u32; // the number of elements is kept in `kind`
    if index >= size {
        wlog!("[winrt] {}: GetAt({index}) is out of range (size {size}) -> E_BOUNDS", o.class);
        return HRESULT(0x8000000Bu32 as i32);
    }
    // The title compares users by identity (a controller's user against the user list), so the
    // user list always hands out the same object.
    if o.class == "Windows.Xbox.System.User.Users" && index == 0 {
        let user = shared_user();
        unsafe { *out = user };
        wlog!("[winrt] {}: GetAt(0) -> the shared user element", o.class);
        return HRESULT(0);
    }
    if o.class == "Windows.Xbox.Input.Controller.Controllers" && index == 0 && crate::VCCORLIB110.load(Ordering::Relaxed) {
        unsafe { *out = controller_element() };
        wlog_rare!("[winrt] {}: GetAt(0) -> the controller element", o.class);
        return HRESULT(0);
    }
    let element = create_generic(&format!("{} (element {index})", o.class), &o.iid);
    wlog!("[winrt] {}: GetAt({index}) -> a stand-in element", o.class);
    unsafe { *out = element };
    HRESULT(0)
}

/// `IVectorView<T>::get_Size(uint* out)`.
unsafe extern "system" fn vector_view_get_size(this: *mut c_void, out: *mut u32) -> HRESULT {
    let o = unsafe { &*(this as *const GenericObj) };
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        wlog!("[winrt] {}: get_Size -> {}", o.class, o.kind);
    }
    unsafe { *out = o.kind as u32 };
    HRESULT(0)
}

/// `IVectorView<T>::IndexOf(T value, uint* index, bool* found)`: nothing is found.
unsafe extern "system" fn vector_view_index_of(_this: *mut c_void, _value: *mut c_void, index: *mut u32, found: *mut bool) -> HRESULT {
    unsafe {
        if !index.is_null() {
            *index = 0;
        }
        if !found.is_null() {
            *found = false;
        }
    }
    HRESULT(0)
}

/// Vtable for a vector-view stand-in: IUnknown and IInspectable as in the generic object, then
/// the three `IVectorView` methods (`GetAt` 6, `get_Size` 7, `IndexOf` 8); the rest are the
/// generic logging slots.
fn vector_view_vtbl() -> *const usize {
    static VTBL: OnceLock<[usize; GENERIC_SLOTS]> = OnceLock::new();
    VTBL.get_or_init(|| {
        let mut v = [0usize; GENERIC_SLOTS];
        v[0] = generic_qi as usize;
        v[1] = generic_add_ref as usize;
        v[2] = generic_release as usize;
        v[3] = com_get_iids as usize;
        v[4] = generic_class_name as usize;
        v[5] = com_get_trust_level as usize;
        let slots = generic_slots!(
            6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34,
            35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63
        );
        v[6..].copy_from_slice(&slots);
        v[6] = vector_view_get_at as usize;
        v[7] = vector_view_get_size as usize;
        v[8] = vector_view_index_of as usize;
        v[9] = vector_view_get_many as usize;
        v
    })
    .as_ptr()
}

/// A vector-view stand-in holding `size` generic elements. The size is stored in `kind` (kinds 0
/// to 2 are taken by the other generic objects, so the vtable, not the kind, selects behavior).
/// The one signed-in user element (see `vector_view_get_at`).
/// Fills a `GamepadReading` from the first XInput pad and, while the layer's window has the focus,
/// the keyboard (arrows = d-pad, Z/X/A/S = A/B/X/Y, Enter = Menu, Backspace = View, Q/W = shoulders).
unsafe fn fill_gamepad_reading(out: *mut u8) {
    #[repr(C)]
    #[derive(Default)]
    struct XInputState {
        packet: u32,
        buttons: u16,
        lt: u8,
        rt: u8,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
    }
    unsafe extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn GetAsyncKeyState(key: i32) -> i16;
        fn GetForegroundWindow() -> *mut c_void;
        fn GetWindowThreadProcessId(window: *mut c_void, pid: *mut u32) -> u32;
        fn GetCurrentProcessId() -> u32;
    }
    static XINPUT: OnceLock<usize> = OnceLock::new();
    static STAMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let get_state = *XINPUT.get_or_init(|| unsafe {
        let m = LoadLibraryA(b"xinput1_4.dll ".as_ptr());
        if m.is_null() { 0 } else { GetProcAddress(m, b"XInputGetState ".as_ptr()) as usize }
    });
    let mut pad = XInputState::default();
    let mut found_xinput = false;
    if get_state != 0 {
        let f: unsafe extern "system" fn(u32, *mut XInputState) -> u32 = unsafe { std::mem::transmute(get_state) };
        // The first connected pad of the four XInput slots.
        let mut found = false;
        for slot in 0..4u32 {
            let mut st = XInputState::default();
            if unsafe { f(slot, &mut st) } == 0 {
                pad = st;
                found = true;
                static LOGGED: AtomicBool = AtomicBool::new(false);
                if !LOGGED.swap(true, Ordering::Relaxed) {
                    wlog!("[winrt] XInput pad found in slot {slot}");
                }
                break;
            }
        }
        found_xinput = found;
        if !found {
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                wlog!("[winrt] no XInput pad connected (only XInput devices are read)");
            }
        }
    }
    // No XInput pad: try the legacy joystick API (winmm), which sees DirectInput/HID pads such as a
    // DualSense or DualShock over USB or Bluetooth. Layout assumed from the usual Windows mapping:
    // buttons 0..13 = Square, Cross, Circle, Triangle, L1, R1, L2, R2, Create, Options, L3, R3, PS,
    // touchpad; X/Y = left stick, Z/R = right stick, U/V = triggers, POV = d-pad.
    let mut extra_buttons = 0u32;
    let mut extra_axes: Option<[f32; 6]> = None;
    crate::dualsense::start();
    let ds = if found_xinput { None } else { crate::dualsense::state() };
    if let Some((b, axes)) = ds {
        extra_buttons = b;
        extra_axes = Some(axes);
    }
    if !found_xinput && ds.is_none() {
        #[repr(C)]
        #[derive(Default)]
        struct JoyInfoEx {
            size: u32,
            flags: u32,
            x: u32,
            y: u32,
            z: u32,
            r: u32,
            u: u32,
            v: u32,
            buttons: u32,
            button_number: u32,
            pov: u32,
            reserved: [u32; 2],
        }
        #[link(name = "winmm")]
        unsafe extern "system" {
            fn joyGetPosEx(id: u32, info: *mut JoyInfoEx) -> u32;
        }
        for id in 0..4u32 {
            let mut j = JoyInfoEx { size: std::mem::size_of::<JoyInfoEx>() as u32, flags: 0xFF, ..Default::default() };
            if unsafe { joyGetPosEx(id, &mut j) } != 0 {
                continue;
            }
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                wlog!("[winrt] joystick {id} found through winmm (assumed DualSense/DualShock layout)");
            }
            // XB1_INPUT_LOG=1 prints the raw state whenever it changes, to map an unknown pad.
            if std::env::var_os("XB1_INPUT_LOG").is_some() {
                static LAST: std::sync::Mutex<[u32; 8]> = std::sync::Mutex::new([0; 8]);
                let now = [j.x, j.y, j.z, j.r, j.u, j.v, j.buttons, j.pov];
                let mut last = LAST.lock().unwrap();
                if *last != now {
                    wlog!("[input] joystick {id}: x {} y {} z {} r {} u {} v {} buttons {:#x} pov {}", j.x, j.y, j.z, j.r, j.u, j.v, j.buttons, j.pov);
                    *last = now;
                }
            }
            let bmap = [(1u32, 16u32), (2, 32), (0, 64), (3, 128), (4, 4096), (5, 8192), (9, 4), (8, 8), (10, 16384), (11, 32768)];
            for (b, bit) in bmap {
                if j.buttons & (1 << b) != 0 {
                    extra_buttons |= bit;
                }
            }
            match j.pov {
                0..=36000 => {
                    let a = j.pov / 100;
                    if a >= 315 || a <= 45 { extra_buttons |= 256; }
                    if (45..=135).contains(&a) { extra_buttons |= 2048; }
                    if (135..=225).contains(&a) { extra_buttons |= 512; }
                    if (225..=315).contains(&a) { extra_buttons |= 1024; }
                }
                _ => {}
            }
            let ax = |v: u32| ((v as f32 - 32767.5) / 32767.5).clamp(-1.0, 1.0);
            let lt = if j.buttons & (1 << 6) != 0 { 1.0 } else { 0.0 };
            let rt = if j.buttons & (1 << 7) != 0 { 1.0 } else { 0.0 };
            extra_axes = Some([lt, rt, ax(j.x), -ax(j.y), ax(j.z), -ax(j.r)]);
            break;
        }
    }
    // XInput button bit -> GamepadButtons bit (Menu 4, View 8, A 16, B 32, X 64, Y 128, DPad up 256,
    // down 512, left 1024, right 2048, shoulders 4096/8192, thumbsticks 16384/32768).
    let map = [(0x10u16, 4u32), (0x20, 8), (0x1000, 16), (0x2000, 32), (0x4000, 64), (0x8000, 128), (1, 256), (2, 512), (4, 1024), (8, 2048), (0x100, 4096), (0x200, 8192), (0x40, 16384), (0x80, 32768)];
    let mut buttons = 0u32;
    for (x, bit) in map {
        if pad.buttons & x != 0 {
            buttons |= bit;
        }
    }
    buttons |= extra_buttons;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
    if pid == unsafe { GetCurrentProcessId() } {
        let keys = [(0x0Du8 as i32, 4u32), (0x08, 8), (b'Z' as i32, 16), (b'X' as i32, 32), (b'A' as i32, 64), (b'S' as i32, 128), (0x26, 256), (0x28, 512), (0x25, 1024), (0x27, 2048), (b'Q' as i32, 4096), (b'W' as i32, 8192)];
        for (key, bit) in keys {
            if unsafe { GetAsyncKeyState(key) } < 0 {
                buttons |= bit;
            }
        }
    }
    // Test hook: XB1_AUTOPRESS="30:4:1;34:16:1" presses Menu for 1 s at t=30 s and A at t=34 s.
    if let Ok(spec) = std::env::var("XB1_AUTOPRESS") {
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        let t = START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64();
        for item in spec.split(';') {
            let v: Vec<f64> = item.split(':').filter_map(|x| x.parse().ok()).collect();
            if v.len() == 3 && t >= v[0] && t < v[0] + v[2] {
                buttons |= v[1] as u32;
            }
        }
    }
    let axis = |v: i16| (v as f32 / 32768.0).clamp(-1.0, 1.0);
    let r: [f32; 6] = extra_axes.unwrap_or([pad.lt as f32 / 255.0, pad.rt as f32 / 255.0, axis(pad.lx), axis(pad.ly), axis(pad.rx), axis(pad.ry)]);
    unsafe {
        ptr::write_unaligned(out as *mut u64, STAMP.fetch_add(16_667, Ordering::Relaxed));
        ptr::write_unaligned(out.add(8) as *mut u32, buttons);
        for (i, v) in r.iter().enumerate() {
            ptr::write_unaligned(out.add(12 + i * 4) as *mut f32, *v);
        }
    }
}

fn shared_user() -> *mut c_void {
    static USER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let mut u = USER.load(Ordering::Acquire);
    if u == 0 {
        let o = create_generic("Windows.Xbox.System.User.Users (element 0)", &GUID::from_u128(0xB4F23A93_7FC6_4A50_B153_950016389E50));
        u = o as usize;
        USER.store(u, Ordering::Release);
    }
    unsafe { (*(u as *const GenericObj)).refs.fetch_add(1, Ordering::AcqRel) };
    u as *mut c_void
}

/// `IIterable<T>::First(IIterator<T>** out)` of an empty collection.
unsafe extern "system" fn iterable_first(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let it = create_generic("empty iterator", &GUID::from_u128(0));
    unsafe { (*(it as *mut GenericObj)).vtbl = iterator_vtbl() };
    unsafe { *out = it };
    HRESULT(0)
}

/// `IIterator<T>::get_Current` (slot 6): nothing to return.
unsafe extern "system" fn iterator_current(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    unsafe { *out = std::ptr::null_mut() };
    HRESULT(0x8000000Bu32 as i32)
}

/// `IIterator<T>::get_HasCurrent` (slot 7) and `MoveNext` (slot 8): both false.
unsafe extern "system" fn iterator_false(_this: *mut c_void, out: *mut u8) -> HRESULT {
    unsafe { *out = 0 };
    HRESULT(0)
}

fn stand_in_vtbl(set: &[(usize, usize)]) -> [usize; GENERIC_SLOTS] {
    let mut v = [0usize; GENERIC_SLOTS];
    v[0] = generic_qi as usize;
    v[1] = generic_add_ref as usize;
    v[2] = generic_release as usize;
    v[3] = com_get_iids as usize;
    v[4] = generic_class_name as usize;
    v[5] = com_get_trust_level as usize;
    let slots = generic_slots!(
        6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34,
        35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63
    );
    v[6..].copy_from_slice(&slots);
    for &(i, f) in set {
        v[i] = f;
    }
    v
}

fn iterable_vtbl() -> *const usize {
    static VTBL: OnceLock<[usize; GENERIC_SLOTS]> = OnceLock::new();
    VTBL.get_or_init(|| stand_in_vtbl(&[(6, iterable_first as usize)])).as_ptr()
}

fn iterator_vtbl() -> *const usize {
    static VTBL: OnceLock<[usize; GENERIC_SLOTS]> = OnceLock::new();
    VTBL.get_or_init(|| stand_in_vtbl(&[(6, iterator_current as usize), (7, iterator_false as usize), (8, iterator_false as usize)])).as_ptr()
}

fn create_vector_view(class: &str, iid: &GUID, size: u8) -> *mut c_void {
    let o = create_generic(class, iid);
    unsafe {
        (*(o as *mut GenericObj)).kind = size;
        (*(o as *mut GenericObj)).vtbl = vector_view_vtbl();
    }
    o
}

fn create_generic(class: &str, iid: &GUID) -> *mut c_void {
    Box::into_raw(Box::new(GenericObj { vtbl: generic_vtbl(), refs: AtomicU32::new(1), class: class.to_owned(), iid: *iid, kind: 0 })) as *mut c_void
}

// ---------------------------------------------------------------------------------------
// MMDevice enumerator proxy
// ---------------------------------------------------------------------------------------
//
// The title creates the standard MMDeviceEnumerator, then queries it for an interface that
// only exists on the console (an audio extension). The proxy forwards everything standard
// to the real enumerator and answers that one IID with a stand-in.

const CLSID_MM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);
const IID_MM_DEVICE_ENUMERATOR: GUID = GUID::from_u128(0xA95664D2_9614_4F35_A746_DE8DB63617E6);
const IID_XBOX_AUDIO_EXTENSION: GUID = GUID::from_u128(0x8B557ADC_555E_47A0_B223_43477E481DAD);

#[repr(C)]
struct MmProxy {
    vtbl: *const usize,
    refs: AtomicU32,
    real: *mut c_void,
}

unsafe extern "system" fn mm_qi(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    let p = unsafe { &*(this as *const MmProxy) };
    let iid = unsafe { *iid };
    if iid == IUnknown::IID || iid == IID_MM_DEVICE_ENUMERATOR {
        p.refs.fetch_add(1, Ordering::AcqRel);
        unsafe { *out = this };
        return HRESULT(0);
    }
    if iid == IID_XBOX_AUDIO_EXTENSION {
        let o = create_generic("MMDevAPI console audio extension", &iid);
        unsafe {
            (*(o as *mut GenericObj)).kind = 1;
            *out = o;
        }
        wlog!("[winrt] MMDeviceEnumerator: QueryInterface for the console audio extension -> stand-in");
        return HRESULT(0);
    }
    // Anything else goes to the real object.
    let real_qi: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(*(*(p.real as *const *const usize))) };
    unsafe { real_qi(p.real, &iid, out) }
}

unsafe extern "system" fn mm_add_ref(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const MmProxy)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn mm_release(this: *mut c_void) -> u32 {
    let left = unsafe { (*(this as *const MmProxy)).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        let p = unsafe { Box::from_raw(this as *mut MmProxy) };
        let real_release: unsafe extern "system" fn(*mut c_void) -> u32 =
            unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(2))) };
        unsafe { real_release(p.real) };
    }
    left
}

/// Forwards IMMDeviceEnumerator method `N` (slots 3..=7, at most 4 arguments) to the real object.
unsafe extern "system" fn mm_forward<const N: usize>(this: *mut c_void, a1: usize, a2: usize, a3: usize, a4: usize) -> HRESULT {
    let p = unsafe { &*(this as *const MmProxy) };
    let f: unsafe extern "system" fn(*mut c_void, usize, usize, usize, usize) -> HRESULT =
        unsafe { std::mem::transmute(*((*(p.real as *const *const usize)).add(N))) };
    unsafe { f(p.real, a1, a2, a3, a4) }
}

fn mm_vtbl() -> *const usize {
    static VTBL: OnceLock<[usize; 8]> = OnceLock::new();
    VTBL.get_or_init(|| {
        [
            mm_qi as usize,
            mm_add_ref as usize,
            mm_release as usize,
            mm_forward::<3> as usize,
            mm_forward::<4> as usize,
            mm_forward::<5> as usize,
            mm_forward::<6> as usize,
            mm_forward::<7> as usize,
        ]
    })
    .as_ptr()
}

/// Number of vtable entries in `ICoreWindow` (IInspectable's 6, 18 methods/properties, and
/// 17 event add/remove pairs).
const CORE_WINDOW_SLOTS: usize = 6 + 18 + 34;

/// Debug aid (`--trace-window`): replaces the window object's vtable pointer with a copy whose
/// entries log "CoreWindow slot N" before jumping to the real implementation, so the slots
/// the title really calls are visible. The title may have been built against a different
/// `ICoreWindow` layout than desktop Windows.
pub fn instrument_window() {
    let sh = shell();
    let obj = Interface::as_raw(&sh.window) as *mut usize;
    let orig = unsafe { *obj } as *const usize;
    let mut arena = Box::new(crate::StubArena::new());
    let mut copy: Vec<usize> = (0..CORE_WINDOW_SLOTS).map(|i| unsafe { *orig.add(i) }).collect();
    for i in 6..CORE_WINDOW_SLOTS {
        copy[i] = arena.make_trace(format!("CoreWindow slot {i}"), copy[i] as u64) as usize;
    }
    // Both the arena and the copy must outlive the process.
    Box::leak(arena);
    let copy: &'static [usize] = Vec::leak(copy);
    unsafe { *obj = copy.as_ptr() as usize };
    wlog!("[winrt] ICoreWindow vtable instrumented ({CORE_WINDOW_SLOTS} slots)");
}

/// Names the shell object whose vtable is at `vtbl`, for crash reports. Allocation-free except
/// for reading the shell's state, so it only looks at objects that already exist.
pub fn identify_vtable(vtbl: usize) -> Option<&'static str> {
    if vtbl == CoreAppTag::VTBL as *const _ as usize {
        return Some("shell ICoreApplication");
    }
    if vtbl == AppViewTag::VTBL as *const _ as usize {
        return Some("shell ICoreApplicationView");
    }
    if vtbl == DispatcherTag::VTBL as *const _ as usize {
        return Some("shell ICoreDispatcher");
    }
    if vtbl == generic_vtbl() as usize {
        return Some("generic Xbox stand-in");
    }
    if vtbl == mm_vtbl() as usize {
        return Some("MMDevice enumerator proxy");
    }
    if let Some(sh) = SHELL.get() {
        let w = Interface::as_raw(&sh.window);
        if vtbl == unsafe { *(w as *const usize) } {
            return Some("shell ICoreWindow");
        }
    }
    None
}

/// Called after a successful `CoCreateInstance`. If the object is the standard MMDevice
/// enumerator, replaces `*out` with a proxy that owns the real object.
pub fn wrap_co_created(clsid: &GUID, iid: &GUID, out: *mut *mut c_void) {
    if *clsid != CLSID_MM_DEVICE_ENUMERATOR || (*iid != IID_MM_DEVICE_ENUMERATOR && *iid != IUnknown::IID) {
        return;
    }
    let real = unsafe { *out };
    if real.is_null() {
        return;
    }
    let proxy = Box::into_raw(Box::new(MmProxy { vtbl: mm_vtbl(), refs: AtomicU32::new(1), real }));
    unsafe { *out = proxy as *mut c_void };
    wlog!("[winrt] wrapped MMDeviceEnumerator in a proxy");
}

// ---------------------------------------------------------------------------------------
// Hook for GetActivationFactoryByPCWSTR
// ---------------------------------------------------------------------------------------

/// Called before the real activation-factory lookup. Returns `Some(hr)` when the class is
/// one we implement ourselves.
pub fn try_activation_factory(class: &str, iid: &GUID, out: *mut *mut c_void) -> Option<HRESULT> {
    if class.starts_with("Windows.Xbox.") || class.starts_with("Microsoft.Xbox.") || class.starts_with("Windows.Kinect.") {
        unsafe { *out = create_generic(class, iid) };
        wlog!("[winrt] generic stand-in for console-only class {class} (iid {iid:?})");
        return Some(HRESULT(0));
    }
    // `Package.Current` fails in a process without package identity (this one), but the title
    // reads it at startup (IPackageStatics slot 6), so it gets a logging stand-in.
    if class == "Windows.ApplicationModel.Package" || class == "Windows.ApplicationModel.Store.CurrentApp" {
        unsafe { *out = create_generic(class, iid) };
        wlog!("[winrt] generic stand-in for {class} (iid {iid:?})");
        return Some(HRESULT(0));
    }
    if class == CoreWindowStaticsTag::NAME {
        let obj = Com::<CoreWindowStaticsTag>::create(());
        let hr = unsafe { com_query_interface::<CoreWindowStaticsTag>(obj, iid, out) };
        unsafe { com_release::<CoreWindowStaticsTag>(obj) };
        wlog_rare!("[winrt] CoreWindow statics: iid={iid:?} -> {:#x}", hr.0 as u32);
        return Some(hr);
    }
    if class != CoreAppTag::NAME {
        return None;
    }
    let obj = Com::<CoreAppTag>::create(());
    let mut hr = unsafe { com_query_interface::<CoreAppTag>(obj, iid, out) };
    unsafe { com_release::<CoreAppTag>(obj) };
    // The title asks this factory for two console-only extension interfaces:
    //  - 9FF3799E-6E83-468C-A5B0-501CE6BB0FA1: the title tolerates E_NOINTERFACE. Answering it
    //    with the generic stand-in made the title jump to a bogus address (it called a returned
    //    pointer as code), so it is left unimplemented.
    //  - 42A6A529-3CC6-4867-B5B7-6D1FAF5325F9: the title does not tolerate its absence (C++/CX
    //    throws InvalidCastException, nothing catches it, terminate() is called), so it gets the
    //    generic stand-in.
    // 9FF3799E: Forza tolerates E_NOINTERFACE (a generic stand-in made it jump to a bogus address),
    // Sonic Mania throws InvalidCastException without it. A null object (every method succeeds and
    // writes nothing) is the default (XB1_NO_COREAPP_9FF=1 restores E_NOINTERFACE); Forza behaves the same either way.
    if hr == E_NOINTERFACE && *iid == GUID::from_u128(0x9FF3799E_6E83_468C_A5B0_501CE6BB0FA1) && std::env::var_os("XB1_NO_COREAPP_9FF").is_none() {
        unsafe { *out = crate::null_object() as *mut c_void };
        wlog!("[winrt] CoreApplication extension 9FF3799E -> null object");
        hr = HRESULT(0);
    }
    if hr == E_NOINTERFACE && *iid == GUID::from_u128(0x42A6A529_3CC6_4867_B5B7_6D1FAF5325F9) {
        unsafe { *out = create_generic(&format!("{class} (console extension 42A6A529)"), iid) };
        hr = HRESULT(0);
    }
    wlog!("[winrt] activation factory for {class}: iid={iid:?} -> {:#x}", hr.0 as u32);
    Some(hr)
}


/// `IVectorView<T>::GetMany(uint start, uint capacity, T* items, uint* actual)`: GetAt for each
/// index from `start` on (the older Minecraft build reads its controller list this way).
unsafe extern "system" fn vector_view_get_many(this: *mut c_void, start: u32, capacity: u32, items: *mut *mut c_void, actual: *mut u32) -> HRESULT {
    let o = unsafe { &*(this as *const GenericObj) };
    let size = o.kind as u32;
    let mut written = 0u32;
    if !items.is_null() {
        let mut i = start;
        while i < size && written < capacity {
            if unsafe { vector_view_get_at(this, i, items.add(written as usize)) }.0 != 0 {
                break;
            }
            written += 1;
            i += 1;
        }
    }
    if !actual.is_null() {
        unsafe { *actual = written };
    }
    wlog!("[winrt] {}: GetMany({start}, {capacity}) -> {written} element(s)", o.class);
    HRESULT(0)
}

/// True if `p` lies inside this layer's own image (its vtables and statics): never a token slot.
/// (`GetModuleHandle(NULL)` is the title here, because the bootstrap points the PEB at it, so the layer's
/// own module is found from the address of this function.)
fn in_layer_image(p: *mut c_void) -> bool {
    unsafe extern "system" {
        fn GetModuleHandleExA(flags: u32, address: *const c_void, module: *mut *mut u8) -> i32;
    }
    static RANGE: OnceLock<(usize, usize)> = OnceLock::new();
    let (lo, hi) = *RANGE.get_or_init(|| unsafe {
        let mut base: *mut u8 = ptr::null_mut();
        // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT
        if GetModuleHandleExA(0x4 | 0x2, in_layer_image as *const c_void, &mut base) == 0 || base.is_null() {
            return (0, 0);
        }
        let size = *(base.add(*(base.add(0x3C) as *const u32) as usize + 0x50) as *const u32) as usize;
        (base as usize, base as usize + size)
    });
    (p as usize) >= lo && (p as usize) < hi
}
