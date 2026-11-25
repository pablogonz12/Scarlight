//! The file of title pipelines to build before the title starts
//! (`warm-pipelines.bin` beside the pipeline library; read and written by
//! `victory-gpu`'s `pipeline::warm`).
//!
//! The early list names pipelines by a key and is only useful once the
//! title creates them again. This file keeps what the key was made from, so
//! that a pipeline can be built before the title runs: the shader
//! containers the title gave and the graphics state, in the host's own
//! formats. It is appended to, never rewritten: a frame is a tag, a length
//! and a payload, and a part-written last frame (the run ended mid-write)
//! is ignored and cut off before the next append.
//!
//! - `S`: a shader container: its BLAKE3 (32 bytes), its stage code, bytes.
//! - `P`: a pipeline: its key (32 characters), the graphics state's bytes
//!   (`u32` length, `u32::MAX` for none), the shader count (`u8`) and the
//!   BLAKE3 of each shader, in the order the pipeline lists them.

use std::collections::HashMap;

pub const MAGIC: &[u8; 8] = b"VWARM001";

const STAGES: [&str; 7] = ["vertex", "pixel", "compute", "mesh", "domain", "hull", "geometry"];

pub fn stage_code(stage: &str) -> Option<u8> {
    STAGES.iter().position(|s| *s == stage).map(|i| i as u8)
}

pub fn stage_name(code: u8) -> Option<&'static str> {
    STAGES.get(code as usize).copied()
}

const NO_STATE: u32 = u32::MAX;

pub fn shader_frame(hash: &[u8; 32], stage: u8, container: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(33 + container.len());
    payload.extend_from_slice(hash);
    payload.push(stage);
    payload.extend_from_slice(container);
    frame(b'S', &payload)
}

pub fn pipeline_frame(key: &str, state: Option<&[u8]>, shaders: &[[u8; 32]]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(key.as_bytes());
    match state {
        Some(s) => {
            payload.extend_from_slice(&(s.len() as u32).to_le_bytes());
            payload.extend_from_slice(s);
        }
        None => payload.extend_from_slice(&NO_STATE.to_le_bytes()),
    }
    payload.push(shaders.len() as u8);
    for h in shaders {
        payload.extend_from_slice(h);
    }
    frame(b'P', &payload)
}

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// A shader container and the stage the title gave it to.
pub struct StoredShader {
    pub stage: &'static str,
    pub container: Vec<u8>,
}

/// A listed pipeline: the key, its graphics state and its shaders.
pub struct StoredPipeline {
    pub key: String,
    pub state: Option<Vec<u8>>,
    pub shaders: Vec<[u8; 32]>,
}

/// What a file holds: shaders by hash, pipelines in file order, and the
/// length of the file's good part (0 for a file that is not ours).
#[derive(Default)]
pub struct Contents {
    pub shaders: HashMap<[u8; 32], StoredShader>,
    pub pipelines: Vec<StoredPipeline>,
    pub good: usize,
}

/// Reads `bytes`; stops at the first frame that is not whole or is not
/// understood.
pub fn parse(bytes: &[u8]) -> Contents {
    let mut out = Contents::default();
    if !bytes.starts_with(MAGIC) {
        return out;
    }
    let mut at = MAGIC.len();
    out.good = at;
    while let Some(head) = bytes.get(at..at + 5) {
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let Some(payload) = bytes.get(at + 5..at + 5 + len) else { break };
        let ok = match head[0] {
            b'S' => read_shader(payload, &mut out),
            b'P' => read_pipeline(payload, &mut out),
            _ => false,
        };
        if !ok {
            break;
        }
        at += 5 + len;
        out.good = at;
    }
    out
}

fn read_shader(payload: &[u8], out: &mut Contents) -> bool {
    let (Some(hash), Some(&code), Some(container)) = (payload.get(..32), payload.get(32), payload.get(33..)) else {
        return false;
    };
    let Some(stage) = stage_name(code) else { return false };
    let hash: [u8; 32] = hash.try_into().expect("32 bytes");
    out.shaders.insert(hash, StoredShader { stage, container: container.to_vec() });
    true
}

fn read_pipeline(payload: &[u8], out: &mut Contents) -> bool {
    let Some(key) = payload.get(..32).and_then(|k| std::str::from_utf8(k).ok()) else {
        return false;
    };
    let Some(size) = payload.get(32..36).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])) else {
        return false;
    };
    let mut at = 36;
    let state = if size == NO_STATE {
        None
    } else {
        let Some(s) = payload.get(at..at + size as usize) else { return false };
        at += size as usize;
        Some(s.to_vec())
    };
    let Some(&n) = payload.get(at) else { return false };
    at += 1;
    let mut shaders = Vec::new();
    for _ in 0..n {
        let Some(h) = payload.get(at..at + 32) else { return false };
        shaders.push(h.try_into().expect("32 bytes"));
        at += 32;
    }
    out.pipelines.push(StoredPipeline { key: key.to_owned(), state, shaders });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let mut file = MAGIC.to_vec();
        file.extend(shader_frame(&a, stage_code("vertex").unwrap(), b"vs bytes"));
        file.extend(shader_frame(&b, stage_code("pixel").unwrap(), b"ps bytes"));
        file.extend(pipeline_frame(&"k".repeat(32), Some(b"state"), &[a, b]));
        file.extend(pipeline_frame(&"c".repeat(32), None, &[a]));
        file
    }

    #[test]
    fn frames_read_back() {
        let file = sample();
        let c = parse(&file);
        assert_eq!(c.good, file.len());
        assert_eq!(c.shaders.len(), 2);
        assert_eq!(c.shaders[&[2u8; 32]].stage, "pixel");
        assert_eq!(c.shaders[&[1u8; 32]].container, b"vs bytes");
        assert_eq!(c.pipelines.len(), 2);
        assert_eq!(c.pipelines[0].key, "k".repeat(32));
        assert_eq!(c.pipelines[0].state.as_deref(), Some(&b"state"[..]));
        assert_eq!(c.pipelines[0].shaders, [[1u8; 32], [2u8; 32]]);
        assert_eq!(c.pipelines[1].state, None);
    }

    #[test]
    fn a_part_written_last_frame_is_cut_off() {
        let file = sample();
        for cut in 1..40 {
            let c = parse(&file[..file.len() - cut]);
            assert!(c.good < file.len());
            // What is kept reads the same again.
            assert_eq!(parse(&file[..c.good]).good, c.good);
        }
        // Cutting off the last frame keeps the first pipeline.
        let last = pipeline_frame(&"c".repeat(32), None, &[[1u8; 32]]).len();
        assert_eq!(parse(&file[..file.len() - 1]).pipelines.len(), 1);
        assert_eq!(parse(&file[..file.len() - 1]).good, file.len() - last);
    }

    #[test]
    fn a_file_of_another_kind_is_empty() {
        let c = parse(b"early list text\nnot ours");
        assert_eq!(c.good, 0);
        assert!(c.pipelines.is_empty());
    }

    #[test]
    fn unknown_stages_and_tags_stop_the_read() {
        let mut file = MAGIC.to_vec();
        file.extend(shader_frame(&[3u8; 32], 99, b"x"));
        assert_eq!(parse(&file).good, MAGIC.len());
        let mut file = MAGIC.to_vec();
        file.extend(frame(b'Z', b"?"));
        assert_eq!(parse(&file).good, MAGIC.len());
    }
}
