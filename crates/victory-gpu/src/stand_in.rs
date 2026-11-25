//! A constant in place of what a program that does not run would write to
//! its unordered views: a stand-in, opt-in at run time.
//!
//! A program can be held back (one walking acceleration structures the
//! builder has not built), dropped, or left without a host pipeline (it does
//! not translate). Whatever it would have written then holds what the memory
//! held, and the passes that read it see garbage: a shadow mask nothing
//! writes can read as "in shadow" everywhere.
//!
//! `VICTORY_STAND_IN=<program>:<view>:<value>`, rules separated by `;`:
//! - `<program>`: the 16-hex-digit shader hash the log prints for the
//!   compute program (`compute 0123456789abcdef`);
//! - `<view>`: the unordered view as the console shader addresses it, root
//!   parameter and offset in dwords in its descriptor table, `s1+176` or
//!   `1+176` (the notation of the diagnostics' table listings). That
//!   address comes from the console code itself, so it holds across
//!   translator versions, where an order of the views would not;
//! - `<value>`: a colour `r,g,b[,a]` (alpha 1 if left out), optionally
//!   with the packing of an integer view: `rgb9e5=`, `r11g11b10=` or
//!   `rgba16f=`; or a raw word `0x...`, `mask` (all ones) or `zero` for an
//!   integer view of any width, which keeps each channel's low bits
//!   (`zero` clears a float view too). A view in a float or normalized
//!   format takes the colour as it is.
//!
//! Where a dispatch of the program, direct or indirect, does not run, its
//! named views are cleared to the values instead, between unordered access
//! barriers. Nothing of the program runs, so it needs no translation and
//! walks nothing. Only named programs are touched, and nothing happens
//! while the variable is unset. Since the program's views are not known
//! when it does not run, its rules are not checked against them.
//!
//! The masks and zeros for narrower integer outputs follow ScarlettPort's
//! stand-in encodings (`emit/stand_in.rs`, GPL-3.0-or-later), whose
//! shadow-mask stand-in suggested this one; that version rewrites the
//! program's stores at translation, where this one runs nothing.
//! shortcut: the same value everywhere, every frame.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How an integer view holds a colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Packing {
    Rgb9e5,
    R11g11b10,
    Rgba16f,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Value {
    /// A colour, and the packing an integer view takes (none: by the
    /// texture's format).
    Colour([f32; 4], Option<Packing>),
    /// A word for an integer view (its low bits per channel).
    Raw(u32),
}

/// One stand-in: program, view, value.
#[derive(Debug)]
pub(crate) struct Rule {
    /// Lowercase, as logged.
    pub(crate) program: String,
    pub(crate) slot: u8,
    pub(crate) offset: u32,
    pub(crate) value: Value,
    /// The first match was logged.
    pub(crate) seen: AtomicBool,
}

impl Rule {
    /// The view as written in the variable and the logs.
    pub(crate) fn view(&self) -> String {
        format!("s{}+{}", self.slot, self.offset)
    }
}

/// The rules in `text` (the variable's value).
pub(crate) fn parse(text: &str) -> Result<Vec<Rule>, String> {
    text.split(';')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|rule| parse_rule(rule).map_err(|e| format!("{rule:?}: {e}")))
        .collect()
}

fn parse_rule(rule: &str) -> Result<Rule, String> {
    let mut parts = rule.splitn(3, ':');
    let (Some(program), Some(view), Some(value)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err("not <program>:<view>:<value>".into());
    };
    let program = program.trim().to_ascii_lowercase();
    if program.len() != 16 || !program.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the program is not a 16-hex-digit hash".into());
    }
    let view = view.trim();
    let view = view.strip_prefix(['s', 'S']).unwrap_or(view);
    let Some((slot, offset)) = view.split_once('+') else {
        return Err("the view is not <root parameter>+<dwords>".into());
    };
    let slot = slot
        .trim()
        .parse::<u8>()
        .map_err(|_| "the view's root parameter is not a number")?;
    let offset = offset
        .trim()
        .parse::<u32>()
        .map_err(|_| "the view's offset is not a number")?;
    Ok(Rule {
        program,
        slot,
        offset,
        value: parse_value(value.trim())?,
        seen: AtomicBool::new(false),
    })
}

