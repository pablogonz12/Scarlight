# How we test

Scarlight is a compatibility layer, so "it looks right" and "it is faster" are separate claims, and
both have to be shown. The rule is simple: nothing is published on the strength of one run, and the
cheapest check that can answer a question runs first.

## The ladder

Each step is cheaper than the one below it, and a change stops at the first step that fails.

**1. Unit tests and lint.** `cargo test --workspace` and clippy across the workspace, both feature
sets, warnings as errors.

**2. Offline shader translation.** The test title's world shader set — **751 programs** — is translated
to DXIL and checked with the DXIL validator without the title, in about **21 s** of CPU for the 278
that compile on this machine. Two numbers come out of this step: how many programs translate, and
how many produce valid intermediate code.

This step says nothing about a Mesa driver: it creates no pipeline on any GPU, so an ANV- or
RADV-specific compile failure cannot appear here. That check is separate: the world set is compiled
into pipelines on the target Mesa driver (RADV today, ANV planned) through vkd3d-proton, and the
refused or slow ones are reported with their keys and timings. The per-family matrix is listed as
planned work in [ROADMAP.md](ROADMAP.md); until it runs, a driver-specific failure shows up in
step 4, as a stall or a missing draw.

**3. Per-program output diff.** A change to the translator is diffed program by program against the
previous build. The diff lists every program whose output changed; an unexplained change fails the
check. This is what catches "it fixed one shader and broke forty" before a frame is drawn.

**4. The fixed test scene.** One scene in the world, reached the same way every time: the first idle
view. A run is instrumented and stops itself there. The number we quote is the **median frame rate at
that scene**, and it is only compared against runs that reached the same scene. Runs that did not get
there produce no frame-rate answer at all, and are not quietly averaged in.

**5. Two runs per side.** A change and its baseline each get two runs on the scene. The same build
has varied by about 10 % between runs here, so a single run proves nothing, and a gap smaller than
that variation means "no evidence yet".

**6. Picture before speed.** A frame-rate win that costs picture quality is reverted. Picture
verdicts need eyes: frames are captured and looked at for faces, walls and close-up detail, because
the two problems that matter most here (a missing face, a wall that does not draw) do not move a
summary statistic much.

## What is measured, and what is not

- We quote **median frame rate at the fixed scene**, time to the world, and the offline program
  count. Each figure carries the build it came from and how many runs.
- We do not compare frame rates across different scenes, or average a cutscene and the world
  together: those numbers move for reasons that have nothing to do with the code.
- Stalls are tracked separately from the median, because a 3 s stall hides in an average.
- Driver compilation is measured with Mesa's own statistics
  (`VK_KHR_pipeline_executable_properties`), so "this pipeline is slow" is a number, not a guess.

## Why this matters for Mesa

The same ladder runs on any Mesa GPU. A driver-specific failure shows up in step 2 or 3 — compile or
validate — before anyone boots the title, and the pipeline statistics in step 5 say which driver
behaviour to look at. That is the advantage of an open driver: a bad result can be traced into Mesa
and fixed there, instead of being worked around forever.

## Reporting a run

A report is numbers and text, no screenshots, game files or hashes of game data
([asset policy](asset-policy.md)). Send:

- **GPU**, and the device section of `vulkaninfo --summary`: `driverName` (`radv`,
  `Intel open-source Mesa driver` for ANV, `nvk`), `driverVersion` and `apiVersion`;
- **Mesa version** and where it came from: distribution package, `mesa-git`, a local build;
- **kernel** (`uname -r`), **distribution** and session (Wayland or X11);
- **Proton build** and how you call it, plus your **vkd3d-proton** version and how it is selected;
- the **commit** you built;
- how far it got, **time to the world**, and the frame lines if your run prints them;
- **what looked wrong**, and what looked right;
- the **log tail** with `VICTORY_LOG=info`: `does not translate`, dropped or "being made" pipelines,
  device-removed and crash lines.

Use the [test report form](https://github.com/pablogonz12/Scarlight/issues/new?template=test-report.yml)
for a run and the [bug report form](https://github.com/pablogonz12/Scarlight/issues/new?template=bug-report.yml)
for a crash, a hang or a wrong picture. One report per commit, so results can be compared.
