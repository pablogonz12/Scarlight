//! A stand-in for ray-traced reflections while the structures they walk
//! are not built.
//!
//! The title's specular reflections are ray traced (`sr_tracing:*`): rays
//! walk the ray-tracing structures (`CS_RaytraceVBuffer_*`), the hits are
//! shaded by about 60 per-material `<material>:CS_RaytracingResolve`
//! programs, the misses get sky and probe radiance from `CS_RayMiss`
//! (06bb6e728c7c99bf), and `CS_ResolveScreenSpace` adds what the screen
//! itself shows; all of them write one half-resolution radiance image
//! (R11G11B10, table UAV `s1+104` of `CS_RayMiss`), which the denoiser
//! (`sr_denoising:*`) then blurs and keeps a history of. The structure
//! builder does not run here (see `crate::pipeline::builder_runs`): every
//! ray walks whatever bytes lie where the structures would be, so what the
//! resolves and `CS_RayMiss` leave is mostly sky, even on eyes in shadow,
//! inside mouths, on skin (the eyes that glow, the mouths that sparkle and
//! the chrome skin a tester saw, with the builder off).
//!
//! With `VICTORY_REFLECTION=miss`, that image is cleared to zero right
//! after `CS_RayMiss` (which runs after the resolves, before
//! `CS_ResolveScreenSpace`): what the structures' rays brought is dropped,
//! the screen-space reflections that follow stay. Unset, empty or `0` is
//! off (the default); any other value is not understood and stays off.
//! It does not act when the builder was asked for (`VICTORY_BVH_BUILDER=1`,
//! where the structures are the title's own) or once it has run whole.
//!
//! The image is found as `ambient.rs` of fxceless/victory-fork pull
//! request 917 (VICTORY_AMBIENT, after Belobug's VIctory-Belobug-AMD)
//! finds its own: the program known at its dispatch, the view its table
//! names at the place its code reads. It is cleared with a render target
//! clear right after the dispatch in the same list, after a transition the
//! list records (it waits for the pass's writes); the next use moves it
//! back.
//!
//! After Chezzuss's VIctory batch 7 (`reflection.rs`: VICTORY_REFLECTION
//! with `miss`, `alpha`, `water` and `history`; the tester's finding was
//! `VICTORY_REFLECTION=miss`).
//!
//! shortcut: one image, the opaque pass's. The transparent and water
//! passes' images are left as the title has them (batch 7 told them apart
//! by their sky-probe fallbacks), and the same darkness for every glossy
//! surface whatever the scene; of no use once the builder runs.

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
use crate::host::HostList;
#[cfg(windows)]
use crate::object::This;
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D12::*;
#[cfg(windows)]
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

/// `sr_tracing:CS_RayMiss`.
const RAY_MISS: &str = "06bb6e728c7c99bf";
/// Where it writes the radiance image: root parameter and place in its
/// table, in dwords (`s1+104` in the frame listings).
const RADIANCE: (u8, u32) = (1, 104);

/// What `VICTORY_REFLECTION` asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Mode {
    /// The level `CS_RayMiss`'s image is cleared to (linear radiance;
    /// `miss` leaves zero).
    level: f32,
}

/// The mode `set` asks for: none if off, the token not understood if any.
fn mode_of(set: &str) -> Result<Option<Mode>, String> {
    let set = set.trim().to_ascii_lowercase();
    match set.as_str() {
        "" | "0" | "off" | "false" | "no" => Ok(None),
        "miss" => Ok(Some(Mode { level: 0.0 })),
        _ => Err(set),
    }
}

/// The mode `VICTORY_REFLECTION` asks for; none if off.
#[cfg(windows)]
fn mode() -> Option<&'static Mode> {
    static MODE: std::sync::OnceLock<Option<Mode>> = std::sync::OnceLock::new();
    MODE.get_or_init(|| {
        let set = std::env::var("VICTORY_REFLECTION").unwrap_or_default();
        match mode_of(&set) {
            Ok(Some(mode)) => {
                tracing::info!(
                    set,
                    ?mode,
                    "the ray-traced reflection image is cleared while the ray-tracing structures are not built (VICTORY_REFLECTION)"
                );
                Some(mode)
            }
            Ok(None) => None,
            Err(token) => {
                tracing::warn!(set, token, "VICTORY_REFLECTION is not understood: it stays off");
                None
            }
        }
    })
    .as_ref()
}

