//! Local tool (not upstream): translate dumped console compute containers to
//! SPIR-V with the spike emitter (`victory_shader::emit::translate_spirv_program`).
//! usage: spirv-emit <in dir> <out dir> [name filter]
//! Prints `emitted=N gap=M wave64=W wave32=V full_subgroups=F`, per-reason
//! counts, and writes `<out>/rows.tsv`
//! (`name<TAB>stage<TAB>reason<TAB>wave`). Each module gets `<out>/<name>.meta`
//! (`wave_size=N`, `local_size=x y z`, `full_subgroups=0|1`) so the pipeline
//! helper can chain the required subgroup size (see `spirv::SpirvProgram`).
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (input, out) = (&args[1], &args[2]);
    let filter = args.get(3).cloned().unwrap_or_default();
    std::fs::create_dir_all(out).unwrap();
    let mut names: Vec<_> = std::fs::read_dir(input)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            let n = p.file_name()?.to_string_lossy().to_string();
            (n.ends_with(".dxbc") && n.contains(&filter)).then_some((n, p))
        })
        .collect();
    names.sort();
    let (mut ok, mut gap) = (0, 0);
    let (mut wave64, mut wave32, mut short_groups) = (0, 0, 0);
    let mut reasons: std::collections::BTreeMap<String, u32> = Default::default();
    let mut rows = String::new();
    for (name, path) in &names {
        let bytes = std::fs::read(path).unwrap();
        let stem = &name[..name.len() - 5];
        let result =
            std::panic::catch_unwind(|| victory_shader::emit::translate_spirv_program(&bytes));
        let reason = match result {
            Ok(Ok(prog)) => {
                let mut bytes = Vec::with_capacity(prog.words.len() * 4);
                for w in &prog.words {
                    bytes.extend_from_slice(&w.to_le_bytes());
                }
                std::fs::write(std::path::Path::new(out).join(format!("{stem}.spv")), bytes)
                    .unwrap();
                let full = prog.full_subgroups();
                std::fs::write(
                    std::path::Path::new(out).join(format!("{stem}.meta")),
                    format!(
                        "wave_size={}\nlocal_size={} {} {}\nfull_subgroups={}\n",
                        prog.wave_size,
                        prog.local_size[0],
                        prog.local_size[1],
                        prog.local_size[2],
                        u8::from(full)
                    ),
                )
                .unwrap();
                ok += 1;
                let w = prog.wave_size;
                if w == 64 {
                    wave64 += 1;
                } else {
                    wave32 += 1;
                }
                if !full {
                    short_groups += 1;
                }
                let lane = if w == 64 { "wave64" } else { "wave32" };
                rows.push_str(&format!("{name}\tok\t-\t{lane}\n"));
                continue;
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => "PANIC".into(),
        };
        gap += 1;
        let reason = match reason.find(": ") {
            Some(i) if reason[..i].contains("code at") => reason[i + 2..].to_string(),
            _ => reason,
        };
        *reasons.entry(reason.clone()).or_default() += 1;
        rows.push_str(&format!("{name}\tgap\t{reason}\t-\n"));
    }
    std::fs::write(std::path::Path::new(out).join("rows.tsv"), &rows).unwrap();
    println!(
        "emitted={ok} gap={gap} wave64={wave64} wave32={wave32} full_subgroups={}",
        ok - short_groups
    );
    println!("no_full_subgroups={short_groups}");
    let mut v: Vec<_> = reasons.into_iter().collect();
    v.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    for (r, n) in v.into_iter().take(25) {
        println!("  {n:4} {r}");
    }
}