fn parse_value(value: &str) -> Result<Value, String> {
    match value.to_ascii_lowercase().as_str() {
        "mask" => return Ok(Value::Raw(u32::MAX)),
        "zero" => return Ok(Value::Raw(0)),
        _ => {}
    }
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16)
            .map(Value::Raw)
            .map_err(|_| "the raw word is not 32-bit hex".into());
    }
    let (packing, colour) = match value.split_once('=') {
        Some((name, colour)) => {
            let packing = match name.trim().to_ascii_lowercase().as_str() {
                "rgb9e5" => Packing::Rgb9e5,
                "r11g11b10" => Packing::R11g11b10,
                "rgba16f" => Packing::Rgba16f,
                other => return Err(format!("no packing {other:?} (rgb9e5, r11g11b10, rgba16f)")),
            };
            (Some(packing), colour)
        }
        None => (None, value),
    };
    let channels = colour
        .split(',')
        .map(|c| {
            c.trim()
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
        })
        .collect::<Option<Vec<f32>>>()
        .ok_or("the colour is not 3 or 4 finite numbers of at least 0")?;
    let rgba = match channels[..] {
        [r, g, b] => [r, g, b, 1.0],
        [r, g, b, a] => [r, g, b, a],
        _ => return Err("the colour is not 3 or 4 numbers".into()),
    };
    Ok(Value::Colour(rgba, packing))
}

/// The rules `VICTORY_STAND_IN` sets (in place of a program that does not
/// run), read once; none if unset or wrong.
pub(crate) fn rules() -> &'static [Rule] {
    #[cfg(test)]
    if let Some(rules) = TEST_RULES.with(std::cell::Cell::get) {
        return rules;
    }
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let Ok(text) = std::env::var("VICTORY_STAND_IN") else {
            return Vec::new();
        };
        match parse(&text) {
            Ok(rules) => {
                for r in &rules {
                    tracing::info!(program = %r.program, view = %r.view(), value = ?r.value, "stand-in set");
                }
                rules
            }
            Err(e) => {
                tracing::warn!(error = %e, "VICTORY_STAND_IN not understood: no stand-in");
                Vec::new()
            }
        }
    })
}

// Rules that tests set for their own thread in place of the variable's.
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_RULES: std::cell::Cell<Option<&'static [Rule]>> =
        const { std::cell::Cell::new(None) };
}

/// What the clear writes: floats for a float or normalized view, words for
/// an integer one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Fill {
    Float([f32; 4]),
    Uint([u32; 4]),
}

/// Whether `format` (a host format code) is an integer one, which a float
/// clear does not take.
pub(crate) fn integer(format: u32) -> bool {
    matches!(
        format,
        3 | 4 | 7 | 8 | 12 | 13 | 17 | 18 | 30 | 31 | 36 | 37 | 42 | 43 | 56 | 57 | 61 | 62
    )
}

/// The packing a console format implies for an integer view of it, if it
/// says one.
fn implied(format: u32) -> Option<Packing> {
    match format {
        26 => Some(Packing::R11g11b10),
        67 => Some(Packing::Rgb9e5),
        _ => None,
    }
}

/// What the clear of view format `format` writes for `value`: a `Float`
/// clear for a float or normalized view, a `Uint` one for an integer view,
/// where the colour is packed as the value names, or else as the format
/// says.
pub(crate) fn fill(format: u32, value: Value) -> Result<Fill, &'static str> {
    if format == 0 {
        return Err("a view format a clear does not take");
    }
    if !integer(format) {
        return match value {
            // Zero clears any view; a word is no colour in a float one.
            Value::Raw(0) => Ok(Fill::Float([0.0; 4])),
            Value::Raw(_) => Err("a float view takes a colour, or zero"),
            Value::Colour(rgba, _) => Ok(Fill::Float(rgba)),
        };
    }
    let rgba = match value {
        // The clear keeps each channel's low bits (all ones stays a mask).
        Value::Raw(word) => return Ok(Fill::Uint([word; 4])),
        Value::Colour(rgba, packing) => match packing.or_else(|| implied(format)) {
            Some(packing) => (rgba, packing),
            None => {
                return Err(
                    "an integer view of a texture whose format does not say the packing: name it",
                );
            }
        },
    };
    let ([r, g, b, a], packing) = rgba;
    let half = |v: f32| small_float(v, 10);
    match (packing, format) {
        (Packing::Rgb9e5, 42 | 43) => Ok(Fill::Uint([rgb9e5([r, g, b]); 4])),
        (Packing::R11g11b10, 42 | 43) => Ok(Fill::Uint([r11g11b10([r, g, b]); 4])),
        // R16G16B16A16_UINT: a half per channel.
        (Packing::Rgba16f, 12) => Ok(Fill::Uint([half(r), half(g), half(b), half(a)])),
        // R32G32_UINT: two halves per word.
        (Packing::Rgba16f, 17) => Ok(Fill::Uint([
            half(r) | half(g) << 16,
            half(b) | half(a) << 16,
            0,
            0,
        ])),
        _ => Err("a packing whose size is not the view's"),
    }
}

