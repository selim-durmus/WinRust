//! Being a folder window to the shell, like Explorer's. The window is in the shell's list of open folder windows
//! (IShellWindows) with the folder it shows, and answers the shell's "browser" and "view" questions. That is what
//! "Show in folder" of other programs needs (Firefox's downloads, SHOpenFolderAndSelectItems): the shell starts us
//! with the folder, waits for a window showing it to appear in that list (20 s, then gives up), and asks its view
//! to select the item. Without it the item was never selected and the caller hung for 20 s.

use std::{cell::{Cell, RefCell}, ffi::c_void, mem::ManuallyDrop, os::windows::ffi::OsStrExt, path::{Path, PathBuf}, ptr, sync::{Mutex, MutexGuard}};
use windows::{core::*, Win32::{Foundation::*, System::{Com::*, Ole::*, Variant::*}, UI::{Controls::*, Shell::{Common::ITEMIDLIST, *}, WindowsAndMessaging::*}}};

/// Posted to the window: wp 0 = put the window in the shell's list / tell it the new folder, 1 = select what the
/// shell asked for (`take_select`), 2 = go to the folder the shell asked for (`take_browse`).
pub const WM_SHWIN: u32 = WM_APP + 11;

/// The service of a browser that is its folder view (not in the windows crate).
const SID_SFOLDERVIEW: GUID = GUID::from_u128(0xcde725b0_ccc9_4519_917e_325d72fab4ce);

type Reg = (IShellWindows, IDispatch, Vec<i32>);

thread_local! {
    // the shell's list, our window's object in it and its entries (one per id of the folder, see `update`).
    // ManuallyDrop: never released by the thread-local's destructor. That runs inside ExitProcess, after every other
    // thread is gone, COM's included; releasing the list (a proxy to another process) then waits for a reply that
    // never comes. The process hung there for good, keeping its exe locked, and could not be killed (it was already
    // exiting). `gone` releases it while the window still works.
    static REG: RefCell<Option<ManuallyDrop<Reg>>> = const { RefCell::new(None) };
    static CLOSED: Cell<bool> = const { Cell::new(false) }; // the window closes: no new entries (gone's calls let messages through)
}

// The shell calls our objects on its own threads (windows-rs objects are agile), so what they share with the
// window lives here, not in thread-locals
static SHOWN: Mutex<PathBuf> = Mutex::new(PathBuf::new());          // the folder the list knows us at
static SELECT: Mutex<Option<Vec<String>>> = Mutex::new(None);       // names to select, from the shell
static BROWSE: Mutex<Option<PathBuf>> = Mutex::new(None);           // a folder to go to, from the shell

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> { m.lock().unwrap_or_else(|e| e.into_inner()) }
fn shown() -> PathBuf { lock(&SHOWN).clone() }

fn wide(p: &Path) -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() }

/// The shell's id of `dir` (free with ILFree).
fn pidl_of(dir: &Path) -> Option<*mut ITEMIDLIST> {
    let mut pidl = ptr::null_mut();
    unsafe { SHParseDisplayName(PCWSTR(wide(dir).as_ptr()), None, &mut pidl, 0, None).ok()? };
    (!pidl.is_null()).then_some(pidl)
}

fn path_of(pidl: *const ITEMIDLIST) -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    if !unsafe { SHGetPathFromIDListW(pidl, &mut buf) }.as_bool() { return None; }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// The window now shows `dir`: the shell's list learns it (on the next message, so a new window is on screen first).
pub fn navigated(h: *mut c_void, dir: &Path) {
    *lock(&SHOWN) = dir.to_path_buf();
    unsafe { let _ = PostMessageW(Some(HWND(h)), WM_SHWIN, WPARAM(0), LPARAM(0)); }
}

