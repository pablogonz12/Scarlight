//! A cube texture's level query in a pixel program (`image_get_lod`): the
//! console asks for a level of a cube map and the lowering refused it, so the
//! program was dropped (fxceless/victory-fork #1024).

use super::*;

#[test]
fn cube_image_get_lod_lowers_and_validates() {
    use crate::native::{RootParam, RootType};

    // Pixel shader:
    // s_mov_b32 s1, 0
    // s_mov_b32 s3, 0
    // s_load_dwordx8 s[4:11], s[0:1], null
    // s_load_dwordx4 s[12:15], s[2:3], null
    // v_mov_b32 v0..v2, 0
    // image_get_lod (op 0x60, dim=3, dmask=3) v[0..1], v[0..2], s[4:11], s[12:15]
    // v_cmpx_ne_u32_e32 v0, 0
    // s_endpgm
    let mimg_w0 = 0xF000_0000 | (0x60 << 18) | (3 << 8) | (3 << 3);
    // srsrc = 1 (s[4:11]), ssamp = 3 (s[12:15]): (3 << 21) | (1 << 16) = 0x0061_0000
    let words = [
        0xBE81_0380u32,
        0xBE83_0380,
        0xF40C_0100,
        0xFA00_0000,
        0xF408_0301,
        0xFA00_0000,
        0x7E00_0280,
        0x7E02_0280,
        0x7E04_0280,
        mimg_w0,
        0x0061_0000,
        0x7D9A_0080,
        S_ENDPGM,
    ];
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 4,
        layout: vec![
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 2,
                slot: 1,
            },
        ],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code,
    };
    let p = ir::build(&native.code, false, 4).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let lowered = lower(&Parts {
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
    .unwrap();
    #[cfg(windows)]
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut lowered.dxil.clone())
            .expect("DXIL validation failed for cube image_get_lod");
    }
    #[cfg(not(windows))]
    let _ = lowered;
}
