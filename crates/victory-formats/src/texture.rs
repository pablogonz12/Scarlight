//! Texture layouts of the console GPU (RDNA2, GFX10.3): a texture's size and
//! alignment, where each mip and array slice starts, the address of each
//! element, and copies between tiled memory and linear rows.
//!
//! Units: an element is a texel, or a 4x4 block of a block-compressed
//! format. Mips of one array slice (or one depth slab of a thick 3D texture)
//! are stored together, smallest first: the mip tail (the small mips sharing
//! one block) at offset 0, mip 0 last.
//!
//! The algorithm follows AMD's address library (addrlib), as shipped in Mesa
//! under `src/amd/addrlib`: block dimensions, mip placement and mip tails
//! from `gfx10addrlib.cpp` and `addrlib2.cpp`
//! (`ComputeSurfaceInfoMacroTiled`, `ComputeThinBlockDimension`,
//! `GetMipTailDim`, `GetMaxNumMipsInTail`, `HwlComputeSurfaceInfoLinear`,
//! `ComputeSurfaceAddrFromCoordMacroTiled`), and the 256-byte micro tile
//! patterns in `MICRO` from `gfx10SwizzlePattern.h` (the RB+ tables, as on
//! RDNA2). addrlib carries this notice:
//!
//! Copyright (C) 2007-2026 Advanced Micro Devices, Inc. All rights reserved.
//! SPDX-License-Identifier: MIT
//!
//! Permission is hereby granted, free of charge, to any person obtaining a
//! copy of this software and associated documentation files (the
//! "Software"), to deal in the Software without restriction, including
//! without limitation the rights to use, copy, modify, merge, publish,
//! distribute, sublicense, and/or sell copies of the Software, and to permit
//! persons to whom the Software is furnished to do so, subject to the
//! following conditions: the above copyright notice and this permission
//! notice shall be included in all copies or substantial portions of the
//! Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
//! EXPRESS OR IMPLIED.

/// A swizzle mode, by its hardware number (addrlib's `ADDR_SW_*` order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Swizzle(pub u32);

impl Swizzle {
    pub const LINEAR: Swizzle = Swizzle(0);
    pub const SW_256B_S: Swizzle = Swizzle(1);
    pub const SW_256B_D: Swizzle = Swizzle(2);
    pub const SW_4KB_S: Swizzle = Swizzle(5);
    pub const SW_4KB_D: Swizzle = Swizzle(6);
    pub const SW_64KB_S: Swizzle = Swizzle(9);
    pub const SW_64KB_D: Swizzle = Swizzle(10);
    pub const SW_64KB_S_T: Swizzle = Swizzle(17);
    pub const SW_64KB_D_T: Swizzle = Swizzle(18);
    pub const SW_4KB_S_X: Swizzle = Swizzle(21);
    pub const SW_4KB_D_X: Swizzle = Swizzle(22);
    pub const SW_4KB_R_X: Swizzle = Swizzle(23);
    pub const SW_64KB_Z_X: Swizzle = Swizzle(24);
    pub const SW_64KB_S_X: Swizzle = Swizzle(25);
    pub const SW_64KB_D_X: Swizzle = Swizzle(26);
    pub const SW_64KB_R_X: Swizzle = Swizzle(27);

    /// log2 of the block size in bytes (0 for linear); `None` for modes
    /// GFX10 does not have.
    pub fn block_log2(self) -> Option<u32> {
        Some(match self.0 {
            0 => 0,
            1 | 2 => 8,
            5 | 6 | 21..=23 => 12,
            9 | 10 | 17 | 18 | 24..=27 => 16,
            _ => return None,
        })
    }

    fn standard(self) -> bool {
        matches!(self.0, 1 | 5 | 9 | 17 | 21 | 25)
    }

    fn display(self) -> bool {
        matches!(self.0, 2 | 6 | 10 | 18 | 22 | 26)
    }

    fn z_order(self) -> bool {
        self.0 == 24
    }

    /// Pipe and bank XOR modes (`_T`, `_X`): their addresses depend on the
    /// GPU's pipe configuration.
    pub fn xor(self) -> bool {
        self.0 >= 16
    }
}

/// The GPU's pipe and packer counts (log2), from its address configuration
/// (`GB_ADDR_CONFIG`): the addresses of the XOR modes depend on them, the
/// others' do not. Pipe interleave is 256 bytes on every console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pipes {
    pub pipes_log2: u32,
    pub packers_log2: u32,
}

impl Pipes {
    /// Xbox Series X: 32 pipes, 8 packers.
    pub const SERIES_X: Pipes = Pipes {
        pipes_log2: 5,
        packers_log2: 3,
    };
    /// Xbox Series S: 8 pipes, 4 packers.
    pub const SERIES_S: Pipes = Pipes {
        pipes_log2: 3,
        packers_log2: 2,
    };
}

#[rustfmt::skip]
mod patterns;

#[cfg(test)]
mod generate;

/// A block's swizzle pattern (generated in `patterns.rs`): for each address
/// bit inside the block, low to high, the element coordinate bits whose
/// parity it takes ([`x`], [`y`] and [`z`] bits; 0 for the bytes inside an
/// element). Coordinates above the block take part in the XOR modes.
struct Pattern {
    swizzle: Swizzle,
    /// The 3D pattern of a thick block; else z is the array or depth slice.
    thick: bool,
    /// The configuration an XOR mode's pattern is for; `None` for the modes
    /// without XOR, whose patterns hold for every one.
    pipes: Option<Pipes>,
    element_log2: u32,
    bits: &'static [u64],
}

/// Coordinate bits of a pattern's address bit: x in bits 0-15, y 16-31, z
/// 32-47.
const fn x(bit: u32) -> u64 {
    1 << bit
}

const fn y(bit: u32) -> u64 {
    1 << (16 + bit)
}

const fn z(bit: u32) -> u64 {
    1 << (32 + bit)
}

/// Bits of a coordinate: every extent is below 2^14 ([`MAX_EXTENT`]).
const COORD_BITS: usize = 14;

/// For each axis and coordinate bit, the address bits inside a block it
/// flips: a pattern's address bits read the other way round.
type Flips = [[u32; COORD_BITS]; 3];

/// The pattern of a mode, if this module has it: all GFX10 modes but
/// `4KB_R_X` (addrlib has no pattern for it), single-sampled.
fn flips(swizzle: Swizzle, thick: bool, element_log2: u32, pipes: Pipes) -> Option<Flips> {
    let p = patterns::PATTERNS.iter().find(|p| {
        p.swizzle == swizzle
            && p.thick == thick
            && p.element_log2 == element_log2
            && p.pipes.is_none_or(|q| q == pipes)
    })?;
    let mut flips = [[0; COORD_BITS]; 3];
    for (bit, &coords) in p.bits.iter().enumerate() {
        for (axis, f) in flips.iter_mut().enumerate() {
            let mask = (coords >> (16 * axis)) & 0xffff;
            for (c, f) in f.iter_mut().enumerate() {
                if mask >> c & 1 != 0 {
                    *f |= 1 << bit;
                }
            }
        }
    }
    Some(flips)
}

/// The XOR of the address bits a coordinate's set bits flip.
fn flip(flips: &[u32; COORD_BITS], c: u32) -> u32 {
    flips
        .iter()
        .enumerate()
        .filter(|&(bit, _)| c >> bit & 1 != 0)
        .fold(0, |acc, (_, &f)| acc ^ f)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Tex1d,
    Tex2d,
    Tex3d,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The description makes no texture (zero size, bad element size, a
    /// mode this dimension cannot use, too many mips).
    Invalid,
    /// A valid layout whose addressing is not implemented here.
    Unsupported,
    /// A buffer is smaller than the layout or the rows need.
    TooSmall,
}

/// One plane of a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desc {
    pub dimension: Dimension,
    /// In texels.
    pub width: u32,
    pub height: u32,
    /// Depth of a 3D texture, else the array size.
    pub depth: u32,
    pub mips: u32,
    /// 1, 2, 4, 8 or 16.
    pub element_bytes: u32,
    /// Texels per element side: 1, or 4 for block compression.
    pub block: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
}

/// Elements along one side of mip `mip`, as data and copies see it: texels
/// halve rounding down (to at least 1), then round up to whole elements.
fn dim(texels: u32, mip: u32, block: u32) -> u32 {
    (texels >> mip).max(1).div_ceil(block)
}

/// Elements along one side of mip `mip` for placement (padding, tail,
/// offsets): addrlib halves mip 0's elements rounding up, which can be one
/// more than [`dim`] for odd sizes.
fn padded_dim(texels: u32, mip: u32, block: u32) -> u32 {
    texels.max(1).div_ceil(block).div_ceil(1 << mip)
}

/// Where one mip lives inside a slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mip {
    /// Size in elements (depth 1 unless the texture is 3D).
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    /// Padded size in elements: whole blocks, or the tail's share of one.
    pub pitch: u32,
    pub rows: u32,
    pub depth_pitch: u32,
    /// Byte offset of its first block in the slice (0 in the tail).
    pub offset: u64,
    /// In the mip tail: byte offset inside the tail block and the element
    /// position the mip starts at.
    pub tail: Option<Tail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    pub offset: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub desc: Desc,
    /// Block dimensions in elements (linear: the pitch alignment, 1, 1).
    pub block: [u32; 3],
    /// 3D blocks span `block[2]` depth slices (GFX10 3D `S` and `D` modes).
    pub thick: bool,
    /// Array slices, depth slabs of `block[2]` slices (thick), or depth
    /// slices (thin and linear 3D).
    pub slices: u32,
    /// Bytes of one slice: its whole mip chain.
    pub slice_size: u64,
    pub size: u64,
    pub alignment: u64,
    /// The first mip in the tail; `mips.len()` without one.
    pub first_tail_mip: u32,
    pub mips: Vec<Mip>,
    /// The GPU's pipe and packer counts: the XOR modes' addresses follow
    /// them, the others' do not.
    pub pipes: Pipes,
    /// The base swizzle (addrlib's `pipeBankXor`) of an XOR mode, 0 for the
    /// others: pipe bits, then bank bits, XORed into every block from
    /// address bit 8.
    pub pipe_bank_xor: u32,
    /// The pattern, read by coordinate bit; `None` for linear and the
    /// layouts without addressing.
    flips: Option<Flips>,
    /// `pipe_bank_xor` as it lands in a block's offset.
    xor: u32,
}

