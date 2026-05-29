//! Native Linux networking primitives: ioctl-based bridge/link management and
//! RTNETLINK messages for address, route, neighbour, veth, and netns operations.
//!
//! All functions that map to operations with natural "already exists" semantics
//! (`addr_add_*`, `route_add_*`, `neigh_add_*`, `create_bridge`, `netns_create`)
//! treat `EEXIST` as success.

use std::ffi::CString;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::io::RawFd;

// ── Constants not exported by libc ────────────────────────────────────────────

const SIOCBRADDBR: libc::c_ulong = 0x89a0;
const SIOCBRADDIF: libc::c_ulong = 0x89a2;

const IFLA_IFNAME: u16 = 3;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_BROADCAST: u16 = 4;
const IFA_FLAGS: u16 = 8;

const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;

const NDA_DST: u16 = 1;
const NDA_LLADDR: u16 = 2;

const RT_SCOPE_UNIVERSE: u8 = 0;
const RTPROT_STATIC: u8 = 4;
const RTN_UNICAST: u8 = 1;

const NUD_STALE: u16 = 4;
const IFA_F_NODAD: u32 = 0x02;

const NLM_F_REQUEST: u16 = 0x0001;
const NLM_F_ACK: u16 = 0x0004;
const NLM_F_CREATE: u16 = 0x0400;
const NLM_F_EXCL: u16 = 0x0200;
const NLM_F_REPLACE: u16 = 0x0100;
const NLMSG_ERROR_TYPE: u16 = 2;

// ── ioctl structs ─────────────────────────────────────────────────────────────

#[repr(C)]
struct IfreqIdx {
    ifr_name: [u8; 16],
    ifr_ifindex: i32,
    _pad: [u8; 18],
}

// ── Low-level helpers ─────────────────────────────────────────────────────────

fn copy_ifname(dst: &mut [u8; 16], name: &str) -> io::Result<()> {
    let b = name.as_bytes();
    if b.len() >= 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name too long",
        ));
    }
    dst[..b.len()].copy_from_slice(b);
    dst[b.len()] = 0;
    Ok(())
}

fn if_index_of(name: &str) -> io::Result<i32> {
    let cname = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interface name contains NUL"))?;
    let idx = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if idx == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(idx as i32)
}

fn udp_sock() -> io::Result<RawFd> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

// ── Netlink message builder ───────────────────────────────────────────────────

struct NlBuf(Vec<u8>);

impl NlBuf {
    fn new() -> Self {
        NlBuf(Vec::with_capacity(256))
    }

    fn push_u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn push_u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn push_u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn push_i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn push_bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn pad_to_4(&mut self) {
        let rem = self.0.len() % 4;
        if rem != 0 {
            for _ in 0..(4 - rem) {
                self.0.push(0);
            }
        }
    }

    /// Append an rtattr (type, data), padded to 4 bytes.
    fn rta(&mut self, rta_type: u16, data: &[u8]) {
        let total = 4 + data.len();
        self.push_u16(total as u16);
        self.push_u16(rta_type);
        self.push_bytes(data);
        self.pad_to_4();
    }

    /// Reserve space for a nested rtattr header; returns the offset of the
    /// length field so it can be patched after the nested content is written.
    fn rta_begin_nested(&mut self, rta_type: u16) -> usize {
        let pos = self.0.len();
        self.push_u16(0); // placeholder length
        self.push_u16(rta_type);
        pos
    }

    /// Patch the length of a nested rtattr started with `rta_begin_nested`.
    fn rta_end_nested(&mut self, start: usize) {
        let len = (self.0.len() - start) as u16;
        self.0[start..start + 2].copy_from_slice(&len.to_le_bytes());
        self.pad_to_4();
    }
}

// ── Netlink socket send/recv ───────────────────────────────────────────────────

fn nl_socket() -> io::Result<RawFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // sockaddr_nl has a private padding field; zero-initialise then patch family.
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    sa.nl_pid = 0;
    sa.nl_groups = 0;
    let ret = unsafe {
        libc::bind(
            fd,
            &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }

    Ok(fd)
}

