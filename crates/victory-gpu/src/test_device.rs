//! Which adapter the GPU tests run on: `VICTORY_TEST_ADAPTER=warp|hw|auto`.
//!
//! The tests used to ask DXGI for WARP (`EnumWarpAdapter`) and return when it
//! was not there. Under Wine and vkd3d-proton there is no WARP, so every one
//! of them returned at once and counted as passed: only CI's Windows runner
//! (where WARP exists) exercised them. `auto`, the default, keeps that run
//! unchanged (WARP when DXGI has it) and falls back to the first hardware
//! adapter everywhere else, which is the real GPU through vkd3d-proton.
//! `warp` and `hw` pin one adapter, for a run that asks "what does the
//! software one say, what does the card say".
//!
//! A test with no device prints one line, `skipped: no device: ...`, so a run
//! shows how many skipped instead of a silent pass.

use std::sync::Once;

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL;
use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device};
use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, CreateDXGIFactory2, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_FLAGS,
    IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIFactory4,
};

/// Which adapter `VICTORY_TEST_ADAPTER` names; anything else is `auto`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Want {
    Warp,
    Hw,
    Auto,
}

fn want() -> Want {
    match std::env::var("VICTORY_TEST_ADAPTER").as_deref() {
        Ok("warp") => Want::Warp,
        Ok("hw") => Want::Hw,
        Ok("auto") | Err(_) => Want::Auto,
        Ok(other) => {
            eprintln!("VICTORY_TEST_ADAPTER={other} is not warp, hw or auto: using auto");
            Want::Auto
        }
    }
}

/// Whether `adapter` is the software one DXGI flags as such.
fn software(adapter: &IDXGIAdapter) -> Option<bool> {
    // SAFETY: `adapter` owns an IDXGIAdapter1 interface.
    let desc = unsafe { adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()? };
    Some(desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0)
}

/// WARP, if DXGI presents one as software. Under Proton `EnumWarpAdapter`
/// hands back the hardware adapter, which a WARP-only test must not take
/// (`host::warp_adapter` refuses it for the same reason).
fn warp() -> Option<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
        software(&adapter).filter(|&is| is).map(|_| adapter)
    }
}

/// The first adapter that is not software: the card, under Windows or Wine.
fn hardware() -> Option<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            if software(&adapter) == Some(false) {
                return Some(adapter.into());
            }
            i += 1;
        }
        None
    }
}

/// The vendor and device of the adapter a device runs on, for the one line a
/// run prints (`None` when the device cannot name its adapter).
fn named(device: &ID3D12Device) -> Option<(u32, u32)> {
    // SAFETY: plain DXGI queries on the device's own adapter.
    unsafe {
        let dxgi = device.cast::<IDXGIDevice>().ok()?;
        let desc = dxgi.GetAdapter().ok()?.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
        Some((desc.VendorId, desc.DeviceId))
    }
}

/// Whether `device` runs on the software adapter (WARP): there the runtime's
/// and the kernel's rules are all there is, and a test that pins what a card's
/// driver adds says so.
pub(crate) fn is_software(device: &ID3D12Device) -> bool {
    // SAFETY: plain DXGI queries on the device's own adapter.
    unsafe {
        let Ok(dxgi) = device.cast::<IDXGIDevice>() else { return false };
        let Ok(adapter) = dxgi.GetAdapter() else { return false };
        software(&adapter) == Some(true)
    }
}

/// A D3D12 device of `level` on the adapter the run asks for, if any.
///
/// `auto` is WARP when DXGI has it and the first hardware adapter otherwise.
/// A hardware device that DXGI will not make from the adapter itself falls
/// back to the default device, which is that same card (and is the only way
/// under Wine, where DXGI lists no adapter of its own); a WARP run never falls
/// back to the card.
pub(crate) fn device(level: D3D_FEATURE_LEVEL) -> Option<ID3D12Device> {
    let (name, adapter) = match want() {
        Want::Warp => ("warp", warp()),
        Want::Hw => ("hw", hardware()),
        Want::Auto => match warp() {
            Some(a) => ("warp", Some(a)),
            None => ("hw", hardware()),
        },
    };
    // SAFETY: the out pointer is ours; `adapter`, if any, is a DXGI adapter.
    let made = |adapter: Option<&IDXGIAdapter>, out: &mut Option<ID3D12Device>| unsafe {
        match adapter {
            Some(a) => D3D12CreateDevice(a, level, out),
            None => D3D12CreateDevice(None, level, out),
        }
    };
    let mut device: Option<ID3D12Device> = None;
    // A WARP run never falls back to the card: no software adapter, no device.
    let mut error = if name == "warp" && adapter.is_none() {
        Some(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
    } else {
        made(adapter.as_ref(), &mut device).err()
    };
    if device.is_none() && name == "hw" {
        // DXGI would not make the device from its own adapter: the default
        // device is that same card under Wine.
        error = made(None, &mut device).err();
    }
    if device.is_none() {
        eprintln!(
            "skipped: no device: no D3D12 device on the {name} adapter (VICTORY_TEST_ADAPTER): {}",
            error.map_or_else(|| "no error".to_string(), |e| format!("{e}"))
        );
        return None;
    }
    let device = device?;
    static SAID: Once = Once::new();
    SAID.call_once(|| match (name, named(&device)) {
        (name, Some((vendor, device_id))) => {
            eprintln!("test device: {name}, vendor={vendor:#06x} device={device_id:#06x}")
        }
        (name, None) => eprintln!("test device: {name}"),
    });
    Some(device)
}