/// The all-zero pattern: no address bit is flipped.
const NO_FLIPS: Flips = [[0; COORD_BITS]; 3];

const MAX_MIPS: u32 = 16;

/// The largest width, height and depth (or array size) accepted, in texels.
/// GFX10's image descriptors hold at most 16384 texels per side, so no
/// texture the GPU can use is refused. The bound also keeps every size
/// computed below within `u64` (and its `u32` steps), which unchecked
/// values from a title would otherwise overflow.
const MAX_EXTENT: u32 = 1 << 14;

/// 256-byte micro tile dimensions by log2 element size, 2D and 3D.
const BLOCK_256_2D: [[u32; 2]; 5] = [[16, 16], [16, 8], [8, 8], [8, 4], [4, 4]];
const BLOCK_256_3D: [[u32; 3]; 5] = [[8, 4, 8], [4, 4, 8], [4, 4, 4], [4, 2, 4], [2, 2, 4]];
const BLOCK_1K_3D: [[u32; 3]; 5] = [[16, 8, 8], [8, 8, 8], [8, 8, 4], [8, 4, 4], [4, 4, 4]];

impl Layout {
    /// The layout on an Xbox Series X, with base swizzle 0.
    pub fn new(desc: &Desc) -> Result<Layout, Error> {
        Layout::for_gpu(desc, Pipes::SERIES_X, 0)
    }

    /// The layout on a GPU of `pipes`, whose XOR modes take base swizzle
    /// `pipe_bank_xor` (its bits past the configuration's pipe and bank bits
    /// are ignored, as the GPU does).
    pub fn for_gpu(desc: &Desc, pipes: Pipes, pipe_bank_xor: u32) -> Result<Layout, Error> {
        let d = *desc;
        let block_log2 = d.swizzle.block_log2().ok_or(Error::Invalid)?;
        let valid = (1..=MAX_EXTENT).contains(&d.width)
            && (1..=MAX_EXTENT).contains(&d.height)
            && (1..=MAX_EXTENT).contains(&d.depth)
            && (1..=MAX_MIPS).contains(&d.mips)
            && d.element_bytes.is_power_of_two()
            && d.element_bytes <= 16
            && matches!(d.block, 1 | 4)
            && d.samples.is_power_of_two()
            && d.samples <= 8
            && (d.samples == 1
                || (d.dimension == Dimension::Tex2d
                    && d.mips == 1
                    && d.swizzle != Swizzle::LINEAR))
            && (d.dimension != Dimension::Tex1d || d.height == 1)
            && (d.dimension != Dimension::Tex3d
                || matches!(d.swizzle.0, 0 | 5 | 9 | 17 | 21 | 24..=27));
        if !valid {
            return Err(Error::Invalid);
        }
        if block_log2 == 0 {
            Ok(Layout::linear(d, pipes))
        } else {
            Ok(Layout::tiled(d, block_log2, pipes, pipe_bank_xor))
        }
    }

    /// `HwlComputeSurfaceInfoLinear`: rows aligned to 256 bytes, mips packed
    /// smallest first, a 3D texture's depth slices each hold the chain.
    fn linear(d: Desc, pipes: Pipes) -> Layout {
        let bpe = u64::from(d.element_bytes);
        let align = 256 / d.element_bytes;
        let three_d = d.dimension == Dimension::Tex3d;
        let mut mips = vec![];
        let mut offset = 0;
        for i in (0..d.mips).rev() {
            let h = padded_dim(d.height, i, d.block);
            let pitch = padded_dim(d.width, i, d.block).next_multiple_of(align);
            mips.push(Mip {
                width: dim(d.width, i, d.block),
                height: dim(d.height, i, d.block),
                depth: if three_d { dim(d.depth, i, 1) } else { 1 },
                pitch,
                rows: h,
                depth_pitch: if three_d { d.depth } else { 1 },
                offset,
                tail: None,
            });
            offset += u64::from(pitch) * u64::from(h) * bpe;
        }
        mips.reverse();
        Layout {
            desc: d,
            block: [align, 1, 1],
            thick: false,
            slices: d.depth,
            slice_size: offset,
            size: offset * u64::from(d.depth),
            alignment: 256,
            first_tail_mip: d.mips,
            mips,
            pipes,
            pipe_bank_xor: 0,
            flips: None,
            xor: 0,
        }
    }

    fn tiled(d: Desc, block_log2: u32, pipes: Pipes, pipe_bank_xor: u32) -> Layout {
        let element_log2 = d.element_bytes.trailing_zeros();
        let bpe = u64::from(d.element_bytes);
        let block_bytes = 1u64 << block_log2;
        let thick =
            d.dimension == Dimension::Tex3d && (d.swizzle.standard() || d.swizzle.display());
        let block = if thick {
            // ComputeThickBlockDimension
            let amp = (block_log2 - 10) / 3;
            let rest = (block_log2 - 10) % 3;
            let b = BLOCK_1K_3D[element_log2 as usize];
            [
                b[0] << amp,
                b[1] << (amp + rest / 2),
                b[2] << (amp + u32::from(rest != 0)),
            ]
        } else {
            // ComputeThinBlockDimension
            let samples_log2 = d.samples.trailing_zeros();
            let elements_log2 = block_log2 - element_log2 - samples_log2;
            let wide = samples_log2 & 1 == 0 || block_log2 & 1 != 0;
            let w = (elements_log2 + u32::from(wide)) / 2;
            [1 << w, 1 << (elements_log2 - w), 1]
        };
        let slices = d.depth.next_multiple_of(block[2]);
        let three_d = d.dimension == Dimension::Tex3d;
        // Mip depths placed by a thick layout (thin ones store depth as
        // slices).
        let mip_depth = |i: u32| if thick { padded_dim(d.depth, i, 1) } else { 1 };
        let depth_of = |i: u32| if three_d { dim(d.depth, i, 1) } else { 1 };
        let (width, height) = (|i| dim(d.width, i, d.block), |i| dim(d.height, i, d.block));
        let padded = |i| {
            (
                padded_dim(d.width, i, d.block),
                padded_dim(d.height, i, d.block),
            )
        };
        let mut mips = Vec::with_capacity(d.mips as usize);
        let mut first_tail_mip = d.mips;
        let mut slice_size = 0u64;
        if d.mips == 1 {
            mips.push(Mip {
                width: width(0),
                height: height(0),
                depth: depth_of(0),
                pitch: width(0).next_multiple_of(block[0]),
                rows: height(0).next_multiple_of(block[1]),
                depth_pitch: if three_d { slices } else { 1 },
                offset: 0,
                tail: None,
            });
            slice_size =
                u64::from(mips[0].pitch) * u64::from(mips[0].rows) * bpe * u64::from(d.samples);
        } else {
            // GetMipTailDim: the largest mip the tail takes.
            let mut tail_max = block;
            if thick {
                match block_log2 % 3 {
                    0 => tail_max[1] >>= 1,
                    1 => tail_max[0] >>= 1,
                    _ => tail_max[2] >>= 1,
                }
            } else {
                tail_max[0] >>= 1;
            }
            // GetMaxNumMipsInTail; 256-byte blocks have no tail
            // (`ComputeSurfaceInfoMicroTiled`).
            let effective = if thick {
                block_log2 - (block_log2 - 8) / 3
            } else {
                block_log2
            };
            let max_in_tail = match effective {
                8 => 0,
                9..=11 => 1 + (1 << (effective - 9)),
                _ => effective - 4,
            };
            // dsMipmapHtileFix: 8- and 16-bit depth enter the tail as 32-bit.
            let mut fixed = tail_max;
            if d.swizzle.z_order() && element_log2 <= 1 {
                let b = BLOCK_256_2D[element_log2 as usize];
                fixed[0] /= b[0] / BLOCK_256_2D[2][0];
                fixed[1] /= b[1] / BLOCK_256_2D[2][1];
            }
            let mut sizes = vec![];
            for i in 0..d.mips {
                let (w, h) = padded(i);
                if w <= fixed[0] && h <= fixed[1] && d.mips - i <= max_in_tail {
                    first_tail_mip = i;
                    slice_size += block_bytes / u64::from(block[2]);
                    break;
                }
                let pitch = w.next_multiple_of(block[0]);
                let rows = h.next_multiple_of(block[1]);
                let depth = mip_depth(i).next_multiple_of(block[2]);
                let one = u64::from(pitch) * u64::from(rows) * bpe;
                slice_size += one;
                sizes.push(one * u64::from(block[2]));
                mips.push(Mip {
                    width: width(i),
                    height: height(i),
                    depth: depth_of(i),
                    pitch,
                    rows,
                    depth_pitch: if three_d { depth } else { 1 },
                    offset: 0,
                    tail: None,
                });
            }
            // Standard mips from the smallest up, after the tail block.
            let mut offset = if first_tail_mip < d.mips {
                block_bytes
            } else {
                0
            };
            for i in (0..first_tail_mip as usize).rev() {
                mips[i].offset = offset;
                offset += sizes[i];
            }
            let micro = if thick {
                let b = BLOCK_256_3D[element_log2 as usize];
                [b[0], b[1]]
            } else {
                BLOCK_256_2D[element_log2 as usize]
            };
            let (mut pitch, mut rows) = (tail_max[0], tail_max[1]);
            for i in first_tail_mip..d.mips {
                let m = max_in_tail - 1 - (i - first_tail_mip);
                let offset = if m > 6 { 16 << m } else { m << 8 };
                // Address bits 8.. of the offset, odd ones x, even ones y.
                let bits = |from: u32| (0..6).map(|b| ((offset >> (from + 2 * b)) & 1) << b).sum();
                let (mut x, mut y): (u32, u32) = (bits(9), bits(8));
                if block_log2 & 1 != 0 {
                    std::mem::swap(&mut x, &mut y);
                    if element_log2 & 1 != 0 {
                        y = (y << 1) | (x & 1);
                        x >>= 1;
                    }
                }
                mips.push(Mip {
                    width: width(i),
                    height: height(i),
                    depth: depth_of(i),
                    pitch,
                    rows,
                    depth_pitch: if thick {
                        mip_depth(i).next_multiple_of(BLOCK_256_3D[element_log2 as usize][2])
                    } else {
                        1
                    },
                    offset: 0,
                    tail: Some(Tail {
                        offset,
                        x: x * micro[0],
                        y: y * micro[1],
                    }),
                });
                pitch = (pitch >> 1).max(micro[0]);
                rows = (rows >> 1).max(micro[1]);
            }
        }
        // Thick layouts store each depth slab's chain together.
        let slab = if thick { u64::from(block[2]) } else { 1 };
        let slice_size = slice_size * slab;
        let slices = slices / block[2];
        // MSAA has patterns of its own in addrlib (`*_2xaa` to `*_8xaa`):
        // titles render those on the host, so they are not here.
        let flips = if d.samples == 1 && (!d.swizzle.xor() || xor_addressing()) {
            flips(d.swizzle, thick, element_log2, pipes)
        } else {
            None
        };
        // ComputeSurfaceAddrFromCoordMacroTiled: the pipe bits and the bank
        // bits above them (GetBankXorBits, 2 column bits between), shifted
        // past the 256-byte pipe interleave, inside the block.
        let xor = if d.swizzle.xor() {
            let p = pipes.pipes_log2;
            let banks = block_log2.saturating_sub(8 + p + 2).min(4);
            let mask = ((1 << p) - 1) | (((1 << banks) - 1) << (p + 2));
            ((pipe_bank_xor & mask) << 8) & ((1 << block_log2) - 1)
        } else {
            0
        };
        Layout {
            desc: d,
            block,
            thick,
            slices,
            slice_size,
            size: slice_size * u64::from(slices),
            alignment: block_bytes,
            first_tail_mip,
            mips,
            pipes,
            pipe_bank_xor: if d.swizzle.xor() { pipe_bank_xor } else { 0 },
            flips,
            xor,
        }
    }