/// Finalise the nlmsghdr length field (first 4 bytes) and send the message.
fn nl_send(fd: RawFd, buf: &mut NlBuf) -> io::Result<()> {
    let total = buf.len() as u32;
    buf.0[0..4].copy_from_slice(&total.to_le_bytes());

    let ret = unsafe { libc::send(fd, buf.0.as_ptr() as *const libc::c_void, buf.0.len(), 0) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receive the ACK and return Ok(()) or the kernel's errno as an error.
fn nl_recv_ack(fd: RawFd) -> io::Result<()> {
    let mut rbuf = [0u8; 4096];
    let n = unsafe { libc::recv(fd, rbuf.as_mut_ptr() as *mut libc::c_void, rbuf.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = n as usize;
    // nlmsghdr is 16 bytes; NLMSG_ERROR payload starts at byte 16 with i32 errno.
    if n < 20 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "netlink response too short",
        ));
    }
    let msg_type = u16::from_le_bytes([rbuf[4], rbuf[5]]);
    if msg_type != NLMSG_ERROR_TYPE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected netlink msg type {msg_type}"),
        ));
    }
    let errno = i32::from_le_bytes([rbuf[16], rbuf[17], rbuf[18], rbuf[19]]);
    if errno < 0 {
        return Err(io::Error::from_raw_os_error(-errno));
    }
    Ok(())
}

/// Build and send one netlink request, receive ACK; translate EEXIST to Ok when requested.
fn nl_request(buf: &mut NlBuf, eexist_ok: bool) -> io::Result<()> {
    let fd = nl_socket()?;
    let res = nl_send(fd, buf).and_then(|()| nl_recv_ack(fd));
    unsafe { libc::close(fd) };
    match res {
        Err(e) if eexist_ok && e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        other => other,
    }
}

// ── nlmsghdr helpers ──────────────────────────────────────────────────────────

/// Write the 16-byte nlmsghdr at the current position.
/// The `len` field is a placeholder (0) — patched later in `nl_send`.
fn push_nlmsghdr(buf: &mut NlBuf, msg_type: u16, flags: u16) {
    buf.push_u32(0); // len — patched in nl_send
    buf.push_u16(msg_type);
    buf.push_u16(flags);
    buf.push_u32(1); // seq
    buf.push_u32(0); // pid
}

/// Write the 16-byte ifinfomsg.
fn push_ifinfomsg(buf: &mut NlBuf, index: i32, flags: u32, change: u32) {
    buf.push_u8(libc::AF_UNSPEC as u8);
    buf.push_u8(0); // pad
    buf.push_u16(0); // ifi_type (ARPHRD_NETROM = 0, kernel fills in)
    buf.push_i32(index);
    buf.push_u32(flags);
    buf.push_u32(change);
}

/// Write the 8-byte ifaddrmsg.
fn push_ifaddrmsg(buf: &mut NlBuf, family: u8, prefixlen: u8, scope: u8, index: i32) {
    buf.push_u8(family);
    buf.push_u8(prefixlen);
    buf.push_u8(0); // flags
    buf.push_u8(scope);
    buf.push_u32(index as u32);
}

/// Write the 12-byte rtmsg.
fn push_rtmsg(
    buf: &mut NlBuf,
    family: u8,
    dst_len: u8,
    table: u8,
    protocol: u8,
    scope: u8,
    rt_type: u8,
) {
    buf.push_u8(family);
    buf.push_u8(dst_len);
    buf.push_u8(0); // src_len
    buf.push_u8(0); // tos
    buf.push_u8(table);
    buf.push_u8(protocol);
    buf.push_u8(scope);
    buf.push_u8(rt_type);
    buf.push_u32(0); // flags
}

