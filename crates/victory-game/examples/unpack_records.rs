//! Local tool (not upstream): unpack the SGD5 records at the listed offsets.
//! usage: unpack_records <archive> <list: 0xOFFSET<TAB>...> <out dir>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let want: std::collections::HashMap<usize, String> = std::fs::read_to_string(&a[2])
        .unwrap()
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let off = usize::from_str_radix(f.next()?.trim_start_matches("0x"), 16).ok()?;
            let why = f.nth(1).unwrap_or("");
            Some((off, if why.contains("scalar") { "sm" } else { "dd" }.to_string()))
        })
        .collect();
    let (mut n, mut bad) = (0, 0);
    for r in victory_game::sgd5::records(&data) {
        if let Some(tag) = want.get(&r.offset) {
            match r.unpack() {
                Some(b) => {
                    std::fs::write(format!("{}/{tag}-{:#x}.dxbc", a[3], r.offset), b).unwrap();
                    n += 1;
                }
                None => bad += 1,
            }
        }
    }
    println!("unpacked={n} corrupt={bad} wanted={}", want.len());
}