    /// Whether [`Layout::offset`] and the copies work for this layout:
    /// every single-sampled GFX10 mode but `4KB_R_X`, which addrlib has no
    /// pattern for.
    ///
    /// shortcut: no addressing for MSAA; titles render those on the host
    /// GPU. Add the sample patterns (`*_2xaa` to `*_8xaa`) when a title
    /// copies one through the CPU.
    pub fn addressable(&self) -> bool {
        self.desc.swizzle == Swizzle::LINEAR || self.flips.is_some()
    }

    /// The slice and the depth inside it for `z` (an array slice or a 3D
    /// depth slice).
    fn split(&self, z: u32) -> (u32, u32) {
        if self.thick {
            (z / self.block[2], z % self.block[2])
        } else {
            (z, 0)
        }
    }

    /// Byte offset of element (`x`, `y`) of mip `mip` in array slice or
    /// depth slice `z`. Coordinates must lie inside the mip.
    pub fn offset(&self, mip: u32, x: u32, y: u32, z: u32) -> Result<u64, Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = if self.desc.dimension == Dimension::Tex3d {
            m.depth
        } else {
            self.slices
        };
        let inside = x < m.width && y < m.height && z < depth;
        if !inside || !self.addressable() {
            return Err(if inside {
                Error::Unsupported
            } else {
                Error::Invalid
            });
        }
        let (slice, z) = self.split(z);
        let base = u64::from(slice) * self.slice_size + m.offset;
        if self.desc.swizzle == Swizzle::LINEAR {
            let row = u64::from(y) * u64::from(m.pitch) + u64::from(x);
            return Ok(base + row * u64::from(self.desc.element_bytes));
        }
        let (row, xor) = self.row(m, y, z);
        Ok(base + row + (self.column(m, x) ^ u64::from(xor)))
    }

    /// The part of an address that depends on the row and the slice: whole
    /// blocks, and the bits it XORs into the block offset (y, slice and the
    /// base swizzle). `z` is the array slice or depth slice: thick blocks
    /// take its bits inside the block, the XOR modes its bits above too.
    fn row(&self, m: &Mip, y: u32, z: u32) -> (u64, u32) {
        let [bw, bh, _] = self.block;
        let log2 = self.alignment.trailing_zeros();
        // In the tail the pattern of the shifted position gives the mip's
        // offset inside the tail block.
        let y = y + m.tail.map_or(0, |t| t.y);
        let blocks = u64::from(y / bh) * u64::from(m.pitch / bw);
        let f = self.flips.as_ref().unwrap_or(&NO_FLIPS);
        let xor = flip(&f[1], y) ^ flip(&f[2], z) ^ self.xor;
        (blocks << log2, xor)
    }

    /// The part of an address that depends on the column: whole blocks plus
    /// the bits the column flips inside the block. A row's XOR applies to
    /// the low bits alone, so it combines with this by XOR.
    fn column(&self, m: &Mip, x: u32) -> u64 {
        let bw = self.block[0];
        let x = x + m.tail.map_or(0, |t| t.x);
        (u64::from(x / bw) << self.alignment.trailing_zeros())
            | u64::from(flip(&self.flips.as_ref().unwrap_or(&NO_FLIPS)[0], x))
    }

    /// Depth slices a subresource of mip `mip` covers: the mip's depth for
    /// 3D textures, else 1 (one array slice).
    pub fn subresource_depth(&self, mip: u32) -> u32 {
        self.mips.get(mip as usize).map_or(0, |m| m.depth)
    }

    /// The bytes of one slice that hold each mip, `(start, end)` per mip in
    /// the layout's mip order: where a mip's memory lies, for a texture
    /// whose pages arrive a few at a time (a streamed texture, the smallest
    /// mips first). Mips in the tail share the tail block at the start of
    /// the slice, so each tail mip's range is that whole block: a page
    /// mapped there holds every tail mip of the slice.
    pub fn mip_extents(&self) -> Vec<(u64, u64)> {
        let first_tail = (self.first_tail_mip as usize).min(self.mips.len());
        // The mips outside the tail, by where they start; each runs up to
        // the next one's start, the last to the slice's end.
        let mut sorted: Vec<u64> = self.mips[..first_tail].iter().map(|m| m.offset).collect();
        sorted.sort_unstable();
        sorted.dedup();
        // The tail block is what lies before the first of them.
        let tail_end = sorted
            .first()
            .copied()
            .unwrap_or(self.slice_size)
            .min(self.slice_size);
        self.mips
            .iter()
            .map(|m| {
                if m.tail.is_some() {
                    return (0, tail_end);
                }
                let at = sorted.binary_search(&m.offset).unwrap_or(0);
                let end = sorted.get(at + 1).copied().unwrap_or(self.slice_size);
                (m.offset, end)
            })
            .collect()
    }

    /// Copies one subresource out of tiled memory (the whole texture,
    /// [`Layout::size`] bytes) into linear rows: mip `mip` of array slice
    /// `slice` (3D: every depth slice of the mip, `slice` 0). Rows are
    /// `row_pitch` bytes apart, depth slices `depth_pitch`.
    pub fn detile(
        &self,
        tiled: &[u8],
        mip: u32,
        slice: u32,
        linear: &mut [u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        if slower_copies() {
            return self.detile_spans(tiled, mip, slice, linear, row_pitch, depth_pitch);
        }
        self.walk(
            Buffers::Detile { tiled, linear },
            mip,
            slice,
            row_pitch,
            depth_pitch,
        )
    }

    /// The reverse of [`Layout::detile`]: linear rows into tiled memory.
    pub fn tile(
        &self,
        tiled: &mut [u8],
        mip: u32,
        slice: u32,
        linear: &[u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        if slower_copies() {
            return self.tile_spans(tiled, mip, slice, linear, row_pitch, depth_pitch);
        }
        self.walk(
            Buffers::Tile { tiled, linear },
            mip,
            slice,
            row_pitch,
            depth_pitch,
        )
    }

    /// Validates the full subresource before writing any bytes.
    fn walk(
        &self,
        mut copy: Buffers<'_>,
        mip: u32,
        slice: u32,
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = m.depth;
        let first = if self.desc.dimension == Dimension::Tex3d {
            if slice != 0 {
                return Err(Error::Invalid);
            }
            0
        } else {
            slice
        };
        if first >= self.slices {
            return Err(Error::Invalid);
        }
        if !self.addressable() {
            return Err(Error::Unsupported);
        }
        let n = self.desc.element_bytes as usize;
        let bytes = m.width as usize * n;
        let slice_bytes = (m.height as usize - 1)
            .checked_mul(row_pitch)
            .and_then(|offset| offset.checked_add(bytes))
            .ok_or(Error::TooSmall)?;
        // Multiple depth slices must not overwrite each other.
        if depth > 1 && depth_pitch < slice_bytes {
            return Err(Error::TooSmall);
        }
        let need = (depth as usize - 1)
            .checked_mul(depth_pitch)
            .zip((m.height as usize - 1).checked_mul(row_pitch))
            .and_then(|(depth_offset, row_offset)| depth_offset.checked_add(row_offset))
            .and_then(|offset| offset.checked_add(bytes))
            .ok_or(Error::TooSmall)?;
        let (tiled_len, linear_len) = match &copy {
            Buffers::Tile { tiled, linear } => (tiled.len(), linear.len()),
            Buffers::Detile { tiled, linear } => (tiled.len(), linear.len()),
        };
        if tiled_len < self.size as usize || linear_len < need || row_pitch < bytes {
            return Err(Error::TooSmall);
        }
        if self.desc.swizzle == Swizzle::LINEAR {
            for z in 0..depth {
                let base = (u64::from(first + z) * self.slice_size + m.offset) as usize;
                for y in 0..m.height as usize {
                    copy.span(
                        base + y * m.pitch as usize * n,
                        z as usize * depth_pitch + y * row_pitch,
                        bytes,
                    );
                }
            }
            return Ok(());
        }

        // A pipe or bank XOR row moves the low bits it flips, so the runs of
        // a row are not the runs of the row above: element by element.
        if self.desc.swizzle.xor() {
            self.walk_columns(m, first, row_pitch, depth_pitch, &mut copy);
            return Ok(());
        }
        // The low contiguous x bits repeat in every block. Tail origins are
        // micro-tile aligned, so only the last run of a row can be short.
        let run = self.flips.as_ref().map_or(0, |f| {
            (0..self.block[0])
                .take_while(|&x| flip(&f[0], x) as usize == x as usize * n)
                .count()
        }) * n;
        match run {
            4 => self.walk_runs::<4>(m, first, row_pitch, depth_pitch, &mut copy),
            8 => self.walk_runs::<8>(m, first, row_pitch, depth_pitch, &mut copy),
            16 => self.walk_runs::<16>(m, first, row_pitch, depth_pitch, &mut copy),
            32 => self.walk_runs::<32>(m, first, row_pitch, depth_pitch, &mut copy),
            // Only those widths are contiguous in GFX10's patterns; copy
            // element by element if one ever is not.
            _ => self.walk_columns(m, first, row_pitch, depth_pitch, &mut copy),
        }
        Ok(())
    }

    /// The floor the run walk needs: one visit per element.
    fn walk_columns(
        &self,
        m: &Mip,
        first: u32,
        row_pitch: usize,
        depth_pitch: usize,
        copy: &mut Buffers<'_>,
    ) {
        let n = self.desc.element_bytes as usize;
        for z in 0..m.depth {
            let (slice, inner) = self.split(first + z);
            let base = u64::from(slice) * self.slice_size + m.offset;
            for y in 0..m.height {
                let (row, xor) = self.row(m, y, inner);
                let tiled_row = (base + row) as usize;
                let linear_row = z as usize * depth_pitch + y as usize * row_pitch;
                for x in 0..m.width {
                    copy.span(
                        tiled_row + (self.column(m, x) ^ u64::from(xor)) as usize,
                        linear_row + x as usize * n,
                        n,
                    );
                }
            }
        }
    }

    fn walk_runs<const N: usize>(
        &self,
        m: &Mip,
        first: u32,
        row_pitch: usize,
        depth_pitch: usize,
        copy: &mut Buffers<'_>,
    ) {
        let n = self.desc.element_bytes as usize;
        let run = N / n;
        let bytes = m.width as usize * n;
        // Column indices are in N-byte units, shared by all rows and slices.
        let columns: Vec<_> = (0..bytes / N)
            .map(|i| self.column(m, (i * run) as u32) as usize / N)
            .collect();
        let end = columns.len() * N;
        let last = (end < bytes).then(|| self.column(m, (end / n) as u32) as usize);
        // Pair short runs to keep writes to nearby tiled addresses together.
        // Wide runs already transfer 32 bytes per visit.
        let rows = if N < 32 { 2 } else { 1 };
        let bd = self.block[2];
        for slab in (0..m.depth).step_by(bd as usize) {
            let (slice, _) = self.split(first + slab);
            let base = u64::from(slice) * self.slice_size + m.offset;
            for y in (0..m.height).step_by(rows) {
                // Tail origins and row pairs both start on even y. The low y
                // bit gives the distance to the second row in this block.
                let f = self.flips.as_ref().unwrap_or(&NO_FLIPS);
                let second = (rows == 2 && y + 1 < m.height).then(|| flip(&f[1], 1) as usize);
                let (blocks, xor) = self.row(m, y, 0);
                let row = base + blocks + u64::from(xor);
                // Depth slices in this slab share the same physical blocks.
                for inner in 0..bd.min(m.depth - slab) {
                    let t = row as usize + flip(&f[2], inner) as usize;
                    let l = (slab + inner) as usize * depth_pitch + y as usize * row_pitch;
                    copy.runs::<N>(&columns, t, l, row_pitch, second);
                    if let Some(col) = last {
                        copy.span(t + col, l + end, bytes - end);
                        if let Some(dy) = second {
                            copy.span(t + dy + col, l + row_pitch + end, bytes - end);
                        }
                    }
                }
            }
        }
    }

    /// The run-width walk above at its slowest: one closure call per span,
    /// the version the fast path is checked byte for byte against
    /// (`VICTORY_TEXTURE_RUNS=0` picks it at runtime).
    fn detile_spans(
        &self,
        tiled: &[u8],
        mip: u32,
        slice: u32,
        linear: &mut [u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        self.walk_spans(
            tiled.len(),
            mip,
            slice,
            linear.len(),
            row_pitch,
            depth_pitch,
            |t, l, len| {
                linear[l..][..len].copy_from_slice(&tiled[t..][..len]);
            },
        )
    }

    /// [`Layout::detile_spans`] the other way round.
    fn tile_spans(
        &self,
        tiled: &mut [u8],
        mip: u32,
        slice: u32,
        linear: &[u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        self.walk_spans(
            tiled.len(),
            mip,
            slice,
            linear.len(),
            row_pitch,
            depth_pitch,
            |t, l, len| {
                tiled[t..][..len].copy_from_slice(&linear[l..][..len]);
            },
        )
    }

    /// Visits contiguous runs as (tiled byte offset, linear byte offset, length).
    #[allow(clippy::too_many_arguments)]
    fn walk_spans(
        &self,
        tiled_len: usize,
        mip: u32,
        slice: u32,
        linear_len: usize,
        row_pitch: usize,
        depth_pitch: usize,
        mut copy: impl FnMut(usize, usize, usize),
    ) -> Result<(), Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = m.depth;
        let first = if self.desc.dimension == Dimension::Tex3d {
            if slice != 0 {
                return Err(Error::Invalid);
            }
            0
        } else {
            slice
        };
        if first >= self.slices {
            return Err(Error::Invalid);
        }
        if !self.addressable() {
            return Err(Error::Unsupported);
        }
        let n = self.desc.element_bytes as usize;
        let bytes = m.width as usize * n;
        let slice_bytes = (m.height as usize - 1)
            .checked_mul(row_pitch)
            .and_then(|offset| offset.checked_add(bytes))
            .ok_or(Error::TooSmall)?;

        // Multiple depth slices must not overwrite each other.
        if depth > 1 && depth_pitch < slice_bytes {
            return Err(Error::TooSmall);
        }

        let need = (depth as usize - 1)
            .checked_mul(depth_pitch)
            .and_then(|offset| offset.checked_add(slice_bytes))
            .ok_or(Error::TooSmall)?;
        if tiled_len < self.size as usize || linear_len < need || row_pitch < bytes {
            return Err(Error::TooSmall);
        }
        let linear = self.desc.swizzle == Swizzle::LINEAR;
        // In the XOR modes every row moves the bits it flips, so the runs of
        // one row are not those of the next: one call per element.
        if !linear && self.desc.swizzle.xor() {
            for z in 0..depth {
                let (slice, inner) = self.split(first + z);
                let base = u64::from(slice) * self.slice_size + m.offset;
                for y in 0..m.height {
                    let (row, xor) = self.row(m, y, inner);
                    let tiled_row = (base + row) as usize;
                    let linear_row = z as usize * depth_pitch + y as usize * row_pitch;
                    for x in 0..m.width {
                        let t = tiled_row + (self.column(m, x) ^ u64::from(xor)) as usize;
                        copy(t, linear_row + x as usize * n, n);
                    }
                }
            }
            return Ok(());
        }
        // The row and depth contribute one common base to every column, so
        // the same contiguous runs repeat on every row, including mip tails.
        let mut runs = Vec::new();
        if !linear {
            let mut first_linear = 0;
            let mut first_tiled = self.column(m, 0) as usize;
            for x in 1..m.width {
                let offset = self.column(m, x) as usize;
                let at = x as usize * n;
                if offset != first_tiled + (at - first_linear) {
                    runs.push((first_tiled, first_linear, at - first_linear));
                    first_tiled = offset;
                    first_linear = at;
                }
            }
            runs.push((first_tiled, first_linear, bytes - first_linear));
        }
        // A mip can end partway through the last run. Check the other runs
        // once so each row can copy them with a compile-time length.
        let (body, last) = runs.split_at(runs.len().saturating_sub(1));
        let run_len = body
            .first()
            .map(|run| run.2)
            .filter(|&len| body.iter().all(|run| run.2 == len));
        for z in 0..depth {
            let (s, inner) = self.split(first + z);
            let base = u64::from(s) * self.slice_size + m.offset;
            for y in 0..m.height {
                let linear_row = z as usize * depth_pitch + y as usize * row_pitch;
                if linear {
                    let tiled_row = base + u64::from(y) * u64::from(m.pitch) * n as u64;
                    copy(tiled_row as usize, linear_row, bytes);
                } else {
                    let (blocks, xor) = self.row(m, y, inner);
                    let tiled_row = (base + blocks + u64::from(xor)) as usize;
                    match run_len {
                        Some(8) => copy_runs::<8>(body, tiled_row, linear_row, &mut copy),
                        Some(16) => copy_runs::<16>(body, tiled_row, linear_row, &mut copy),
                        Some(32) => copy_runs::<32>(body, tiled_row, linear_row, &mut copy),
                        _ => copy_runs::<0>(body, tiled_row, linear_row, &mut copy),
                    }
                    copy_runs::<0>(last, tiled_row, linear_row, &mut copy);
                }
            }
        }
        Ok(())
    }
}

/// Copy micro-tile runs inline at a known width, or use each run's length for N = 0.
fn copy_runs<const N: usize>(
    runs: &[(usize, usize, usize)],
    tiled_row: usize,
    linear_row: usize,
    copy: &mut impl FnMut(usize, usize, usize),
) {
    for &(tiled, linear, len) in runs {
        let len = if N == 0 { len } else { N };
        copy(tiled_row + tiled, linear_row + linear, len);
    }
}

/// Whether the pipe and bank XOR modes are addressed at all:
/// `VICTORY_TEXTURE_XOR=0` leaves them as they were (a size, no addressing),
/// which is the way back if a title's XOR texture comes out wrong.
fn xor_addressing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_XOR").is_none_or(|value| value != "0"))
}

/// `VICTORY_TEXTURE_RUNS=0` copies through [`Layout::walk_spans`], the
/// span-per-call version, to compare against or to fall back to.
fn slower_copies() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_RUNS").is_some_and(|value| value == "0"))
}

