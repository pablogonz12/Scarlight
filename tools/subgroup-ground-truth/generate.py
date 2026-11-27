#!/usr/bin/env python3
"""Generate independent compute workloads; no input data is required."""
import argparse
import json
import pathlib
import subprocess

OPS = {
    "broadcast": "subgroupBroadcast(x, 0)",
    "broadcast_first": "subgroupBroadcastFirst(x)",
    "shuffle": "subgroupShuffle(x, lane ^ 1u)",
    "shuffle_xor": "subgroupShuffleXor(x, 1u)",
    "shuffle_up": "subgroupShuffleUp(x, 1u)",
    "shuffle_down": "subgroupShuffleDown(x, 1u)",
    "ballot": "subgroupBallot((x & 1u) != 0u).x",
    "ballot_bit_count": "subgroupBallotBitCount(subgroupBallot((x & 1u) != 0u))",
    "ballot_inclusive": "subgroupBallotInclusiveBitCount(subgroupBallot((x & 1u) != 0u))",
    "ballot_exclusive": "subgroupBallotExclusiveBitCount(subgroupBallot((x & 1u) != 0u))",
    "ballot_find_lsb": "subgroupBallotFindLSB(subgroupBallot(true))",
    "ballot_find_msb": "subgroupBallotFindMSB(subgroupBallot(true))",
    "ballot_bit_extract": "uint(subgroupBallotBitExtract(subgroupBallot(true), lane))",
    "all": "uint(subgroupAll((x & 1u) != 0u))",
    "any": "uint(subgroupAny((x & 1u) != 0u))",
    "all_equal": "uint(subgroupAllEqual(x))",
    "add": "subgroupAdd(x)",
    "inclusive_add": "subgroupInclusiveAdd(x)",
    "exclusive_add": "subgroupExclusiveAdd(x)",
    "mul": "subgroupMul(x)",
    "min": "subgroupMin(x)",
    "max": "subgroupMax(x)",
    "and": "subgroupAnd(x)",
    "or": "subgroupOr(x)",
    "xor": "subgroupXor(x)",
    "elect": "uint(subgroupElect())",
    "quad_broadcast": "subgroupQuadBroadcast(x, 0)",
    "quad_swap_horizontal": "subgroupQuadSwapHorizontal(x)",
    "quad_swap_vertical": "subgroupQuadSwapVertical(x)",
    "quad_swap_diagonal": "subgroupQuadSwapDiagonal(x)",
}
CONTEXTS = {
    "straight": "result = OP;",
    "if": "if ((x & 1u) != 0u) { result = OP; }",
    "loop": "for (uint i = 0; i < 4; i++) { if ((x & 3u) == i) break; result ^= OP; }",
    "nested": "if ((x & 1u) != 0u) { if ((x & 2u) != 0u) { result = OP; } }",
}

HEADER = """#version 450
#extension GL_KHR_shader_subgroup_basic : require
#extension GL_KHR_shader_subgroup_vote : require
#extension GL_KHR_shader_subgroup_ballot : require
#extension GL_KHR_shader_subgroup_shuffle : require
#extension GL_KHR_shader_subgroup_shuffle_relative : require
#extension GL_KHR_shader_subgroup_arithmetic : require
#extension GL_KHR_shader_subgroup_quad : require
layout(local_size_x = 64) in;
layout(set = 0, binding = 0, std430) buffer Data { uint data[]; };
void main() {
    uint index = gl_GlobalInvocationID.x;
    uint lane = gl_SubgroupInvocationID;
    uint x = data[index];
    uint result = x;
    BODY
    data[index] = result;
}
"""


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--compiler", default="glslangValidator")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    cases = []
    for op, expression in OPS.items():
        for context, template in CONTEXTS.items():
            name = f"{op}-{context}"
            source = args.output / (name + ".comp")
            binary = args.output / (name + ".spv")
            source.write_text(HEADER.replace("BODY", template.replace("OP", expression)))
            subprocess.run([args.compiler, "-V", "--target-env", "vulkan1.2",
                            "-o", str(binary), str(source)], check=True,
                           stdout=subprocess.DEVNULL)
            cases.append({"name": name, "operation": op, "context": context,
                          "source": source.name, "spirv": binary.name})
    (args.output / "manifest.json").write_text(json.dumps(cases, indent=2) + "\n")
    print(f"Generated {len(cases)} cases")


if __name__ == "__main__":
    main()
