# Releases

Scarlight ships as versioned releases: a tarball you extract into Steam's
`compatibilitytools.d`, which Steam then lists under a title's **Properties > Compatibility** as
**Scarlight `<version>`**. This page is the scheme, the install, and when a new release is cut.
For every option of the tool itself, [steam.md](steam.md) is the reference.

## 1. What a version means

A Scarlight version is **`<GE major>.<GE release>.<revision>`**, built on one GE-Proton release:

| Version | Tag | Runs on | Notes |
|---|---|---|---|
| `11.14.1` | `v11.14.1` | GE-Proton11-14 | first revision on that Proton |
| `11.14.2` | `v11.14.2` | GE-Proton11-14 | the next batch of merged work, same Proton |
| `11.15.1` | `v11.15.1` | GE-Proton11-15 | a rebase: the revision starts again at `.1` |

So the first two numbers are the Proton the release is tested on, and the third only counts releases
within it. The Proton is named in exactly one place, [`tools/steam-compat/BASE`](../tools/steam-compat/BASE);
`tools/steam-compat/package.sh` derives the version from it and from the tags that already exist
(`v11.14.*` → the next one), and refuses a `--version` that does not belong to that GE release.

Every release installs **side by side**: the display name is `Scarlight 11.14.1` and the internal name
is `Scarlight-11.14.1`, so Steam keeps two of them apart the way it keeps two GE-Protons apart, and
you can switch a title back to the previous one by picking it again.

## 2. Install a release

1. Download `Scarlight-<version>.tar.gz` from [Releases](https://github.com/pablogonz12/Scarlight/releases)
   and check it against `SHA256SUMS.txt`:

   ```bash
   sha256sum -c SHA256SUMS.txt     # in the folder you downloaded into
   ```

2. Extract it into Steam's compatibility tools folder:

   ```bash
   mkdir -p ~/.local/share/Steam/compatibilitytools.d
   tar -xzf Scarlight-<version>.tar.gz -C ~/.local/share/Steam/compatibilitytools.d/
   ```

   Flatpak Steam uses
   `~/.var/app/com.valvesoftware.Steam/data/Steam/compatibilitytools.d/` instead.

3. Install the Proton the release runs on: **GE-Proton11-14** from
   [proton-ge-custom releases](https://github.com/GloriousEggroll/proton-ge-custom/releases), extracted
   into the same `compatibilitytools.d`. The tool looks for it by name; if only a newer
   `GE-Proton11-*` is installed it uses the newest one and says so in `scarlight.log` (and on stderr).
   With no GE-Proton at all, it stops and names this URL.

4. Restart Steam, then *Library → right-click the title → Properties → Compatibility →* tick
   **Force the use of a specific Steam Play compatibility tool** and pick **Scarlight `<version>`**.

DXC's `dxil.dll` and `dxcompiler.dll`, and Microsoft's `d3dcompiler_47.dll`, are not in a release:
both are the user's own (see [steam.md](steam.md#dxc-which-a-release-does-not-carry)). Everything
else is in the tarball: our build, vkd3d-proton's pair, the licence texts, and a `VERSION` file with
the version, the commit it was built from, the GE-Proton base and the vkd3d-proton it carries.

## 3. What is in a release

`Scarlight-<version>.tar.gz` holds one top folder, `Scarlight-<version>/`:

```
compatibilitytool.vdf     display name "Scarlight <version>", internal name Scarlight-<version>
toolmanifest.vdf          the command Steam calls
scarlight                 the entry script: finds the Proton, stages the DLLs, runs the title
scarlight.conf.example    the settings, copied to scarlight.conf on first launch only
VERSION                   version, commit, BASE, vkd3d-proton and DXC versions
BASE                      the GE-Proton this release runs on
files/victory-default.exe our build
files/d3d12.dll           vkd3d-proton, built from source at the commit VERSION names
files/d3d12core.dll
files/licenses/           GPL-3.0 and LGPL-2.1 texts, VERSIONS.txt with the source offers
LICENSE
```

`dxgi.dll`, `d3d11.dll` and `d3d10core.dll` come from the Proton you installed, at launch: they are
its DXVK files, not ours to ship. **No game files are ever in a release**, and none are needed to
build one: what the layer knows about a title lives in your own config directory, not in the repo.

## 4. When a new release is cut

A **new revision** is cut whenever a batch of work has merged into `main` and the dry run passes:
`SCARLIGHT_DRY_RUN=1` through the installed tool must print `build.toml matched, 2 hooks` with every
module `missing=0`. No schedule, no fixed number of PRs: a batch big enough to be worth a download,
and nothing that fails the dry run ships. Rebasing on a newer GE-Proton is a release of its own
(`11.15.1`) after the same dry run on that Proton.

To cut one:

```bash
git -C <clone> fetch hub --tags
git -C <clone> worktree add -b release/v<version> ../rel hub/main
cd <clone>/../rel                    # worktree add does not enter it: the rest runs inside
tools/steam-compat/package.sh --vkd3d <dlls> --exe <victory.exe>   # version from BASE + tags
git tag v<version> && git push hub v<version>
```

Pushing the tag is the release: the `Release` workflow cross-builds the exe on Linux, builds the
vkd3d-proton pair at the pinned commits, packs the tarball, and publishes a **pre-release** with the
tarball, `SHA256SUMS.txt`, the vkd3d-proton corresponding source (LGPL-2.1) and the `CHANGELOG.md`
section for that version as the body. `tools/release-notes.sh <from-tag>` drafts that section from the
merged PR titles, grouped as Picture / Speed / Load time / Stability / Tools.

The revision a tag must carry is whatever `package.sh` prints as its default (tags of the GE series,
`+1`); a tag that disagrees with `BASE` is a mistake, not a variant.

## 5. Why a pre-release

Every release so far is early work, and the status page says so: the game is not playable yet. GitHub
pre-releases keep a stable "latest" from promising more than the release is, and Steam's own list
still shows the version you picked. When the picture holds up across a whole scene, a revision is
promoted out of pre-release by editing the tag's release; the tarball does not change.
