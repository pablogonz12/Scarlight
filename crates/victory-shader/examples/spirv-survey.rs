//! Local tool (not upstream): survey the dumped console programs for the
//! SPIR-V spike -- per program, its size, its opcode set and what its
//! resources are; aggregate how many programs each opcode appears in.
//! usage: spirv-survey <dump dir> [stage filter]

use std::collections::{BTreeMap, BTreeSet};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = &args[1];
    let filter = args.get(2).cloned().unwrap_or_default();
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            let n = p.file_name()?.to_string_lossy().to_string();
            (n.ends_with(".dxbc") && n.contains(&filter)).then_some((n, p))
        })
        .collect();
    names.sort();
    // Programs that use each opcode, and the total instruction count of it.
    let mut per_op: BTreeMap<String, (u32, u64)> = BTreeMap::new();
    let mut rows = Vec::new();
    for (name, path) in &names {
        let bytes = std::fs::read(path).unwrap();
        let r = std::panic::catch_unwind(|| survey(&bytes));
        match r {
            Ok(Ok(s)) => {
                for (op, n) in &s.ops {
                    let e = per_op.entry(op.clone()).or_default();
                    e.0 += 1;
                    e.1 += u64::from(*n);
                }
                rows.push(format!(
                    "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    s.stage,
                    s.insts,
                    s.ops.len(),
                    s.scalar,
                    s.descs,
                    s.exits,
                    s.ops.keys().cloned().collect::<Vec<_>>().join(",")
                ));
            }
            Ok(Err(e)) => rows.push(format!("{name}\tFAIL\t{e}")),
            Err(_) => rows.push(format!("{name}\tPANIC")),
        }
    }
    let out = std::env::var("SURVEY_OUT").unwrap_or_else(|_| "/tmp/oc/survey.tsv".into());
    std::fs::create_dir_all(std::path::Path::new(&out).parent().unwrap()).ok();
    std::fs::write(&out, rows.join("\n") + "\n").unwrap();
    let mut ops: Vec<_> = per_op.into_iter().collect();
    ops.sort_by_key(|&(_, (p, n))| std::cmp::Reverse((p, n)));
    let mut text = String::new();
    for (op, (p, n)) in &ops {
        text.push_str(&format!("{op}\t{p}\t{n}\n"));
    }
    let out2 = std::env::var("SURVEY_OPS").unwrap_or_else(|_| "/tmp/oc/survey-ops.tsv".into());
    std::fs::write(&out2, text).unwrap();
    println!(
        "programs={} opcodes={} rows={out} ops={out2}",
        names.len(),
        ops.len()
    );
}

struct S {
    stage: String,
    insts: usize,
    exits: usize,
    scalar: usize,
    descs: usize,
    ops: BTreeMap<String, u32>,
}

fn survey(bytes: &[u8]) -> Result<S, String> {
    let c = victory_shader::Container::parse(bytes).map_err(|e| e.to_string())?;
    let dxil = c.part(victory_shader::FourCC::DXIL).ok_or("no DXIL part")?;
    let natives = victory_shader::native_programs(dxil.data).map_err(|e| e.to_string())?;
    let native = natives
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                victory_shader::HwStage::Vs
                    | victory_shader::HwStage::Ps
                    | victory_shader::HwStage::Cs
                    | victory_shader::HwStage::Ms
            )
        })
        .ok_or("no program")?;
    let p =
        victory_shader::ir::build_with(&native.patched_code(), native.wave64, native.vgprs, false)
            .map_err(|e| e.to_string())?;
    let mut ops: BTreeMap<String, u32> = BTreeMap::new();
    let mut all = BTreeSet::new();
    let mut insts = 0;
    let mut exits = 0;
    for b in &p.blocks {
        exits += 1;
        for i in &b.insts {
            insts += 1;
            let n = i.name().to_string();
            *ops.entry(n.clone()).or_default() += 1;
            all.insert(n);
        }
    }
    let st = victory_shader::structure::structure(&p);
    let st = st.map_err(|e| e.to_string())?;
    let res = victory_shader::resource::track(&p, &native.layout, native.stage, Some(&st))
        .map_err(|e| format!("resource: {e}"))?;
    let ops: BTreeMap<String, u32> = all
        .into_iter()
        .map(|n| {
            let c = ops[&n];
            (n, c)
        })
        .collect();
    Ok(S {
        stage: format!("{:?}", native.stage),
        insts,
        exits,
        scalar: res.scalar.len(),
        descs: res.descriptors.len(),
        ops,
    })
}