/// WM_SHWIN wp 0: puts the window in the shell's list the first time, and tells the list the folder shown.
pub fn update(h: *mut c_void) {
    let dir = shown();
    // a folder can have two ids: the plain path's (ILCreateFromPathW: what Firefox and Chrome pass) and the parsed
    // one (SHParseDisplayName: Downloads, Documents... under the user's folder). The list matches ids byte for byte,
    // so the window is in it once for each
    let mut locs: Vec<Vec<u8>> = vec![];
    let plain = unsafe { ILCreateFromPathW(PCWSTR(wide(&dir).as_ptr())) };
    for p in [(!plain.is_null()).then_some(plain), pidl_of(&dir)].into_iter().flatten() {
        let b = unsafe { std::slice::from_raw_parts(p as *const u8, ILGetSize(Some(p)) as usize).to_vec() };
        unsafe { ILFree(Some(p)) };
        if !locs.contains(&b) { locs.push(b); }
    }
    if locs.is_empty() || CLOSED.get() { return; }
    if REG.with_borrow(|r| r.is_none()) {
        let Ok(sw) = (unsafe { CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL) }) else { return };
        let Ok(disp) = IWebBrowser2::from(Browser { hwnd: h as isize }).cast::<IDispatch>() else { return };
        REG.set(Some(ManuallyDrop::new((sw, disp, vec![]))));
    }
    // (no borrow held across the calls: they let messages through)
    let Some((sw, disp, mut cookies)) = REG.with_borrow(|r| r.as_deref().cloned()) else { return };
    while cookies.len() < locs.len() {
        // the folder first (pending), then the window: a "Show in folder" that started us waits for the window's
        // registration and looks at its folder right then, so the folder must already be there (as Explorer does)
        let b = &locs[cookies.len()];
        let Ok(v) = (unsafe { InitVariantFromBuffer(b.as_ptr() as *const c_void, b.len() as u32) }) else { break };
        let tid = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() } as i32;
        let pending = unsafe { sw.RegisterPending(tid, &v, &VARIANT::default(), SWC_BROWSER) };
        match unsafe { sw.Register(&disp, h as isize as i32, SWC_BROWSER) } {
            Ok(c) => {
                if let Ok(p) = pending { if p != c { unsafe { let _ = sw.Revoke(p); } } }
                cookies.push(c);
            }
            Err(_) => { if let Ok(p) = pending { unsafe { let _ = sw.Revoke(p); } } break }
        }
    }
    for (i, &c) in cookies.iter().enumerate() {
        let b = &locs[i.min(locs.len() - 1)]; // more entries than ids: the last id again
        if let Ok(v) = unsafe { InitVariantFromBuffer(b.as_ptr() as *const c_void, b.len() as u32) } { unsafe { let _ = sw.OnNavigate(c, &v); } }
    }
    REG.with_borrow_mut(|r| if let Some(r) = r { r.2 = cookies; });
}

/// The window closes (WM_DESTROY): out of the shell's list, and the list released.
pub fn gone() {
    CLOSED.set(true);
    if let Some(r) = REG.take() {
        let (sw, _, cookies) = ManuallyDrop::into_inner(r);
        for c in cookies { unsafe { let _ = sw.Revoke(c); } }
    }
}

/// The names the shell asked to select in the shown folder (WM_SHWIN wp 1), if any.
pub fn take_select() -> Option<Vec<String>> { lock(&SELECT).take() }
pub fn wants_select() -> bool { lock(&SELECT).is_some() }
/// The folder the shell asked the window to go to (WM_SHWIN wp 2), if any.
pub fn take_browse() -> Option<PathBuf> { lock(&BROWSE).take() }

/// The window's automation object in the shell's list. "Show in folder" asks it for the window (HWND) and its
/// Document, and through the Document's services for the view (SID_SFolderView), whose SelectItem selects.
/// Registered as SWC_BROWSER: the shell looks for that kind, not SWC_EXPLORER.
#[implement(IWebBrowser2, IServiceProvider, IShellBrowser)]
struct Browser { hwnd: isize }

impl Browser_Impl {
    fn wnd(&self) -> HWND { HWND(self.hwnd as *mut c_void) }
}

fn no<T>() -> Result<T> { Err(E_NOTIMPL.into()) }

/// The parts of a shell window's automation object the shell uses: its window, that it is visible, where it is.
impl IWebBrowser_Impl for Browser_Impl {
    fn GoBack(&self) -> Result<()> { no() }
    fn GoForward(&self) -> Result<()> { no() }
    fn GoHome(&self) -> Result<()> { no() }
    fn GoSearch(&self) -> Result<()> { no() }
    fn Navigate(&self, _: &BSTR, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT) -> Result<()> { no() }
    fn Refresh(&self) -> Result<()> { no() }
    fn Refresh2(&self, _: *const VARIANT) -> Result<()> { no() }
    fn Stop(&self) -> Result<()> { Ok(()) }
    fn Application(&self) -> Result<IDispatch> { no() }
    fn Parent(&self) -> Result<IDispatch> { no() }
    fn Container(&self) -> Result<IDispatch> { no() }
    fn Document(&self) -> Result<IDispatch> { IShellFolderViewDual::from(Doc { hwnd: self.hwnd }).cast() }
    fn TopLevelContainer(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_TRUE) }
    fn Type(&self) -> Result<BSTR> { no() }
    fn Left(&self) -> Result<i32> { no() }
    fn SetLeft(&self, _: i32) -> Result<()> { no() }
    fn Top(&self) -> Result<i32> { no() }
    fn SetTop(&self, _: i32) -> Result<()> { no() }
    fn Width(&self) -> Result<i32> { no() }
    fn SetWidth(&self, _: i32) -> Result<()> { no() }
    fn Height(&self) -> Result<i32> { no() }
    fn SetHeight(&self, _: i32) -> Result<()> { no() }
    fn LocationName(&self) -> Result<BSTR> { let d = shown(); Ok(BSTR::from(d.file_name().map_or_else(|| d.to_string_lossy(), |n| n.to_string_lossy()).as_ref())) }
    fn LocationURL(&self) -> Result<BSTR> { Ok(BSTR::from(format!("file:///{}", shown().to_string_lossy().replace('\\', "/")))) }
    fn Busy(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_FALSE) }
}

