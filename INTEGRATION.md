# Integration branch

This branch is VIctory `main` plus open pull requests and fixes from other branches, each one tested together, aiming for a build that boots and plays as far as possible. Every PR is merged as is, so its commits keep their authors. Credit goes to the people below; please review and merge their PRs upstream.

Maintained by @pablogonz12. Tested on Linux (RX 9070 XT, RADV, GE-Proton / vkd3d-proton master).

## The base: the fxceless fork

This branch is not built on VIctory `main` any more. Its starting point is
[@fxceless](https://github.com/fxceless)'s fork (GPL-3.0, based on VIctory `main`
`ffe310e`), whose two commits are what first make the world render lit on a PC. Scarlight's own
history is merged on top of that base and its fixes are re-added one at a time.

| Commit | Author | What it brought |
|---|---|---|
| `578d459` | @fxceless | One squashed commit on VIctory `main` (+32,334/−1,349): meshlet culling read scene buffers as the wrong view kind, lights packed by compute were read back through empty copies, implicit-level samples outside pixel shaders (730 compute programs), gathers at a named mip read level 0, depth/stencil twins, descriptor-table exhaustion, card-memory big-buffer copies, relay/late-wait submission, the ray-tracing structure builder |
| `7b0f736d` | @fxceless (+ @ZazaJr24 `d279e69`, `e9765a7`) | shadow/trace translate, pipeline waits, and the wave64 barrier gate that fixes the device loss the previous wave64 update introduced |

Measured with this base (24 Nov 2026): offline world set **766 of 768** programs translate, validate
as SPIR-V and build a RADV pipeline; the full archive reaches **25,526 of 25,694** programs (99.3%)
against VIctory `main`'s 24,114 of 25,562; the world picture is lit on RX 9070 XT / RADV / Wine at
about 1–4 fps (test run `fx-d3dc-1`).

## Wine's `d3dcompiler_47`

