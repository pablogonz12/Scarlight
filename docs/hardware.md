# Hardware

ScarLight is built for Mesa's open Vulkan drivers, not for one card. This page records what has
actually been run, what we expect, and what nobody has looked at yet.

## Status

| **Tested** | **Expected** | **Unknown** |
|---|---|---|
| **RX 9070 XT (RDNA4) · RADV**<br>The only hardware this has been run on: every published number comes from it, Mesa 26.3.1 | **RDNA3, RDNA2, RDNA1 (RX 7000 / 6000 / 5000) and Vega · RADV**<br>Same driver, older generations. Not yet checked.<br><br>**Arc A-series, Arc B-series, Xe2 · ANV**<br>A second open driver. Not yet checked. | **Intel Gen9 and older integrated · ANV**<br>**Polaris and older (GCN 4 and earlier) · RADV**<br>**AMD APUs**<br>Any Mesa older than the one we test<br>Not yet checked. |

**Tested** means a full run on the fixed test scene, with the picture looked at, on a named Mesa
version. **Expected** means the card has a Mesa driver this project is aimed at and no known blocker.
**Unknown** covers everything else: it may work, and nobody has said. The last two columns move to
Tested only with a report, so treat "Expected" as "we think so", never as a promise.

NVIDIA's NVK driver is a roadmap item, not an expected target yet: see [ROADMAP.md](ROADMAP.md).

Native Windows and NVIDIA's proprietary driver are not supported. The build is a Linux project: it
runs through Wine and vkd3d-proton on Mesa, and no path here exists for a closed driver.

## Compile check per family

The same 751 world programs, compiled into real pipelines on each family's Mesa driver. Families we
do not own go through Mesa's noop drm-shim, which reports a fake device of that family; RADV and ANV
then build a real physical device with that family's ISA and limits. **Offline (stub)** means the
answer comes from the compiler, on a device nobody has run.

| Family | Driver | Pipelines created | Median per program | Status |
|---|---|---|---|---|
| RDNA4 (gfx1201), this card | RADV | 751 / 751 | 38 ms | **Ran** (test machine) |
| RDNA4 (gfx1201), stub control | RADV | 751 / 751 | 30 ms | Offline (stub) |
| RDNA3 (gfx1100) | RADV | 751 / 751 | 33 ms | Offline (stub) |
| RDNA2 (gfx1030) | RADV | 751 / 751 | 33 ms | Offline (stub) |
| RDNA1 (gfx1010) | RADV | 669 / 751 | 22 ms | Offline (stub); the 82 mesh programs are skipped, no `VK_EXT_mesh_shader` |
| Vega (gfx900) | RADV | 669 / 751 | 23 ms | Offline (stub); the 82 mesh programs are skipped, that family aborted the driver |
| Arc B (bmg, Xe2) | ANV | 751 / 751 | 35 ms | Offline (stub) |
| Arc A (dg2) | ANV | 750 / 751 | 78 ms | Offline (stub); one miss, the stub's query gap |
| Xe-LPG (mtl) | ANV | 750 / 751 | 69 ms | Offline (stub); one miss, the same gap |

A pass means the driver's compiler accepted every pipeline we asked for. It does not mean the program
draws correctly, and it does not prove a card has the features at run time: on RDNA1 all 82 mesh
programs create, yet RADV does not advertise `VK_EXT_mesh_shader` there, and on Vega asking for one
ended the process inside RADV instead of being refused.

Both families below RDNA2 are now handled before the driver sees a mesh pipeline. In a run the check
reads the D3D12 mesh-shader tier and, when that says yes, the adapter's device id: RADV exposes
`VK_EXT_mesh_shader` from RDNA2 up, so a Vega or Navi 1x adapter is named by id and its mesh pipeline
is refused once, with the family in the warning, instead of being created. That pipeline is gone for
the run: its draws are lost while everything else goes on, where before it went to a driver that
either refused it or, on Vega, aborted the process and took every other draw with it. Nothing changes
on RDNA2 and newer — APUs included: Van Gogh and Mendocino are RDNA2, not Vega, and the RX 9070 XT
still creates all 751. The same two families are the ones the offline matrix above skips.

The one Intel miss on Arc A and Xe-LPG stops on the shim's own `drm_i915_query_item` gap, not on a
shader the compiler refused.
Only RDNA4 has actually been run, and every row above changes to Tested the day a report shows a run
on it.

## What we need from a report

A report only needs numbers and text: no screenshots, game files or hashes of game data
([asset policy](asset-policy.md)). The list of what to send is in the README under
[help us test](../README.md#help-us-test), and a form asks for each item:

- [test report](https://github.com/pablogonz12/Scarlight/issues/new?template=test-report.yml) for a
  run, good or bad;
- [bug report](https://github.com/pablogonz12/Scarlight/issues/new?template=bug-report.yml) for a
  crash, a hang or a wrong picture.

A row moves when a report shows the build reaching the world and the picture being right enough to
look at. A card that fails is at least as useful: a failure on a gated path is our bug, and a failure
in the driver goes to Mesa with the report attached.

## Known driver-side questions

These are open questions, not conclusions. Each one is written down so a report can settle it:

- **Mesa minimum.** We test on one Mesa version. The oldest version that works is to be confirmed.
- **Kernel minimum.** `amdgpu`, `i915` and `xe` all change; the oldest kernel that works is to be
  confirmed.
- **Vulkan 1.3 features.** vkd3d-proton needs Vulkan 1.3. Which optional features a driver must also
  expose (`VK_EXT_robustness2`, descriptor buffers, host memory import) is still being mapped out.
- **Wave width.** Some console programs assume 64-lane waves. On a 32-lane host they are emulated;
  which Mesa GPUs hit that path most is unmeasured.