impl IWebBrowserApp_Impl for Browser_Impl {
    fn Quit(&self) -> Result<()> { unsafe { PostMessageW(Some(self.wnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) } }
    fn ClientToWindow(&self, _: *mut i32, _: *mut i32) -> Result<()> { no() }
    fn PutProperty(&self, _: &BSTR, _: &VARIANT) -> Result<()> { no() }
    fn GetProperty(&self, _: &BSTR) -> Result<VARIANT> { no() }
    fn Name(&self) -> Result<BSTR> { Ok(BSTR::from("Rust Explorer")) }
    fn HWND(&self) -> Result<SHANDLE_PTR> { Ok(SHANDLE_PTR(self.hwnd)) }
    fn FullName(&self) -> Result<BSTR> { Ok(BSTR::from(std::env::current_exe().unwrap_or_default().to_string_lossy().as_ref())) }
    fn Path(&self) -> Result<BSTR> { Ok(BSTR::from(std::env::current_exe().unwrap_or_default().parent().unwrap_or(Path::new("")).to_string_lossy().as_ref())) }
    fn Visible(&self) -> Result<VARIANT_BOOL> { Ok(if unsafe { IsWindowVisible(self.wnd()) }.as_bool() { VARIANT_TRUE } else { VARIANT_FALSE }) }
    fn SetVisible(&self, v: VARIANT_BOOL) -> Result<()> {
        if v.as_bool() { unsafe { let _ = ShowWindow(self.wnd(), if IsIconic(self.wnd()).as_bool() { SW_RESTORE } else { SW_SHOW }); } }
        Ok(())
    }
    fn StatusBar(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_TRUE) }
    fn SetStatusBar(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
    fn StatusText(&self) -> Result<BSTR> { Ok(BSTR::new()) }
    fn SetStatusText(&self, _: &BSTR) -> Result<()> { Ok(()) }
    fn ToolBar(&self) -> Result<i32> { Ok(1) }
    fn SetToolBar(&self, _: i32) -> Result<()> { Ok(()) }
    fn MenuBar(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_FALSE) }
    fn SetMenuBar(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
    fn FullScreen(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_FALSE) }
    fn SetFullScreen(&self, _: VARIANT_BOOL) -> Result<()> { no() }
}

impl IWebBrowser2_Impl for Browser_Impl {
    fn Navigate2(&self, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT) -> Result<()> { no() }
    fn QueryStatusWB(&self, _: OLECMDID) -> Result<OLECMDF> { no() }
    fn ExecWB(&self, _: OLECMDID, _: OLECMDEXECOPT, _: *const VARIANT, _: *mut VARIANT) -> Result<()> { no() }
    fn ShowBrowserBar(&self, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT) -> Result<()> { no() }
    fn ReadyState(&self) -> Result<READYSTATE> { Ok(READYSTATE_COMPLETE) }
    fn Offline(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_FALSE) }
    fn SetOffline(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
    fn Silent(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_FALSE) }
    fn SetSilent(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
    fn RegisterAsBrowser(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_TRUE) }
    fn SetRegisterAsBrowser(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
    fn RegisterAsDropTarget(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_TRUE) }
    fn SetRegisterAsDropTarget(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
    fn TheaterMode(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_FALSE) }
    fn SetTheaterMode(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
    fn AddressBar(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_TRUE) }
    fn SetAddressBar(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
    fn Resizable(&self) -> Result<VARIANT_BOOL> { Ok(VARIANT_TRUE) }
    fn SetResizable(&self, _: VARIANT_BOOL) -> Result<()> { Ok(()) }
}

