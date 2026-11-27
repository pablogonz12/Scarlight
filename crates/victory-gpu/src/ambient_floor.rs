//! The least indirect light, in place of what one program's resolve leaves.
//!
//! A title whose indirect diffuse light comes from probes filled by tracing
//! rays leaves next to nothing there while those rays do not run: whatever
//! the sun does not reach directly is black. After the program that resolves
//! them, this pass replaces the light with a constant, shaded from the
//! scene's depth so that creases and corners get less of it. The title's own
//! lighting then applies it to the surfaces' colours. Written, not added: the
//! result is what the next frame's history keeps.
//!
//! `VICTORY_AMBIENT_FLOOR=<program>:<view>:<level>[:<depth>]`, rules
//! separated by `;`:
//! - `<program>`: the 16-hex-digit shader hash the log prints for the
//!   compute program (`compute 0123456789abcdef`);
//! - `<view>`: the light's unordered view as the console shader addresses
//!   it, root parameter and offset in dwords in its descriptor table,
//!   `s1+176` or `1+176` (the notation of the diagnostics' table listings);
//! - `<level>`: the light's level, a number of at least 0;
//! - `<depth>`: the view of the scene's depth the program read, same
//!   notation. Left out, the light is the same everywhere.
//!
//! `VICTORY_AMBIENT_FLOOR_TINT=r,g,b` sets the light's colour (warm white
//! by default) and `VICTORY_AMBIENT_FLOOR_SHADE=s,r,n,t` how creases darken
//! it: strength (0 for none), the radius looked at in metres, the near
//! plane's distance and the tangent of half the vertical field of view.
//!
//! Off while the variable is unset. The idea, the depth-only shading and the
//! R9G9B9E5 packing come from the Belobug fork's `ambient.rs`, which names
//! its program by name and the title by its constants; here the runner names
//! the program and both views, so a title's own hold in the environment of
//! its run and in no file of this repository.
//! Ported from fxceless/victory-fork #1093 by m335671 (GPL-3.0-or-later).
//! shortcut: the same light nearly everywhere, gone once rays run.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::host::{HostList, host};
use crate::object::This;

/// One rule: the program, the light's view, its level, and the depth's view.
#[derive(Debug)]
pub(crate) struct Rule {
    /// Lowercase, as logged.
    pub(crate) program: String,
    pub(crate) level: f32,
    /// Root parameter and table offset in dwords.
    pub(crate) light: (u8, u32),
    pub(crate) depth: Option<(u8, u32)>,
}

/// The rules in the variable, made once; empty (`None`) while it is unset.
fn all() -> Option<&'static [Rule]> {
    static RULES: OnceLock<Option<Vec<Rule>>> = OnceLock::new();
    RULES
        .get_or_init(|| {
            let set = std::env::var("VICTORY_AMBIENT_FLOOR").unwrap_or_default();
            if set.trim().is_empty() || set.trim() == "off" {
                return None;
            }
            match parse(set.trim()) {
                Ok(rules) if rules.is_empty() => None,
                Ok(rules) => {
                    for rule in &rules {
                        tracing::info!(
                            program = %rule.program,
                            light = format_args!("s{}+{}", rule.light.0, rule.light.1),
                            level = rule.level,
                            depth = rule.depth.map(|d| format!("s{}+{}", d.0, d.1)),
                            "the least indirect light"
                        );
                    }
                    Some(rules)
                }
                Err(e) => {
                    tracing::warn!(error = e, "no least indirect light");
                    None
                }
            }
        })
        .as_deref()
}

/// Whether `VICTORY_AMBIENT_FLOOR` names any.
pub(crate) fn enabled() -> bool {
    all().is_some_and(|rules| !rules.is_empty())
}

/// The rules following `program` (lowercase hex, as logged).
pub(crate) fn rules(program: &str) -> Vec<&'static Rule> {
    all()
        .unwrap_or_default()
        .iter()
        .filter(|r| r.program == program)
        .collect()
}

/// The rules in `text` (the variable's value).
pub(crate) fn parse(text: &str) -> Result<Vec<Rule>, String> {
    text.split(';')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|rule| parse_rule(rule).map_err(|e| format!("{rule:?}: {e}")))
        .collect()
}

fn parse_view(view: &str) -> Result<(u8, u32), String> {
    let view = view.trim();
    let view = view.strip_prefix(['s', 'S']).unwrap_or(view);
    let Some((slot, offset)) = view.split_once('+') else {
        return Err("a view is <root parameter>+<dwords>".into());
    };
    let slot = slot
        .trim()
        .parse::<u8>()
        .map_err(|_| "the view's root parameter is not a number")?;
    let offset = offset
        .trim()
        .parse::<u32>()
        .map_err(|_| "the view's offset is not a number")?;
    Ok((slot, offset))
}

