//! Textures shaders write that the title's own code then reads on the CPU.
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read. Here it stays
//! in the texture's host copy. The title's ocean is one: its wave passes write
//! a 256x256 height map (R32_FLOAT) and a displacement map (R32G32_FLOAT), and
//! its CPU code samples both, which is what boats float on; it read memory
//! nothing had written (run rw1, `VICTORY_READ_WATCH=1`: about 30 places in
//! the title read those two kinds), and boats sank.
//!
//! For textures of the kinds in `VICTORY_TEXTURE_BACK` (comma-separated
//! `<width>x<height>f<format>`; `0` for none; unset or `1`: the four water
//! formats found by the upstream trace), a list copies the mip and slices
//! exposed by each bound texture UAV into a read-back buffer when it closes
//! (fxceless #1152), and restores the texture's final recorded state afterward
//! (fxceless #1170). Once the list has run, those texels are tiled into the
//! texture's title memory in the console's layout without overwriting
//! untouched mips or array slices.
//!
//! shortcut: by kind, not by texture: textures of those kinds the title never
//! reads are copied back too (about a megabyte per such list). The copy shows
//! what the list's last write left; a title read in between sees the frame
//! before, as it would before the console's GPU has run.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// The kinds copied back: (width, height, format).
fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";
        let list = match std::env::var("VICTORY_TEXTURE_BACK").as_deref() {
            // On unless told otherwise, as on this line since vx-layer eee5667:
            // without it boats sink (the title's CPU reads the ocean's height map).
            Ok("1") | Err(_) => DEFAULT.to_owned(),
            Ok("0") => String::new(),
            Ok(spec) => spec.to_owned(),
        };
        let kinds: Vec<(u64, u32, u32)> = list
            .split(',')
            .filter_map(|k| {
                let (size, format) = k.trim().split_once('f')?;
                let (w, h) = size.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
            })
            .collect();
        if !kinds.is_empty() {
            tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (VICTORY_TEXTURE_BACK)");
        }
        kinds
    })
}

/// The title format when `resource` is of a kind copied back.
pub(crate) fn wanted_format(resource: This) -> Option<u32> {
    let kinds = kinds();
    if kinds.is_empty() {
        return None;
    }
    let kind = crate::resource::texture_kind(resource)?;
    kinds.contains(&kind).then_some(kind.2)
}

static TILE_WARNING: std::sync::Once = std::sync::Once::new();

/// Whether the copy takes only the mip and slices the list's texture UAVs
/// expose (fxceless #1152), and keeps the texels it did not take as they were.
/// `VICTORY_TEXTURE_BACK_UNION=0` (default): every mip and slice of the
/// texture, as the copy did before, whatever the views say.
pub(crate) fn union() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var_os("VICTORY_TEXTURE_BACK_UNION") {
        None => false,
        Some(v) => !v.is_empty() && v != "0",
    })
}

/// An outstanding write to a specific allocation. Mapping and resource
/// changes invalidate only the writes that could touch the replaced memory.
struct Ticket {
    owner: usize,
    at: usize,
    end: usize,
    valid: AtomicBool,
}

#[derive(Default)]
struct Landed {
    copies: u64,
    bindings: u64,
    restricted_bindings: u64,
    copied_subresources: u64,
    skipped_subresources: u64,
}

#[derive(Default)]
struct Tracking {
    tickets: Vec<Weak<Ticket>>,
    landed: Landed,
}

static TRACKED: LazyLock<Mutex<Tracking>> = LazyLock::new(Mutex::default);
static TRACKING: AtomicBool = AtomicBool::new(false);

fn track(owner: usize, at: usize, end: usize) -> Arc<Ticket> {
    let ticket = Arc::new(Ticket { owner, at, end, valid: AtomicBool::new(true) });
    let mut tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    tracked.tickets.retain(|weak| weak.strong_count() != 0);
    tracked.tickets.push(Arc::downgrade(&ticket));
    TRACKING.store(true, Ordering::Relaxed);
    ticket
}

fn invalidate(matches: impl Fn(&Ticket) -> bool) {
    if !TRACKING.load(Ordering::Relaxed) {
        return;
    }
    TRACKED.lock().unwrap_or_else(|e| e.into_inner()).tickets.retain(|weak| {
        let Some(ticket) = weak.upgrade() else { return false };
        if matches(&ticket) {
            ticket.valid.store(false, Ordering::Release);
        }
        true
    });
}