/// The mode, if `hash` (as logged) is the program whose image it clears.
#[cfg(windows)]
pub(crate) fn pass(hash: &str) -> Option<&'static Mode> {
    (hash == RAY_MISS).then(mode).flatten()
}

/// Of the table UAVs a program names (root parameter, place in dwords),
/// the one that is `CS_RayMiss`'s radiance image: at its known place, or
/// the only one.
#[cfg_attr(not(windows), allow(dead_code))]
fn radiance_of(uavs: &[(u8, u32)]) -> Option<usize> {
    uavs.iter()
        .position(|&u| u == RADIANCE)
        .or((uavs.len() == 1).then_some(0))
}

/// The place in `uavs` (root parameter, place in dwords, resource) of the
/// image to clear: `CS_RayMiss`'s radiance image, if it names a live one.
/// A null or unresolvable view there clears nothing, and another live UAV
/// is never taken for it.
#[cfg_attr(not(windows), allow(dead_code))]
fn target_of(uavs: &[(u8, u32, usize)]) -> Option<usize> {
    let places: Vec<(u8, u32)> = uavs.iter().map(|&(slot, at, _)| (slot, at)).collect();
    radiance_of(&places).filter(|&i| uavs[i].2 != 0)
}

/// The colour a render target clear of `format` (DXGI) to `level` takes;
/// none if the format is not one this clears (typeless, packed
/// shared-exponent, depth), or an integer format asked for other than zero.
#[cfg_attr(not(windows), allow(dead_code))]
fn colour_of(format: u32, level: f32) -> Option<[f32; 4]> {
    const FLOAT: [u32; 8] = [
        2,  // R32G32B32A32_FLOAT
        10, // R16G16B16A16_FLOAT
        24, // R10G10B10A2_UNORM
        26, // R11G11B10_FLOAT
        28, // R8G8B8A8_UNORM
        34, // R16G16_FLOAT
        41, // R32_FLOAT
        54, // R16_FLOAT
    ];
    const INTEGER: [u32; 2] = [
        42, // R32_UINT
        57, // R16_UINT
    ];
    if FLOAT.contains(&format) {
        Some([level, level, level, 1.0])
    } else if INTEGER.contains(&format) && level == 0.0 {
        Some([0.0; 4])
    } else {
        None
    }
}

/// What a title's unordered access view `desc` (D3D12 words: format,
/// dimension, then the dimension's fields) of a texture with `format` and
/// `slices` covers: its format, mip level, first slice and slice count;
/// none if it is not of a 2D texture.
#[cfg_attr(not(windows), allow(dead_code))]
fn extent_of(desc: Option<[u32; 10]>, format: u32, slices: u32) -> Option<(u32, u32, u32, u32)> {
    const TEXTURE2D: u32 = 4;
    const TEXTURE2DARRAY: u32 = 5;
    let Some(d) = desc else {
        return Some((format, 0, 0, slices));
    };
    let format = if d[0] != 0 { d[0] } else { format };
    match d[1] {
        TEXTURE2D => Some((format, d[2], 0, 1)),
        TEXTURE2DARRAY => {
            let count = if d[4] == u32::MAX || d[4] == 0 {
                slices.saturating_sub(d[3])
            } else {
                d[4]
            };
            Some((format, d[2], d[3], count.max(1)))
        }
        _ => None,
    }
}

/// A table UAV a dispatch names: root parameter, place in dwords, title
/// resource and view description.
#[cfg(windows)]
pub(crate) type Uav = (u8, u32, usize, Option<[u32; 10]>);

