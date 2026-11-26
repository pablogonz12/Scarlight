//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned: `present`
//! (swap chain).

#[cfg(windows)]
mod ambient;
#[cfg(any(windows, test))]
mod bindcache;
#[cfg(any(windows, test))]
mod bound;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
pub mod console;
// Byte compares (`VICTORY_AVX2_COMPARE`): used by the Windows paths'
// copy-back (`shadow`); the walks and their tests run anywhere.
#[cfg_attr(not(windows), allow(dead_code))]
mod compare;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
#[cfg(any(windows, test))]
mod declareonce;
// The arena is the layer a resident stands on (#980 Stage A); no resident
// places anything yet, so only the tests exercise it (`notes/arena-lane.md`).
#[allow(dead_code)]
#[cfg(windows)]
mod arena;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
#[cfg(windows)]
mod dxc;
pub mod fence;
pub mod frame;
#[cfg(any(windows, test))]
mod frame_budget;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod inplace;
pub mod list;
pub mod object;
pub mod pipeline;
#[cfg(windows)]
#[cfg(windows)]
mod placed;
mod primitive;
pub mod query;
pub mod queue;
mod reflection;
pub mod resource;
#[cfg(any(windows, test))]
mod scratchfill;
pub mod root;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
mod switches;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod shader_meta;
#[cfg(windows)]
mod shadow;
pub mod signature;
#[cfg(all(windows, test))]
mod test_device;
#[cfg(windows)]
mod texback;
#[cfg(windows)]
mod texture;
#[cfg(windows)]
mod upload_pool;
#[cfg(all(test, windows))]
mod warptest;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod writeback;

#[cfg(windows)]
pub use host::on_failure;
#[cfg(windows)]
pub use arena::{title_memory_committed, title_memory_released};
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_view};
#[cfg(windows)]
pub use watch::{
    title_memory_reading, title_memory_remapped, title_memory_remapping, writing as kernel_will_write,
};

/// Total installed physical RAM in bytes, queried once from the OS.
#[cfg(windows)]
pub(crate) fn total_system_ram_bytes() -> Option<u64> {
    static TOTAL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *TOTAL.get_or_init(|| {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: plain data the call fills in.
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: a sized structure.
        let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
        (ok && m.ullTotalPhys > 0).then_some(m.ullTotalPhys)
    })
}