pub(crate) fn released(owner: usize) {
    invalidate(|ticket| ticket.owner == owner);
}

pub(crate) fn replaced(at: usize) {
    invalidate(|ticket| ticket.at == at);
}

pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    invalidate(|ticket| at < ticket.end && ticket.at < end);
}

pub(crate) struct Target {
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    layout: victory_formats::texture::Layout,
    format: u32,
    ticket: Arc<Ticket>,
    subresources: BTreeSet<u32>,
    bindings: u64,
    restricted_bindings: u64,
}

impl Target {
    pub(crate) fn new(resource: This, format: u32, view: Option<&[u32; 10]>) -> Option<Self> {
        // Keep the host copy independently of the title's resource lifetime.
        let copy = crate::resource::host_copy(resource)?;
        // Serialize the reading of the title allocation and ticket
        // registration against concurrent remaps on `[at, end)`: a remap
        // after the observation but before `track` must mark the ticket
        // invalid, which it cannot do for one made later.
        // Lock order: READING -> TRACKED (matches REMAPPING -> TRACKED).
        let _mapping = crate::watch::title_memory_reading();
        let (layout, at, size) = crate::resource::title_texture(resource)?;
        let end = at.checked_add(size).filter(|&end| at != 0 && end > at)?;
        let ticket = track(resource as usize, at, end);
        let mut target = Self {
            copy,
            layout,
            format,
            ticket,
            subresources: BTreeSet::new(),
            bindings: 0,
            restricted_bindings: 0,
        };
        target.include(view);
        (!target.subresources.is_empty()).then_some(target)
    }

    pub(crate) fn owner(&self) -> usize {
        self.ticket.owner
    }

    /// Whether the title layout can accept any subresource after a GPU copy.
    pub(crate) fn can_tile(&self) -> bool {
        if self.layout.addressable() {
            return true;
        }
        TILE_WARNING.call_once(|| {
            let first = self.subresources.first().copied();
            let mips = self.layout.desc.mips;
            tracing::warn!(
                format = self.format,
                layout = ?self.layout.desc,
                first_mip = first.map(|i| i % mips),
                first_slice = first.map(|i| i / mips),
                subresources = self.subresources.len(),
                "skipping texture readback because the title layout has no address equation"
            );
        });
        false
    }

    /// The subresources this copy takes: every mip of every slice of the
    /// texture without the union, else what [`written`] says.
    pub(crate) fn include(&mut self, words: Option<&[u32; 10]>) {
        let mips = self.layout.desc.mips;
        let slices = self.layout.desc.depth;
        let taken = covered(union(), words, mips, slices);
        if !taken.is_empty() {
            self.bindings += 1;
            self.restricted_bindings += u64::from(taken.len() < (mips as usize * slices as usize));
        }
        self.subresources.extend(taken);
    }
}

/// The subresources of a `mips`x`slices` texture a copy takes: every one
/// without the union ([`union`]), else the mip and slices the texture UAV
/// described by `words` exposes — mip zero of every slice for no `words`
/// (the default view), nothing for a view on another dimension, a plane
/// (depth-stencil), a mip the texture lacks or slices it does not have.
pub(crate) fn covered(
    union: bool,
    words: Option<&[u32; 10]>,
    mips: u32,
    slices: u32,
) -> Vec<u32> {
    if !union {
        return (0..mips.saturating_mul(slices)).collect();
    }
    let (mip, first, count, plane) = match words.map(crate::descriptor::host::uav_desc) {
        None => (0, 0, slices, 0),
        Some(desc) => match desc.ViewDimension {
            D3D12_UAV_DIMENSION_TEXTURE2D => {
                // SAFETY: the member named by ViewDimension.
                let v = unsafe { desc.Anonymous.Texture2D };
                (v.MipSlice, 0, 1, v.PlaneSlice)
            }
            D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                // SAFETY: the member named by ViewDimension.
                let v = unsafe { desc.Anonymous.Texture2DArray };
                (v.MipSlice, v.FirstArraySlice, v.ArraySize, v.PlaneSlice)
            }
            _ => return Vec::new(),
        },
    };
    // UINT_MAX means every remaining slice from FirstArraySlice.
    let end = if count == u32::MAX {
        Some(slices)
    } else {
        first.checked_add(count)
    };
    match end.filter(|&end| plane == 0 && mip < mips && first < end && end <= slices) {
        Some(end) => (first..end).map(|slice| slice * mips + mip).collect(),
        None => Vec::new(),
    }
}