enum Buffers<'a> {
    Detile {
        tiled: &'a [u8],
        linear: &'a mut [u8],
    },
    Tile {
        tiled: &'a mut [u8],
        linear: &'a [u8],
    },
}

impl Buffers<'_> {
    fn span(&mut self, t: usize, l: usize, len: usize) {
        match self {
            Self::Detile { tiled, linear } => {
                linear[l..][..len].copy_from_slice(&tiled[t..][..len])
            }
            Self::Tile { tiled, linear } => tiled[t..][..len].copy_from_slice(&linear[l..][..len]),
        }
    }

    fn runs<const N: usize>(
        &mut self,
        columns: &[usize],
        t: usize,
        l: usize,
        pitch: usize,
        second: Option<usize>,
    ) {
        let len = columns.len() * N;
        match self {
            Self::Detile { tiled, linear } => {
                let (src, _) = tiled[t..].as_chunks::<N>();
                let (top, rest) = linear[l..].split_at_mut(len);
                let (top, _) = top.as_chunks_mut::<N>();
                if let Some(dy) = second {
                    let (bottom, _) = rest[pitch - len..][..len].as_chunks_mut::<N>();
                    for ((a, b), &x) in top.iter_mut().zip(bottom).zip(columns) {
                        *a = src[x];
                        *b = src[x + dy / N];
                    }
                } else {
                    for (out, &x) in top.iter_mut().zip(columns) {
                        *out = src[x];
                    }
                }
            }
            Self::Tile { tiled, linear } => {
                let (dst, _) = tiled[t..].as_chunks_mut::<N>();
                let (top, rest) = linear[l..].split_at(len);
                let (top, _) = top.as_chunks::<N>();
                if let Some(dy) = second {
                    let (bottom, _) = rest[pitch - len..][..len].as_chunks::<N>();
                    for ((a, b), &x) in top.iter().zip(bottom).zip(columns) {
                        dst[x] = *a;
                        dst[x + dy / N] = *b;
                    }
                } else {
                    for (value, &x) in top.iter().zip(columns) {
                        dst[x] = *value;
                    }
                }
            }
        }
    }
}

