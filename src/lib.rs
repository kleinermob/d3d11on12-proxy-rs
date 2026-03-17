//! d3d11.dll — D3D11on12 proxy
//!
//! ===========================================================================
//! What this does
//! ===========================================================================
//!
//! Intercepts D3D11CreateDevice and D3D11CreateDeviceAndSwapChain and
//! redirects them through D3D11On12CreateDevice so that all D3D11 rendering
//! is backed by a D3D12 device internally. This gives the game a proper
//! DXGI swap chain, enabling DXGI-level hooks (overlays, vsync-off proxies)
//! to work on top of games that only support D3D11.
//!
//! All other d3d11.dll exports are forwarded transparently to the real DLL.
//!
//! ===========================================================================
//! Fixes vs the original C++ implementation
//! ===========================================================================
//!
//! 1. d3d12device leaked when CreateCommandQueue fails — fixed via RAII
//! 2. d3d12device leaked when D3D11On12CreateDevice not found — fixed via RAII
//! 3. d3d12queue never released after D3D11On12CreateDevice call — fixed via RAII
//! 4. d3d11Device + d3d11Context leaked on early returns in
//!    D3D11CreateDeviceAndSwapChain — fixed via RAII ComPtr wrappers
//! 5. getPfnD3D11On12CreateDevice race condition — fixed via OnceLock
//! 6. D3D12_COMMAND_QUEUE_DESC not zero-initialized — fixed
//! 7. DriverType = WARP silently ignored — now handled
//! 8. D3D12CreateDevice / CreateCommandQueue called with wrong number of
//!    args for windows crate 0.58 API — fixed by loading raw fn pointers

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::{BOOL, HMODULE, E_FAIL, E_INVALIDARG, S_OK};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

// ---------------------------------------------------------------------------
// Raw D3D12 / D3D11on12 function pointer types
// ---------------------------------------------------------------------------
//
// DEVNOTE: windows crate 0.58 exposes D3D12CreateDevice and CreateCommandQueue
// as generic functions with Result<T> returns, not matching the raw COM ABI
// our vtable-based pattern expects. We load them as raw function pointers
// from the system DLLs to get the exact signature we need.

type FnD3D12CreateDevice = unsafe extern "system" fn(
    p_adapter: *mut core::ffi::c_void,
    minimum_feature_level: D3D_FEATURE_LEVEL,
    riid: *const GUID,
    pp_device: *mut *mut core::ffi::c_void,
) -> HRESULT;

type FnD3D12CreateCommandQueue = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    p_desc: *const D3D12_COMMAND_QUEUE_DESC,
    riid: *const GUID,
    pp_command_queue: *mut *mut core::ffi::c_void,
) -> HRESULT;

type FnD3D11On12CreateDevice = unsafe extern "system" fn(
    p_device: *mut core::ffi::c_void,
    flags: u32,
    p_feature_levels: *const D3D_FEATURE_LEVEL,
    feature_levels: u32,
    pp_command_queues: *mut *mut core::ffi::c_void,
    num_queues: u32,
    node_mask: u32,
    pp_device: *mut *mut core::ffi::c_void,
    pp_immediate_context: *mut *mut core::ffi::c_void,
    p_chosen_feature_level: *mut D3D_FEATURE_LEVEL,
) -> HRESULT;

// ---------------------------------------------------------------------------
// Real d3d11.dll handle
// ---------------------------------------------------------------------------

static REAL_D3D11: AtomicUsize = AtomicUsize::new(0);

unsafe fn real_d3d11() -> Option<HMODULE> {
    let raw = REAL_D3D11.load(Ordering::SeqCst);
    if raw != 0 { Some(HMODULE(raw as *mut core::ffi::c_void)) } else { None }
}

unsafe fn resolve<T: Copy>(name: &[u8]) -> Option<T> {
    let h = real_d3d11()?;
    let proc = GetProcAddress(h, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&proc))
}