/// R9G9B9E5_SHAREDEXP as the D3D specification packs it: a 5-bit shared
/// exponent (bias 15) over three 9-bit mantissas, channels clamped to
/// [0, 65408], rounded to nearest.
pub(crate) fn rgb9e5(rgb: [f32; 3]) -> u32 {
    const MAX: f64 = 65408.0;
    let c = rgb.map(|v| {
        if v.is_nan() {
            0.0
        } else {
            f64::from(v).clamp(0.0, MAX)
        }
    });
    let most = c[0].max(c[1]).max(c[2]);
    // floor(log2(most)), exact for the powers of two; -16 at least.
    let floor_log2 = if most > 0.0 {
        most.log2().floor() as i32
    } else {
        -16
    };
    let floor_log2 = match floor_log2 {
        // log2 may round up just under a power of two.
        e if 2f64.powi(e) > most => e - 1,
        e => e,
    };
    let mut exponent = floor_log2.max(-16) + 16;
    let mut scale = 2f64.powi(24 - exponent);
    if (most * scale + 0.5).floor() >= 512.0 {
        exponent += 1;
        scale *= 0.5;
    }
    let m = c.map(|v| ((v * scale + 0.5).floor() as u32).min(511));
    m[0] | m[1] << 9 | m[2] << 18 | (exponent as u32) << 27
}

/// R11G11B10_FLOAT: 11-, 11- and 10-bit floats without sign.
pub(crate) fn r11g11b10([r, g, b]: [f32; 3]) -> u32 {
    small_float(r, 6) | small_float(g, 6) << 11 | small_float(b, 5) << 22
}

/// `v` as a float without sign of a 5-bit exponent (bias 15) and
/// `mantissa` bits, rounded to nearest even (a half's bits for 10, its
/// sign 0). Negative and NaN give 0; too large and infinity the largest
/// finite value (shortcut: not infinity, which a stand-in light must not
/// be).
pub(crate) fn small_float(v: f32, mantissa: u32) -> u32 {
    let largest = (30 << mantissa) | ((1 << mantissa) - 1);
    if v.is_nan() || v <= 0.0 {
        return 0;
    }
    let bits = v.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127;
    if exponent > 15 {
        return largest;
    }
    let significand = u64::from(bits & 0x7f_ffff | 0x80_0000);
    let shift = (23 - mantissa as i32 + (-14 - exponent).max(0)) as u32;
    if shift > 40 {
        return 0;
    }
    let mut out = significand >> shift;
    let rest = significand & ((1 << shift) - 1);
    let half = 1u64 << (shift - 1);
    if rest > half || (rest == half && out & 1 == 1) {
        out += 1;
    }
    let out = if exponent >= -14 {
        out - (1 << mantissa) + (((exponent + 15) as u64) << mantissa)
    } else {
        out
    };
    (out as u32).min(largest)
}

static FILLED: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);