/// What a DXGI format stores per element: bytes, texels per element side
/// (4 for block compression), and whether its stencil is a second plane
/// (the GPU keeps depth and stencil apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub bytes: u32,
    pub block: u32,
    pub stencil: bool,
}

/// By DXGI format number. `None` for 96-bit, 1-bit, packed 4:2:2 and video
/// formats.
///
/// shortcut: those formats have no layout here; add them when a title uses
/// one.
pub fn format(dxgi: u32) -> Option<Format> {
    let (bytes, block, stencil) = match dxgi {
        1..=4 => (16, 1, false),
        9..=18 => (8, 1, false),
        19..=22 | 44..=47 => (4, 1, true),
        23..=43 | 67 | 87..=93 | 116 | 117 => (4, 1, false),
        48..=59 | 85 | 86 | 115 => (2, 1, false),
        118..=120 => (2, 1, true),
        60..=65 => (1, 1, false),
        70..=72 | 79..=81 => (8, 4, false),
        73..=78 | 82..=84 | 94..=99 => (16, 4, false),
        _ => return None,
    };
    Some(Format {
        bytes,
        block,
        stencil,
    })
}

/// A texture as the console allocates it: a D3D12-style description with
/// a swizzle mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureDesc {
    pub dimension: Dimension,
    /// In texels.
    pub width: u32,
    pub height: u32,
    /// Depth of a 3D texture, else the array size.
    pub depth_or_array: u32,
    /// 0: the full chain.
    pub mips: u32,
    /// DXGI format number.
    pub format: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    /// From the start of the texture.
    pub offset: u64,
    pub stencil: bool,
    pub layout: Layout,
}

/// Every data plane of a texture (depth, then stencil), placed one after
/// the other.
///
/// shortcut: no compression metadata planes (HTILE, CMASK, FMASK, DCC);
/// the console adds them to render targets and depth buffers unless the
/// description denies them, so those are smaller here than on the console.
/// The host GPU renders them, so nothing reads that memory; add the planes
/// if a title sizes a pool from console sizes and runs out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    pub planes: Vec<Plane>,
    pub size: u64,
    pub alignment: u64,
}

impl Texture {
    /// The texture on an Xbox Series X, with base swizzle 0.
    pub fn new(desc: &TextureDesc) -> Result<Texture, Error> {
        Texture::for_gpu(desc, Pipes::SERIES_X, 0)
    }

