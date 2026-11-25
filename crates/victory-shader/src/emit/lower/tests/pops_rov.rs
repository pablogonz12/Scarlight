//! A pixel shader under the console's primitive ordered pixel shading (`DB_SHADER_CONTROL` bit 16)
//! reads and writes its UAVs as rasterizer ordered views: their handles carry DXC's ROV property
//! and the root UAVs are declared rasterizer ordered, which dxil-spirv turns into a pixel
//! interlock. Kept in its own file (lower.rs is at the size limit).

use super::*;

    /// [`lower_with`] as a pixel shader whose `DB_SHADER_CONTROL` is
    /// `db_shader_control`.
    fn lower_ps(
        words: &[u32],
        db_shader_control: u32,
        layout: Vec<RootParam>,
        used: Vec<UsedResource>,
    ) -> Result<Lowered, crate::ShaderError> {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 8,
            layout,
            used,
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control,
                patches: vec![],
            },
            code,
        };
        let p = ir::build(&native.code, false, 8).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, &native).unwrap();
        lower(&Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
        })
    }

    #[test]
    fn a_pops_pixel_shader_writes_through_rasterizer_ordered_views() {
        // `buffer_store_dword v0, off, s[0:3], 0`: the root UAV at user
        // data 0, its descriptor's bounds and format words in s2, s3.
        let words = [
            0xBE82_03C1, // s_mov_b32 s2, -1
            0xBE83_03FF, // s_mov_b32 s3, -1
            0x7E00_0280, // v_mov_b32 v0, 0
            0xE070_0000,
            0x8000_0000, // buffer_store_dword v0, off, s[0:3], 0
            0x7E00_02F2, // v_mov_b32 v0, 1.0
            0xF800_1883,
            0x0000_0100, // exp mrtz v0, v0, off, off done vm
            S_ENDPGM,
        ];
        let uav = || {
            vec![UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 4,
                dwords: 4,
                user_data: 0,
                slot: 0,
            }]
        };
        let layout = || vec![root(RootType::Descriptor, 0, 0)];
        let ps = lower_ps(&words, 0, layout(), uav()).unwrap();
        assert_eq!(ps.root_uavs, vec![0]);
        assert!(!ps.rovs, "a shader without POPS keeps plain UAVs");

        // `DB_SHADER_CONTROL` bit 16: the console's POPS. The same store is
        // a rasterizer ordered view's (`0x2000` on the handle and the
        // resources metadata, which dxil-spirv turns into a pixel
        // interlock), so the DXIL differs by that bit alone.
        let pops = lower_ps(&words, 1 << 16, layout(), uav()).unwrap();
        assert!(pops.rovs, "a POPS pixel shader's UAVs are rasterizer ordered");
        // The two differ in the root UAV's handle properties and in the
        // resource metadata's rasterizer-ordered flag (bit 9, which the
        // offline check reads back: dxil-spirv turns them into a pixel
        // interlock).
        assert_eq!(pops.root_uavs, ps.root_uavs);
        assert_ne!(ps.dxil, pops.dxil);
        // DXC's handle properties: bit 12 UAV, bit 13 rasterizer ordered
        // (`AnnotateROVMask`, which dxil-spirv reads as `rov`).
        assert_eq!(crate::emit::shader::UAV, 1 << 12);
        assert_eq!(crate::emit::shader::ROV, 1 << 13);
    }
