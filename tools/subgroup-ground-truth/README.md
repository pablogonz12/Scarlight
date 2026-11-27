# Subgroup compilation probe

This is an **incomplete diagnostic**, not yet a ground-truth semantic test.
It generates GLSL/SPIR-V workloads, captures Navi 21 machine code from a
locally instrumented Mesa 25.3.0 RADV null device, and checks decoder and
early IR/structuring coverage. It does not translate or execute the shaders.
The translator requires console container, resource and register ABI metadata
that the Mesa pipeline does not supply. Inactive-lane shuffle and quad reads
and election also need defined-result rules before comparing execution.

## Observed results

Mesa 25.3.0, ACO, null Navi 21, wave64; glslang output targeting Vulkan 1.2.
Each row covers straight line, conditional, divergent-break loop and nested
conditional (four cases). `decode` includes the opcode-name lookup, and `IR`
means `ir::build` with wave64 and a 256-VGPR bound plus structurisation.
IR success does **not** establish translation support or correctness.

| Operation | Decode | Unknown opcode | IR + structure | Translated |
|---|---:|---:|---:|---:|
| broadcast, broadcast first | 8/8 | 0 | 8/8 | not attempted |
| shuffle, shuffle xor, up, down | 16/16 | 0 | 16/16 | not attempted |
| ballot and six ballot queries | 28/28 | 0 | 28/28 | not attempted |
| all, any, all equal | 12/12 | 0 | 12/12 | not attempted |
| add, inclusive add, exclusive add | 12/12 | 0 | 12/12 | not attempted |
| multiply, min, max, and, or, xor | 24/24 | 0 | 24/24 | not attempted |
| elect | 4/4 | 0 | 4/4 | not attempted |
| quad broadcast and three swaps | 16/16 | 0 | 16/16 | not attempted |
| **Total** | **120/120** | **0** | **120/120** | **not attempted** |

No unhandled opcode patterns were found by these checks. This is not proof
that any opcode has a correct translation: later translation was not run.

## Components

`generate.py OUTPUT` uses `glslangValidator` to write sources, SPIR-V and a
manifest. `decode_raw` is a `victory-shader` example for raw executable bytes.
`mesa-25.3-capture.patch` is an instrumentation patch for a **local** Mesa
25.3.0 build: with `RADV_DEBUG=shaders,nocache` it writes the compute
executable bytes to `HARNESS_ISA_PATH` and logs the wave size. It captures
the executable section, not any following constant data or the console ABI.
Do not use it for production.

`run.py` needs the local ICD JSON, an independently provided `vkpipe`
compute-pipeline helper with its `VKPIPE_TRACE` interface, the built
`decode_raw` example, the generated cases and a fresh output directory.
The helper used for the measurements is not included here. The runner
checks its null-device trace, disables implicit layers and takes a shared
Vulkan boot gate for each invocation; use only a trusted local Mesa 25.3
ICD configured with the patch. It records capture and decode failures
separately and labels translation as not attempted. All inputs are generated.
