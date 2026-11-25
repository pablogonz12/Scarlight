//! The console's ray-tracing structure builder, as the title's own dumped
//! containers: every stage of it translates, and the stages are told apart
//! from the programs that walk the structures by the shader crate's own
//! marks.
//!
//! `BVH_BUILDER_DUMP=<dir>` points at a dump of the title's programs (a
//! `VICTORY_DUMP_ALL` runtime folder, or the builder-only set); without it
//! there is nothing to check and the tests pass. No dump is committed.
//!
//! Why this is a test and not a note: the builder runs **all or none**
//! (`victory-gpu`'s `pipeline.rs`: one missing host pipeline turns every
//! stage off for the run), so one stage that stops translating keeps the
//! frame as dark as with the builder off. That precondition is
//! offline-checkable on any dump, so it is checked here before anyone reads
//! a game run. The rest of a dump's refusals (unrelated programs) are
//! counted and printed, not asserted on.

use std::path::{Path, PathBuf};

/// The containers in the dump, sorted by name.
fn dump() -> Option<Vec<PathBuf>> {
    let dir = std::env::var_os("BVH_BUILDER_DUMP")?;
    let mut out: Vec<PathBuf> = std::fs::read_dir(Path::new(&dir))
        .ok()?
        .filter_map(|e| {
            let p = e.ok()?.path();
            p.extension()
                .is_some_and(|x| x == "dxbc")
                .then_some(p)
        })
        .collect();
    out.sort();
    Some(out)
}

/// A container's programs, if it is a container.
fn programs(bytes: &[u8]) -> Option<Vec<crate::NativeProgram>> {
    let c = crate::Container::parse(bytes).ok()?;
    let part = c.part(crate::FourCC::DXIL)?;
    crate::native_programs(part.data).ok()
}

/// Whether a container runs the console's node test (`image_bvh*`): what the
/// programs that walk the structures are known by. Its answer is the only
/// hardware part of the title's ray tracing.
fn node_test(programs: &[crate::NativeProgram]) -> bool {
    programs.iter().any(|p| {
        crate::gfx10::decode(&p.code).is_ok_and(|code| {
            code.iter()
                .any(|i| i.name().is_some_and(|n| n.starts_with("image_bvh")))
        })
    })
}

/// Every builder stage and every node-test program translates.
#[test]
fn the_structure_builder_and_its_consumers_translate() {
    let Some(files) = dump() else { return };
    let (mut builder, mut trace, mut all, mut other) = (0, 0, 0, 0);
    let mut refused = Vec::new();
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let Some(programs) = programs(&bytes) else {
            continue;
        };
        all += 1;
        let builder_stage = programs.iter().any(crate::spill::is_spilled);
        let node_test = node_test(&programs);
        if builder_stage {
            builder += 1;
        }
        if node_test {
            trace += 1;
        }
        if !builder_stage && !node_test {
            other += 1;
            continue;
        }
        if let Err(e) = crate::emit::translate(&bytes) {
            refused.push(format!(
                "{}: {e}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    println!("{all} programs, {builder} builder stages, {trace} with the node test, {other} other");
    assert!(
        refused.is_empty(),
        "{} of {} builder or node-test programs are refused (\
         every stage of the builder runs or none does):\n{}",
        refused.len(),
        builder + trace,
        refused.join("\n")
    );
    assert!(builder > 0, "no stage of the structure builder in the dump");
}

/// The mark the GPU layer keys the whole builder on is the one this test
/// reads: a compute program that keeps its root parameters in a table in
/// memory (`victory-gpu`'s `of_builder`, `spill::is_spilled`).
#[test]
fn a_builder_stage_is_a_compute_program_with_a_spilled_user_data_table() {
    let Some(files) = dump() else { return };
    let mut seen = 0;
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let Some(programs) = programs(&bytes) else {
            continue;
        };
        if !programs.iter().any(crate::spill::is_spilled) {
            continue;
        }
        seen += 1;
        assert_eq!(programs.len(), 1, "{}: more than one program", path.display());
        assert_eq!(
            programs[0].stage,
            crate::HwStage::Cs,
            "{}: not compute",
            path.display()
        );
        assert!(
            matches!(programs[0].io, crate::native::StageIo::Cs(_)),
            "{}: no compute settings",
            path.display()
        );
    }
    assert!(seen > 0, "no stage of the structure builder in the dump");
}
