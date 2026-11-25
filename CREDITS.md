# Credits

Scarlight is a port of **VIctory** ([Marvisak/VIctory](https://github.com/Marvisak/VIctory)),
GPL-3.0. Its history, authorship and commit dates are kept.

The lists below are every author with a commit in this tree on `main`, plus the contributors named
in `Co-authored-by:` trailers, read from `git log main`.

## Project

- **VIctory** — Marvis and the VIctory contributors, GPL-3.0.

## Authors

9hvn9yt2vk-netizen, A6thGraderFr, Abdellah K., acleofe, adamfx, Advait Palve, AlanQiang0418,
alessiodeia, Andrea Casale, artor410, BaggyALPHA, bartikele, BastienRdz, Brazlucas, CaravelaLabs,
Carlos Sant'Anna, dadou000, Dathinel, desp, Duartt - Ryu Garage, Duff, EatCodeSleep2,
Enzo Souza Duarte, ESAT, Exorion, fever308, FIocker, franticfrostin, fuzeofficial, Gabriel Cruz,
gingertonik, Halfect, Henrique Bueno, hophaver, ifBars, ismailchaka1, itsRevela, Jeremy Goldberg,
JMAN730, julio, KaiserBloo, KoLolX3, lameloball2813-ui, LaunchDoesntCode, LF2000 Builder,
localshotgun, lordon1a, Louis Faust, LUIS LARIOS, Luiz Neves, Lyrnc, Manuel Pérez, Maraco91,
marquezini, Marvis, Matheus, meower, nathanbzrr, Nikita Romanovich, Noname0202, Nostyxx, Ortomanic,
pablogonz12, Pablo González, Pedro Fortunato, Peter Bonanni, QB, quaternion, r4., raffclar,
ruvenesh, Saitken91, Sakura-Byte, SkippySteve, swarm, tedin7, TG0000, undeadhuman37, Vanthys,
wrothgaril, Yaekai.

## Co-authors

BlueKaiman.

## Design notes

- **xcl** ([ivall/xcl](https://github.com/ivall/xcl) and its fork Bathyal), GPL-2.0-only: the
  wave-pairing approach and the NVIDIA wave lane-assignment report used in
  [docs/wave64-on-wave32.md](docs/wave64-on-wave32.md). No code taken.
- **JMAN730/victory** ([project-flamingo#671](https://github.com/JMAN730/project-flamingo/pull/671)),
  GPL-3.0: telling the GPU side's write watch which of the title's views a write into physical
  pages (DirectStorage's destinations) landed in, by section and slice rather than page number, and
  noting the pages written without lifting what guards them
  (`crates/victory-hle/src/modules/xmem.rs`, `crates/victory-gpu/src/watch.rs`).
- **JMAN730/victory** ([project-flamingo#672](https://github.com/JMAN730/project-flamingo/pull/672)),
  GPL-3.0: narrowing a host shader resource view of a streamed texture to the mips whose memory is
  there, and remaking it when more levels arrive or a host copy fills the chain
  (`crates/victory-gpu/src/texture.rs`, `crates/victory-gpu/src/descriptor/host.rs`).

## Tools

Some commits were made with AI coding tools (Claude, Codex, Devin, Gemini, GPT). A contributor is
responsible for the code they submit, whoever or whatever helped write it.

Missing or wrong name? Open an issue.