fn parse_rule(rule: &str) -> Result<Rule, String> {
    let mut parts = rule.split(':');
    let (Some(program), Some(view), Some(level)) = (parts.next(), parts.next(), parts.next()) else {
        return Err("not <program>:<view>:<level>[:<depth>]".into());
    };
    let program = program.trim().to_ascii_lowercase();
    if program.len() != 16 || !program.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the program is not a 16-hex-digit hash".into());
    }
    let light = parse_view(view)?;
    let level = level
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|l| l.is_finite() && *l >= 0.0)
        .ok_or("the level is not a number of at least 0")?;
    let depth = match parts.next() {
        Some(depth) => Some(parse_view(depth)?),
        None => None,
    };
    if parts.next().is_some() {
        return Err("more than <program>:<view>:<level>:<depth>".into());
    }
    Ok(Rule {
        program,
        level,
        light,
        depth,
    })
}

/// Three floats from `name`, else `default`.
fn floats<const N: usize>(name: &str, default: [f32; N]) -> [f32; N] {
    let set: Vec<f32> = std::env::var(name)
        .unwrap_or_default()
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    match <[f32; N]>::try_from(set) {
        Ok(v) => v,
        Err(_) => default,
    }
}

/// Where the pass puts its views, and the pass itself.
struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: AtomicU32,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

/// How many pairs of views the heap holds: more than the lists in flight.
const SLOTS: u32 = 1024;

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), DescriptorTable(UAV(u0), SRV(t0))"
cbuffer Floor : register(b0) {
    float level; float3 tint;
    float strength; float radius; float near; float tan_y;
};
RWTexture2D<uint> light : register(u0);
Texture2D<float> depth : register(t0);
// The point seen at a texel, in metres from the eye (x right, y up, z away).
float3 seen(int2 p, float2 size) {
    float d = depth.Load(int3(clamp(p, int2(0, 0), int2(size) - 1), 0));
    float z = near / max(d, 1e-7);
    float2 ndc = (float2(p) + 0.5) / size * 2.0 - 1.0;
    return float3(ndc.x * tan_y * size.x / size.y * z, -ndc.y * tan_y * z, z);
}
// How much of the light from around reaches the texel: 1 in the open.
float open_to_light(int2 p, float2 size) {
    float3 c = seen(p, size);
    if (c.z > 400.0) return 1.0;
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx));
    if (dot(n, c) > 0.0) n = -n;
    float px = radius * size.y / (2.0 * tan_y * c.z);
    if (px < 1.5) return 1.0;
    px = min(px, 128.0);
    float noise = frac(52.9829189 * frac(dot(float2(p), float2(0.06711056, 0.00583715))));
    float sum = 0.0;
    const int N = 16;
    for (int i = 0; i < N; i++) {
        float a = i * 2.399963 + noise * 6.2831853;
        float2 o = float2(cos(a), sin(a)) * (sqrt((i + 0.5) / N) * px);
        float3 v = seen(p + int2(round(o)), size) - c;
        float vv = dot(v, v);
        float over = dot(v, n) - 0.004 * c.z - 0.005;
        sum += max(over, 0.0) / sqrt(vv + 1e-6) * saturate(1.0 - vv / (radius * radius));
    }
    return saturate(1.0 - strength * 2.0 * sum / N);
}
float3 unpack(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float most = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(most, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (most * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    light.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    // What the resolve left is not kept: without rays its inputs are never
    // written and it holds whatever their memory did.
    float open = strength > 0.0 ? open_to_light(int2(at.xy), float2(w, h)) : 1.0;
    light[at.xy] = pack(tint * level * open);
}
"#;

/// The pass's root signature (`RS` in [`SOURCE`]), serialized here rather
/// than taken from the compiled shader: Wine's `D3DCompile` succeeds but
/// leaves the `[RootSignature]` part out, and `CreateRootSignature` on it
/// then fails (0x80004005).
fn root_signature(device: &ID3D12Device) -> windows::core::Result<ID3D12RootSignature> {
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let ranges = [
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
        },
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
        },
    ];
    let params = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 8,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: ranges.len() as u32,
                    pDescriptorRanges: ranges.as_ptr(),
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
    ];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: params.len() as u32,
        pParameters: params.as_ptr(),
        NumStaticSamplers: 0,
        pStaticSamplers: std::ptr::null(),
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: `desc` and what it points to live through the call; the out pointer is ours.
    unsafe {
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, bytes)
    }
}

fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
    // The layer's shared compiler, as the indirect clamp uses (and any fallback it gets).
    let code = crate::host::compile(SOURCE, "main", "cs_5_1")?;
    // SAFETY: plain creations from descriptions that live through the calls.
    unsafe {
        let root = root_signature(device)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        })?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        let views = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: kind,
            NumDescriptors: SLOTS * 2,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        Ok(Pass {
            root,
            pso,
            views,
            step: device.GetDescriptorHandleIncrementSize(kind),
            next: AtomicU32::new(0),
        })
    }
}