@deadkid14 and @ItsYusei99 found that Wine's built-in `d3dcompiler_47` rejects helper HLSL with
`E5005` (no `InterlockedMax` on `RWByteAddressBuffer`, `RootSignature` attribute ignored), which
silently drops every indirect draw and dispatch and leaves the picture black (VIctory issues
[#909](https://github.com/Marvisak/VIctory/issues/909), [#170](https://github.com/Marvisak/VIctory/issues/170);
fixes: PR [#994](https://github.com/Marvisak/VIctory/pull/994), the `VICTORY_DXC` fallback). Our own
measurement agrees: the same tree stays black with Wine's built-in compiler and renders lit with a
native `d3dcompiler_47` installed. Building the indirect-argument clamp with our own emitter
(Scarlight `ffa13108`, VIctory [#1470](https://github.com/Marvisak/VIctory/pull/1470)) is the way out
without a native compiler.

## Merged pull requests

| PR | Author | Title | Why |
|---|---|---|---|
| [#177](https://github.com/Marvisak/VIctory/pull/177) | @Ortomanic | Shaders: key heap handles by element type (atomic on a float-read UAV) | valid SPIR-V for atomics on float-read images (Linux) |
| [#190](https://github.com/Marvisak/VIctory/pull/190) | @LaunchDoesntCode | GPU: build the indirect-argument clamp with our own writer (no shader compiler at run time) | indirect-argument clamp without a shader compiler (Wine) |
| [#439](https://github.com/Marvisak/VIctory/pull/439) | @r4tedx | Shaders: one console wave sharing LDS runs in the console flow on 32-lane hosts | +3 world shaders |
| [#453](https://github.com/Marvisak/VIctory/pull/453) | @AntoninMichelq | Shaders: pixel barycentrics through SV_Barycentrics (values and computed interpolation) | barycentrics: +2 world pixel shaders |
| [#454](https://github.com/Marvisak/VIctory/pull/454) | @ruvenesh | Loader: opt-in VICTORY_NO_WATCHDOG hook for the title's watchdog (build 1.0.2.1) | VICTORY_NO_WATCHDOG for long world runs |
| [#457](https://github.com/Marvisak/VIctory/pull/457) | @jeremygold02 | fix(shader): accept overwritten VGPR inputs in counted indexed writes | +1 world shader |
| [#493](https://github.com/Marvisak/VIctory/pull/493) | @hophaver | GPU: bind root SRVs as one-view tables bounded to their host copy | root SRVs as bounded tables (world GPU faults, #752) |
| [#539](https://github.com/Marvisak/VIctory/pull/539) | @SAitken91 | Shaders: 64-bit image atomics | 64-bit image atomics: +4 world pixel shaders |
| [#571](https://github.com/Marvisak/VIctory/pull/571) | @SAitken91 | Shaders: LDS byte and half accesses | LDS bytes and halves: +1 world shader |
| [#573](https://github.com/Marvisak/VIctory/pull/573) | @banbanzhige | Shaders: lower ds_bpermute_b32 | ds_bpermute: +2 world shaders |
| [#579](https://github.com/Marvisak/VIctory/pull/579) | @wbkepler | GPU: VICTORY_SKIP_WAVE64 diagnostics switch for wave64 programs | VICTORY_SKIP_WAVE64 diagnostics |
| [#638](https://github.com/Marvisak/VIctory/pull/638) | @claude-the title | GPU diagnostics: find what the world hang waits for, and look under a black frame | world-hang diagnostics |
| [#704](https://github.com/Marvisak/VIctory/pull/704) | @r4tedx (carries [#193](https://github.com/Marvisak/VIctory/pull/193), @n-romanovich) | Shaders: structured buffers of unknown stride | world shaders 684 -> 692 of 751, all valid SPIR-V; conflicts with our draw.rs/pipeline/host.rs/lower.rs changes resolved keeping both sides |
| [#393](https://github.com/Marvisak/VIctory/pull/393) | @kxiserino | GPU: page-granular refresh ranges, journal fast path | with #124, 2 world runs each on RX 9070 XT / Wine: copies 2431-2571 -> 1732-2036 ms/frame, write-watch checks 1338-1351 -> 829-942 ms/frame, no device removed |
| [#124](https://github.com/Marvisak/VIctory/pull/124) | @TG0000 | GPU: remember compared unchanged pages | see #393 |
| [#383](https://github.com/Marvisak/VIctory/pull/383) | @xmvo | Shader: LDS bytes and halves, constant sample offsets, descriptor-data | buffer descriptors read by `s_buffer_load` and as data: +231 of 1,692 #798 programs, +4 world shaders (677 to 681 of 751). Conflicts resolved: main's LDS byte/half code kept, the PR's tests updated to current signatures |
| [#592](https://github.com/Marvisak/VIctory/pull/592) | @SAitken91 | Shaders: second rows of packed 16-bit pixel inputs | full-archive run: vertex+pixel 13,144 -> 13,271 (+127), "a packed input half no element holds" 166 -> 0; world set unchanged |
| [#683](https://github.com/Marvisak/VIctory/pull/683) | @Halfect | Shaders: zero unavailable mixed-precision FMA halves | full-archive run: vertex+pixel 13,144 -> 13,152 (+8), "the first half of an interpolation" 28 -> 20; world set unchanged |
| fork commit [61474f6](https://github.com/MartinIndra02/victory/commit/61474f6) | @Noname0202, on @MartinIndra02's branch (issue [#783](https://github.com/Marvisak/VIctory/issues/783), @rocobalt1) | GPU: `VirtualQuery` region fallback and partial-copy progress in `copy_mapped`/`write_mapped` | the copy fallback skips contiguous unmapped regions instead of failing page by page; not measured on its own |

## Our own fixes

| Fix | Upstream |
|---|---|
| Shaders: a table descriptor read only as numbers takes its raw view's values (816/820 of that class; world 681 -> 684/751) | not sent yet |
| GPU: no blending on integer render targets in host pipelines | [#806](https://github.com/Marvisak/VIctory/issues/806) |

## Known test failure

None on this base: `cargo test -p victory-shader --lib` is 158 passed, 0 failed, 2 ignored. Tests
that need a real GPU (`vkd3d-proton`, a D3D12 device) stay out of CI's Windows runner the same way
they always have.

## Measured

With the fxceless base: offline world set 766 of 768; full archive 25,526 of 25,694; world lit on
RX 9070 XT / RADV / Wine with a native `d3dcompiler_47`, 1–4 fps.
