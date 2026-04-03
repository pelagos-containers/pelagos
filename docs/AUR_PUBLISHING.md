# Publishing to the AUR

Pelagos ships two AUR packages:

| Package | Description |
|---|---|
| `pelagos` | Source build — compiles from tarball with `cargo` |
| `pelagos-bin` | Pre-built binary — downloads arch-specific release artifact |

Package definitions live in `pkg/aur/` in this repo.

## One-time setup

1. Create an account at https://aur.archlinux.org/register (username + email + SSH public key).
2. Add your SSH public key under *My Account → SSH Public Key*.

## Initial publication (claim the name)

Run once per package from inside its directory:

```bash
cd pkg/aur/pelagos
git init
git remote add aur ssh://aur@aur.archlinux.org/pelagos.git
git add PKGBUILD .SRCINFO pelagos.install
git push aur master
```

```bash
cd pkg/aur/pelagos-bin
git init
git remote add aur ssh://aur@aur.archlinux.org/pelagos-bin.git
git add PKGBUILD .SRCINFO pelagos-bin.install
git push aur master
```

Pushing to a package name that does not yet exist **creates it** and makes you the maintainer. There is no approval gate.

## Updating at release time

**Wait for the release CI workflow to complete successfully before updating the AUR.**
The sha256sums are derived from the final release artifacts, which are only produced
after all CI gates pass.

Once CI is green, run the update script:

```bash
scripts/update-aur.sh <version>
# e.g. scripts/update-aur.sh 0.61.0
```

This script:
1. Fetches sha256sums for the x86_64 binary, aarch64 binary, and source tarball from the GitHub release
2. Updates `pkgver` and `sha256sums` in both PKGBUILDs
3. Regenerates `.SRCINFO` via `makepkg --printsrcinfo`
4. Commits and pushes to both AUR remotes
5. Commits the updated PKGBUILDs back to the main repo

**Requires:** `makepkg` (Arch Linux), and both AUR git remotes configured in `pkg/aur/pelagos/` and `pkg/aur/pelagos-bin/` (see Initial publication section above).

Note: Automating this in CI is tracked in [issue #190](https://github.com/pelagos-containers/pelagos/issues/190).

## Testing the AUR package locally

If you have a manually-installed build in `/usr/local/bin`, remove it first so it doesn't shadow the AUR-installed binary:

```bash
sudo rm \
  /usr/local/bin/pelagos \
  /usr/local/bin/pelagos-dns \
  /usr/local/bin/pelagos-shim-wasm
```

Then install and verify:

```bash
yay -S pelagos-bin
which pelagos        # should be /usr/bin/pelagos
pacman -Q pelagos-bin
pelagos run --rm alpine echo hello
```

## Rules for AUR repos

- The AUR git repo must contain **only** `PKGBUILD`, `.SRCINFO`, and any `.install` or patch files — not the upstream source.
- `.SRCINFO` must stay in sync with `PKGBUILD`; the AUR web UI derives its metadata from `.SRCINFO`.
- Each package is a separate AUR git repo with its own remote.

## Maintainership

Co-maintainers can be added via the AUR web UI (*Package Actions → Manage Co-Maintainers*). To transfer or orphan a package, use *Package Actions → Disown*.
