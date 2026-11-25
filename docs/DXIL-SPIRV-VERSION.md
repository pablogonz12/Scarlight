# dxil-spirv version: what the tool's vkd3d-proton must bundle

The translated DXIL is read by dxil-spirv inside vkd3d-proton. Three node-test compute
programs (ray-tracing BVH node tests) of the 2,912-program runtime set crash the converter
we bundled before 25 Nov 2026 with a stack overflow inside
`CFGNode::find_common_dominator` (unbounded idom recomputation around a loop header that
has two latches). Valid DXIL, older converter.

Minimum dxil-spirv: `b540c8a2` ("Cleanup recompute_dominance_frontier a bit.", upstream
master on 25 Nov 2026). The pinned `34f0ff84` (vkd3d-proton master's submodule) is too old.

Measured, offline check with `b540c8a2` as the stage-2 converter:

| set | translated | dxil-spirv + spirv-val + RADV pipeline |
|---|---|---|
| runtime set | 2912 / 2912 | 2912 / 2912 (no crashes) |
| world dump | 751 / 751 | 751 / 751 |

The converter lives in the shipped `d3d12.dll`/`d3d12core.dll` pair. Build a vkd3d-proton
with the newer submodule and point the launcher's `VKD3D_DIR` at it; no DXIL byte changes,
so nothing else in the shader pipeline moves. Details and the gdb trace: local
`xcl-bin/notes/dxil-spirv-crash.md` (game-derived, not in this repo).
