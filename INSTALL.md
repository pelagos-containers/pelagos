# Installing Pelagos

## Ubuntu / Debian

### Download

Get the `.deb` for your architecture from the [releases page](https://github.com/pelagos-containers/pelagos/releases):

| Architecture | File |
|---|---|
| x86\_64 (Intel/AMD) | `pelagos_VERSION_amd64.deb` |
| arm64 (Apple Silicon VM, AWS Graviton, Raspberry Pi) | `pelagos_VERSION_arm64.deb` |

### Install

The package declares all required dependencies (`nftables`, `iproute2`, `fuse-overlayfs`, `passt`).
Use `dpkg -i` followed by `apt-get install -f` so apt resolves them automatically:

```
sudo dpkg -i pelagos_VERSION_ARCH.deb
sudo apt-get install -f
```

> **Note:** If you download to your home directory and see a warning about `_apt` permission
> denied, this is harmless — the install still completes. To avoid it, download to `/tmp`
> first, or use the two-command form above (dpkg + apt-get -f) which does not trigger the
> sandbox check.

The post-install script runs `pelagos system setup` automatically, which:
- Creates `/var/lib/pelagos` (image store, volumes)
- Creates the `pelagos` group
- Configures `/etc/fuse.conf` for rootless overlay mounts

### Add yourself to the pelagos group

```
sudo usermod -aG pelagos $USER
```

Then log out and back in, or run `newgrp pelagos` in the current shell.
This allows `pelagos image pull` and other non-root commands to access the image store.

### Ubuntu 24.04+: enable unprivileged user namespaces

Ubuntu 24.04 restricts unprivileged user namespace creation via AppArmor by default.
This causes `pelagos run` (rootless, without sudo) to fail with `Invalid argument`.

To fix permanently:

```
echo 'kernel.apparmor_restrict_unprivileged_userns=0' \
  | sudo tee /etc/sysctl.d/99-pelagos-userns.conf
sudo sysctl -p /etc/sysctl.d/99-pelagos-userns.conf
```

This setting is not needed when running with `sudo`.

### DNS with bridge networking (root)

When running as root, the default network is a bridge. DNS is not injected into containers
automatically, so you must pass `--dns` explicitly:

```
sudo pelagos run --rm --dns 1.1.1.1 alpine ping -c 4 google.com
```

Rootless containers use `pasta` for networking and inherit DNS from the host automatically —
no `--dns` flag needed.

### IPv6

Bridge networking provides IPv4 connectivity only. If you need IPv6 inside containers, use
`pasta` mode (`--network pasta`) which proxies through the host network stack and will carry
IPv6 if the host has it.

---

## Arch Linux

### Install

**Binary package (recommended):**

```
yay -S pelagos-bin
```

**Source build** (compiles from source, requires Rust):

```
yay -S pelagos
```

Both packages support `x86_64` and `aarch64`; `makepkg` / your AUR helper selects the right
binary automatically.

### Optional dependencies (strongly recommended)

The AUR packages list these as optional, but most workflows require them:

```
sudo pacman -S passt fuse-overlayfs
```

| Package | Purpose |
|---|---|
| `passt` | Rootless networking — required for `pelagos run` without sudo |
| `fuse-overlayfs` | Overlay filesystem on kernels without `CONFIG_OVERLAY_FS` |
| `dnsmasq` | Production-grade DNS backend (optional; builtin DNS daemon is the default) |

### Add yourself to the pelagos group

Same as Ubuntu — the post-install hook runs `pelagos system setup` automatically:

```
sudo usermod -aG pelagos $USER
```

Log out and back in, or `newgrp pelagos`.

### AppArmor / user namespace restriction

Standard Arch Linux does not enable AppArmor or restrict unprivileged user namespaces.
Rootless containers work out of the box after the group setup above.

---

## Quick-start (all platforms)

```
# Pull an image (no sudo needed if you're in the pelagos group):
pelagos image pull alpine

# Run rootless (pasta networking, full internet):
pelagos run --rm alpine echo hello

# Run as root (bridge + NAT, explicit DNS):
sudo pelagos run --rm --dns 1.1.1.1 alpine ping -c 4 google.com
```

See [docs/USER_GUIDE.md](docs/USER_GUIDE.md) for the full CLI reference.
