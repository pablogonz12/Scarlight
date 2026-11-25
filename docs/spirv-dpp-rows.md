# DPP row ops as subgroup shuffles

The console's wave reads other lanes through DPP (`v_mov_b32_dpp`'s second word,
the `quad_perm` / `row_*` families). This is how the SPIR-V emitter lowers them,
what the AMD RDNA2 ISA says each control means, and what runs on this machine's
RADV to check it.

The pinning the previous step landed (`docs/spirv-subgroup-size.md`: the
pipeline asks for `requiredSubgroupSize = wave size` and
`REQUIRE_FULL_SUBGROUPS`) is what makes this a shuffle at all: a host subgroup
is only the console wave when it is pinned, and then
`SubgroupLocalInvocationId` *is* the console lane a DPP read addresses.

## The model

One lane per invocation, so a DPP read becomes one shuffle:

| DPP control | instruction |
| --- | --- |
| `quad_perm:[0,1,2,3]` | nothing: the lane reads itself |
| `quad_perm` = xor 1 / 2 / 3 | `OpGroupNonUniformQuadSwap` direction 0 / 1 / 2 |
| any other `quad_perm` | `OpGroupNonUniformShuffle`, lane = `(lane & ~3) \| table[lane & 3]` |
| `row_mirror` (xor 15), `row_half_mirror` (xor 8), `row_xmask:n` (xor n) | `OpGroupNonUniformShuffleXor` with that mask |
| `row_shl:n`, `row_shr:n`, `row_ror:n`, `row_bcast15`, `row_share:n`, DPP8 | `OpGroupNonUniformShuffle` with the row's lane |

Everything stays inside a 16-lane row (`lane & ~15`), which is what the ISA
guarantees; `row_bcast31` is the one that names lane 31 of a 64-lane wave, and a
32-lane wave does not have it (a gap with that reason, not a silent
wrong answer).

The shuffle's `Id` operand is the *subgroup* invocation id, so the lane
arithmetic is done in the unsigned 32-bit domain and the module declares
`GroupNonUniform` + `GroupNonUniformShuffle` (`GroupNonUniformQuad` when a
whole-quad swap is present) and `SubgroupLocalInvocationId` — and only for a
program that has a DPP word at all.

## Out-of-row lanes, `bound_ctrl`, row/bank masks

* `row_shl`/`row_shr` read past the row: those lanes get **zero**, or **the
  lane's own value** when `bound_ctrl` is set (`OpSelect` on the row position,
  so the shuffle index is always a valid lane in both cases — a shuffle with an
  out-of-range id is undefined in SPIR-V and would still validate).
* `row_ror` wraps and `row_share` holds at the row's last lane, so neither can
  read past the row.
* `row_mask` bit `lane / 16` and `bank_mask` bit `(lane / 4) % 4` decide which
  lanes are *written*; a masked-off lane keeps the destination's previous
  value (the machine's semantics — DPP rewrites the destination in place). All
  ones is the whole wave and emits no mask at all.
* The DPP replaces the instruction's **first** source only: a second source
  naming the same register stays the lane's own value.

## What is not modelled

* EXEC is still a scalar all-ones mask in this emitter, so a DPP read from a
  lane that the machine would have masked off is not modelled, and the `fi`
  bit has nothing to act on.
* DPP controls the ISA does not define (`ctrl[7:4] = 3`, `0x144..0x14F`,
  `0x170..0x1FF`) are gaps with a reason.

## The checks

* `cargo test -p victory-shader emit::spirv` — hand-built words for every
  control family: the shuffle/xor/quad-swap choice, the lane formula (the
  xor masks, the row's end as `OpSelect` on zero and on the lane itself, DPP8's
  three-bit selectors), the row/bank write mask and the one-source rule. Every
  module goes through `spirv-val --target-env vulkan1.3`.
* `xcl-bin/offline/dpp-rows.c` (built and run by `spirv-check.sh`, wave64 and
  wave32): the same lowering on this machine's RADV, one dispatch per control,
  every lane compared against a CPU implementation of the ISA rules written
  from the lane table rather than from the emitter's formula. 19 controls,
  64/64 and 32/32 lanes agree (RX 9070 XT, RADV GFX1201).

## Coverage

Over the 751-program dump (232 compute programs translate):
`emitted=232 gap=519`, **0 `spirv-val` errors, 0 pipeline failures**, 464/464
rows ok. The count does not go up, because DPP was previously emitted as the
identity (wrong, but it translated); what goes up is that the 25 programs whose
DPP8 selectors and the row-op programs now compute the console's values instead
of the lane's own.