struct Subresource {
    index: u32,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    rows: u32,
}

/// The readback resources and the allocation the pixels belong to.
pub(crate) struct Back {
    buffer: ID3D12Resource,
    subresources: Vec<Subresource>,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
    bindings: u64,
    restricted_bindings: u64,
}

impl Back {
    pub(crate) fn range(&self) -> (usize, usize) {
        (self.ticket.at, self.ticket.end)
    }
}

/// Records a host copy from a texture already in COPY_SOURCE.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, target: Target) -> Option<Back> {
    if target.subresources.is_empty() || !target.ticket.valid.load(Ordering::Acquire) {
        return None;
    }
    let host = crate::host::host()?;
    let texture = &target.copy.texture;
    // SAFETY: a live texture kept through the command list's state tracking.
    let desc = unsafe { texture.GetDesc() };
    let mut subresources = Vec::with_capacity(target.subresources.len());
    let mut total = 0u64;
    for index in target.subresources {
        let offset = total.checked_next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT))?;
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut rows = 0;
        // SAFETY: outputs for one subresource of the live texture.
        unsafe {
            host.device.GetCopyableFootprints(
                &desc,
                index,
                1,
                offset,
                Some(&mut footprint),
                Some(&mut rows),
                None,
                None,
            );
        }
        // The tiler reads pitched rows, including padding after the last row.
        let format = footprint.Footprint;
        let pitched = u64::from(format.RowPitch)
            .checked_mul(u64::from(rows))?
            .checked_mul(u64::from(format.Depth))?;
        total = footprint.Offset.checked_add(pitched)?;
        subresources.push(Subresource { index, footprint, rows });
    }
    let buffer = host.read_back_buffer(total)?;
    for sub in &subresources {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: sub.footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub.index },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    Some(Back {
        buffer,
        subresources,
        layout: target.layout,
        ticket: target.ticket,
        bindings: target.bindings,
        restricted_bindings: target.restricted_bindings,
    })
}