unsafe fn resolve_from<T: Copy>(h: HMODULE, name: &[u8]) -> Option<T> {
    if h.0.is_null() { return None; }
    let proc = GetProcAddress(h, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&proc))
}

macro_rules! forward {
    ($name:expr, $fn_type:ty $(, $arg:expr)*) => {{
        static PROC: OnceLock<$fn_type> = OnceLock::new();
        let f = PROC.get_or_init(|| unsafe {
            resolve::<$fn_type>($name).unwrap_or_else(||
                panic!("d3d11on12 proxy: {} not found",
                    std::str::from_utf8($name).unwrap_or("?")))
        });
        f($($arg),*)
    }};
}

// ---------------------------------------------------------------------------
// Cached system function pointers
// ---------------------------------------------------------------------------

static D3D12_CREATE_DEVICE_FN: OnceLock<FnD3D12CreateDevice> = OnceLock::new();
static D3D11ON12_CREATE_DEVICE_FN: OnceLock<FnD3D11On12CreateDevice> = OnceLock::new();

unsafe fn get_d3d12_create_device() -> Option<FnD3D12CreateDevice> {
    Some(*D3D12_CREATE_DEVICE_FN.get_or_init(|| {
        let name: Vec<u16> = "C:\\Windows\\System32\\d3d12.dll\0"
            .encode_utf16().collect();
        let h = LoadLibraryW(windows::core::PCWSTR(name.as_ptr()))
            .unwrap_or(HMODULE(std::ptr::null_mut()));
        resolve_from::<FnD3D12CreateDevice>(h, b"D3D12CreateDevice\0")
            .expect("D3D12CreateDevice not found in d3d12.dll")
    }))
}

unsafe fn get_d3d11on12_create_device() -> Option<FnD3D11On12CreateDevice> {
    Some(*D3D11ON12_CREATE_DEVICE_FN.get_or_init(|| {
        resolve::<FnD3D11On12CreateDevice>(b"D3D11On12CreateDevice\0")
            .expect("D3D11On12CreateDevice not found in real d3d11.dll")
    }))
}

// ---------------------------------------------------------------------------
// RAII COM pointer wrapper
// ---------------------------------------------------------------------------
//
// DEVNOTE: Calls IUnknown::Release (vtable slot 2) on drop.
// Using this for all COM objects eliminates every leak from the original
// C++ code — early returns automatically release all live objects.

struct ComPtr<T>(*mut T);

impl<T> ComPtr<T> {
    fn null() -> Self { Self(std::ptr::null_mut()) }
    fn as_ptr(&self) -> *mut T { self.0 }
    fn as_void(&self) -> *mut core::ffi::c_void { self.0 as _ }
    fn is_null(&self) -> bool { self.0.is_null() }
    /// Transfer ownership out — caller is now responsible for Release.
    fn take(mut self) -> *mut T {
        let p = self.0;
        self.0 = std::ptr::null_mut();
        p
    }
}

impl<T> Drop for ComPtr<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                type ReleaseFn = unsafe extern "system" fn(*mut core::ffi::c_void) -> u32;
                let vtable = *(self.0 as *const *const usize);
                let release: ReleaseFn = std::mem::transmute_copy(&*vtable.add(2));
                release(self.0 as _);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Core D3D11on12 device creation
// ---------------------------------------------------------------------------

