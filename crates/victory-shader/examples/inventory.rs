//! Local tool (not upstream): classify dumped console containers against the
//! shader crate's own helpers, without a GPU or a game run.
//!
//! usage: inventory <dir> [dir ...]
//!
//! Prints one TSV row per container: name, stage, programs, builder (programs
//! with a spilled user-data table, the structure builder's mark), bvh (the
//! `image_bvh*` node tests, the traversal programs' mark), threads/waves of
//! the first program, and the translation result (`ok`, or the reason).
fn main() {
    let mut n = 0;
    let mut builder = 0;
    let mut trace = 0;
    let mut with_map = 0;
    let mut ok = 0;
    println!("name\tstage\tprograms\tbuilder\tbvh\tthreads\twave\tmap\tresult");
    for dir in std::env::args().skip(1) {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !name.ends_with(".dxbc") {
                continue;
            }
            n += 1;
            let bytes = std::fs::read(&path).unwrap();
            let (mut stage, mut programs, mut spilled, mut bvh, mut threads, mut wave) =
                ("?".to_string(), 0, 0, 0, String::new(), "");
            if let Ok(c) = victory_shader::Container::parse(&bytes)
                && let Some(part) = c.part(victory_shader::FourCC::DXIL)
                && let Ok(list) = victory_shader::native_programs(part.data)
            {
                programs = list.len();
                for p in &list {
                    if victory_shader::spill::is_spilled(p) {
                        spilled += 1;
                    }
                    if let Ok(code) = victory_shader::gfx10::decode(&p.code) {
                        let k = code
                            .iter()
                            .filter(|i| i.name().is_some_and(|n| n.starts_with("image_bvh")))
                            .count();
                        if k > 0 && bvh == 0 {
                            bvh = k;
                            threads = match &p.io {
                                victory_shader::native::StageIo::Cs(cs) => {
                                    format!("{}x{}x{}", cs.threads[0], cs.threads[1], cs.threads[2])
                                }
                                _ => String::new(),
                            };
                            wave = if p.wave64 { "64" } else { "32" };
                        }
                    }
                }
                if let Some(p) = list.first() {
                    stage = format!("{:?}", p.stage).to_lowercase();
                    if threads.is_empty() {
                        threads = match &p.io {
                            victory_shader::native::StageIo::Cs(cs) => {
                                format!("{}x{}x{}", cs.threads[0], cs.threads[1], cs.threads[2])
                            }
                            _ => String::new(),
                        };
                        wave = if p.wave64 { "64" } else { "32" };
                    }
                }
            }
            let mut map = 0;
            let result = match std::panic::catch_unwind(|| victory_shader::emit::translate(&bytes))
            {
                Ok(Ok(host)) => {
                    ok += 1;
                    // Title memory by raw address: the directory (SLOT_MEMORY)
                    // or the faults buffer (SLOT_FAULTS) is in the root ABI.
                    if host.root_srvs.contains(&254) || host.root_uavs.contains(&253) {
                        map = 1;
                    }
                    "ok".to_string()
                }
                Ok(Err(e)) => {
                    let text = e.to_string();
                    match text.find(": ") {
                        Some(i) if text[..i].contains("code at") => text[i + 2..].to_string(),
                        _ => text,
                    }
                }
                Err(_) => "PANIC".to_string(),
            };
            if spilled > 0 {
                builder += 1;
            }
            if bvh > 0 {
                trace += 1;
            }
            if map > 0 {
                with_map += 1;
            }
            println!(
                "{name}\t{stage}\t{programs}\t{spilled}\t{bvh}\t{threads}\t{wave}\t{map}\t{result}"
            );
        }
    }
    eprintln!(
        "inventory: {n} containers, {builder} builder, {trace} trace, {with_map} by-address, {ok} translated"
    );
}
