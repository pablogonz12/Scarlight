<!-- isa:start -->
## ISA coverage, counted from source

![ISA coverage](progress/shaders.svg)

**CI regenerates this section from source on every push, without an executable. It does not rerun the runtime measurements above.** It compares the decoder's opcode tables with a public reference list of every RDNA2 instruction form, then asks each backend whether it has a case for the form. Decoded, and a case in a backend, are statements about our code, not about a shader that ran: a form we name can still be translated wrongly, and nothing here is a picture.

**1203 of 1236 reference forms decoded; 711 with a DXIL-path case; the direct SPIR-V emitter is not on this branch, so that column is empty.**

| Encoding | Forms | Decoded | DXIL path | … via a family prefix | SPIR-V direct | Bar |
|---|---|---|---|---|---|---|
| SOP1 | 66 | 66 | 30 | 2 | not on this branch | ████████████ 100 % |
| SOP2 | 51 | 51 | 44 | 0 | not on this branch | ████████████ 100 % |
| SOPK | 27 | 27 | 24 | 12 | not on this branch | ████████████ 100 % |
| SOPC | 18 | 18 | 14 | 8 | not on this branch | ████████████ 100 % |
| SOPP | 37 | 37 | 34 | 4 | not on this branch | ████████████ 100 % |
| SMEM | 84 | 84 | 8 | 0 | not on this branch | ████████████ 100 % |
| VOP1 | 81 | 81 | 60 | 0 | not on this branch | ████████████ 100 % |
| VOP2 | 51 | 45 | 37 | 1 | not on this branch | ███████████░ 88 % |
| VOP3 | 118 | 112 | 73 | 3 | not on this branch | ███████████░ 95 % |
| VOP3P | 28 | 28 | 22 | 19 | not on this branch | ████████████ 100 % |
| VOPC | 190 | 170 | 170 | 166 | not on this branch | ███████████░ 89 % |
| DS | 154 | 154 | 12 | 0 | not on this branch | ████████████ 100 % |
| MUBUF | 75 | 75 | 75 | 59 | not on this branch | ████████████ 100 % |
| MTBUF | 16 | 16 | 8 | 8 | not on this branch | ████████████ 100 % |
| MIMG | 103 | 103 | 40 | 16 | not on this branch | ████████████ 100 % |
| FLAT | 54 | 54 | 0 | 0 | not on this branch | ████████████ 100 % |
| GLOBAL | 57 | 57 | 57 | 57 | not on this branch | ████████████ 100 % |
| SCRATCH | 22 | 22 | 0 | 0 | not on this branch | ████████████ 100 % |
| EXP | 1 | 0 | 0 | 0 | not on this branch | ░░░░░░░░░░░░ 0 % |
| VINTRP | 3 | 3 | 3 | 3 | not on this branch | ████████████ 100 % |
| **All encodings** | **1236** | **1203** | **711** | **358** | **—** | **███████████░ 97 %** |

- A form is one opcode in one encoding, from `tools/progress/rdna2_isa_forms.txt` (public source, provenance in the file header). "Decoded" means the decoder's opcode tables name it; "DXIL path" means the translator's own sources contain that name, or a family prefix it dispatches on (`ds_read_` covers `ds_read_b32`), which is one code path, not one case per opcode.
- A case is where a form is *mentioned*, so the count over-reads in one direction and under-reads in another: a form can be named by a length or a print helper and still be refused at translation. The 751-program offline check is the separate, per-title measure.

### Modifier forms

DPP and SDWA re-address a base instruction's operands in a second dword; they are not opcodes of their own, so they are counted once per base encoding rather than per form.

| Modifier | Base encodings | Forms they apply to | Decoder parses it | DXIL path reads it |
|---|---|---|---|---|
| DPP16 | VOP1, VOP2, VOPC | 322 | yes | yes |
| DPP8 | VOP1, VOP2, VOPC | 322 | yes | yes |
| SDWA | VOP1, VOP2, VOPC | 322 | yes | yes |

- The form count is the base encodings' own: the modifier is a dword, so a form is "decoded" here when the base opcode is and the modifier's fields are parsed.

### Formats

| Set | Handled | Of | Mapped to a Vulkan format | What it means |
|---|---|---|---|---|
| Typed-buffer formats (`buffer_load_format_*`) | 77 | 77 | 0 | the emitter converts the channel widths and numeric kind itself, so no Vulkan format is chosen for these
| Texture formats with a pixel layout (DXGI numbers) | 98 | 98 | 0 | `victory-formats` knows the bytes and block size of these; the host view kind is vkd3d-proton's business
| Image formats (MIMG) | — | — | 0 | the format is in the descriptor the title sets, not in the instruction, so there is nothing for the shader side to map

- "Mapped to a Vulkan format" is 0 distinct format names in this tree, which is the honest answer for a stack that hands formats to vkd3d-proton and converts typed-buffer reads in the shader instead.

### Console modules (HLE)

