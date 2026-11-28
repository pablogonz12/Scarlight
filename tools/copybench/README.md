# copybench

Replays one world frame's copy pattern (counters of `base-fx-b`) to attribute the time it spends in "copies"
and "copying back", with or without the title's address-space churn (`--contend`, 8 threads committing and
freeing 32 MB blocks, which is what the world does while our copying thread works).

Result and the frame it came from: `xcl-bin/notes/copy-bandwidth.md`. Legs: `a` 14700 tiny
`ReadProcessMemory`; `b` 2409 `GetWriteWatch(RESET)` over 10.7 GB of spans; `c` copy-back of 409.9 MB declared,
20.5 % written, old three-way merge vs first/last-difference trim with one bulk copy, plus its write leg by
`WriteProcessMemory` vs `ptr::copy`.

```bash
cargo xwin build --release --target x86_64-pc-windows-msvc
WINEPREFIX=~/.cache/wwstall-prefix WINEDEBUG=-all wine target/x86_64-pc-windows-msvc/release/copybench.exe all --contend
```
