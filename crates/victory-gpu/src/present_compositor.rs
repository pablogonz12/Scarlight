//! Composes the two full-screen RGBA8 planes used by the Series S menu.
//! The fullscreen triangle and premultiplied blend follow Yaekai's PresentX
//! compositor, commit 24a387d700bc. The root signature is explicit for Wine.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Error, Interface, Result};

use crate::host::transition;

const SOURCE: &str = r#"
Texture2D<float4> plane : register(t0);
SamplerState linear_clamp : register(s0);

void vs(uint id : SV_VertexID, out float4 position : SV_Position, out float2 uv : TEXCOORD0) {
    uv = float2((id << 1) & 2, id & 2);
    position = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}

// Keep the position input before TEXCOORD0 for matching stage registers.
float4 ps(float4 position : SV_Position, float2 uv : TEXCOORD0) : SV_Target {
    return plane.Sample(linear_clamp, uv);
}
"#;

pub(crate) struct PresentCompositor {
    root: ID3D12RootSignature,
    opaque: ID3D12PipelineState,
    overlay: ID3D12PipelineState,
    rtv: ID3D12DescriptorHeap,
    srvs: ID3D12DescriptorHeap,
    srv_stride: u32,
}

/// Require two distinct, sampleable single-subresource textures. Other
/// profiles keep their existing path, including the HDR swap-chain handling.
pub(crate) fn extent(base: &ID3D12Resource, overlay: &ID3D12Resource) -> Option<(u32, u32)> {
    if base.as_raw() == overlay.as_raw() {
        return None;
    }
    let size = |texture: &ID3D12Resource| {
        // SAFETY: a live texture retained through presentation.
        let desc = unsafe { texture.GetDesc() };
        let width = u32::try_from(desc.Width).ok()?;
        (matches!(
            desc.Format,
            DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_TYPELESS
        ) && desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
            && desc.SampleDesc.Count == 1
            && desc.DepthOrArraySize == 1
            && desc.MipLevels == 1
            && !desc
                .Flags
                .contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
            && width > 0
            && desc.Height > 0)
            .then_some((width, desc.Height))
    };
    let (w0, h0) = size(base)?;
    let (w1, h1) = size(overlay)?;
    Some((w0.max(w1), h0.max(h1)))
}

fn at_stage<T>(result: Result<T>, stage: &str) -> Result<T> {
    result.inspect_err(|error| {
        tracing::debug!(stage, %error, "PresentX compositor initialization failed");
        #[cfg(test)]
        eprintln!("PRESENTX_GPU_INIT_STAGE: {stage}: {error:?}");
    })
}

impl PresentCompositor {
    pub(crate) fn new(device: &ID3D12Device, format: DXGI_FORMAT) -> Result<Self> {
        let vs = at_stage(
            crate::host::compile(SOURCE, "vs", "vs_5_1"),
            "compile vertex shader",
        )?;
        let ps = at_stage(
            crate::host::compile(SOURCE, "ps", "ps_5_1"),
            "compile pixel shader",
        )?;
        let range = D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            ..Default::default()
        };
        let parameter = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: &range,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        };
        let sampler = D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
            MaxLOD: f32::MAX,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            ..Default::default()
        };
        let desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: 1,
            pParameters: &parameter,
            NumStaticSamplers: 1,
            pStaticSamplers: &sampler,
            ..Default::default()
        };
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        // SAFETY: descriptors point to live data through the creation calls.
        unsafe {
            let mut signature = None;
            at_stage(
                D3D12SerializeRootSignature(
                    &desc,
                    D3D_ROOT_SIGNATURE_VERSION_1,
                    &mut signature,
                    None,
                ),
                "serialize the root signature",
            )?;
            let signature = signature.ok_or_else(Error::empty)?;
            let signature = std::slice::from_raw_parts(
                signature.GetBufferPointer().cast::<u8>(),
                signature.GetBufferSize(),
            );
            let root: ID3D12RootSignature = at_stage(
                device.CreateRootSignature(0, signature),
                "create the root signature",
            )?;
            let make_pipeline = |blend: bool| -> Result<ID3D12PipelineState> {
                let mut blend_state = D3D12_BLEND_DESC::default();
                blend_state.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
                    BlendEnable: blend.into(),
                    SrcBlend: D3D12_BLEND_ONE,
                    DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
                    BlendOp: D3D12_BLEND_OP_ADD,
                    SrcBlendAlpha: D3D12_BLEND_ONE,
                    DestBlendAlpha: D3D12_BLEND_INV_SRC_ALPHA,
                    BlendOpAlpha: D3D12_BLEND_OP_ADD,
                    LogicOp: D3D12_LOGIC_OP_NOOP,
                    RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
                    ..Default::default()
                };
                let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
                formats[0] = format;
                let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: ManuallyDrop::new(Some(root.clone())),
                    VS: code(&vs),
                    PS: code(&ps),
                    BlendState: blend_state,
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    RTVFormats: formats,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                };
                let result = device.CreateGraphicsPipelineState(&desc);
                drop(ManuallyDrop::into_inner(desc.pRootSignature));
                result
            };
            let opaque = at_stage(make_pipeline(false), "create the opaque pipeline")?;
            let overlay = at_stage(make_pipeline(true), "create the overlay pipeline")?;
            let rtv = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 1,
                ..Default::default()
            })?;
            let srvs = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })?;
            let srv_stride =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            Ok(Self {
                root,
                opaque,
                overlay,
                rtv,
                srvs,
                srv_stride,
            })
        }
    }

    /// Records the two-plane pass and restores the sources to their entry
    /// states. The back buffer enters PRESENT and leaves RENDER_TARGET.
    /// The caller owns the serialization of descriptor writes and GPU use.
    pub(crate) fn record(
        &self,
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        back: &ID3D12Resource,
        sources: [(&ID3D12Resource, D3D12_RESOURCE_STATES); 2],
        (width, height): (u32, u32),
    ) {
        const READ: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;
        // SAFETY: the list is open and the caller retains the GPU resources
        // and heaps through its fence. The two sources passed extent().
        unsafe {
            let rtv = self.rtv.GetCPUDescriptorHandleForHeapStart();
            device.CreateRenderTargetView(back, None, rtv);
            let cpu = self.srvs.GetCPUDescriptorHandleForHeapStart();
            let gpu = self.srvs.GetGPUDescriptorHandleForHeapStart();
            let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            for (i, &(texture, state)) in sources.iter().enumerate() {
                let handle = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: cpu.ptr + i * self.srv_stride as usize,
                };
                device.CreateShaderResourceView(texture, Some(&srv), handle);
                if state != READ {
                    list.ResourceBarrier(&[transition(texture, state, READ)]);
                }
            }
            list.ResourceBarrier(&[transition(
                back,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            )]);
            list.SetDescriptorHeaps(&[Some(self.srvs.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                Width: width as f32,
                Height: height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]);
            list.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            }]);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            for (i, pipeline) in [&self.opaque, &self.overlay].into_iter().enumerate() {
                list.SetPipelineState(pipeline);
                list.SetGraphicsRootDescriptorTable(
                    0,
                    D3D12_GPU_DESCRIPTOR_HANDLE {
                        ptr: gpu.ptr + i as u64 * u64::from(self.srv_stride),
                    },
                );
                list.DrawInstanced(3, 1, 0, 0);
            }
            for (texture, state) in sources {
                if state != READ {
                    list.ResourceBarrier(&[transition(texture, READ, state)]);
                }
            }
        }
    }
}
