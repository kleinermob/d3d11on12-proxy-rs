# d3d11on12-proxy-rs

A drop-in `d3d11.dll` replacement written in Rust that transparently forces **D3D11on12** for any Direct3D 11 application.

## What it does

This DLL intercepts `D3D11CreateDevice` and `D3D11CreateDeviceAndSwapChain` and redirects them through `D3D11On12CreateDevice`, so all D3D11 rendering is backed by a D3D12 device internally. This gives the application a proper DXGI swap chain, enabling DXGI-level hooks such as FPS overlays and vsync-off proxies to work on top of D3D11 applications.

All other `d3d11.dll` exports are forwarded transparently to the real system DLL.

## Fixes vs the original C++ implementation 

1. `d3d12device` leaked when `CreateCommandQueue` fails — fixed via RAII
2. `d3d12device` leaked when `D3D11On12CreateDevice` not found — fixed via RAII
3. `d3d12queue` never released after `D3D11On12CreateDevice` call — fixed via RAII
4. `d3d11Device` + `d3d11Context` leaked on early returns in `D3D11CreateDeviceAndSwapChain` — fixed via RAII `ComPtr` wrappers
5. `getPfnD3D11On12CreateDevice` race condition — fixed via `OnceLock`
6. `D3D12_COMMAND_QUEUE_DESC` not zero-initialized — fixed
7. `DriverType = WARP` silently ignored — now handled
8. `D3D12CreateDevice` / `CreateCommandQueue` called with wrong number of args for `windows` crate 0.58 API — fixed by loading raw fn pointers

## Requirements

- Windows 10/11 (D3D11on12 is built-in)
- Rust + `cargo`
- MSVC toolchain

```bash
rustup target add x86_64-pc-windows-msvc
rustup target add i686-pc-windows-msvc
```

## Building

```bash
# 64-bit (recommended — nearly all D3D11 apps are 64-bit)
cargo build --target x86_64-pc-windows-msvc --release

# 32-bit
cargo build --target i686-pc-windows-msvc --release
```

Output:
```
target/x86_64-pc-windows-msvc/release/d3d11.dll    ← 64-bit
target/i686-pc-windows-msvc/release/d3d11.dll      ← 32-bit
```

## Usage

Place the compiled `d3d11.dll` next to the application's `.exe`. The proxy loads automatically on startup and redirects D3D11 calls through D3D11on12.

> ⚠️ Use the correct bitness — 64-bit DLL for 64-bit apps, 32-bit for 32-bit apps.

## How it works

```
App
 └─ D3D11CreateDevice()
      └─ [this proxy]
           ├─ D3D12CreateDevice()         ← creates D3D12 device
           ├─ CreateCommandQueue()        ← creates D3D12 command queue
           └─ D3D11On12CreateDevice()     ← wraps D3D12 device as D3D11
                └─ D3D12 device + DXGI swap chain
```

All COM objects are managed via a RAII `ComPtr` wrapper, eliminating every resource leak present in the original C++ implementation.

## Project structure

```
.cargo/config.toml   — linker flags for both targets
src/lib.rs           — proxy implementation
d3d11.def            — export ordinals (must match system d3d11.dll)
Cargo.toml           — crate config (cdylib)
```

## License

MIT