/// A barrier between the title's accesses to the light and the pass's.
fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    // SAFETY: an open list; the barrier names no resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Replaces what `light` holds with `rule`'s level, in `list`, after the
/// program that wrote it as an image. Whether it recorded anything (the
/// list's bindings are then ours).
pub(crate) fn raise(list: &HostList, rule: &Rule, light: This, depth: Option<This>) -> bool {
    let Some(host) = host() else {
        return false;
    };
    let Some(light) = crate::resource::host_resource(light) else {
        return false;
    };
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    let Some(pass) = PASS
        .get_or_init(|| {
            make(&host.device)
                .inspect_err(|e| tracing::warn!(error = %e, "no least indirect light"))
                .ok()
        })
        .as_ref()
    else {
        return false;
    };
    // SAFETY: a live resource.
    let d = unsafe { light.GetDesc() };
    if d.Format != DXGI_FORMAT_R32_UINT {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(format = d.Format.0, "no least indirect light: another format"));
        return false;
    }
    // The depth the program read, in the state it read it in; of the light's
    // size, or the shading is left out.
    let depth = depth.and_then(crate::resource::host_resource).filter(|t| {
        // SAFETY: a live resource.
        let t = unsafe { t.GetDesc() };
        t.Format == DXGI_FORMAT_R32G8X24_TYPELESS && (t.Width, t.Height) == (d.Width, d.Height)
    });
    let slot = pass.next.fetch_add(1, Ordering::Relaxed) % SLOTS * 2;
    list.keep(light);
    if let Some(depth) = depth {
        list.keep(depth);
    }
    let [r, g, b] = floats("VICTORY_AMBIENT_FLOOR_TINT", [1.0, 0.98, 0.94]);
    let [strength, radius, near, tan_y] = floats("VICTORY_AMBIENT_FLOOR_SHADE", [1.0, 0.6, 0.1, 0.45]);
    let strength = if depth.is_some() { strength } else { 0.0 };
    let l = &list.list;
    uav_barrier(l);
    // SAFETY: an open list; a live texture the list keeps, in the
    // unordered-access state as the program before left it; a view in a slot
    // no list in flight reads.
    unsafe {
        let cpu = pass.views.GetCPUDescriptorHandleForHeapStart();
        let gpu = pass.views.GetGPUDescriptorHandleForHeapStart();
        let at = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: cpu.ptr + (slot * pass.step) as usize,
        };
        let view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        host.device
            .CreateUnorderedAccessView(light, None::<&ID3D12Resource>, Some(&view), at);
        let seen = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MipLevels: 1,
                    ..Default::default()
                },
            },
        };
        let beside = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: at.ptr + pass.step as usize,
        };
        host.device.CreateShaderResourceView(depth, Some(&seen), beside);
        l.SetDescriptorHeaps(&[Some(pass.views.clone())]);
        l.SetComputeRootSignature(&pass.root);
        l.SetPipelineState(&pass.pso);
        let constants = [rule.level, r, g, b, strength, radius, near, tan_y];
        l.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(
            1,
            D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: gpu.ptr + u64::from(slot * pass.step),
            },
        );
        l.Dispatch(d.Width.div_ceil(8) as u32, d.Height.div_ceil(8), 1);
    }
    uav_barrier(l);
    true
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn a_rule_names_its_program_its_two_views_and_its_level() {
        let rules = parse("0123456789abcdef:s1+176:0.6:s0+464").expect("a rule");
        assert_eq!(rules.len(), 1);
        let rule = &rules[0];
        assert_eq!(rule.program, "0123456789abcdef");
        assert_eq!(rule.light, (1, 176));
        assert_eq!(rule.depth, Some((0, 464)));
        assert_eq!(rule.level, 0.6);
        // Without a depth view the light is the same everywhere.
        let rules = parse("0123456789abcdef:1+176:0.6").expect("a rule");
        assert_eq!(rules[0].depth, None);
        // Rules are separated by `;`, empty ones left out.
        assert_eq!(parse(" ; 0123456789abcdef:s1+176:1 ; ").expect("rules").len(), 1);
    }

    #[test]
    fn a_rule_that_is_not_one_is_refused() {
        for bad in [
            "0123456789abcdef:s1+176",          // no level
            "012345:s1+176:0.6",                // not a 16-digit program
            "xyz:s1+176:0.6",                   // not hex
            "0123456789abcdef:s1+176:-1",       // a negative level
            "0123456789abcdef:s1+176:nan",      // not a number
            "0123456789abcdef:s1:0.6",          // a view without its offset
            "0123456789abcdef:s1+176:0.6:s0+464:s2+0", // one field too many
        ] {
            assert!(parse(bad).is_err(), "{bad:?} was taken");
        }
    }
}
