//! Decode a raw, little-endian GFX10 instruction stream.
use std::{env, fs, process};
use victory_shader::gfx10;

fn main() {
    let Some(path) = env::args_os().nth(1) else {
        eprintln!("usage: decode_raw <isa.bin>");
        process::exit(2);
    };
    let bytes = fs::read(path).unwrap_or_else(|error| {
        eprintln!("read error: {error}");
        process::exit(2);
    });
    match gfx10::decode(&bytes) {
        Ok(instructions) => {
            let mut unknown = 0;
            for inst in &instructions {
                if inst.name().is_none() {
                    eprintln!(
                        "unknown offset={:#x} encoding={:?} opcode={:#x}",
                        inst.offset, inst.encoding, inst.opcode
                    );
                    unknown += 1;
                }
            }
            println!("decoded={} unknown={unknown}", instructions.len());
            if unknown != 0 {
                process::exit(1);
            }
            match victory_shader::ir::build(&bytes, true, 256) {
                Ok(program) => match victory_shader::structure::structure(&program) {
                    Ok(_) => println!("ir=ok structure=ok"),
                    Err(error) => println!("ir=ok structure_error={error}"),
                },
                Err(error) => println!("ir_error={error}"),
            }
        }
        Err(error) => {
            eprintln!("decode error: {error}");
            process::exit(1);
        }
    }
}