/// Write the 12-byte ndmsg.
fn push_ndmsg(buf: &mut NlBuf, family: u8, ifindex: i32, state: u16) {
    buf.push_u8(family);
    buf.push_u8(0); // pad1
    buf.push_u16(0); // pad2
    buf.push_i32(ifindex);
    buf.push_u16(state);
    buf.push_u8(0); // flags
    buf.push_u8(0); // type
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Bring up a network interface using RTM_NEWLINK with IFF_UP.
pub fn link_set_up(ifname: &str) -> io::Result<()> {
    let idx = if_index_of(ifname)?;
    let mut buf = NlBuf::new();
    push_nlmsghdr(&mut buf, libc::RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK);
    push_ifinfomsg(&mut buf, idx, libc::IFF_UP as u32, libc::IFF_UP as u32);
    nl_request(&mut buf, false)
}

/// Create a Linux bridge.  Returns Ok if it already exists.
pub fn create_bridge(name: &str) -> io::Result<()> {
    let cname = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bridge name contains NUL"))?;
    let fd = udp_sock()?;
    let ret = unsafe { libc::ioctl(fd, SIOCBRADDBR as _, cname.as_ptr()) };
    let err = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if ret < 0 && err.raw_os_error() != Some(libc::EEXIST) {
        return Err(err);
    }
    Ok(())
}

/// Assign an IPv4 address to an interface.  Returns Ok if already assigned.
pub fn addr_add_ipv4(ifname: &str, addr: Ipv4Addr, prefix: u8) -> io::Result<()> {
    let idx = if_index_of(ifname)?;
    let addr_b = addr.octets();

    // Compute broadcast: addr | ~mask
    let mask: u32 = if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix)
    };
    let bcast = u32::from(addr) | !mask;
    let bcast_b = bcast.to_be_bytes();

    let mut buf = NlBuf::new();
    push_nlmsghdr(
        &mut buf,
        libc::RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_REPLACE | NLM_F_ACK,
    );
    push_ifaddrmsg(
        &mut buf,
        libc::AF_INET as u8,
        prefix,
        RT_SCOPE_UNIVERSE,
        idx,
    );
    buf.rta(IFA_LOCAL, &addr_b);
    buf.rta(IFA_ADDRESS, &addr_b);
    buf.rta(IFA_BROADCAST, &bcast_b);
    nl_request(&mut buf, true)
}

/// Assign an IPv6 address to an interface.  Returns Ok if already assigned.
///
/// When `nodad` is true the kernel skips Duplicate Address Detection, which
/// avoids ~1 s of tentative-state first-packet loss for deterministic ULA addresses.
pub fn addr_add_ipv6(ifname: &str, addr: Ipv6Addr, prefix: u8, nodad: bool) -> io::Result<()> {
    let idx = if_index_of(ifname)?;
    let addr_b = addr.octets();

    let mut buf = NlBuf::new();
    push_nlmsghdr(
        &mut buf,
        libc::RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_REPLACE | NLM_F_ACK,
    );
    push_ifaddrmsg(
        &mut buf,
        libc::AF_INET6 as u8,
        prefix,
        RT_SCOPE_UNIVERSE,
        idx,
    );
    buf.rta(IFA_ADDRESS, &addr_b);
    buf.rta(IFA_LOCAL, &addr_b);
    if nodad {
        buf.rta(IFA_FLAGS, &IFA_F_NODAD.to_le_bytes());
    }
    nl_request(&mut buf, true)
}

/// Attach an interface to a bridge using SIOCBRADDIF.
pub fn link_set_master(ifname: &str, master: &str) -> io::Result<()> {
    let slave_idx = if_index_of(ifname)?;
    let mut req = IfreqIdx {
        ifr_name: [0u8; 16],
        ifr_ifindex: slave_idx,
        _pad: [0u8; 18],
    };
    copy_ifname(&mut req.ifr_name, master)?;

    let fd = udp_sock()?;
    let ret = unsafe { libc::ioctl(fd, SIOCBRADDIF as _, &req as *const IfreqIdx) };
    let err = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if ret < 0 {
        return Err(err);
    }
    Ok(())
}

/// Create a veth pair `(host_name, peer_name)` using RTM_NEWLINK.
pub fn create_veth(host_name: &str, peer_name: &str) -> io::Result<()> {
    let mut host_name_b = host_name.as_bytes().to_vec();
    host_name_b.push(0);
    let mut peer_name_b = peer_name.as_bytes().to_vec();
    peer_name_b.push(0);

    let mut buf = NlBuf::new();
    push_nlmsghdr(
        &mut buf,
        libc::RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK,
    );
    push_ifinfomsg(&mut buf, 0, 0, 0);
    buf.rta(IFLA_IFNAME, &host_name_b);

    let li = buf.rta_begin_nested(IFLA_LINKINFO);
    buf.rta(IFLA_INFO_KIND, b"veth\0");

    let id = buf.rta_begin_nested(IFLA_INFO_DATA);
    let peer = buf.rta_begin_nested(VETH_INFO_PEER);
    // peer ifinfomsg (16 zero bytes = all-default)
    push_ifinfomsg(&mut buf, 0, 0, 0);
    buf.rta(IFLA_IFNAME, &peer_name_b);
    buf.rta_end_nested(peer);
    buf.rta_end_nested(id);
    buf.rta_end_nested(li);

    nl_request(&mut buf, false)
}

