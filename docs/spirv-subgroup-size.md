# The SPIR-V path's subgroup size

The native SPIR-V emitter (`crates/victory-shader/src/emit/spirv.rs`, draft) turns
a console compute program straight into SPIR-V, with no DXIL and no dxil-spirv in
between. This page is the contract between that module and whatever creates the
`VkComputePipeline` from it, and the reason it exists at all.

## Why

The console runs a program's threads in waves of 32 or 64 lanes, and the program
reads other lanes of *its own wave*: DPP source modifiers, `v_readlane_b32` /
`v_writelane_b32`, `v_mbcnt_lo/hi_u32`, `ds_swizzle_b32`, `ds_bpermute_b32`. The
DXIL path carries that wave size into the host shader (`Psv::wave_size`, the ISA
record's `wave32` flag, narrowing a 64-lane console wave to 32 host lanes only
where the cross-lane code stays inside 32-lane halves).

The SPIR-V path had no such carrier: a module could be created on any subgroup
size the device happened to pick, and RADV may run a 64-lane program as 32-lane
subgroups. Then `SubgroupLocalInvocationId` (the host's lane) and the console's
lane index disagree by half a wave, and every cross-lane read lands in the wrong
place — silently, with the module still valid.

## The contract

The emitter returns the words together with what the pipeline needs:

```rust
pub struct SpirvProgram {
    pub words: Vec<u32>,
    pub wave_size: u32,      // 32 or 64, the console's wave
    pub local_size: [u32; 3] // the module's own LocalSize
}
```

`SpirvProgram::subgroup_plan()` turns that into the pipeline's side: the required
subgroup size, whether full subgroups can be required, and the reason when they
cannot. A program that reads other lanes of its wave also declares
`GroupNonUniform` and `GroupNonUniformShuffle`, which the DPP lowering will use.

Whoever creates the pipeline then:

1. Checks the device offer, and refuses with a clear message rather than
   silently creating a pipeline with the wrong wave size:
   - `VkPhysicalDeviceSubgroupSizeControlFeatures.subgroupSizeControl` (and
     `computeFullSubgroups` for the flag below),
   - `VkPhysicalDeviceSubgroupSizeControlProperties.minSubgroupSize` ..
     `maxSubgroupSize` (32..64 on RADV, RDNA4 here),
   - `requiredSubgroupSizeStages` contains `VK_SHADER_STAGE_COMPUTE_BIT`.
2. Chains `VkPipelineShaderStageRequiredSubgroupSizeCreateInfo` with
   `requiredSubgroupSize = wave_size` onto the `VkPipelineShaderStageCreateInfo`
   of the stage, with `VK_PIPELINE_SHADER_STAGE_CREATE_ALLOW_VARYING_SUBGROUP_SIZE_BIT`
   *not* set: the size must be exactly what was asked for.
3. Sets `VK_PIPELINE_SHADER_STAGE_CREATE_REQUIRE_FULL_SUBGROUPS_BIT` when
   `subgroup_plan().require_full_subgroups` — every subgroup complete, as a
   console wave is.

`REQUIRE_FULL_SUBGROUPS` needs `local_size[0]` to be a multiple of the subgroup
size (only x is constrained, not the group's product). Many console programs
declare a group of 1, 2, 4, 8, 16 or 32 threads in x, or 25 (a non-power of two),
and a 64-lane wave on such a group cannot be full: the subgroup size is still
pinned, the flag is left off, and the program is *reported* as
`full=0 why=local_size_x_not_a_multiple` rather than dropped quietly. Flattening
such a group to `[product, 1, 1]` would buy the flag back but moves the lane
index the program computes from `LocalInvocationId`, so it is a separate change.

## Where it is implemented

- Emitter + plan: `crates/victory-shader/src/emit/spirv.rs`, `translate_spirv_program`
  (the older `translate_spirv` still returns just the words).
- Pipeline: the offline harness, `xcl-bin/offline/vkpipe.c` (`--subgroup N`,
  prints `SUBGROUP req=… full=… local_x=… min=… max=…`), driven per program by
  `xcl-bin/offline/spirv-check.sh` from the `.meta` file `spirv-emit` writes.
  There is no other Vulkan compute pipeline creator in this tree: the game-side
  host path still creates D3D12 pipelines only, so nothing else needed changing.
- Proof on real hardware: `xcl-bin/offline/subgroup-lane.c` builds a program
  whose group is exactly one wave, has every invocation write
  `SubgroupLocalInvocationId`, `LocalInvocationIndex` and `SubgroupSize`, pins the
  subgroup with the chain above and checks that the first two agree for all 64
  (and all 32) lanes. Ask for 32 lanes on a 64-thread group instead and the same
  test fails on 32 of 64 lanes, which is exactly the bug this pinning prevents.

## DPP row ops

`v_mov_b32_dpp` and the rest of the DPP family lower to
`OpGroupNonUniformShuffle` / `ShuffleXor` / `QuadSwap` inside one 16-lane row of
the console wave, with the lane read from `SubgroupLocalInvocationId` — which is
the console lane exactly because the subgroup is pinned here. The controls, the
out-of-row and `bound_ctrl` rules, the row/bank write masks and the lane table
are in `docs/spirv-dpp-rows.md`.

## Credits

The pinning was suggested by u/General_Pool_3859 on r/vulkan
([thread](https://www.reddit.com/r/vulkan/comments/1y5e5gi/)).