impl IDispatch_Impl for Browser_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> { Ok(0) }
    fn GetTypeInfo(&self, _: u32, _: u32) -> Result<ITypeInfo> { Err(E_NOTIMPL.into()) }
    fn GetIDsOfNames(&self, _: *const GUID, _: *const PCWSTR, _: u32, _: u32, _: *mut i32) -> Result<()> { Err(DISP_E_UNKNOWNNAME.into()) }
    fn Invoke(&self, _: i32, _: *const GUID, _: u32, _: DISPATCH_FLAGS, _: *const DISPPARAMS, _: *mut VARIANT, _: *mut EXCEPINFO, _: *mut u32) -> Result<()> { Err(DISP_E_MEMBERNOTFOUND.into()) }
}

impl IServiceProvider_Impl for Browser_Impl {
    fn QueryService(&self, service: *const GUID, riid: *const GUID, out: *mut *mut c_void) -> Result<()> { service_of(self.hwnd, service, riid, out) }
}

/// The browser's services: the browser itself (its view through QueryActiveShellView).
fn service_of(hwnd: isize, service: *const GUID, riid: *const GUID, out: *mut *mut c_void) -> Result<()> {
    unsafe {
        if out.is_null() { return Err(E_POINTER.into()); }
        *out = ptr::null_mut();
        let s = *service;
        if s == SID_STopLevelBrowser || s == IShellBrowser::IID {
            let unk: IUnknown = Browser { hwnd }.into();
            return unk.query(riid, out).ok();
        }
        if s == SID_SFOLDERVIEW {
            let unk: IUnknown = View { hwnd }.into();
            return unk.query(riid, out).ok();
        }
    }
    Err(E_NOINTERFACE.into())
}

impl IOleWindow_Impl for Browser_Impl {
    fn GetWindow(&self) -> Result<HWND> { Ok(HWND(self.hwnd as *mut c_void)) }
    fn ContextSensitiveHelp(&self, _: BOOL) -> Result<()> { Err(E_NOTIMPL.into()) }
}

impl IShellBrowser_Impl for Browser_Impl {
    fn InsertMenusSB(&self, _: HMENU, _: *mut OLEMENUGROUPWIDTHS) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn SetMenuSB(&self, _: HMENU, _: isize, _: HWND) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn RemoveMenusSB(&self, _: HMENU) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn SetStatusTextSB(&self, _: &PCWSTR) -> Result<()> { Ok(()) }
    fn EnableModelessSB(&self, _: BOOL) -> Result<()> { Ok(()) }
    fn TranslateAcceleratorSB(&self, _: *const MSG, _: u16) -> Result<()> { Err(S_FALSE.into()) }
    fn BrowseObject(&self, pidl: *const ITEMIDLIST, flags: u32) -> Result<()> {
        // an absolute folder only (SBSP_RELATIVE / PARENT / NAVIGATEBACK ... are not asked for by "Show in folder")
        if pidl.is_null() || flags & (SBSP_RELATIVE | SBSP_PARENT | SBSP_NAVIGATEBACK | SBSP_NAVIGATEFORWARD) != 0 { return Err(E_NOTIMPL.into()); }
        let Some(p) = path_of(pidl) else { return Err(E_FAIL.into()) };
        *lock(&BROWSE) = Some(p);
        unsafe { PostMessageW(Some(HWND(self.hwnd as *mut c_void)), WM_SHWIN, WPARAM(2), LPARAM(0)) }
    }
    fn GetViewStateStream(&self, _: u32) -> Result<IStream> { Err(E_NOTIMPL.into()) }
    fn GetControlWindow(&self, _: u32) -> Result<HWND> { Err(E_NOTIMPL.into()) }
    fn SendControlMsg(&self, _: u32, _: u32, _: WPARAM, _: LPARAM, _: *mut LRESULT) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn QueryActiveShellView(&self) -> Result<IShellView> { Ok(View { hwnd: self.hwnd }.into()) }
    fn OnViewWindowActive(&self, _: Ref<IShellView>) -> Result<()> { Ok(()) }
    fn SetToolbarItems(&self, _: *const TBBUTTON, _: u32, _: u32) -> Result<()> { Ok(()) }
}

/// The view's automation object (the browser's Document): "Show in folder" selects through it.
#[implement(IShellFolderViewDual, IServiceProvider)]
struct Doc { hwnd: isize }