/// Move an interface into a network namespace (by fd), optionally renaming it atomically.
pub fn link_move_to_netns(
    ifname: &str,
    netns_fd: RawFd,
    rename_to: Option<&str>,
) -> io::Result<()> {
    let idx = if_index_of(ifname)?;

    let mut buf = NlBuf::new();
    push_nlmsghdr(&mut buf, libc::RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK);
    push_ifinfomsg(&mut buf, idx, 0, 0);
    buf.rta(IFLA_NET_NS_FD, &(netns_fd as u32).to_le_bytes());
    if let Some(new_name) = rename_to {
        let mut nb = new_name.as_bytes().to_vec();
        nb.push(0);
        buf.rta(IFLA_IFNAME, &nb);
    }
    nl_request(&mut buf, false)
}

/// Delete an interface by name using RTM_DELLINK.
pub fn link_del(ifname: &str) -> io::Result<()> {
    let idx = if_index_of(ifname)?;
    let mut buf = NlBuf::new();
    push_nlmsghdr(&mut buf, libc::RTM_DELLINK, NLM_F_REQUEST | NLM_F_ACK);
    push_ifinfomsg(&mut buf, idx, 0, 0);
    nl_request(&mut buf, false)
}

/// Add an IPv4 default route via `gw` on `dev`.  Returns Ok if already present.
pub fn route_add_default_ipv4(gw: Ipv4Addr, dev: &str) -> io::Result<()> {
    let idx = if_index_of(dev)?;
    let gw_b = gw.octets();

    let mut buf = NlBuf::new();
    push_nlmsghdr(
        &mut buf,
        libc::RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_ACK,
    );
    push_rtmsg(
        &mut buf,
        libc::AF_INET as u8,
        0,
        libc::RT_TABLE_MAIN,
        RTPROT_STATIC,
        RT_SCOPE_UNIVERSE,
        RTN_UNICAST,
    );
    buf.rta(RTA_GATEWAY, &gw_b);
    buf.rta(RTA_OIF, &(idx as u32).to_le_bytes());
    nl_request(&mut buf, true)
}

/// Add an IPv6 default route via `gw` on `dev`.  Returns Ok if already present.
pub fn route_add_default_ipv6(gw: Ipv6Addr, dev: &str) -> io::Result<()> {
    let idx = if_index_of(dev)?;
    let gw_b = gw.octets();

    let mut buf = NlBuf::new();
    push_nlmsghdr(
        &mut buf,
        libc::RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_ACK,
    );
    push_rtmsg(
        &mut buf,
        libc::AF_INET6 as u8,
        0,
        libc::RT_TABLE_MAIN,
        RTPROT_STATIC,
        RT_SCOPE_UNIVERSE,
        RTN_UNICAST,
    );
    buf.rta(RTA_GATEWAY, &gw_b);
    buf.rta(RTA_OIF, &(idx as u32).to_le_bytes());
    nl_request(&mut buf, true)
}

/// Add a static NDP neighbour entry (IPv6, NUD_STALE).  Returns Ok if already present.
pub fn neigh_add_ipv6(dev: &str, ip: Ipv6Addr, mac: &[u8; 6]) -> io::Result<()> {
    let idx = if_index_of(dev)?;

    let mut buf = NlBuf::new();
    push_nlmsghdr(
        &mut buf,
        libc::RTM_NEWNEIGH,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_REPLACE | NLM_F_ACK,
    );
    push_ndmsg(&mut buf, libc::AF_INET6 as u8, idx, NUD_STALE);
    buf.rta(NDA_DST, &ip.octets());
    buf.rta(NDA_LLADDR, mac);
    nl_request(&mut buf, true)
}

