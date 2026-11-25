//! Synthetic PresentX planes check what the compositor writes to the host
//! swap chain before a flip can discard the back buffer.

use std::cell::Cell;

use windows::Win32::Foundation::E_FAIL;

use super::*;
use crate::present_compositor::PresentCompositor;

const BASE_SIDE: u32 = 16;
const OVERLAY_SIDE: u32 = 32;
const FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;
const SOURCE_STATE: u32 = D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32;
const RED: [u8; 4] = [255, 0, 0, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];

fn log_adapter(host: &Host) {
    // SAFETY: plain queries for the adapter that created this live device.
    let (luid, desc) = unsafe {
        let luid = host.device.GetAdapterLuid();
        let desc = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter1>(luid))
            .and_then(|adapter| adapter.GetDesc1());
        (luid, desc)
    };
    match desc {
        Ok(desc) => eprintln!(
            "PresentX GPU adapter: {} (vendor {:#06x}, device {:#06x})",
            adapter_name(&desc.Description),
            desc.VendorId,
            desc.DeviceId
        ),
        Err(error) => eprintln!(
            "PresentX GPU adapter LUID {:08x}:{:08x} (DXGI description unavailable: {error})",
            luid.HighPart, luid.LowPart
        ),
    }
}

fn clear_plane(host: &Host, list: &HostList, side: u32, rgba: [u8; 4]) -> ID3D12Resource {
    let mut texture = None;
    // SAFETY: the device owns an open command list and RTV heap. The resource
    // is created as a render target and stays alive through the submission.
    unsafe {
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: u64::from(side),
                    Height: side,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: FORMAT,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                None,
                &mut texture,
            )
            .expect("create synthetic present plane");
        let texture = texture.expect("present plane resource");
        list.list.ClearRenderTargetView(
            host.rtv(Some(&texture), None),
            &rgba.map(|channel| f32::from(channel) / 255.0),
            None,
        );
        texture
    }
}

fn assert_pixels(
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    (side, expected): (u32, [u8; 4]),
    case: &str,
) {
    assert_eq!(footprint.Footprint.Format, FORMAT, "{case}");
    assert_eq!(footprint.Footprint.Width, side, "{case}");
    assert_eq!(footprint.Footprint.Height, side, "{case}");
    let offset = footprint.Offset as usize;
    let pitch = footprint.Footprint.RowPitch as usize;
    let width = side as usize;
    let mut data = std::ptr::null_mut();
    // SAFETY: the present submission waited for the GPU. The footprint names
    // the copied extent and each row exposes four bytes per RGBA8 pixel.
    let mismatch = unsafe {
        buffer
            .Map(0, None, Some(&mut data))
            .expect("map completed PresentX readback");
        let data = data.cast::<u8>().add(offset);
        let mismatch = (0..side as usize).find_map(|y| {
            let row = std::slice::from_raw_parts(data.add(y * pitch), width * 4);
            row.as_chunks::<4>()
                .0
                .iter()
                .enumerate()
                .find(|(_, pixel)| **pixel != expected)
                .map(|(x, &pixel)| (x, y, pixel))
        });
        buffer.Unmap(0, None);
        mismatch
    };
    assert_eq!(
        mismatch, None,
        "{case}: first (x, y, pixel), expected {expected:?}"
    );
}

fn check_present(
    host: &Host,
    primary: &ID3D12Resource,
    overlay: &ID3D12Resource,
    frame: u64,
    create: impl Fn(&ID3D12Device, DXGI_FORMAT) -> windows::core::Result<PresentCompositor>,
    expected: (u32, [u8; 4]),
    case: &str,
) {
    let mut inspected = false;
    let result = host.present_planes(
        std::ptr::null_mut(),
        primary,
        SOURCE_STATE,
        Some((overlay, SOURCE_STATE)),
        frame,
        false,
        create,
        |buffer, footprint| {
            assert_pixels(buffer, footprint, expected, case);
            inspected = true;
        },
    );
    assert!(inspected, "PRESENTX_GPU_NO_READBACK: {case}");
    assert_eq!(result, Some(false), "{case}: present a nonblack frame");
}

// Adapted from the upstream PresentX recovery and pre-flip readback tests.
#[test]
fn present_overlay_composites_pixels_and_recovers_from_initialization_failure() {
    let Some(host) = host() else {
        eprintln!("PRESENTX_GPU_SKIP: no host D3D12 device");
        return;
    };
    log_adapter(host);

    let list = host.new_list().expect("clear list");
    let red = clear_plane(host, &list, BASE_SIDE, RED);
    let black = clear_plane(host, &list, BASE_SIDE, BLACK);
    let green = clear_plane(host, &list, OVERLAY_SIDE, GREEN);
    let half_blue = clear_plane(host, &list, OVERLAY_SIDE, [0, 0, 128, 128]);
    let transparent = clear_plane(host, &list, OVERLAY_SIDE, [0, 0, 0, 0]);
    list.close();
    host.execute(&[&list]);

    assert_eq!(
        host.present(std::ptr::null_mut(), &red, SOURCE_STATE, 1, false),
        Some(false),
        "PRESENTX_GPU_NO_SINGLE_PLANE_FRAME: initial copy-only presentation"
    );
    let attempted = Cell::new(false);
    check_present(
        host,
        &red,
        &half_blue,
        2,
        |_, _| {
            attempted.set(true);
            Err(windows::core::Error::from_hresult(E_FAIL))
        },
        (BASE_SIDE, RED),
        "compositor initialization failure presents the primary plane",
    );
    assert!(attempted.get(), "the compositor factory was called");
    {
        let mut retry = host.present_compositor_retry.lock().unwrap();
        assert_eq!(retry.attempts, 1);
        retry.next = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
    }
    check_present(
        host,
        &red,
        &half_blue,
        3,
        |_, _| panic!("retry must wait for its backoff"),
        (BASE_SIDE, RED),
        "during backoff the base plane still presents",
    );
    assert_eq!(
        host.present(std::ptr::null_mut(), &red, SOURCE_STATE, 4, false),
        Some(false),
        "copy-only presentation after a compositor initialization failure"
    );

    host.present_compositor_retry.lock().unwrap().next = None;
    for (primary, overlay, frame, expected, case) in [
        (
            &red,
            &half_blue,
            5,
            [127, 0, 128, 255],
            "retry blends premultiplied half-blue over red",
        ),
        (
            &black,
            &green,
            6,
            GREEN,
            "opaque green over a black primary",
        ),
        (
            &red,
            &transparent,
            7,
            RED,
            "transparent overlay preserves red",
        ),
    ] {
        check_present(
            host,
            primary,
            overlay,
            frame,
            |device, format| {
                PresentCompositor::new(device, format).inspect_err(|error| {
                    eprintln!("PRESENTX_GPU_COMPOSITOR_INIT_ERROR: {error:?}");
                })
            },
            (OVERLAY_SIDE, expected),
            case,
        );
    }
    check_present(
        host,
        &red,
        &red,
        8,
        |_, _| panic!("identical resources must use the copy path"),
        (BASE_SIDE, RED),
        "duplicate source falls back to single-plane extent",
    );
    assert_eq!(host.present_compositor_retry.lock().unwrap().attempts, 0);
    eprintln!("PRESENTX_GPU_READBACK_OK: fallback, half-alpha, opaque and transparent");
}