unsafe fn create_d3d11on12_device(
    p_adapter: *mut core::ffi::c_void,
    driver_type: D3D_DRIVER_TYPE,
    flags: u32,
    p_feature_levels: *const D3D_FEATURE_LEVEL,
    feature_levels: u32,
    pp_device: *mut *mut core::ffi::c_void,
    pp_immediate_context: *mut *mut core::ffi::c_void,
    p_chosen_feature_level: *mut D3D_FEATURE_LEVEL,
) -> HRESULT {
    let pfn_create_device = match get_d3d12_create_device() {
        Some(f) => f, None => return E_FAIL,
    };
    let pfn_d3d11on12 = match get_d3d11on12_create_device() {
        Some(f) => f, None => return E_FAIL,
    };

    // Resolve WARP adapter if requested.
    // For HARDWARE, we use the caller's adapter (may be null = default GPU).
    // DEVNOTE: For WARP we must NOT wrap p_adapter in ComPtr since we don't
    // own it. The WARP path creates a new adapter we DO own.
    let warp_adapter: ComPtr<IDXGIAdapter>;
    let adapter_ptr: *mut core::ffi::c_void;

    if driver_type == D3D_DRIVER_TYPE_WARP {
        // Get WARP adapter via IDXGIFactory4::EnumWarpAdapter (vtable slot 26).
        type FnCreateDXGIFactory2 = unsafe extern "system" fn(
            u32, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT;
        type FnEnumWarpAdapter = unsafe extern "system" fn(
            *mut core::ffi::c_void, *const GUID,
            *mut *mut core::ffi::c_void) -> HRESULT;

        let dxgi_name: Vec<u16> = "C:\\Windows\\System32\\dxgi.dll\0"
            .encode_utf16().collect();
        let dxgi_mod = LoadLibraryW(windows::core::PCWSTR(dxgi_name.as_ptr()))
            .unwrap_or(HMODULE(std::ptr::null_mut()));

        let cf: Option<FnCreateDXGIFactory2> =
            resolve_from(dxgi_mod, b"CreateDXGIFactory2\0");

        // IDXGIFactory4 IID: {1bc6ea02-ef36-464f-bf0c-21ca39e5168a}
        let iid_factory4 = GUID {
            data1: 0x1bc6ea02, data2: 0xef36, data3: 0x464f,
            data4: [0xbf, 0x0c, 0x21, 0xca, 0x39, 0xe5, 0x16, 0x8a],
        };

        let mut factory_raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut found_warp: *mut IDXGIAdapter = std::ptr::null_mut();

        if let Some(create_factory) = cf {
            let hr = create_factory(0, &iid_factory4, &mut factory_raw);
            if hr.is_ok() && !factory_raw.is_null() {
                let factory = ComPtr::<core::ffi::c_void>(factory_raw);
                let vtable = *(factory.as_ptr() as *const *const usize);
                let enum_warp: FnEnumWarpAdapter =
                    std::mem::transmute_copy(&*vtable.add(26));

                // IDXGIAdapter IID: {2411e7e1-12ac-4ccf-bd14-9798e8534dc0}
                let iid_adapter = GUID {
                    data1: 0x2411e7e1, data2: 0x12ac, data3: 0x4ccf,
                    data4: [0xbd, 0x14, 0x97, 0x98, 0xe8, 0x53, 0x4d, 0xc0],
                };
                let _ = enum_warp(
                    factory.as_ptr(),
                    &iid_adapter,
                    &mut found_warp as *mut _ as _,
                );
                // factory drops here, releasing the IDXGIFactory4 ref
            }
        }

        warp_adapter = ComPtr(found_warp);
        adapter_ptr = warp_adapter.as_void();
    } else {
        // HARDWARE — use caller's adapter, don't wrap (we don't own it)
        warp_adapter = ComPtr::null();
        std::mem::forget(warp_adapter); // prevent drop of null
        adapter_ptr = p_adapter;
    }

    // ID3D12Device IID: {189819f1-1db6-4b57-be54-1821339b85f7}
    let iid_d3d12device = GUID {
        data1: 0x189819f1, data2: 0x1db6, data3: 0x4b57,
        data4: [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
    };

    // Create D3D12 device.
    let mut d3d12_device_raw: *mut ID3D12Device = std::ptr::null_mut();
    let hr = pfn_create_device(
        adapter_ptr,
        D3D_FEATURE_LEVEL_11_0,
        &iid_d3d12device,
        &mut d3d12_device_raw as *mut _ as _,
    );
    if hr.is_err() { return hr; }
    // DEVNOTE: d3d12_device wrapped in ComPtr — releases on any early return.
    let d3d12_device = ComPtr(d3d12_device_raw);

    // Create D3D12 command queue via raw vtable call.
    // DEVNOTE: Zero-initialized desc fixes the original C++ uninitialized bug.
    let desc = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
        Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: 0,
    };

    // ID3D12CommandQueue IID: {0ec870a6-5d7e-4c22-8cfc-5baae07616ed}
    let iid_queue = GUID {
        data1: 0x0ec870a6, data2: 0x5d7e, data3: 0x4c22,
        data4: [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    };

    // CreateCommandQueue is vtable slot 8 on ID3D12Device.
    let vtable = *(d3d12_device.as_ptr() as *const *const usize);
    let create_queue: FnD3D12CreateCommandQueue =
        std::mem::transmute_copy(&*vtable.add(8));

    let mut d3d12_queue_raw: *mut ID3D12CommandQueue = std::ptr::null_mut();
    let hr = create_queue(
        d3d12_device.as_void(),
        &desc,
        &iid_queue,
        &mut d3d12_queue_raw as *mut _ as _,
    );
    // DEVNOTE: d3d12_device in scope — drops and releases if we return here.
    if hr.is_err() { return hr; }
    // DEVNOTE: d3d12_queue wrapped — releases on drop.
    // After D3D11On12CreateDevice the on12 layer holds its own ref.
    let d3d12_queue = ComPtr(d3d12_queue_raw);

    // Call D3D11On12CreateDevice.
    let mut queue_void = d3d12_queue.as_void();
    let hr = pfn_d3d11on12(
        d3d12_device.as_void(),
        flags,
        p_feature_levels,
        feature_levels,
        &mut queue_void,
        1,
        0,
        pp_device,
        pp_immediate_context,
        p_chosen_feature_level,
    );

    // d3d12_queue and d3d12_device both drop here.
    // D3D11on12 holds its own refs so the underlying objects stay alive.
    hr
}

// ---------------------------------------------------------------------------
// Exported: D3D11CreateDevice
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn D3D11CreateDevice(
    p_adapter: *mut core::ffi::c_void,
    driver_type: D3D_DRIVER_TYPE,
    _software: HMODULE,
    flags: u32,
    p_feature_levels: *const D3D_FEATURE_LEVEL,
    feature_levels: u32,
    _sdk_version: u32,
    pp_device: *mut *mut core::ffi::c_void,
    p_feature_level: *mut D3D_FEATURE_LEVEL,
    pp_immediate_context: *mut *mut core::ffi::c_void,
) -> HRESULT {
    create_d3d11on12_device(
        p_adapter,
        driver_type,
        flags,
        p_feature_levels,
        feature_levels,
        pp_device,
        pp_immediate_context,
        p_feature_level,
    )
}

// ---------------------------------------------------------------------------
// Exported: D3D11CreateDeviceAndSwapChain
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn D3D11CreateDeviceAndSwapChain(
    p_adapter: *mut core::ffi::c_void,
    driver_type: D3D_DRIVER_TYPE,
    _software: HMODULE,
    flags: u32,
    p_feature_levels: *const D3D_FEATURE_LEVEL,
    feature_levels: u32,
    _sdk_version: u32,
    p_swap_chain_desc: *const DXGI_SWAP_CHAIN_DESC,
    pp_swap_chain: *mut *mut core::ffi::c_void,
    pp_device: *mut *mut core::ffi::c_void,
    p_feature_level: *mut D3D_FEATURE_LEVEL,
    pp_immediate_context: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if !pp_swap_chain.is_null() && p_swap_chain_desc.is_null() {
        return E_INVALIDARG;
    }

    let mut raw_device: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut raw_context: *mut core::ffi::c_void = std::ptr::null_mut();

    let hr = create_d3d11on12_device(
        p_adapter, driver_type, flags,
        p_feature_levels, feature_levels,
        &mut raw_device, &mut raw_context,
        p_feature_level,
    );
    if hr.is_err() { return hr; }

    // Wrap immediately — any early return below releases both.
    let d3d11_device  = ComPtr(raw_device);
    let d3d11_context = ComPtr(raw_context);

    if !pp_swap_chain.is_null() {
        let desc = *p_swap_chain_desc;

        // QueryInterface for IDXGIDevice (vtable slot 0).
        type QIFn = unsafe extern "system" fn(
            *mut core::ffi::c_void, *const GUID,
            *mut *mut core::ffi::c_void) -> HRESULT;
        let qi: QIFn = std::mem::transmute_copy(
            &**(d3d11_device.as_ptr() as *const *const usize));

        // IDXGIDevice IID: {54ec77fa-1377-44e6-8c32-88fd5f44c84c}
        let iid_dxgi_device = GUID {
            data1: 0x54ec77fa, data2: 0x1377, data3: 0x44e6,
            data4: [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c],
        };
        let mut dxgi_device_raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = qi(d3d11_device.as_ptr(), &iid_dxgi_device, &mut dxgi_device_raw);
        // DEVNOTE: d3d11_device + d3d11_context drop on error — no leak.
        if hr.is_err() { return E_INVALIDARG; }
        let dxgi_device = ComPtr::<core::ffi::c_void>(dxgi_device_raw);

        // GetParent (IDXGIObject vtable slot 6) → IDXGIAdapter
        type GetParentFn = unsafe extern "system" fn(
            *mut core::ffi::c_void, *const GUID,
            *mut *mut core::ffi::c_void) -> HRESULT;
        let get_parent: GetParentFn = std::mem::transmute_copy(
            &*(*(dxgi_device.as_ptr() as *const *const usize)).add(6));

        // IDXGIAdapter IID: {2411e7e1-12ac-4ccf-bd14-9798e8534dc0}
        let iid_adapter = GUID {
            data1: 0x2411e7e1, data2: 0x12ac, data3: 0x4ccf,
            data4: [0xbd, 0x14, 0x97, 0x98, 0xe8, 0x53, 0x4d, 0xc0],
        };
        let mut adapter_raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = get_parent(dxgi_device.as_ptr(), &iid_adapter, &mut adapter_raw);
        if hr.is_err() { return E_INVALIDARG; }
        let dxgi_adapter = ComPtr::<core::ffi::c_void>(adapter_raw);

        // GetParent → IDXGIFactory
        let get_parent2: GetParentFn = std::mem::transmute_copy(
            &*(*(dxgi_adapter.as_ptr() as *const *const usize)).add(6));

        // IDXGIFactory IID: {7b7166ec-21c7-44ae-b21a-c9ae321ae369}
        let iid_factory = GUID {
            data1: 0x7b7166ec, data2: 0x21c7, data3: 0x44ae,
            data4: [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69],
        };
        let mut factory_raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = get_parent2(dxgi_adapter.as_ptr(), &iid_factory, &mut factory_raw);
        if hr.is_err() { return E_INVALIDARG; }
        let dxgi_factory = ComPtr::<core::ffi::c_void>(factory_raw);

        // IDXGIFactory::CreateSwapChain — vtable slot 10.
        type CreateSCFn = unsafe extern "system" fn(
            *mut core::ffi::c_void,
            *mut core::ffi::c_void,
            *mut DXGI_SWAP_CHAIN_DESC,
            *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        let create_sc: CreateSCFn = std::mem::transmute_copy(
            &*(*(dxgi_factory.as_ptr() as *const *const usize)).add(10));

        let mut sc_desc = desc;
        let hr = create_sc(
            dxgi_factory.as_ptr(),
            d3d11_device.as_ptr(),
            &mut sc_desc,
            pp_swap_chain,
        );
        // All DXGI wrappers drop here.
        if hr.is_err() { return hr; }
    }

    // Hand ownership to caller — take() prevents drop from releasing.
    if !pp_device.is_null() {
        *pp_device = d3d11_device.take();
    }
    if !pp_immediate_context.is_null() {
        *pp_immediate_context = d3d11_context.take();
    }

    S_OK
}

// ---------------------------------------------------------------------------
// Forwarded exports
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn D3D11CoreCreateDevice(
    p_factory: *mut core::ffi::c_void,
    p_adapter: *mut core::ffi::c_void,
    flags: u32,
    p_feature_levels: *const D3D_FEATURE_LEVEL,
    feature_levels: u32,
    pp_device: *mut *mut core::ffi::c_void,
) -> HRESULT {
    forward!(
        b"D3D11CoreCreateDevice\0",
        unsafe extern "system" fn(
            *mut core::ffi::c_void, *mut core::ffi::c_void, u32,
            *const D3D_FEATURE_LEVEL, u32,
            *mut *mut core::ffi::c_void) -> HRESULT,
        p_factory, p_adapter, flags, p_feature_levels, feature_levels, pp_device
    )
}

#[no_mangle]
pub unsafe extern "system" fn D3D11CreateDeviceForD3D12(
    p_device: *mut core::ffi::c_void,
    flags: u32,
    p_feature_levels: *const D3D_FEATURE_LEVEL,
    feature_levels: u32,
    pp_command_queues: *mut *mut core::ffi::c_void,
    num_queues: u32,
    node_mask: u32,
    pp_device: *mut *mut core::ffi::c_void,
    pp_immediate_context: *mut *mut core::ffi::c_void,
    p_chosen_feature_level: *mut D3D_FEATURE_LEVEL,
) -> HRESULT {
    forward!(
        b"D3D11CreateDeviceForD3D12\0",
        unsafe extern "system" fn(
            *mut core::ffi::c_void, u32, *const D3D_FEATURE_LEVEL, u32,
            *mut *mut core::ffi::c_void, u32, u32,
            *mut *mut core::ffi::c_void,
            *mut *mut core::ffi::c_void,
            *mut D3D_FEATURE_LEVEL) -> HRESULT,
        p_device, flags, p_feature_levels, feature_levels,
        pp_command_queues, num_queues, node_mask,
        pp_device, pp_immediate_context, p_chosen_feature_level
    )
}

#[no_mangle]
pub unsafe extern "system" fn D3D11On12CreateDevice(
    p_device: *mut core::ffi::c_void,
    flags: u32,
    p_feature_levels: *const D3D_FEATURE_LEVEL,
    feature_levels: u32,
    pp_command_queues: *mut *mut core::ffi::c_void,
    num_queues: u32,
    node_mask: u32,
    pp_device: *mut *mut core::ffi::c_void,
    pp_immediate_context: *mut *mut core::ffi::c_void,
    p_chosen_feature_level: *mut D3D_FEATURE_LEVEL,
) -> HRESULT {
    forward!(
        b"D3D11On12CreateDevice\0",
        unsafe extern "system" fn(
            *mut core::ffi::c_void, u32, *const D3D_FEATURE_LEVEL, u32,
            *mut *mut core::ffi::c_void, u32, u32,
            *mut *mut core::ffi::c_void,
            *mut *mut core::ffi::c_void,
            *mut D3D_FEATURE_LEVEL) -> HRESULT,
        p_device, flags, p_feature_levels, feature_levels,
        pp_command_queues, num_queues, node_mask,
        pp_device, pp_immediate_context, p_chosen_feature_level
    )
}

// ---------------------------------------------------------------------------
// DllMain
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst: HMODULE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        let mut buf = [0u16; 512];
        let len = GetSystemDirectoryW(Some(&mut buf)) as usize;
        if len == 0 { return BOOL(0); }

        let suffix: Vec<u16> = "\\d3d11.dll\0".encode_utf16().collect();
        if len + suffix.len() > buf.len() { return BOOL(0); }
        buf[len..len + suffix.len()].copy_from_slice(&suffix);

        let h = match LoadLibraryW(windows::core::PCWSTR(buf.as_ptr())) {
            Ok(h) => h,
            Err(_) => return BOOL(0),
        };

        REAL_D3D11.store(h.0 as usize, Ordering::SeqCst);
    }
    BOOL(1)
}
