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

A release does this itself now: the `Release` workflow
(`.github/workflows/release.yml`) builds that pair at the two pinned commits
(`VKD3D_COMMIT`, `DXIL_SPIRV_COMMIT`), records both in the tarball's `VERSION` and
`files/licenses/VERSIONS.txt`, and attaches the source of that exact tree — vkd3d-proton with the
overridden dxil-spirv — as `vkd3d-proton-source.tar.gz`. Bumping either commit is a release note.

## The fixes that minimum carries

`b540c8a2` is the floor for two separate reasons; keep both in mind when the pair is bumped
again, because a converter from before either one is a converter we must not ship.

- **The structurizer's dominator recomputation** (`b540c8a2` itself): the three node-test
  compute programs above crash the older converter in `CFGNode::find_common_dominator`.
- **[`7f5a7780`](https://github.com/HansKristian-Work/dxil-spirv/commit/7f5a7780) "Look up
  resource extended properties by tag"** (23 Nov 2026): a resource's extended properties are
  a list of (tag, value) pairs, and the stride was read from the second pair while a typed
  buffer was detected from the first — a fixed first tag assumed. A `RWByteAddressBuffer`
  carrying only `{ Atomic64Use, 1 }` therefore got a stride of 1, took the structured path
  and added the undef element offset to the address, which hangs the GPU on some drivers.
  We emit exactly that resource: `crates/victory-shader/src/emit/bitcode/opt.rs` sets
  `atomic64` for any buffer a program performs a 64-bit atomic on and it becomes the
  `Atomic64Use` property in `crates/victory-shader/src/emit/shader.rs`, while the same
  buffer is read with raw loads. The old pin (`34f0ff84`, 20 Nov) predates the fix; the
  three-program crash above and this hang are independent, so a bump that fixes only one is
  still a bad pin.

The commit is not in the DLL's strings. Check the checkout the DLL was built from, which is
the subproject inside the vkd3d-proton tree and **not** the standalone stock checkout the
offline gates use:

    git -C <vkd3d-proton tree>/subprojects/dxil-spirv merge-base --is-ancestor 7f5a7780 HEAD \
      && echo "has the fix"
