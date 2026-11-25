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

The converter lives in the shipped `d3d12.dll`/`d3d12core.dll` pair. Build a vkd3d-proton with
the newer dxil-spirv submodule, then install that pair where the Steam tool takes it from:
`tools/steam-compat/install.sh --dlls <folder>` (or `package.sh` for a release). That installer
stages a Scarlight build too, so it wants one at
`target/x86_64-pc-windows-msvc/release/victory.exe` or a path passed with `--exe`; with only an
installed tool to hand, copy the two files straight into
`compatibilitytools.d/Scarlight/files/` and press Play again. No DXIL byte changes, so
nothing else in the shader pipeline moves. Details and the gdb trace: local
`xcl-bin/notes/dxil-spirv-crash.md` (game-derived, not in this repo).

That pair is **not** an unmodified upstream release: it carries a dxil-spirv submodule newer than
vkd3d-proton master's pin, so a release may not label it as upstream binaries or point its source
offer at the upstream tag's tree. Before a release ships this pair, record the exact vkd3d-proton
commit and the dxil-spirv submodule revision that built it, and name that same tree in the release
instructions, the generated `VERSIONS.txt` and the attached source archive.