// The stand-ins recorded on this thread (tests): tests run in parallel,
// each on its own thread, and record their stand-ins there.
#[cfg(test)]
thread_local! {
    static TEST_FILLED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A stand-in recorded (`filled`) or not, logged every 5 s with the counts
/// so far.
pub(crate) fn note(filled: bool) {
    if filled {
        FILLED.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        TEST_FILLED.with(|n| n.set(n.get() + 1));
    } else {
        SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let now = Instant::now();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    match *last {
        Some(t) if now.duration_since(t) < Duration::from_secs(5) => {}
        Some(_) => {
            tracing::info!(
                filled = FILLED.load(Ordering::Relaxed),
                skipped = SKIPPED.load(Ordering::Relaxed),
                "stand-ins: views filled so far"
            );
            *last = Some(now);
        }
        None => *last = Some(now),
    }
}

/// How many stand-ins this thread recorded (tests): not the run's count,
/// which tests running alongside add to.
#[cfg(test)]
pub(crate) fn filled() -> u64 {
    TEST_FILLED.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reference R9G9B9E5 encoder written another way: the smallest
    /// shared exponent whose rounded mantissas all fit in 9 bits.
    fn reference_rgb9e5(rgb: [f32; 3]) -> u32 {
        let c = rgb.map(|v| f64::from(v).clamp(0.0, 65408.0));
        for e in 0..32u32 {
            let unit = 2f64.powi(e as i32 - 24);
            let m = c.map(|v| (v / unit + 0.5).floor() as u32);
            if m.iter().all(|&m| m <= 511) {
                return m[0] | m[1] << 9 | m[2] << 18 | e << 27;
            }
        }
        unreachable!("65408 fits at exponent 31")
    }

    fn decode_rgb9e5(w: u32) -> [f64; 3] {
        let unit = 2f64.powi((w >> 27) as i32 - 24);
        [w & 511, (w >> 9) & 511, (w >> 18) & 511].map(|m| f64::from(m) * unit)
    }

    /// The code nearest `v` (ties to the even code), found by decoding
    /// every code of the small float.
    fn reference_small(v: f32, mantissa: u32) -> u32 {
        let decode = |code: u32| {
            let (e, m) = (
                (code >> mantissa) as i32,
                f64::from(code & ((1 << mantissa) - 1)),
            );
            let one = f64::from(1u32 << mantissa);
            if e == 0 {
                m / one * 2f64.powi(-14)
            } else {
                (1.0 + m / one) * 2f64.powi(e - 15)
            }
        };
        let largest = (30 << mantissa) | ((1 << mantissa) - 1);
        let v = f64::from(v.max(0.0));
        (0..=largest)
            .min_by(|&a, &b| {
                let (da, db) = ((decode(a) - v).abs(), (decode(b) - v).abs());
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("a code")
            .min(largest)
    }

    #[test]
    fn rules_are_program_view_value() {
        let rules = parse("0123456789abcdef:s1+176:1,1,1; ABCDEF0123456789 : 2+8 : mask ").unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].program, "0123456789abcdef");
        assert_eq!((rules[0].slot, rules[0].offset), (1, 176));
        assert_eq!(rules[0].value, Value::Colour([1.0, 1.0, 1.0, 1.0], None));
        assert_eq!(rules[0].view(), "s1+176");
        assert_eq!(rules[1].program, "abcdef0123456789");
        assert_eq!((rules[1].slot, rules[1].offset), (2, 8));
        assert_eq!(rules[1].value, Value::Raw(u32::MAX));
    }

    #[test]
    fn a_rule_that_is_not_three_parts_is_refused() {
        assert!(parse("0123456789abcdef:s1+176").is_err());
        assert!(parse("zz23456789abcdef:s1+176:mask").is_err());
        assert!(parse("0123456789abcdef:1-176:mask").is_err());
        assert!(parse("0123456789abcdef:s1+176:1,1").is_err(), "two channels");
        assert!(parse("0123456789abcdef:s1+176:red").is_err());
    }

    #[test]
    fn values_are_colours_raw_words_or_packed_colours() {
        assert_eq!(
            parse("0123456789abcdef:1+0:rgb9e5=0.6,0.588,0.564").unwrap()[0].value,
            Value::Colour([0.6, 0.588, 0.564, 1.0], Some(Packing::Rgb9e5))
        );
        assert_eq!(
            parse("0123456789abcdef:1+0:rgba16f=1,0.5,0.25,0").unwrap()[0].value,
            Value::Colour([1.0, 0.5, 0.25, 0.0], Some(Packing::Rgba16f))
        );
        assert_eq!(
            parse("0123456789abcdef:1+0:0xdeadbeef").unwrap()[0].value,
            Value::Raw(0xdeadbeef)
        );
        assert_eq!(parse("0123456789abcdef:1+0:zero").unwrap()[0].value, Value::Raw(0));
        assert!(parse("0123456789abcdef:1+0:rgb9e6=1,1,1").is_err(), "no such packing");
    }

    #[test]
    fn a_float_view_takes_the_colour_as_it_is() {
        // R16_FLOAT: a "lit" mask.
        assert_eq!(fill(54, Value::Colour([1.0, 1.0, 1.0, 1.0], None)), Ok(Fill::Float([1.0; 4])));
        // R11G11B10_FLOAT and R9G9B9E5 are float views too.
        assert_eq!(fill(26, Value::Colour([0.5, 0.5, 0.5, 1.0], None)), Ok(Fill::Float([0.5, 0.5, 0.5, 1.0])));
        assert_eq!(fill(67, Value::Colour([0.5, 0.5, 0.5, 1.0], None)), Ok(Fill::Float([0.5, 0.5, 0.5, 1.0])));
        // Zero clears any view; a mask is not a float colour.
        assert_eq!(fill(54, Value::Raw(0)), Ok(Fill::Float([0.0; 4])));
        assert_eq!(fill(54, Value::Raw(u32::MAX)), Err("a float view takes a colour, or zero"));
    }

    #[test]
    fn an_integer_view_takes_a_word_whatever_its_width() {
        // R8_UINT: all ones is its mask (the low 8 bits are what it keeps).
        assert_eq!(fill(61, Value::Raw(u32::MAX)), Ok(Fill::Uint([u32::MAX; 4])));
        assert_eq!(fill(56, Value::Raw(0)), Ok(Fill::Uint([0; 4])));
        assert_eq!(fill(42, Value::Raw(0x3daaaaab)), Ok(Fill::Uint([0x3daaaaab; 4])));
    }

    #[test]
    fn an_integer_view_packs_the_colour_as_the_value_or_the_format_says() {
        // R32_UINT says nothing of a colour: it must be named.
        assert_eq!(
            fill(42, Value::Colour([0.6, 0.588, 0.564, 1.0], None)),
            Err("an integer view of a texture whose format does not say the packing: name it")
        );
        let packed = fill(42, Value::Colour([0.6, 0.588, 0.564, 1.0], Some(Packing::Rgb9e5)));
        assert_eq!(packed, Ok(Fill::Uint([rgb9e5([0.6, 0.588, 0.564]); 4])));
        // R11G11B10_FLOAT and R9G9B9E5 are float views: named, a colour
        // packs into an R32_UINT whose format says nothing.
        let small = fill(42, Value::Colour([0.5, 0.25, 1.0, 1.0], Some(Packing::R11g11b10)));
        assert_eq!(small, Ok(Fill::Uint([r11g11b10([0.5, 0.25, 1.0]); 4])));
        let exp = fill(42, Value::Colour([0.5, 0.25, 1.0, 1.0], Some(Packing::Rgb9e5)));
        assert_eq!(exp, Ok(Fill::Uint([rgb9e5([0.5, 0.25, 1.0]); 4])));
        // R16G16B16A16_UINT: a half per channel.
        let halves = fill(12, Value::Colour([1.0, 0.5, 0.25, 0.0], Some(Packing::Rgba16f)));
        assert_eq!(
            halves,
            Ok(Fill::Uint([
                small_float(1.0, 10),
                small_float(0.5, 10),
                small_float(0.25, 10),
                small_float(0.0, 10)
            ]))
        );
        // R32G32_UINT: two halves a word.
        let words = fill(17, Value::Colour([1.0, 0.5, 0.25, 0.0], Some(Packing::Rgba16f)));
        assert_eq!(
            words,
            Ok(Fill::Uint([
                small_float(1.0, 10) | small_float(0.5, 10) << 16,
                small_float(0.25, 10) | small_float(0.0, 10) << 16,
                0,
                0
            ]))
        );
        // A packing of another size is refused.
        assert!(fill(12, Value::Colour([1.0, 1.0, 1.0, 1.0], Some(Packing::Rgb9e5))).is_err());
    }

    #[test]
    fn a_fill_is_recorded_and_a_skip_is_not() {
        let before = filled();
        note(true);
        note(false);
        assert_eq!(filled() - before, 1, "one view filled, one not");
    }

    #[test]
    fn rgb9e5_matches_a_reference_encoder() {
        for rgb in [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.6, 0.588, 0.564],
            [0.001, 0.5, 65408.0],
            [2.0, 0.5, 0.25],
        ] {
            assert_eq!(rgb9e5(rgb), reference_rgb9e5(rgb), "{rgb:?}");
        }
        // Decoded, it is the colour within half a mantissa step.
        let w = rgb9e5([0.6, 0.588, 0.564]);
        let back = decode_rgb9e5(w);
        assert!((back[0] - 0.6).abs() < 1.0 / 512.0, "{back:?}");
    }

    #[test]
    fn small_floats_match_the_nearest_code() {
        for (v, mantissa) in [
            (0.0, 10),
            (0.5, 10),
            (1.0, 10),
            (0.25, 10),
            (0.6, 10),
            (65408.0, 6),
            (1.0, 5),
            (0.3, 5),
            (2.5, 6),
        ] {
            assert_eq!(small_float(v, mantissa), reference_small(v, mantissa), "{v} at {mantissa}");
        }
        // Too large is the largest finite value, not infinity; negative
        // and NaN nothing.
        assert_eq!(small_float(70000.0, 6), (30 << 6) | 63);
        assert_eq!(small_float(f32::INFINITY, 6), (30 << 6) | 63);
        assert_eq!(small_float(-1.0, 10), 0);
        assert_eq!(small_float(f32::NAN, 10), 0);
        // A half's bits for 10 mantissa bits: 1.0 is 0x3c00.
        assert_eq!(small_float(1.0, 10), 0x3c00);
    }
}