/// Create a named network namespace at `/run/netns/<name>`.
///
/// Spawns a thread that calls `unshare(CLONE_NEWNET)` and then bind-mounts
/// `/proc/thread-self/ns/net` onto the file.  Returns Ok if the namespace
/// already exists.
/// Delete a named network namespace created by [`netns_create`].
///
/// Detaches the bind mount at `/run/netns/<name>` and removes the file.
/// Both steps are attempted; the first error encountered is returned if
/// neither succeeds (best-effort teardown).
pub fn netns_del(name: &str) -> io::Result<()> {
    let path = format!("/run/netns/{name}");
    let cpath = CString::new(path.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "netns name contains NUL"))?;

    let umount_err = unsafe {
        let ret = libc::umount2(cpath.as_ptr(), libc::MNT_DETACH);
        if ret < 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        }
    };

    let unlink_err = unsafe {
        let ret = libc::unlink(cpath.as_ptr());
        if ret < 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        }
    };

    match (umount_err, unlink_err) {
        (_, None) => Ok(()),
        (None, Some(e)) => Err(e),
        (Some(e), Some(_)) => Err(e),
    }
}

pub fn netns_create(name: &str) -> io::Result<()> {
    let path = format!("/run/netns/{name}");
    std::fs::create_dir_all("/run/netns")?;

    // Try to create the bind-mount target file exclusively.
    let cpath = CString::new(path.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "netns name contains NUL"))?;
    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o444_u32,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(e);
    }
    unsafe { libc::close(fd) };

    // Spawn a thread to create a new net namespace and bind-mount it.
    let path_clone = path.clone();
    let result = std::thread::spawn(move || -> io::Result<()> {
        let ret = unsafe { libc::unshare(libc::CLONE_NEWNET) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        let src = CString::new("/proc/thread-self/ns/net").unwrap();
        let dst = CString::new(path_clone.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad path"))?;
        let ret = unsafe {
            libc::mount(
                src.as_ptr(),
                dst.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })
    .join()
    .map_err(|_| io::Error::other("netns_create thread panicked"))?;

    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result
}

/// Run a closure inside a named network namespace.
///
/// Opens `/run/netns/<name>`, spawns a thread, calls `setns` to enter the
/// namespace, then executes `f`.  The calling thread's namespace is unaffected.
pub fn in_netns<T, F>(netns_path: &str, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let cpath = CString::new(netns_path.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    std::thread::spawn(move || -> io::Result<T> {
        let ret = unsafe { libc::setns(fd, libc::CLONE_NEWNET) };
        unsafe { libc::close(fd) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        f()
    })
    .join()
    .map_err(|_| io::Error::other("in_netns thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nlbuf_pad() {
        let mut b = NlBuf::new();
        b.push_u8(1);
        b.push_u8(2);
        b.push_u8(3);
        b.pad_to_4();
        assert_eq!(b.0.len(), 4);
        assert_eq!(b.0[3], 0);
    }

    #[test]
    fn nlbuf_rta_roundtrip() {
        let mut b = NlBuf::new();
        // Append a dummy nlmsghdr placeholder first so offsets look realistic.
        for _ in 0..16 {
            b.push_u8(0);
        }
        let data = [192u8, 168, 1, 1];
        b.rta(IFA_LOCAL, &data);
        // rta_len = 4 + 4 = 8, no padding needed
        assert_eq!(&b.0[16..18], &(8u16).to_le_bytes());
        assert_eq!(&b.0[18..20], &IFA_LOCAL.to_le_bytes());
        assert_eq!(&b.0[20..24], &data);
    }

    /// Verify netns_create + netns_del round-trip: path must exist after create,
    /// be gone after del.  Requires root (bind-mount needs CAP_SYS_ADMIN).
    #[test]
    fn test_netns_create_del_roundtrip() {
        if unsafe { libc::getuid() } != 0 {
            return;
        }
        let name = format!("pelagos-test-ns-{}", unsafe { libc::getpid() });
        netns_create(&name).expect("netns_create");
        assert!(
            std::path::Path::new(&format!("/run/netns/{name}")).exists(),
            "netns path must exist after create"
        );
        netns_del(&name).expect("netns_del");
        assert!(
            !std::path::Path::new(&format!("/run/netns/{name}")).exists(),
            "netns path must be gone after del"
        );
    }

    #[test]
    fn nlbuf_nested_rta() {
        let mut b = NlBuf::new();
        let start = b.rta_begin_nested(IFLA_LINKINFO);
        b.rta(IFLA_INFO_KIND, b"veth\0");
        b.rta_end_nested(start);
        // The length at `start` must equal the total bytes written from start.
        let len = u16::from_le_bytes([b.0[start], b.0[start + 1]]) as usize;
        assert_eq!(len, b.0.len() - start);
    }
}