impl IDispatch_Impl for Doc_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> { Ok(0) }
    fn GetTypeInfo(&self, _: u32, _: u32) -> Result<ITypeInfo> { Err(E_NOTIMPL.into()) }
    fn GetIDsOfNames(&self, _: *const GUID, _: *const PCWSTR, _: u32, _: u32, _: *mut i32) -> Result<()> { Err(DISP_E_UNKNOWNNAME.into()) }
    fn Invoke(&self, _: i32, _: *const GUID, _: u32, _: DISPATCH_FLAGS, _: *const DISPPARAMS, _: *mut VARIANT, _: *mut EXCEPINFO, _: *mut u32) -> Result<()> { Err(DISP_E_MEMBERNOTFOUND.into()) }
}

impl IShellFolderViewDual_Impl for Doc_Impl {
    fn Application(&self) -> Result<IDispatch> { no() }
    fn Parent(&self) -> Result<IDispatch> { no() }
    fn Folder(&self) -> Result<Folder> { no() }
    fn SelectedItems(&self) -> Result<FolderItems> { no() }
    fn FocusedItem(&self) -> Result<FolderItem> { no() }
    fn SelectItem(&self, _: *const VARIANT, _: i32) -> Result<()> { no() }
    fn PopupItemMenu(&self, _: Ref<FolderItem>, _: &VARIANT, _: &VARIANT) -> Result<BSTR> { no() }
    fn Script(&self) -> Result<IDispatch> { no() }
    fn ViewOptions(&self) -> Result<i32> { no() }
}

impl IServiceProvider_Impl for Doc_Impl {
    fn QueryService(&self, service: *const GUID, riid: *const GUID, out: *mut *mut c_void) -> Result<()> {
        service_of(self.hwnd, service, riid, out)
    }
}

/// The view of the browser: what the shell selects items through.
#[implement(IShellView)]
struct View { hwnd: isize }

impl IOleWindow_Impl for View_Impl {
    fn GetWindow(&self) -> Result<HWND> { Ok(HWND(self.hwnd as *mut c_void)) }
    fn ContextSensitiveHelp(&self, _: BOOL) -> Result<()> { Err(E_NOTIMPL.into()) }
}

impl IShellView_Impl for View_Impl {
    fn TranslateAccelerator(&self, _: *const MSG) -> HRESULT { S_FALSE }
    fn EnableModeless(&self, _: BOOL) -> Result<()> { Ok(()) }
    fn UIActivate(&self, _: u32) -> Result<()> { Ok(()) }
    fn Refresh(&self) -> Result<()> { Ok(()) }
    fn CreateViewWindow(&self, _: Ref<IShellView>, _: *const FOLDERSETTINGS, _: Ref<IShellBrowser>, _: *const RECT) -> Result<HWND> { Err(E_NOTIMPL.into()) }
    fn DestroyViewWindow(&self) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn GetCurrentInfo(&self) -> Result<FOLDERSETTINGS> { Ok(FOLDERSETTINGS { ViewMode: FVM_DETAILS.0 as u32, fFlags: 0 }) }
    fn AddPropertySheetPages(&self, _: u32, _: LPFNSVADDPROPSHEETPAGE, _: LPARAM) -> Result<()> { Err(E_NOTIMPL.into()) }
    fn SaveViewState(&self) -> Result<()> { Ok(()) }
    fn SelectItem(&self, pidl: *const ITEMIDLIST, flags: u32) -> Result<()> {
        // SVSI_DESELECTOTHERS starts a new selection; SVSI_SELECT adds the item (an id relative to the shown folder)
        if flags & SVSI_DESELECTOTHERS.0 as u32 != 0 { *lock(&SELECT) = Some(vec![]); }
        if !pidl.is_null() && flags & SVSI_SELECT.0 as u32 != 0 {
            let dir = shown();
            let full = pidl_of(&dir).map(|f| unsafe { let c = ILCombine(Some(f), Some(pidl)); ILFree(Some(f)); c });
            let path = full.filter(|c| !c.is_null()).and_then(|c| { let p = path_of(c); unsafe { ILFree(Some(c)) }; p });
            if let Some(name) = path.as_deref().and_then(Path::file_name) {
                let name = name.to_string_lossy().into_owned();
                lock(&SELECT).get_or_insert_with(Vec::new).push(name);
            }
        }
        if wants_select() {
            unsafe { PostMessageW(Some(HWND(self.hwnd as *mut c_void)), WM_SHWIN, WPARAM(1), LPARAM(0))? };
        }
        Ok(())
    }
    fn GetItemObject(&self, _: &_SVGIO, _: *const GUID, _: *mut *mut c_void) -> Result<()> { Err(E_NOTIMPL.into()) }
}