    /// The texture on a GPU of `pipes`; every plane takes base swizzle
    /// `pipe_bank_xor` (see [`Layout::for_gpu`]).
    pub fn for_gpu(desc: &TextureDesc, pipes: Pipes, pipe_bank_xor: u32) -> Result<Texture, Error> {
        let f = format(desc.format).ok_or(Error::Invalid)?;
        let depth3d = if desc.dimension == Dimension::Tex3d {
            desc.depth_or_array
        } else {
            1
        };
        let mips = match desc.mips {
            0 => {
                32 - desc
                    .width
                    .max(desc.height)
                    .max(depth3d)
                    .max(1)
                    .leading_zeros()
            }
            n => n,
        };
        let plane = |bytes| Desc {
            dimension: desc.dimension,
            width: desc.width,
            height: desc.height,
            depth: desc.depth_or_array,
            mips,
            element_bytes: bytes,
            block: f.block,
            samples: desc.samples.max(1),
            swizzle: desc.swizzle,
        };
        let mut planes = vec![Plane {
            offset: 0,
            stencil: false,
            layout: Layout::for_gpu(&plane(f.bytes), pipes, pipe_bank_xor)?,
        }];
        if f.stencil {
            let layout = Layout::for_gpu(&plane(1), pipes, pipe_bank_xor)?;
            let offset = planes[0].layout.size.next_multiple_of(layout.alignment);
            planes.push(Plane {
                offset,
                stencil: true,
                layout,
            });
        }
        let last = &planes[planes.len() - 1];
        let size = last.offset + last.layout.size;
        let alignment = planes.iter().map(|p| p.layout.alignment).max().unwrap_or(1);
        Ok(Texture {
            planes,
            size,
            alignment,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(w: u32, h: u32, mips: u32, bpe: u32, swizzle: Swizzle) -> Desc {
        Desc {
            dimension: Dimension::Tex2d,
            width: w,
            height: h,
            depth: 1,
            mips,
            element_bytes: bpe,
            block: 1,
            samples: 1,
            swizzle,
        }
    }

    /// Active bytes copied per second on synthetic surfaces:
    /// `cargo test --release -p victory-formats detile_throughput -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark"]
    fn detile_throughput() {
        use std::hint::black_box;
        use std::time::Instant;

        let bc = |n, swizzle| Desc {
            block: 4,
            ..desc(4096, 4096, 13, n, swizzle)
        };
        let cases = [
            ("BC7 64KB_D", bc(16, Swizzle::SW_64KB_D), 0),
            ("BC7 64KB_S", bc(16, Swizzle::SW_64KB_S), 0),
            ("BC1 64KB_D", bc(8, Swizzle::SW_64KB_D), 0),
            (
                "RGBA8 64KB_D",
                desc(1920, 1080, 1, 4, Swizzle::SW_64KB_D),
                0,
            ),
            ("RGBA8 4KB_S", desc(1920, 1080, 1, 4, Swizzle::SW_4KB_S), 0),
            ("BC7 64KB_D tail", bc(16, Swizzle::SW_64KB_D), 5),
            (
                "RGBA8 3D 4KB_S",
                Desc {
                    dimension: Dimension::Tex3d,
                    depth: 64,
                    ..desc(128, 128, 1, 4, Swizzle::SW_4KB_S)
                },
                0,
            ),
        ];
        for (name, d, mip) in cases {
            let layout = Layout::new(&d).unwrap();
            let m = layout.mips[mip as usize];
            let row = m.width as usize * d.element_bytes as usize;
            let plane = row * m.height as usize;
            let bytes = plane * m.depth as usize;
            let mut tiled = vec![0x5a; layout.size as usize];
            let mut linear = vec![0; bytes];
            let runs = (256usize << 20).div_ceil(bytes);
            for detile in [true, false] {
                let mut copy = || {
                    if detile {
                        layout.detile(
                            black_box(&tiled),
                            mip,
                            0,
                            black_box(&mut linear),
                            row,
                            plane,
                        )
                    } else {
                        layout.tile(
                            black_box(&mut tiled),
                            mip,
                            0,
                            black_box(&linear),
                            row,
                            plane,
                        )
                    }
                    .unwrap();
                };
                copy();
                let start = Instant::now();
                for _ in 0..runs {
                    copy();
                }
                let rate = (bytes * runs) as f64 / start.elapsed().as_secs_f64() / 1e6;
                let op = if detile { "detile" } else { "tile" };
                println!("{name} {op}: {rate:.1} MB/s");
            }
        }
    }

    /// The fast run walk must copy exactly what the span walk copies.
    /// Random synthetic surfaces, every swizzle, both directions.
    #[test]
    fn run_copies_match_span_copies_on_random_surfaces() {
        let mut seed = 0x2f6e_2b1d_9c40_7a13u64;
        let next = |seed: &mut u64, m: u64| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed % m
        };
        for swizzle in [
            Swizzle::LINEAR,
            Swizzle::SW_256B_S,
            Swizzle::SW_256B_D,
            Swizzle::SW_4KB_S,
            Swizzle::SW_4KB_D,
            Swizzle::SW_64KB_S,
            Swizzle::SW_64KB_D,
        ] {
            for element_bytes in [1, 2, 4, 8, 16] {
                for volume in [false, true] {
                    let block = if element_bytes == 16 { 4 } else { 1 };
                    let d = Desc {
                        dimension: if volume {
                            Dimension::Tex3d
                        } else {
                            Dimension::Tex2d
                        },
                        width: 1 + next(&mut seed, 96) as u32,
                        height: 1 + next(&mut seed, 48) as u32,
                        depth: 1 + next(&mut seed, 4) as u32,
                        mips: 1 + next(&mut seed, 4) as u32,
                        element_bytes,
                        block,
                        samples: 1,
                        swizzle,
                    };
                    let Ok(l) = Layout::new(&d) else { continue };
                    let tiled: Vec<u8> = (0..l.size)
                        .map(|i| next(&mut seed, 256) as u8 ^ i as u8)
                        .collect();
                    for (mip, m) in l.mips.clone().into_iter().enumerate() {
                        let row = m.width as usize * element_bytes as usize + mip % 3;
                        let plane = row * m.height as usize + mip % 2;
                        let len = plane * (m.depth.max(1) as usize - 1)
                            + row * (m.height as usize - 1)
                            + m.width as usize * element_bytes as usize;
                        let linear: Vec<u8> = (0..len)
                            .map(|i| next(&mut seed, 256) as u8 ^ (i >> 3) as u8)
                            .collect();
                        // Detile: the new path and the span path into the same shape.
                        let mut fast = vec![0xee; len];
                        let mut spans = vec![0xee; len];
                        let a = l.detile(&tiled, mip as u32, 0, &mut fast, row, plane);
                        let b = l.detile_spans(&tiled, mip as u32, 0, &mut spans, row, plane);
                        assert_eq!(a, b, "{d:?} mip {mip}");
                        assert_eq!(fast, spans, "detile {d:?} mip {mip}");
                        // Tile: both write the same tiled bytes.
                        let mut fast_tiled = vec![0xdd; l.size as usize];
                        let mut span_tiled = vec![0xdd; l.size as usize];
                        let a = l.tile(&mut fast_tiled, mip as u32, 0, &linear, row, plane);
                        let b = l.tile_spans(&mut span_tiled, mip as u32, 0, &linear, row, plane);
                        assert_eq!(a, b, "{d:?} mip {mip}");
                        assert_eq!(fast_tiled, span_tiled, "tile {d:?} mip {mip}");
                        // Slices that would overwrite each other are refused
                        // by both paths before any byte moves.
                        if m.depth > 1 {
                            let mut fast = vec![0xee; len];
                            let mut spans = vec![0xee; len];
                            let a = l.detile(&tiled, mip as u32, 0, &mut fast, row, plane / 2);
                            let b =
                                l.detile_spans(&tiled, mip as u32, 0, &mut spans, row, plane / 2);
                            assert_eq!(a, b, "{d:?} mip {mip} overlapping");
                            assert_eq!((fast, spans), (vec![0xee; len], vec![0xee; len]));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn linear_layouts_require_one_sample() {
        let linear = desc(256, 256, 1, 4, Swizzle::LINEAR);
        let single = Layout::new(&linear).unwrap();
        assert!(single.addressable());
        assert_eq!(single.size, 256 * 256 * 4);

        for samples in [2, 4, 8] {
            let multisampled = Desc { samples, ..linear };
            assert_eq!(Layout::new(&multisampled), Err(Error::Invalid));

            let tiled = Layout::new(&Desc {
                swizzle: Swizzle::SW_64KB_R_X,
                ..multisampled
            })
            .unwrap();
            assert_eq!(tiled.size, single.size * u64::from(samples));
        }
    }

    /// Sizes past the hardware's 16384 texels per side are refused instead
    /// of overflowing the size arithmetic (found by random-input testing).
    #[test]
    fn extents_past_the_hardware_limit_are_invalid() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_64KB_D_X] {
            let largest = Layout::new(&desc(MAX_EXTENT, MAX_EXTENT, 15, 16, swizzle)).unwrap();
            assert!(largest.size >= u64::from(MAX_EXTENT) * u64::from(MAX_EXTENT) * 16);
            for (w, h) in [
                (MAX_EXTENT + 1, 1),
                (1, MAX_EXTENT + 1),
                (u32::MAX, u32::MAX),
            ] {
                let result = Layout::new(&desc(w, h, 1, 16, swizzle));
                assert_eq!(result, Err(Error::Invalid), "{w}x{h}");
            }
            let mut deep = desc(1, 1, 1, 16, swizzle);
            deep.depth = u32::MAX;
            assert_eq!(Layout::new(&deep), Err(Error::Invalid));
        }
    }

    #[test]
    fn blocks_hold_their_size_in_elements() {
        for log2 in [8u32, 12, 16] {
            let swizzle = match log2 {
                8 => Swizzle::SW_256B_S,
                12 => Swizzle::SW_4KB_S,
                _ => Swizzle::SW_64KB_D,
            };
            for e in 0..5 {
                let l = Layout::new(&desc(1, 1, 1, 1 << e, swizzle)).unwrap();
                let [w, h, d] = l.block;
                assert_eq!(u64::from(w * h * d) << e, 1 << log2, "{log2} {e}");
                // The pattern covers the block's coordinates.
                assert!(l.flips.is_some(), "{log2} {e}");
            }
        }
        // 64 KiB, 16-byte elements: 64x64 (a 256x256 BC7 texture).
        let l = Layout::new(&desc(64, 64, 1, 16, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.block, [64, 64, 1]);
        assert_eq!((l.size, l.alignment), (65536, 65536));
    }

    /// Every element of every mip and slice lands on its own address, inside
    /// the texture, and tiling then detiling gives the rows back.
    #[test]
    fn addresses_are_a_bijection_and_copies_round_trip() {
        let cases = [
            desc(64, 64, 7, 16, Swizzle::SW_64KB_D),
            desc(100, 60, 7, 4, Swizzle::SW_64KB_D),
            desc(250, 175, 8, 8, Swizzle::SW_4KB_S),
            desc(33, 17, 6, 1, Swizzle::SW_4KB_D),
            desc(40, 9, 3, 2, Swizzle::SW_64KB_S),
            desc(30, 20, 5, 4, Swizzle::LINEAR),
            desc(83, 31, 6, 1, Swizzle::SW_256B_S),
            Desc {
                block: 4,
                ..desc(109, 71, 6, 16, Swizzle::SW_256B_D)
            },
            Desc {
                depth: 3,
                ..desc(70, 70, 4, 16, Swizzle::SW_4KB_S)
            },
            Desc {
                dimension: Dimension::Tex3d,
                depth: 16,
                ..desc(64, 32, 7, 4, Swizzle::SW_4KB_S)
            },
            // Pipe and bank XOR modes: every row moves the bits it flips.
            desc(64, 64, 6, 4, Swizzle::SW_64KB_D_X),
            desc(100, 60, 4, 16, Swizzle::SW_4KB_S_X),
            desc(37, 19, 5, 2, Swizzle::SW_64KB_S_T),
            desc(259, 19, 7, 1, Swizzle::SW_4KB_D_X),
            Desc {
                dimension: Dimension::Tex3d,
                depth: 12,
                ..desc(64, 32, 5, 8, Swizzle::SW_64KB_Z_X)
            },
        ];
        // Odd dimensions exercise a short final run and an unpaired row.
        // Cross a block boundary for every addressable pattern and run width.
        let arrays = [
            Swizzle::LINEAR,
            Swizzle::SW_256B_S,
            Swizzle::SW_256B_D,
            Swizzle::SW_4KB_S,
            Swizzle::SW_4KB_D,
            Swizzle::SW_64KB_S,
            Swizzle::SW_64KB_D,
        ]
        .into_iter()
        .flat_map(|swizzle| {
            [1, 2, 4, 8, 16].map(|n| Desc {
                depth: 2,
                ..desc(259, 19, 8, n, swizzle)
            })
        });
        let volumes = [Swizzle::LINEAR, Swizzle::SW_4KB_S, Swizzle::SW_64KB_S]
            .into_iter()
            .flat_map(|swizzle| {
                [1, 2, 4, 8, 16].map(|n| Desc {
                    dimension: Dimension::Tex3d,
                    depth: 35,
                    ..desc(37, 19, 6, n, swizzle)
                })
            });
        for d in cases.into_iter().chain(arrays).chain(volumes) {
            let l = Layout::new(&d).unwrap();
            let mut seen = vec![false; l.size as usize];
            let mut tiled = vec![0xcc; l.size as usize];
            let mut expected = tiled.clone();
            let n = d.element_bytes as usize;
            let slices = if d.dimension == Dimension::Tex3d {
                1
            } else {
                d.depth
            };
            for slice in 0..slices {
                for mip in 0..d.mips {
                    let m = l.mips[mip as usize];
                    let active = m.width as usize * n;
                    let row = active + 13;
                    let plane = row * m.height as usize + 11;
                    let linear: Vec<u8> = (0..plane * m.depth as usize)
                        .map(|i| {
                            ((i ^ (i >> 8) ^ (i >> 16)) + mip as usize * 13 + slice as usize) as u8
                        })
                        .collect();
                    l.tile(&mut tiled, mip, slice, &linear, row, plane).unwrap();
                    let mut back = vec![0xcc; linear.len()];
                    l.detile(&tiled, mip, slice, &mut back, row, plane).unwrap();
                    for z in 0..m.depth {
                        let base = z as usize * plane;
                        for y in 0..m.height {
                            let start = base + y as usize * row;
                            assert_eq!(back[start..start + active], linear[start..start + active]);
                            assert!(back[start + active..start + row].iter().all(|&b| b == 0xcc));
                            for x in 0..m.width {
                                let at = l.offset(mip, x, y, slice + z).unwrap() as usize;
                                let source = start + x as usize * n;
                                expected[at..at + n].copy_from_slice(&linear[source..source + n]);
                                assert!(at + n <= seen.len(), "{d:?} mip {mip} outside");
                                assert!(at.is_multiple_of(n));
                                assert!(!seen[at], "{d:?} mip {mip} ({x},{y},{z}) twice");
                                seen[at] = true;
                            }
                        }
                        assert!(
                            back[base + plane - 11..base + plane]
                                .iter()
                                .all(|&b| b == 0xcc)
                        );
                    }
                    assert!(tiled == expected, "{d:?} mip {mip} slice {slice}");
                }
            }
        }
    }

    /// Hand-checked: in a 64 KiB `D` block of 4-byte elements, x bits go to
    /// address bits 2, 3, 6, 9, 11, 13, 15 and y bits to 4, 5, 7, 8, 10, 12,
    /// 14; a 4 KiB `S` block of 16-byte elements is 16x16 with y first.
    #[test]
    fn hand_checked_addresses() {
        let l = Layout::new(&desc(128, 128, 1, 4, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.offset(0, 1, 0, 0), Ok(1 << 2));
        assert_eq!(l.offset(0, 4, 0, 0), Ok(1 << 6));
        assert_eq!(l.offset(0, 0, 4, 0), Ok(1 << 7));
        assert_eq!(l.offset(0, 0, 8, 0), Ok(1 << 8));
        assert_eq!(l.offset(0, 64, 64, 0), Ok((1 << 15) | (1 << 14)));
        let l = Layout::new(&desc(32, 32, 1, 16, Swizzle::SW_4KB_S)).unwrap();
        assert_eq!(l.block, [16, 16, 1]);
        assert_eq!(l.offset(0, 0, 1, 0), Ok(1 << 4));
        assert_eq!(l.offset(0, 1, 0, 0), Ok(1 << 6));
        // Blocks row by row: (16, 0) is the second block, (0, 16) the third.
        assert_eq!(l.offset(0, 16, 0, 0), Ok(4096));
        assert_eq!(l.offset(0, 0, 16, 0), Ok(8192));
    }

    /// A 256x256 BC7 chain in 64 KiB `D`: one tail block holding mips 1-8,
    /// then mip 0; mip 1 sits at x = 32 elements, 32 KiB into the tail.
    #[test]
    fn mip_extents_cover_each_slice_once_and_the_tail_first() {
        let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_4KB_S)).unwrap();
        let e = l.mip_extents();
        assert_eq!(e.len(), l.mips.len());
        // The tail block first (every tail mip inside it), then mip 2, 1, 0,
        // each up to the next one's start, the last up to the slice's end.
        assert_eq!(e[3], (0, 4096));
        assert_eq!(e[4], (0, 4096));
        assert_eq!(e[2], (4096, 8192));
        assert_eq!(e[1], (8192, 8192 + 16384));
        assert_eq!(e[0].0, 8192 + 16384);
        assert_eq!(e[0].1, l.slice_size);
        // Every mip's range lies in the slice and none overlaps another
        // outside the tail.
        for (i, &(start, end)) in e.iter().enumerate() {
            assert!(
                end <= l.slice_size,
                "mip {i}: {start}..{end} of {}",
                l.slice_size
            );
            assert!(start < end, "mip {i} empty");
        }
        let mut whole: Vec<(u64, u64)> = e.iter().copied().filter(|&(s, _)| s != 0).collect();
        whole.sort_unstable();
        for pair in whole.windows(2) {
            assert!(pair[0].1 <= pair[1].0, "mips overlap: {pair:?}");
        }
    }

    #[test]
    fn mip_tail_comes_first() {
        let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.first_tail_mip, 1);
        assert_eq!(l.size, 2 * 65536);
        assert_eq!(l.mips[0].offset, 65536);
        assert_eq!(
            l.mips[1].tail,
            Some(Tail {
                offset: 32768,
                x: 32,
                y: 0
            })
        );
        assert_eq!(l.offset(1, 0, 0, 0), Ok(32768));
        // 4 KiB blocks hold fewer tail mips: 16x16 BC7 mips stay outside.
        let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_4KB_S)).unwrap();
        assert_eq!(l.first_tail_mip, 3);
        assert_eq!(l.mips[2].offset, 4096);
        assert_eq!(l.mips[1].offset, 8192);
        assert_eq!(l.mips[0].offset, 8192 + 16384);
    }

    #[test]
    fn depth_stencil_is_two_planes() {
        let t = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width: 1920,
            height: 1080,
            depth_or_array: 1,
            mips: 1,
            format: 20,
            samples: 1,
            swizzle: Swizzle::SW_64KB_Z_X,
        })
        .unwrap();
        assert_eq!(t.planes.len(), 2);
        // 4-byte depth in 128x128 blocks, 1-byte stencil in 256x256 blocks.
        assert_eq!(t.planes[0].layout.size, 1920 * 1152 * 4);
        assert_eq!(t.planes[1].offset, t.planes[0].layout.size);
        assert_eq!(t.planes[1].layout.size, 2048 * 1280);
        assert!(t.planes[0].layout.addressable());
    }

    #[test]
    fn texture_copies_reject_overflowing_pitches() {
        let layout = Layout::new(&desc(4, 2, 1, 4, Swizzle::LINEAR)).unwrap();
        let mut tiled = vec![0; layout.size as usize];
        let mut linear = [0; 32];
        assert_eq!(
            layout.detile(&tiled, 0, 0, &mut linear, usize::MAX, 0),
            Err(Error::TooSmall)
        );
        assert_eq!(
            layout.tile(&mut tiled, 0, 0, &linear, usize::MAX, 0),
            Err(Error::TooSmall)
        );
    }

    #[test]
    fn copy_errors_precede_writes_in_both_directions() {
        for (swizzle, samples, mip, slice, error) in [
            (Swizzle::SW_64KB_D_X, 1, 0, 0, Error::TooSmall),
            (Swizzle::SW_64KB_S_X, 1, 0, 0, Error::TooSmall),
            (Swizzle::SW_64KB_S_T, 1, 0, 0, Error::TooSmall),
            (Swizzle::SW_64KB_D, 2, 0, 0, Error::Unsupported),
            (Swizzle::SW_64KB_D_X, 1, 1, 0, Error::Invalid),
            (Swizzle::SW_64KB_S_X, 1, 0, 1, Error::Invalid),
            (Swizzle::SW_64KB_D, 1, 0, 0, Error::TooSmall),
        ] {
            let l = Layout::new(&Desc {
                samples,
                ..desc(17, 9, 1, 16, swizzle)
            })
            .unwrap();
            let mut tiled = [0xac];
            let mut linear = [0xdc];
            assert_eq!(l.tile(&mut tiled, mip, slice, &linear, 0, 0), Err(error));
            assert_eq!(l.detile(&tiled, mip, slice, &mut linear, 0, 0), Err(error));
            assert_eq!((tiled, linear), ([0xac], [0xdc]));
        }
        let l = volume_pitch_layout(Swizzle::SW_4KB_S);
        assert_eq!(l.tile(&mut [], 0, 1, &[], 0, 0), Err(Error::Invalid));
        assert_eq!(l.detile(&[], 0, 1, &mut [], 0, 0), Err(Error::Invalid));
    }

    fn volume_pitch_layout(swizzle: Swizzle) -> Layout {
        Layout::new(&Desc {
            dimension: Dimension::Tex3d,
            depth: 2,
            ..desc(4, 2, 1, 4, swizzle)
        })
        .unwrap()
    }

    #[test]
    fn depth_pitch_rejects_overlapping_detile() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
            let l = volume_pitch_layout(swizzle);
            let tiled = vec![0x42; l.size as usize];
            let mut linear = [0xcc; 72];

            for pitch in [0, 16, 35] {
                assert_eq!(
                    l.detile(&tiled, 0, 0, &mut linear, 20, pitch),
                    Err(Error::TooSmall),
                    "{swizzle:?}, pitch {pitch}"
                );

                assert_eq!(linear, [0xcc; 72]);
            }
        }
    }

    #[test]
    fn depth_pitch_rejects_overlapping_tile() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
            let l = volume_pitch_layout(swizzle);
            let mut tiled = vec![0xcc; l.size as usize];
            let linear = [0x42; 72];

            for pitch in [0, 16, 35] {
                assert_eq!(
                    l.tile(&mut tiled, 0, 0, &linear, 20, pitch),
                    Err(Error::TooSmall),
                    "{swizzle:?}, pitch {pitch}"
                );

                assert!(tiled.iter().all(|&b| b == 0xcc));
            }
        }
    }

    #[test]
    fn depth_pitch_accepts_exact_and_padded_slices() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_4KB_S] {
            let l = volume_pitch_layout(swizzle);

            for pitch in [36usize, 40, 64] {
                let len = pitch + 36;
                let mut tiled = vec![0; l.size as usize];

                let linear: Vec<u8> = (0..len).map(|i| i as u8).collect();

                let mut back = vec![0xcc; len];

                l.tile(&mut tiled, 0, 0, &linear, 20, pitch).unwrap();

                l.detile(&tiled, 0, 0, &mut back, 20, pitch).unwrap();

                for z in 0..2 {
                    for y in 0..2 {
                        let start = z * pitch + y * 20;

                        assert_eq!(back[start..start + 16], linear[start..start + 16]);
                    }
                }
            }
        }
    }

    #[test]
    fn volume_copy_rejects_overflowing_depth_pitch() {
        let layout = Layout::new(&Desc {
            dimension: Dimension::Tex3d,
            depth: 3,
            ..desc(4, 2, 1, 4, Swizzle::LINEAR)
        })
        .unwrap();
        let tiled = vec![0; layout.size as usize];
        let mut linear = [0; 96];
        assert_eq!(
            layout.detile(&tiled, 0, 0, &mut linear, 16, usize::MAX),
            Err(Error::TooSmall)
        );
    }

    #[test]
    fn bad_descriptions_and_buffers_are_refused() {
        assert_eq!(
            Layout::new(&desc(0, 4, 1, 4, Swizzle::SW_4KB_S)),
            Err(Error::Invalid)
        );
        assert_eq!(
            Layout::new(&desc(4, 4, 1, 12, Swizzle::SW_4KB_S)),
            Err(Error::Invalid)
        );
        assert_eq!(
            Layout::new(&desc(4, 4, 1, 4, Swizzle(3))),
            Err(Error::Invalid)
        );
        let l = Layout::new(&desc(64, 64, 1, 4, Swizzle::SW_4KB_S)).unwrap();
        let mut out = vec![0; 64 * 64 * 4];
        assert_eq!(
            l.detile(&[0; 100], 0, 0, &mut out, 256, 0),
            Err(Error::TooSmall)
        );
        assert_eq!(l.offset(0, 64, 0, 0), Err(Error::Invalid));
    }
    /// The XOR modes address every element once too, whatever the console:
    /// Series X takes 32 pipes and 8 packers, Series S 8 and 4.
    #[test]
    fn xor_modes_address_every_element_once_on_both_consoles() {
        let cases = [
            desc(64, 64, 1, 4, Swizzle::SW_64KB_S_X),
            desc(200, 137, 4, 16, Swizzle::SW_4KB_D_X),
            desc(96, 71, 3, 8, Swizzle::SW_64KB_D_T),
            desc(37, 19, 2, 2, Swizzle::SW_4KB_D_X),
            Desc {
                dimension: Dimension::Tex3d,
                depth: 9,
                ..desc(48, 24, 3, 4, Swizzle::SW_4KB_S_X)
            },
        ];
        for d in cases {
            for pipes in [Pipes::SERIES_X, Pipes::SERIES_S] {
                for base in [0, 0x15] {
                    let l = Layout::for_gpu(&d, pipes, base).unwrap();
                    assert!(l.addressable(), "{d:?} {pipes:?}");
                    let n = d.element_bytes as usize;
                    let mut seen = vec![false; l.size as usize];
                    let slices = if d.dimension == Dimension::Tex3d {
                        1
                    } else {
                        d.depth
                    };
                    for slice in 0..slices {
                        for mip in 0..d.mips {
                            let m = l.mips[mip as usize];
                            for z in 0..m.depth {
                                for y in 0..m.height {
                                    for x in 0..m.width {
                                        let at = l.offset(mip, x, y, slice + z).unwrap() as usize;
                                        assert!(at + n <= seen.len(), "{d:?} {pipes:?} outside");
                                        assert!(!seen[at], "{d:?} {pipes:?} ({x},{y},{z}) twice");
                                        seen[at] = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The two consoles' pipe counts give different addresses for the same
    /// XOR texture, and a base swizzle the configuration has no bits for is
    /// ignored: Series S has 3 pipe bits, so 7 stays pipe 7 there but is
    /// Series X's pipe 7 of 32.
    #[test]
    fn the_consoles_xor_addresses_differ() {
        let d = desc(256, 256, 1, 4, Swizzle::SW_64KB_S_X);
        let series_x = Layout::for_gpu(&d, Pipes::SERIES_X, 0).unwrap();
        let series_s = Layout::for_gpu(&d, Pipes::SERIES_S, 0).unwrap();
        let differs = |a: &Layout, b: &Layout| {
            (0..64)
                .flat_map(|y| (0..64).map(move |x| (x, y)))
                .filter(|&(x, y)| a.offset(0, x, y, 0) != b.offset(0, x, y, 0))
                .count()
        };
        // 32 pipes on Series X, 8 on Series S: another pipe, another block.
        assert!(differs(&series_x, &series_s) > 0);
        // The base swizzle lands in the block's offset: pipe bits, then bank
        // bits two columns above them.
        let based = Layout::for_gpu(&d, Pipes::SERIES_X, 0x15).unwrap();
        assert_eq!(based.pipe_bank_xor, 0x15);
        assert!(based.xor != 0);
        assert!(differs(&based, &series_x) > 0);
        // The modes without XOR ignore it entirely.
        let plain = Layout::for_gpu(
            &desc(64, 64, 1, 4, Swizzle::SW_64KB_S),
            Pipes::SERIES_X,
            0x15,
        )
        .unwrap();
        assert_eq!(
            plain.offset(0, 7, 3, 0),
            Layout::new(&desc(64, 64, 1, 4, Swizzle::SW_64KB_S))
                .unwrap()
                .offset(0, 7, 3, 0)
        );
        assert_eq!((plain.pipe_bank_xor, plain.xor), (0, 0));
    }

    /// addrlib has no `4KB_R_X` pattern: such a layout has a size only.
    #[test]
    fn read_only_xor_has_no_pattern() {
        let l = Layout::new(&desc(64, 64, 1, 4, Swizzle::SW_4KB_R_X)).unwrap();
        assert!(!l.addressable());
        assert_eq!(l.offset(0, 0, 0, 0), Err(Error::Unsupported));
    }

    /// The span-per-element walk (`VICTORY_TEXTURE_RUNS=0`) gives the same
    /// rows as the run walk, XOR modes included.
    #[test]
    fn the_span_walk_matches_the_run_walk_in_xor_modes() {
        for swizzle in [
            Swizzle::SW_64KB_D_X,
            Swizzle::SW_4KB_S_X,
            Swizzle::SW_64KB_S,
        ] {
            let d = desc(96, 71, 3, 8, swizzle);
            let l = Layout::for_gpu(&d, Pipes::SERIES_S, 0x9).unwrap();
            let m = l.mips[0];
            let n = d.element_bytes as usize;
            let row = m.width as usize * n + 8;
            let plane = row * m.height as usize;
            let linear: Vec<u8> = (0..plane).map(|i| (i * 7) as u8).collect();
            let mut tiled = vec![0u8; l.size as usize];
            l.tile(&mut tiled, 0, 0, &linear, row, plane).unwrap();
            let mut fast = vec![0u8; linear.len()];
            l.detile(&tiled, 0, 0, &mut fast, row, plane).unwrap();
            let mut spans = vec![0u8; linear.len()];
            l.detile_spans(&tiled, 0, 0, &mut spans, row, plane)
                .unwrap();
            assert!(fast == spans, "{swizzle:?}");
            // Both walks copy into the same tiled bytes, so the faster one
            // cannot have written something the span walk did not.
            let mut tiled_spans = vec![0u8; l.size as usize];
            l.tile_spans(&mut tiled_spans, 0, 0, &linear, row, plane)
                .unwrap();
            assert!(tiled == tiled_spans, "{swizzle:?}");
        }
    }
}
