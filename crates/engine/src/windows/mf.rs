//! Resolve MF lazily so Windows Server without the Media Foundation feature
//! can still start Latch and report an unavailable codec instead of failing in
//! the process loader. The system DLL is retained for the process lifetime.
#![allow(non_snake_case)]
use std::{ffi::c_void, sync::OnceLock};
use windows::{
    core::{s, w, Error, Interface, HRESULT, PCSTR},
    Win32::{
        Foundation::{E_POINTER, HANDLE, HMODULE},
        Media::MediaFoundation::*,
        System::LibraryLoader::*,
    },
};
fn resolve(name: PCSTR) -> windows::core::Result<unsafe extern "system" fn() -> isize> {
    static MODULE: OnceLock<Result<usize, HRESULT>> = OnceLock::new();
    let module = MODULE
        .get_or_init(|| {
            // SAFETY: Load only the system copy; retain its reference while cached
            // function pointers and any MF COM objects can still exist.
            unsafe {
                LoadLibraryExW(
                    w!("mfplat.dll"),
                    HANDLE::default(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            }
            .map(|m| m.0 as usize)
            .map_err(|e| e.code())
        })
        .as_ref()
        .map_err(|code| Error::from_hresult(*code))?;
    // SAFETY: The cached module retains its loader reference permanently.
    unsafe { GetProcAddress(HMODULE(*module as *mut c_void), name) }.ok_or_else(Error::from_win32)
}
macro_rules! bind {
    ($name:literal, $signature:ty) => {{
        static FUNCTION: OnceLock<Result<$signature, HRESULT>> = OnceLock::new();
        *FUNCTION
            .get_or_init(|| {
                let pointer = resolve(s!($name)).map_err(|e| e.code())?;
                // SAFETY: Each signature below is the exact Windows SDK ABI for
                // the named export; resolve retains the module that owns it.
                Ok(unsafe {
                    std::mem::transmute::<unsafe extern "system" fn() -> isize, $signature>(pointer)
                })
            })
            .as_ref()
            .map_err(|code| Error::from_hresult(*code))?
    }};
}
unsafe fn created<T: Interface>(
    call: impl FnOnce(*mut *mut c_void) -> HRESULT,
) -> windows::core::Result<T> {
    let mut pointer = std::ptr::null_mut();
    call(&mut pointer).ok()?;
    if pointer.is_null() {
        return Err(Error::from_hresult(E_POINTER));
    }
    // SAFETY: Successful COM creation transfers one owned reference.
    Ok(unsafe { T::from_raw(pointer) })
}
pub unsafe fn MFStartup(version: u32, flags: u32) -> windows::core::Result<()> {
    // SAFETY: Exact SDK signature; caller balances startup with shutdown.
    let function = bind!("MFStartup", unsafe extern "system" fn(u32, u32) -> HRESULT);
    // SAFETY: Caller owns the matching startup/shutdown scope.
    unsafe { function(version, flags).ok() }
}
pub unsafe fn MFShutdown() -> windows::core::Result<()> {
    // SAFETY: Caller owns a successful MF startup reference.
    let function = bind!("MFShutdown", unsafe extern "system" fn() -> HRESULT);
    // SAFETY: Caller owns a successful startup reference.
    unsafe { function().ok() }
}
pub unsafe fn MFCreateMediaType() -> windows::core::Result<IMFMediaType> {
    let function = bind!(
        "MFCreateMediaType",
        unsafe extern "system" fn(*mut *mut c_void) -> HRESULT
    );
    // SAFETY: Exact SDK creation signature, adopted as an owned interface.
    unsafe { created(|out| function(out)) }
}
pub unsafe fn MFCreateSample() -> windows::core::Result<IMFSample> {
    let function = bind!(
        "MFCreateSample",
        unsafe extern "system" fn(*mut *mut c_void) -> HRESULT
    );
    // SAFETY: Exact SDK creation signature, adopted as an owned interface.
    unsafe { created(|out| function(out)) }
}
pub unsafe fn MFCreateMemoryBuffer(length: u32) -> windows::core::Result<IMFMediaBuffer> {
    let function = bind!(
        "MFCreateMemoryBuffer",
        unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT
    );
    // SAFETY: Exact SDK creation signature, adopted as an owned interface.
    unsafe { created(|out| function(length, out)) }
}
pub unsafe fn MFCreateAlignedMemoryBuffer(
    length: u32,
    alignment: u32,
) -> windows::core::Result<IMFMediaBuffer> {
    let function = bind!(
        "MFCreateAlignedMemoryBuffer",
        unsafe extern "system" fn(u32, u32, *mut *mut c_void) -> HRESULT
    );
    // SAFETY: Exact SDK creation signature, adopted as an owned interface.
    unsafe { created(|out| function(length, alignment, out)) }
}
pub unsafe fn MFCreateDXGIDeviceManager(
    token: *mut u32,
    manager: *mut Option<IMFDXGIDeviceManager>,
) -> windows::core::Result<()> {
    // SAFETY: Caller supplies writable SDK output slots.
    let function = bind!(
        "MFCreateDXGIDeviceManager",
        unsafe extern "system" fn(*mut u32, *mut Option<IMFDXGIDeviceManager>) -> HRESULT
    );
    // SAFETY: Caller supplies the SDK output pointers and optional filters.
    unsafe { function(token, manager).ok() }
}
pub unsafe fn MFCreateDXGISurfaceBuffer(
    iid: *const windows::core::GUID,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    index: u32,
    bottom_up: bool,
) -> windows::core::Result<IMFMediaBuffer> {
    let function = bind!(
        "MFCreateDXGISurfaceBuffer",
        unsafe extern "system" fn(
            *const windows::core::GUID,
            *mut c_void,
            u32,
            windows::Win32::Foundation::BOOL,
            *mut *mut c_void,
        ) -> HRESULT
    );
    // SAFETY: Caller retains the texture; the created buffer adds a reference.
    unsafe { created(|out| function(iid, texture.as_raw(), index, bottom_up.into(), out)) }
}
pub unsafe fn MFTEnumEx(
    category: windows::core::GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<*const MFT_REGISTER_TYPE_INFO>,
    output: Option<*const MFT_REGISTER_TYPE_INFO>,
    activations: *mut *mut Option<IMFActivate>,
    count: *mut u32,
) -> windows::core::Result<()> {
    // SAFETY: Caller supplies writable output slots and valid optional filters.
    let function = bind!(
        "MFTEnumEx",
        unsafe extern "system" fn(
            windows::core::GUID,
            MFT_ENUM_FLAG,
            *const MFT_REGISTER_TYPE_INFO,
            *const MFT_REGISTER_TYPE_INFO,
            *mut *mut Option<IMFActivate>,
            *mut u32,
        ) -> HRESULT
    );
    // SAFETY: Caller supplies the SDK output pointers and optional filters.
    unsafe {
        function(
            category,
            flags,
            input.unwrap_or(std::ptr::null()),
            output.unwrap_or(std::ptr::null()),
            activations,
            count,
        )
        .ok()
    }
}