| Module | DLL names served | Exports | Stubs | Declared |
|---|---|---|---|---|
| files | 4 | 31 | 0 | ████████████ 100 % |
| net | 2 | 31 | 0 | ████████████ 100 % |
| kernel | 2 | 19 | 1 | ███████████░ 95 % |
| xmem | 1 | 13 | 0 | ████████████ 100 % |
| audio | 1 | 9 | 0 | ████████████ 100 % |
| xgameruntime | 1 | 8 | 1 | ██████████░░ 88 % |
| d3d12x | 1 | 6 | 0 | ████████████ 100 % |
| apu | 1 | 5 | 1 | ██████████░░ 80 % |
| dsp | 1 | 5 | 0 | ████████████ 100 % |
| device type | 1 | 4 | 0 | ████████████ 100 % |
| lifecycle | 0 | 4 | 0 | ████████████ 100 % |
| xg | 1 | 2 | 0 | ████████████ 100 % |
| dstorage | 1 | 1 | 0 | ████████████ 100 % |
| gameinput | 1 | 1 | 0 | ████████████ 100 % |
| media | 1 | 1 | 0 | ████████████ 100 % |
| **All modules** | | **140** | **3** | **███████████░ 98 %** |

- Exports are `hle_export!` entries in each module's registry; a stub is an export whose body returns `E_NOTIMPL` or logs "not implemented". Counted from source, so a module we wrote a case for still counts as ours even if no title has called it.

### Forms still needed

33 reference forms are not in the decoder's tables at all: `v_add_u32`, `v_sub_u32`, `v_subrev_u32`, `v_addc_co_u32`, `v_subb_co_u32`, `v_subbrev_co_u32`, `v_add_u16_e64`, `v_sub_u16_e64`, `v_add_i16`, `v_sub_i16`, `v_sub_i32`, `v_add_i32`, `v_cmp_lg_i32`, `v_cmp_tru_i32`, `v_cmp_lg_i16`, `v_cmpx_lg_i32`, `v_cmpx_tru_i32`, `v_cmpx_lg_i16`, `v_cmp_lg_i64`, `v_cmp_tru_i64`, `v_cmp_lg_u16`, `v_cmpx_lg_i64`, `v_cmpx_tru_i64`, `v_cmpx_lg_u16`, `v_cmp_lg_u32`, `v_cmp_tru_u32`, `v_cmpx_lg_u32`, `v_cmpx_tru_u32`, `v_cmp_lg_u64`, `v_cmp_tru_u64`, `v_cmpx_lg_u64`, `v_cmpx_tru_u64`, `exp`.

492 decoded forms have no DXIL-path case; the first 60, by encoding, are `s_wqm_b32`, `s_wqm_b64`, `s_brev_b64`, `s_flbit_i32_i64`, `s_sext_i32_i8`, `s_sext_i32_i16`, `s_bitset0_b64`, `s_bitset1_b64`, `s_and_saveexec_b64`, `s_xor_saveexec_b64`, `s_andn2_saveexec_b64`, `s_orn2_saveexec_b64`, `s_nor_saveexec_b64`, `s_xnor_saveexec_b64`, `s_quadmask_b32`, `s_quadmask_b64`, `s_movrels_b64`, `s_movreld_b32`, `s_movreld_b64`, `s_mov_fed_b32`, `s_andn1_saveexec_b64`, `s_orn1_saveexec_b64`, `s_andn1_wrexec_b64`, `s_andn2_wrexec_b64`, `s_bitreplicate_b64_b32`, `s_and_saveexec_b32`, `s_xor_saveexec_b32`, `s_andn2_saveexec_b32`, `s_orn2_saveexec_b32`, `s_nor_saveexec_b32`, `s_xnor_saveexec_b32`, `s_andn1_saveexec_b32`, `s_orn1_saveexec_b32`, `s_andn1_wrexec_b32`, `s_andn2_wrexec_b32`, `s_movrelsd_2_b32`, `s_bfm_b64`, `s_bfe_u64`, `s_bfe_i64`, `s_absdiff_i32`, `s_pack_ll_b32_b16`, `s_pack_lh_b32_b16`, `s_pack_hh_b32_b16`, `s_getreg_b32`, `s_subvector_loop_begin`, `s_subvector_loop_end`, `s_bitcmp0_b32`, `s_bitcmp1_b32`, `s_bitcmp0_b64`, `s_bitcmp1_b64`, `s_setkill`, `s_sethalt`, `s_trap`, `s_load_dwordx2`, `s_load_dwordx4`, `s_load_dwordx8`, `s_load_dwordx16`, `s_scratch_load_dword`, `s_scratch_load_dwordx2`, `s_scratch_load_dwordx4`. The rest are in the translator's gaps, and the list is generated, so it shrinks as cases land.

- Read the two counts together: a form can be decoded and have no case (a shader using it stops at translation), or have a case and still be wrong (nothing here runs).

Raw numbers: [progress/isa.json](progress/isa.json). History: [progress-history.json](progress/progress-history.json). Generated by `tools/progress/shader_scan.py`; the page is rebuilt on pushes to `main`, daily, and on demand by the Pages workflow.
<!-- isa:end -->
