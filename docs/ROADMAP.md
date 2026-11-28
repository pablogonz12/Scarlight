# Roadmap

Two stages and one idea. The numbers below are measured on one machine (RX 9070 XT, Mesa 26.3.1
RADV, GE-Proton11-14, vkd3d-proton master) and are here to say why each piece is worth doing, not to
promise a result. No dates.

## Stage 1 — what we are doing now

The build already runs the world; the work is picture first, then the time a frame takes.

### Picture parity

Faces, full-detail characters up close, walls, no black flashes, colour as the reference. The open
problems are listed in the README's status table; nothing in stage 1 is worth a frame per second if
the picture regresses, so a speed change that costs picture is reverted.

### Pre-built pipelines, like the console

The console ships every pipeline precompiled: loading one is a deserialise, not a compile. We build
them on first draw instead.

- A measured frame spent **457 ms of 602 ms** waiting for pipelines that were still being made.
- In a run with a warm pipeline library, the same wait was **0.0 ms**, and **1,922 pipelines** were
  built in **1.3 s**.
- A miss should be a bug we can see in a log, not a stall in the middle of a frame: every world
  program and pipeline gets built before it is needed, and a pipeline the driver refuses is named
  and quarantined instead of retried every frame.

### Cache keys that survive a rebuild

The translation cache is keyed by the hash of our executable, so every rebuild throws away **751
translations** and the pipeline library with them. Keying by what actually goes into the translation
(shader and translator version, driver and device) keeps a cache useful across rebuilds, across
Mesa updates where the key does not change, and across machines.

### Mesa cache sizing

RADV's on-disk shader cache on the test machine held **1.2 GB in 312,556 files** against Mesa's
1 GiB default, so it was evicting entries and recompiling them later. Sizing `MESA_SHADER_CACHE_MAX_SIZE`
and keeping our own entries out of the eviction path is a cheap, driver-side win that no closed
driver would let us see.

### Faster asset streaming

Copies and read-back are a large share of a frame:

- copies **91.0 ms** (206 MB) and copy-back **94.5 ms** (74 MB) of a **435.8 ms** frame;
- **99.4 %** of the 1,347 watched memory ranges in one frame were unchanged, yet each one cost a
  call and a walk.

Batch the calls, skip the unchanged ranges, and move less per frame.

### Compile checks across Mesa GPU families

The offline shader check translates and validates the whole world set: **751 programs**, of which
**278 compile in 21.2 s** of CPU. That is small enough to run per driver family — RADV today, ANV
next — before anyone boots the title, so a driver-specific compile failure is a red check and not a
black frame on somebody's card.

## Stage 2 — a native Vulkan backend

Today the chain is console GPU work → our translation → Direct3D 12 → vkd3d-proton → Vulkan → Mesa.
The Direct3D 12 layer is what makes the frame expensive:

- we already own the whole D3D12 implementation the title sees (it imports three functions from its
  console D3D12 module; we implement the 41 interfaces and 128 methods below them);
- vkd3d-proton is **112,790 lines**, and what we need from it is the part behind those 128 calls;
- in the measured frame, **356 ms of 596 ms** was recording and replaying commands, with the GPU
  idle about **89 %** of the time.

So the plan is to implement the backend we call on Vulkan ourselves, starting with the parts that
are already ours:

1. **Copies and memory on Vulkan.** Buffers, images, staging and copy scheduling move to a Vulkan
   backend behind a trait, with Direct3D 12 left as the compatibility path until the new one is
   faster on the fixed scene.
2. **Zero-copy title memory.** Import the title's memory instead of copying it, through
   `VK_EXT_external_memory_host` and `VK_EXT_map_memory_placed`, with dirty tracking kept on our
   side. That removes the copies and read-back listed above and is the largest single item here.
3. **ISA to SPIR-V directly.** The console's shaders are translated to DXIL and then to SPIR-V. With
   our own backend we can emit SPIR-V directly: one translation less, and Mesa gets the program the
   console wrote.

Each step keeps the Direct3D 12 path working and is measured on the fixed test scene before it
replaces anything.

## Long-term idea, not planned

**A native Linux host, without Wine.** Most of the title's imports are Microsoft's C/C++ runtime,
which Wine provides today; replacing that is a much larger job than the GPU path and it is not on
the roadmap. It is written down because it is the logical end of "Linux-first": a Linux program, end
to end, talking to Mesa directly.