/// Once the list has run, tile its texels back into the same title allocation.
pub(crate) fn land(back: &Back) {
    static STALE: AtomicU64 = AtomicU64::new(0);
    let (at, end) = back.range();
    let size = end - at;
    // Lock order: READING -> TRACKED -> LAZY_STATE (via read_into/copy_mapped).
    let _mapping = crate::watch::title_memory_reading();
    let mut tracked = TRACKED.lock().unwrap_or_else(|e| e.into_inner());
    if !back.ticket.valid.load(Ordering::Acquire) {
        let skipped = STALE.fetch_add(1, Ordering::Relaxed) + 1;
        if skipped.is_power_of_two() {
            tracing::info!(skipped, "skipping texture readbacks whose title allocation changed");
        }
        return;
    }
    let mut tiled = vec![0u8; size];
    // Keep title bytes the subresources do not cover.
    if !crate::host::read_into(at as *const u8, &mut tiled) {
        return;
    }
    let mut data = std::ptr::null_mut();
    // SAFETY: our read-back buffer, written by the list that has run.
    unsafe {
        if back.buffer.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
            return;
        }
    }
    let mips = back.layout.desc.mips;
    for Subresource { index, footprint, rows } in &back.subresources {
        let (mip, slice) = (index % mips, index / mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let depth_pitch = pitch * *rows as usize;
        let len = depth_pitch * f.Depth as usize;
        // SAFETY: the subresource's part of the mapped buffer.
        let linear = unsafe { std::slice::from_raw_parts(data.cast::<u8>().add(footprint.Offset as usize), len) };
        if let Err(e) = back.layout.tile(&mut tiled, mip, slice, linear, pitch, depth_pitch) {
            TILE_WARNING.call_once(|| {
                tracing::warn!(
                    ?e,
                    mip,
                    slice,
                    at = format_args!("{at:#x}"),
                    bytes = size,
                    "a texture copied back could not be tiled"
                );
            });
            // SAFETY: mapped above.
            unsafe { back.buffer.Unmap(0, None) };
            return;
        }
    }
    // SAFETY: mapped above.
    unsafe { back.buffer.Unmap(0, None) };
    crate::host::copy_mapped(at as *mut u8, tiled.as_ptr(), size, true);
    let totals = &mut tracked.landed;
    let copied_subresources = back.subresources.len() as u64;
    totals.copies += 1;
    totals.bindings += back.bindings;
    totals.restricted_bindings += back.restricted_bindings;
    totals.copied_subresources += copied_subresources;
    totals.skipped_subresources +=
        u64::from(mips) * u64::from(back.layout.desc.depth) - copied_subresources;
    if totals.copies <= 32 || totals.copies.is_multiple_of(128) {
        tracing::info!(
            copied = totals.copies,
            bytes = size,
            at = format_args!("{at:#x}"),
            bindings = totals.bindings,
            restricted_bindings = totals.restricted_bindings,
            copied_subresources = totals.copied_subresources,
            skipped_subresources = totals.skipped_subresources,
            "texture readback landed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_readbacks_for_changed_allocations_are_invalidated() {
        let first = track(91, 0x1000, 0x2000);
        let next = track(92, 0x2000, 0x3000);
        let far = track(93, 0x4000, 0x5000);

        remapped(0x0f00, 0x200);
        assert!(!first.valid.load(Ordering::Acquire));
        assert!(next.valid.load(Ordering::Acquire));
        replaced(0x2000);
        assert!(!next.valid.load(Ordering::Acquire));
        assert!(far.valid.load(Ordering::Acquire));

        released(93);
        assert!(!far.valid.load(Ordering::Acquire));
        assert!(track(94, 0x2000, 0x3000).valid.load(Ordering::Acquire));
    }

    fn view2d(mip: u32) -> [u32; 10] {
        [41, D3D12_UAV_DIMENSION_TEXTURE2D.0 as u32, mip, 0, 0, 0, 0, 0, 0, 0]
    }

    fn view_array(mip: u32, first: u32, count: u32) -> [u32; 10] {
        [41, D3D12_UAV_DIMENSION_TEXTURE2DARRAY.0 as u32, mip, first, count, 0, 0, 0, 0, 0]
    }

    /// The union takes the mip and slices each bound texture UAV exposes, and
    /// without it the copy takes the whole texture, whatever the views say.
    #[test]
    fn the_union_takes_what_the_views_expose() {
        // 3 mips, 4 slices: 12 subresources, a slice's mips in a row.
        assert_eq!(covered(false, Some(&view2d(1)), 3, 4), (0..12).collect::<Vec<u32>>());
        assert_eq!(covered(false, None, 3, 4), (0..12).collect::<Vec<u32>>());
        // 2D view at mip 1: slice 0 only, that mip.
        assert_eq!(covered(true, Some(&view2d(1)), 3, 4), vec![1]);
        // Array view at mip 2 from slice 2 for the rest: 2 * 3 + 2, 3 * 3 + 2.
        assert_eq!(covered(true, Some(&view_array(2, 2, u32::MAX)), 3, 4), vec![8, 11]);
        // Two slices from slice 1, at mip 0.
        assert_eq!(covered(true, Some(&view_array(0, 1, 2)), 3, 4), vec![3, 6]);
        // No view: mip zero of every slice (the default view).
        assert_eq!(covered(true, None, 3, 4), vec![0, 3, 6, 9]);
        // Nothing outside the texture: a mip it lacks, a slice it lacks, a
        // plane (depth-stencil), a view on another dimension.
        assert!(covered(true, Some(&view2d(3)), 3, 4).is_empty());
        assert!(covered(true, Some(&view_array(1, 4, 1)), 3, 4).is_empty());
        assert!(covered(true, Some(&view_array(0, 3, 2)), 3, 4).is_empty());
        // Texture2D: mip 0 of the second plane (depth-stencil).
        let plane = [41, D3D12_UAV_DIMENSION_TEXTURE2D.0 as u32, 0, 1, 0, 0, 0, 0, 0, 0];
        assert!(covered(true, Some(&plane), 3, 4).is_empty());
        let buffer = [41, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(covered(true, Some(&buffer), 3, 4).is_empty());
    }
}
