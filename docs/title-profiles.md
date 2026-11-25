# Title profiles

A profile is what **one title's build** needs from this layer beyond the generic path: a
code patch to place, shaders to leave out, a label to print. Profiles belong to the user,
not to the repository: they are read at launch from a directory outside the tree, and one
is selected by the title's own identity — the BLAKE3 of the image we are about to run.

Nothing in the code knows a title. The loader hashes the image, asks "is there a profile
for this build?", and the loader, the HLE and the GPU layer then read that profile. No
title means no profile, and the generic path runs unchanged.

## Where profiles live

The first of these directories that exists and holds a profile for the image is used:

| Order | Directory | Notes |
|---|---|---|
| 1 | `VICTORY_PROFILE_DIR` | any directory; a Windows build under Wine needs a Windows path (`Z:\home\you\.config\victory\titles`) |
| 2 | `%APPDATA%\victory\titles` | the roaming AppData inside the prefix: `~/.local/share/<prefix>/drive_c/users/<you>/AppData/Roaming/victory/titles` |
| 3 | `$XDG_CONFIG_HOME/victory/titles`, else `~/.config/victory/titles` | tried through Wine's `Z:` drive when this is a Windows build, before the plain path; some Proton wrappers drop `XDG_CONFIG_HOME`, so use 1 or 2 if it is not found |

Every `*.toml` in a directory is read. A file that does not parse is logged and skipped; one bad
profile must not keep a title from running. When nothing matches, the log says so, with the
short hash of the image and the directories searched:

```
title profile: Z:\home\you\.config\victory\titles\build.toml matched, 2 hooks
title profile: 2 hooks applied
title profile: none matched
```

The log never carries the image hash at info level (logs are pasted into issues, and
`docs/asset-policy.md` keeps hashes of game files out of them). It appears on a debug line, which
only a build with the `diagnostics` feature can emit (`VICTORY_LOG=debug`), or on request:
`victory title-id <exe>` prints the hash, the directories searched and the profile that matches.

**A profile is user data, never repository data.** It may name hashes of the title's files,
per-build offsets and shader hashes — all of which `docs/asset-policy.md` keeps out of the
tree. Do not open a pull request that adds one.

## Fields

```toml
# <profiles dir>/<any name>.toml
name  = "my label for this build"   # what the layer prints at launch
build = ["<hex>"]                   # BLAKE3 of the image, 16+ digits, matched as a prefix

render_scale  = 0.67                # optional; 0.2..=1.0 of the output size
own_upscaler  = true                # optional; the title's own upscaler everywhere

[[hooks]]                           # optional; place code patches after mapping
what   = "our words for the patch"  # logged
rva    = 0x1000                     # offset from the start of the mapped image
expect = "488b0500000000"           # bytes that must be there first (hex, spaces allowed)
new    = "eb189090909090"           # bytes to write, the same length as expect
when   = ["render_scale"]           # optional; only with these switches on

[skip]                              # optional; work the GPU layer leaves out
shaders = ["<16 hex digits>"]       # skip a pipeline containing one of these shaders
stages  = ["mesh", "compute"]       # skip 64-lane programs of these stages
```

Every value above is made up. To get the hash for `build`, run `victory title-id <exe>`: it prints
the BLAKE3, whether a profile already matches it, and the directories searched. Keep the hash in
your own notes, not in an issue.

A hook is checked before anything is written: `new` must be as long as `expect`, `rva..rva+len` must
lie inside the mapped image, the whole range must be committed readable memory, and `expect` must
match the bytes there. Anything else is logged and skipped, and the code never touches memory that
fails those checks. A hook that passes them is applied as written, so the profile's own bytes are
still your decision. Hooks need `VICTORY_HOOKS=1` (formerly `VICTORY_NO_WATCHDOG=1`, still
accepted) — patches are opt-in, never automatic.

### Switches a hook can depend on

A build's resolution work is a set of patches that only make sense together, and which of them
apply depends on what was asked for. Rather than a switch per title, a hook names the switches it
needs in `when`, and the profile (or the environment) says which are on:

| Switch | On when | Written as |
|---|---|---|
| `render_scale` | `VICTORY_RENDER_SCALE` is a number in `0.2..=1.0`, else the profile's `render_scale` | an f32 |
| `own_upscaler` | `VICTORY_OWN_UPSCALER` is set and not `0`, else the profile's `own_upscaler` | a bool |

`!own_upscaler` in `when` means the hook is placed only when the switch is off — a patch for the
fallback path that must not run beside the one for the other, for instance. A hook whose `when`
lists a name that is not one of the two is never placed, and the log says so.

Two kinds of bytes a profile cannot spell out are written for it:

- `new_scale = true` writes the effective render scale as an f32, little-endian, instead of `new`
  (`expect` still gives the length, which must be four). The `render_scale` switch must be on.
- `expect_from = 0x1234` and `new_from = 0x5678` read and write `base + rva` as an 8-byte address,
  where `base` is where the image was loaded, instead of the bytes in `expect` and `new`: some
  places hold a pointer to one of the image's own functions, and only the load address knows it.
  Leave `expect` or `new` out when the matching `*_from` is given.

`[skip]` is read by the GPU layer's diagnostics build (it already excludes draws and dispatches for
other reasons there). In a plain release build the lists are ignored, and `VICTORY_SKIP_PIPELINES`
and `VICTORY_SKIP_WAVE64` are no-ops there too.

## What a profile cannot do

- Add title knowledge to the code: the mechanism is the same for every title, and a
  feature that exists for one title's build has no place in the tree unless it generalises.
- Ship with VIctory: nothing in the repository, its docs or its tests names a hash, an
  offset or a shader of any title.

See `docs/asset-policy.md` for why, and `victory.example.toml` for the paths setting.