/// After a dispatch of `CS_RayMiss` whose table UAVs are `uavs`, recorded
/// in `list`: clears its radiance image in `mode`. Only while the
/// structures the rays walk are not the title's own.
#[cfg(windows)]
pub(crate) fn after(list: &HostList, mode: &Mode, hash: &str, uavs: &[Uav]) {
    if crate::pipeline::builder_asked() || crate::pipeline::builder_whole() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(
                "the ray-tracing structures are the title's own: VICTORY_REFLECTION does not act"
            )
        });
        return;
    }
    let named: Vec<(u8, u32, usize)> = uavs
        .iter()
        .map(|&(slot, at, resource, _)| (slot, at, resource))
        .collect();
    let Some(i) = target_of(&named) else {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                hash,
                ?named,
                "no reflection clear: CS_RayMiss's tables are not as expected"
            )
        });
        return;
    };
    let (_, _, resource, desc) = uavs[i];
    clear(list, (resource, desc), mode.level, hash);
}

/// Clears title texture `resource` through the part of it `desc` names to
/// `level`, in `list`.
#[cfg(windows)]
fn clear(
    list: &HostList,
    (resource, desc): (usize, Option<[u32; 10]>),
    level: f32,
    hash: &str,
) -> bool {
    const RENDER_TARGET: u32 = 0x4;
    let Some(host) = crate::host::host() else {
        return false;
    };
    let Some(texture) = crate::resource::host_resource(resource as This) else {
        return false;
    };
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let extent = (d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
        && d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 != 0
        && d.SampleDesc.Count == 1)
        .then(|| extent_of(desc, d.Format.0 as u32, u32::from(d.DepthOrArraySize)))
        .flatten();
    let colour = extent.and_then(|(format, ..)| colour_of(format, level));
    let (Some((format, mip, first, count)), Some(colour)) = (extent, colour) else {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                hash,
                resource = format_args!("{resource:#x}"),
                dimension = d.Dimension.0,
                flags = d.Flags.0,
                format = d.Format.0,
                ?desc,
                level,
                "no reflection clear: the image is not a render-target 2D texture of a format this clears"
            )
        });
        return false;
    };
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        tracing::info!(
            hash,
            resource = format_args!("{resource:#x}"),
            width = (d.Width >> mip).max(1),
            height = (d.Height >> mip).max(1),
            format,
            mip,
            slices = count,
            level,
            "a reflection image is cleared after its pass while the ray-tracing structures are not built (VICTORY_REFLECTION); the first one"
        )
    });
    let view = if u32::from(d.DepthOrArraySize) > 1 {
        D3D12_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT(format as i32),
            ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_RTV {
                    MipSlice: mip,
                    FirstArraySlice: first,
                    ArraySize: count,
                    PlaneSlice: 0,
                },
            },
        }
    } else {
        D3D12_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT(format as i32),
            ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_RTV {
                    MipSlice: mip,
                    PlaneSlice: 0,
                },
            },
        }
    };
    // The pass wrote it as unordered access: the transition waits for it.
    crate::list::ensure_state(list, resource as This, RENDER_TARGET);
    let rtv = host.rtv(Some(texture), Some(&view));
    #[cfg(feature = "diagnostics")]
    list.note_op(|| {
        format!("reflection clear {resource:#x} f{format} mip {mip} to {level} after [compute {hash}] (VICTORY_REFLECTION)")
    });
    // SAFETY: an open list; a fresh view of a live texture in the render
    // target state, which the list keeps as the title's resource.
    unsafe { list.list.ClearRenderTargetView(rtv, &colour, None) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off unless asked for; what each value asks for.
    #[test]
    fn the_switch_is_read() {
        for off in ["", " ", "0", "off", "OFF", "false", "no"] {
            assert_eq!(mode_of(off), Ok(None), "{off:?}");
        }
        assert_eq!(mode_of("miss"), Ok(Some(Mode { level: 0.0 })));
        assert_eq!(mode_of(" MISS "), Ok(Some(Mode { level: 0.0 })));
        for wrong in ["hist", "-1", "nan", "inf", "miss,0.2", "alpha", "1"] {
            assert_eq!(mode_of(wrong), Err(wrong.to_ascii_lowercase()), "{wrong:?}");
        }
    }

    /// CS_RayMiss's image is at s1+104, or its only UAV.
    #[test]
    fn the_radiance_image_is_found() {
        assert_eq!(radiance_of(&[(1, 104)]), Some(0));
        assert_eq!(radiance_of(&[(1, 96), (1, 104)]), Some(1));
        assert_eq!(radiance_of(&[(1, 96)]), Some(0));
        assert_eq!(radiance_of(&[(1, 96), (1, 112)]), None);
        assert_eq!(radiance_of(&[]), None);
    }

    /// A hand-built dispatch of `CS_RayMiss`: its table names the radiance
    /// image at `s1+104` as an R11G11B10 view of mip 3, and what the clear
    /// makes of it.
    #[test]
    fn a_hand_built_ray_miss_image_is_cleared_to_black() {
        let uavs = [(1, 96), (1, 104)];
        let mode = mode_of("miss").unwrap().unwrap();
        let i = radiance_of(&uavs).unwrap();
        assert_eq!(uavs[i], (1, 104));
        // format R11G11B10_FLOAT, TEXTURE2D, mip 3, no plane.
        let (format, mip, first, count) =
            extent_of(Some([26, 4, 3, 0, 0, 0, 0, 0, 0, 0]), 26, 1).unwrap();
        assert_eq!((format, mip, first, count), (26, 3, 0, 1));
        assert_eq!(colour_of(format, mode.level), Some([0.0, 0.0, 0.0, 1.0]));
        // The transparent pass's image, if it were this one, would be left
        // alone: this port clears the opaque pass's image only.
        let other = [(1, 96), (1, 112)];
        assert_eq!(radiance_of(&other), None);
    }

    /// A null radiance view clears nothing; another UAV is not taken for it.
    #[test]
    fn a_null_image_is_not_replaced() {
        // s1+96 is live and s1+104 is null: nothing is cleared.
        assert_eq!(target_of(&[(1, 96, 0x1000), (1, 104, 0)]), None);
        assert_eq!(target_of(&[(1, 104, 0)]), None);
        assert_eq!(target_of(&[(1, 96, 0x1000), (1, 104, 0x2000)]), Some(1));
        // The only table UAV, when the program names one: cleared if live.
        assert_eq!(target_of(&[(1, 96, 0x1000)]), Some(0));
        assert_eq!(target_of(&[]), None);
    }

    /// Float images to any level, integer ones only to zero, others never.
    #[test]
    fn clear_colours() {
        assert_eq!(colour_of(26, 0.0), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(colour_of(26, 0.1), Some([0.1, 0.1, 0.1, 1.0]));
        assert_eq!(colour_of(42, 0.0), Some([0.0; 4]));
        assert_eq!(colour_of(42, 0.1), None);
        assert_eq!(colour_of(67, 0.0), None); // R9G9B9E5_SHAREDEXP
        assert_eq!(colour_of(23, 0.0), None); // R10G10B10A2_TYPELESS
        assert_eq!(colour_of(0, 0.0), None); // UNKNOWN
    }

    /// What a view covers.
    #[test]
    fn view_extents() {
        assert_eq!(extent_of(None, 26, 1), Some((26, 0, 0, 1)));
        assert_eq!(
            extent_of(Some([0, 4, 0, 0, 0, 0, 0, 0, 0, 0]), 26, 1),
            Some((26, 0, 0, 1))
        );
        assert_eq!(
            extent_of(Some([42, 4, 3, 0, 0, 0, 0, 0, 0, 0]), 42, 1),
            Some((42, 3, 0, 1))
        );
        assert_eq!(
            extent_of(Some([26, 5, 0, 2, 3, 0, 0, 0, 0, 0]), 26, 8),
            Some((26, 0, 2, 3))
        );
        assert_eq!(
            extent_of(Some([26, 5, 1, 2, u32::MAX, 0, 0, 0, 0, 0]), 26, 8),
            Some((26, 1, 2, 6))
        );
        assert_eq!(
            extent_of(Some([26, 1, 0, 0, 0, 0, 0, 0, 0, 0]), 26, 1),
            None
        );
        assert_eq!(
            extent_of(Some([26, 8, 0, 0, 0, 0, 0, 0, 0, 0]), 26, 1),
            None
        );
    }
}
