//! Native nfnetlink (NETLINK_NETFILTER) client for nftables operations.
//!
//! Replaces all `nft` binary shell-outs in `network.rs` with raw netlink
//! socket operations.  No external crate dependencies — uses only `libc`.
//!
//! Wire format verified via `strace` on the `nft` binary.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::io::RawFd;

use crate::network::PortProto;

// ── Netlink/nfnetlink constants ───────────────────────────────────────────────

const NETLINK_NETFILTER: i32 = 12;
const NFNL_SUBSYS_NFTABLES: u16 = 10;
const NFNETLINK_V0: u8 = 0;

const NLM_F_REQUEST: u16 = 0x0001;
const NLM_F_ACK: u16 = 0x0004;
const NLM_F_DUMP: u16 = 0x0300; // NLM_F_ROOT | NLM_F_MATCH
const NLM_F_CREATE: u16 = 0x0400;
const NLM_F_APPEND: u16 = 0x0800;

const NLMSG_ERROR: u16 = 0x0002;
const NLMSG_DONE: u16 = 0x0003;
const NFNL_MSG_BATCH_BEGIN: u16 = 0x0010;
const NFNL_MSG_BATCH_END: u16 = 0x0011;

const NLA_F_NESTED: u16 = 0x8000;

// nft message types (offset within NFNL_SUBSYS_NFTABLES)
const NFT_MSG_NEWTABLE: u16 = 0;
const NFT_MSG_DELTABLE: u16 = 2;
const NFT_MSG_NEWCHAIN: u16 = 3;
const NFT_MSG_DELCHAIN: u16 = 5;
const NFT_MSG_NEWRULE: u16 = 6;
const NFT_MSG_GETRULE: u16 = 7;
const NFT_MSG_DELRULE: u16 = 8;

// Chain-flush: kernel uses NEWCHAIN with NLM_F_ACK (flush reuses NEWCHAIN path with flush flag)
// Actually, we flush by deleting all rules individually or use NFT_MSG_DELCHAIN then re-add.
// For flush chain: use a dedicated approach — delete + re-add base chain, or use setattr.
// Simpler: we can just delete all rules by sending flush (via FLUSH_CHAIN msg=DELCHAIN+flags).

// nfnetlink family
const NFPROTO_IPV4: u8 = 2;
const NFPROTO_IPV6: u8 = 10;

// Table attrs
const NFTA_TABLE_NAME: u16 = 1;
const NFTA_TABLE_FLAGS: u16 = 2;

// Chain attrs
const NFTA_CHAIN_TABLE: u16 = 1;
const NFTA_CHAIN_NAME: u16 = 3;
const NFTA_CHAIN_HOOK: u16 = 4;
const NFTA_CHAIN_POLICY: u16 = 5;
const NFTA_CHAIN_TYPE: u16 = 7;

// Hook attrs
const NFTA_HOOK_HOOKNUM: u16 = 1;
const NFTA_HOOK_PRIORITY: u16 = 2;

// Hook numbers
const NF_INET_PRE_ROUTING: u32 = 0;
const NF_INET_LOCAL_IN: u32 = 1;
const NF_INET_FORWARD: u32 = 2;
const NF_INET_POST_ROUTING: u32 = 4;

// Policy
const NF_ACCEPT: u32 = 1;

// Rule attrs
const NFTA_RULE_TABLE: u16 = 1;
const NFTA_RULE_CHAIN: u16 = 2;
const NFTA_RULE_HANDLE: u16 = 3;
const NFTA_RULE_EXPRESSIONS: u16 = 4;

// Expression list / expression attrs
const NFTA_LIST_ELEM: u16 = 1;
const NFTA_EXPR_NAME: u16 = 1;
const NFTA_EXPR_DATA: u16 = 2;

// Data attrs
const NFTA_DATA_VALUE: u16 = 1;
const NFTA_DATA_VERDICT: u16 = 2;

// Verdict attrs / codes
const NFTA_VERDICT_CODE: u16 = 1;
const NFTA_VERDICT_CHAIN: u16 = 2;
const NFT_JUMP: u32 = (-3i32) as u32; // 0xfffffffd
const NFT_ACCEPT: u32 = NF_ACCEPT; // 1

// Payload expr attrs
const NFTA_PAYLOAD_DREG: u16 = 1;
const NFTA_PAYLOAD_BASE: u16 = 2;
const NFTA_PAYLOAD_OFFSET: u16 = 3;
const NFTA_PAYLOAD_LEN: u16 = 4;
const NFT_PAYLOAD_NETWORK_HEADER: u32 = 1;
const NFT_PAYLOAD_TRANSPORT_HEADER: u32 = 2;

// CMP expr attrs / ops
const NFTA_CMP_SREG: u16 = 1;
const NFTA_CMP_OP: u16 = 2;
const NFTA_CMP_DATA: u16 = 3;
const NFT_CMP_EQ: u32 = 0;
const NFT_CMP_NEQ: u32 = 1;

// Meta expr attrs / keys
const NFTA_META_DREG: u16 = 1;
const NFTA_META_KEY: u16 = 2;
const NFT_META_IIFNAME: u32 = 6;
const NFT_META_OIFNAME: u32 = 7;
const NFT_META_L4PROTO: u32 = 16;

// Bitwise expr attrs
const NFTA_BITWISE_SREG: u16 = 1;
const NFTA_BITWISE_DREG: u16 = 2;
const NFTA_BITWISE_LEN: u16 = 3;
const NFTA_BITWISE_MASK: u16 = 4;
const NFTA_BITWISE_XOR: u16 = 5;

// Immediate expr attrs
const NFTA_IMMEDIATE_DREG: u16 = 1;
const NFTA_IMMEDIATE_DATA: u16 = 2;

// NAT expr attrs / types / flags
const NFTA_NAT_TYPE: u16 = 1;
const NFTA_NAT_FAMILY: u16 = 2;
const NFTA_NAT_REG_ADDR_MIN: u16 = 3;
const NFTA_NAT_REG_PROTO_MIN: u16 = 5;
const NFTA_NAT_FLAGS: u16 = 7;
const NFT_NAT_DNAT: u32 = 1;
const NF_NAT_RANGE_PROTO_SPECIFIED: u32 = 1 << 1;

// IPv4 header offsets
const IPV4_SADDR_OFFSET: u32 = 12;
const IPV4_DADDR_OFFSET: u32 = 16;
const IP_PROTO_TCP: u8 = 6;
const IP_PROTO_UDP: u8 = 17;

// Transport header dport offset (same for TCP and UDP)
const DPORT_OFFSET: u32 = 2;

// IFNAMSIZ
const IFNAMSIZ: usize = 16;

// Registers
const REG_VERDICT: u32 = 0;
const REG1: u32 = 1;
const REG2: u32 = 2;

// ── Low-level message building ────────────────────────────────────────────────

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn push_u16_le(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u32_le(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Write a netlink attribute (TLV) with raw bytes payload.
fn nla_put(buf: &mut Vec<u8>, nla_type: u16, data: &[u8]) {
    let len = 4 + data.len();
    push_u16_le(buf, len as u16);
    push_u16_le(buf, nla_type);
    buf.extend_from_slice(data);
    let pad = align4(len) - len;
    for _ in 0..pad {
        buf.push(0);
    }
}

fn nla_put_str(buf: &mut Vec<u8>, nla_type: u16, s: &str) {
    let mut data = s.as_bytes().to_vec();
    data.push(0); // null-terminate
    nla_put(buf, nla_type, &data);
}

fn nla_put_u32(buf: &mut Vec<u8>, nla_type: u16, v: u32) {
    nla_put(buf, nla_type, &v.to_be_bytes());
}

fn nla_put_u64(buf: &mut Vec<u8>, nla_type: u16, v: u64) {
    nla_put(buf, nla_type, &v.to_be_bytes());
}

/// Write a nested NLA attribute: first reserve the header, fill body, patch length.
fn nla_nest_start(buf: &mut Vec<u8>, nla_type: u16) -> usize {
    let pos = buf.len();
    push_u16_le(buf, 0); // placeholder length
    push_u16_le(buf, nla_type | NLA_F_NESTED);
    pos
}

fn nla_nest_end(buf: &mut Vec<u8>, start: usize) {
    let len = buf.len() - start;
    let len_bytes = (len as u16).to_le_bytes();
    buf[start] = len_bytes[0];
    buf[start + 1] = len_bytes[1];
    // pad to 4-byte boundary
    let pad = align4(len) - len;
    for _ in 0..pad {
        buf.push(0);
    }
}

/// Build the `nlmsghdr` + `nfgenmsg` header for an nftables message.
/// `seq_placeholder_pos` receives the position of the seq field for patching.
fn push_nft_header(
    buf: &mut Vec<u8>,
    msg_type_offset: u16, // e.g. NFT_MSG_NEWTABLE
    flags: u16,
    family: u8,
    seq: u32,
) -> usize {
    let start = buf.len();
    // nlmsghdr: len(4) type(2) flags(2) seq(4) pid(4)
    push_u32_le(buf, 0); // placeholder len
    let nlmsg_type = (NFNL_SUBSYS_NFTABLES << 8) | msg_type_offset;
    push_u16_le(buf, nlmsg_type);
    push_u16_le(buf, flags);
    push_u32_le(buf, seq);
    push_u32_le(buf, 0); // pid = 0 (kernel)
                         // nfgenmsg: family(1) version(1) res_id(2)
    buf.push(family);
    buf.push(NFNETLINK_V0);
    push_u16_le(buf, 0); // res_id
    start
}

fn patch_nlmsg_len(buf: &mut [u8], start: usize) {
    let len = (buf.len() - start) as u32;
    let bytes = len.to_le_bytes();
    buf[start..start + 4].copy_from_slice(&bytes);
}

fn push_batch_ctrl(buf: &mut Vec<u8>, msg_type: u16, seq: u32) {
    let start = buf.len();
    push_u32_le(buf, 0); // placeholder len
    push_u16_le(buf, msg_type); // NFNL_MSG_BATCH_BEGIN or END
    push_u16_le(buf, NLM_F_REQUEST);
    push_u32_le(buf, seq);
    push_u32_le(buf, 0); // pid
                         // nfgenmsg
    buf.push(0); // AF_UNSPEC
    buf.push(NFNETLINK_V0);
    push_u16_le(buf, 10); // res_id=10 (as nft sends)
    patch_nlmsg_len(buf, start);
}

// ── Expression builders ───────────────────────────────────────────────────────

/// Wrap expression name + data into a NFTA_LIST_ELEM nested attribute.
fn expr_wrap(name: &str, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    let elem_start = nla_nest_start(&mut buf, NFTA_LIST_ELEM);
    nla_put_str(&mut buf, NFTA_EXPR_NAME, name);
    if data.is_empty() {
        // empty nested NFTA_EXPR_DATA (masq)
        let d_start = nla_nest_start(&mut buf, NFTA_EXPR_DATA);
        nla_nest_end(&mut buf, d_start);
    } else {
        // pre-built nested data (already fully formed NLA attrs)
        let d_start = nla_nest_start(&mut buf, NFTA_EXPR_DATA);
        buf.extend_from_slice(data);
        nla_nest_end(&mut buf, d_start);
    }
    nla_nest_end(&mut buf, elem_start);
    buf
}

fn expr_payload(dreg: u32, base: u32, offset: u32, len: u32) -> Vec<u8> {
    let mut d = Vec::new();
    nla_put_u32(&mut d, NFTA_PAYLOAD_DREG, dreg);
    nla_put_u32(&mut d, NFTA_PAYLOAD_BASE, base);
    nla_put_u32(&mut d, NFTA_PAYLOAD_OFFSET, offset);
    nla_put_u32(&mut d, NFTA_PAYLOAD_LEN, len);
    expr_wrap("payload", &d)
}

fn expr_cmp(sreg: u32, op: u32, data_bytes: &[u8]) -> Vec<u8> {
    let mut inner = Vec::new();
    nla_put_u32(&mut inner, NFTA_CMP_SREG, sreg);
    nla_put_u32(&mut inner, NFTA_CMP_OP, op);
    // NFTA_CMP_DATA is nested containing NFTA_DATA_VALUE
    let mut val_outer = Vec::new();
    nla_put(&mut val_outer, NFTA_DATA_VALUE, data_bytes);
    // encode val_outer as a nested NLA into inner
    push_u16_le(&mut inner, (4 + val_outer.len()) as u16);
    push_u16_le(&mut inner, NFTA_CMP_DATA | NLA_F_NESTED);
    inner.extend_from_slice(&val_outer);
    expr_wrap("cmp", &inner)
}

fn expr_meta(dreg: u32, key: u32) -> Vec<u8> {
    let mut d = Vec::new();
    nla_put_u32(&mut d, NFTA_META_KEY, key);
    nla_put_u32(&mut d, NFTA_META_DREG, dreg);
    expr_wrap("meta", &d)
}

fn expr_masq() -> Vec<u8> {
    expr_wrap("masq", &[])
}

fn expr_verdict_accept() -> Vec<u8> {
    expr_immediate_verdict(REG_VERDICT, NFT_ACCEPT, None)
}

fn expr_verdict_jump(chain: &str) -> Vec<u8> {
    expr_immediate_verdict(REG_VERDICT, NFT_JUMP, Some(chain))
}

fn expr_immediate_verdict(dreg: u32, code: u32, chain: Option<&str>) -> Vec<u8> {
    let mut verd = Vec::new();
    nla_put_u32(&mut verd, NFTA_VERDICT_CODE, code);
    if let Some(c) = chain {
        nla_put_str(&mut verd, NFTA_VERDICT_CHAIN, c);
    }
    // NFTA_DATA_VERDICT nested
    let mut data_val = Vec::new();
    push_u16_le(&mut data_val, (4 + verd.len()) as u16);
    push_u16_le(&mut data_val, NFTA_DATA_VERDICT | NLA_F_NESTED);
    data_val.extend_from_slice(&verd);

    let mut imm = Vec::new();
    nla_put_u32(&mut imm, NFTA_IMMEDIATE_DREG, dreg);
    // NFTA_IMMEDIATE_DATA nested
    push_u16_le(&mut imm, (4 + data_val.len()) as u16);
    push_u16_le(&mut imm, NFTA_IMMEDIATE_DATA | NLA_F_NESTED);
    imm.extend_from_slice(&data_val);
    expr_wrap("immediate", &imm)
}

fn expr_immediate_bytes(dreg: u32, bytes: &[u8]) -> Vec<u8> {
    let mut val = Vec::new();
    nla_put(&mut val, NFTA_DATA_VALUE, bytes);
    let mut imm = Vec::new();
    nla_put_u32(&mut imm, NFTA_IMMEDIATE_DREG, dreg);
    push_u16_le(&mut imm, (4 + val.len()) as u16);
    push_u16_le(&mut imm, NFTA_IMMEDIATE_DATA | NLA_F_NESTED);
    imm.extend_from_slice(&val);
    expr_wrap("immediate", &imm)
}

/// Bitwise AND: reg &= mask (for CIDR prefix matching)
fn expr_bitwise_and(reg: u32, len: u32, mask: &[u8]) -> Vec<u8> {
    let zeros = vec![0u8; len as usize];
    let mut mask_attr = Vec::new();
    nla_put(&mut mask_attr, NFTA_DATA_VALUE, mask);
    let mut xor_attr = Vec::new();
    nla_put(&mut xor_attr, NFTA_DATA_VALUE, &zeros);

    let mut d = Vec::new();
    nla_put_u32(&mut d, NFTA_BITWISE_SREG, reg);
    nla_put_u32(&mut d, NFTA_BITWISE_DREG, reg);
    nla_put_u32(&mut d, NFTA_BITWISE_LEN, len);
    // mask nested
    push_u16_le(&mut d, (4 + mask_attr.len()) as u16);
    push_u16_le(&mut d, NFTA_BITWISE_MASK | NLA_F_NESTED);
    d.extend_from_slice(&mask_attr);
    // xor nested
    push_u16_le(&mut d, (4 + xor_attr.len()) as u16);
    push_u16_le(&mut d, NFTA_BITWISE_XOR | NLA_F_NESTED);
    d.extend_from_slice(&xor_attr);
    expr_wrap("bitwise", &d)
}

/// Produce the prefix mask bytes for a /prefix_len IPv4 network.
fn ipv4_prefix_mask(prefix_len: u8) -> [u8; 4] {
    if prefix_len == 0 {
        [0, 0, 0, 0]
    } else if prefix_len >= 32 {
        [0xff, 0xff, 0xff, 0xff]
    } else {
        let mask: u32 = !((1u32 << (32 - prefix_len)) - 1);
        mask.to_be_bytes()
    }
}

/// Expressions matching `ip saddr <net>/<prefix>`.
fn exprs_match_ipv4_saddr(net: Ipv4Addr, prefix_len: u8) -> Vec<u8> {
    let mut exprs = Vec::new();
    exprs.extend(expr_payload(
        REG1,
        NFT_PAYLOAD_NETWORK_HEADER,
        IPV4_SADDR_OFFSET,
        4,
    ));
    let mask = ipv4_prefix_mask(prefix_len);
    if prefix_len < 32 {
        exprs.extend(expr_bitwise_and(REG1, 4, &mask));
    }
    let net_bytes: [u8; 4] = net.octets();
    let masked_net = u32::from_be_bytes(net_bytes) & u32::from_be_bytes(mask);
    exprs.extend(expr_cmp(REG1, NFT_CMP_EQ, &masked_net.to_be_bytes()));
    exprs
}

/// Expressions matching `ip daddr <net>/<prefix>`.
fn exprs_match_ipv4_daddr(net: Ipv4Addr, prefix_len: u8) -> Vec<u8> {
    let mut exprs = Vec::new();
    exprs.extend(expr_payload(
        REG1,
        NFT_PAYLOAD_NETWORK_HEADER,
        IPV4_DADDR_OFFSET,
        4,
    ));
    let mask = ipv4_prefix_mask(prefix_len);
    if prefix_len < 32 {
        exprs.extend(expr_bitwise_and(REG1, 4, &mask));
    }
    let net_bytes: [u8; 4] = net.octets();
    let masked_net = u32::from_be_bytes(net_bytes) & u32::from_be_bytes(mask);
    exprs.extend(expr_cmp(REG1, NFT_CMP_EQ, &masked_net.to_be_bytes()));
    exprs
}

/// Expressions matching `oifname != "<bridge>"`.
fn exprs_oifname_neq(bridge: &str) -> Vec<u8> {
    let mut exprs = Vec::new();
    exprs.extend(expr_meta(REG1, NFT_META_OIFNAME));
    let mut padded = [0u8; IFNAMSIZ];
    let bytes = bridge.as_bytes();
    let copy_len = bytes.len().min(IFNAMSIZ - 1);
    padded[..copy_len].copy_from_slice(&bytes[..copy_len]);
    exprs.extend(expr_cmp(REG1, NFT_CMP_NEQ, &padded));
    exprs
}

/// Expressions matching `iifname "<bridge>"`.
fn exprs_iifname_eq(bridge: &str) -> Vec<u8> {
    let mut exprs = Vec::new();
    exprs.extend(expr_meta(REG1, NFT_META_IIFNAME));
    let mut padded = [0u8; IFNAMSIZ];
    let bytes = bridge.as_bytes();
    let copy_len = bytes.len().min(IFNAMSIZ - 1);
    padded[..copy_len].copy_from_slice(&bytes[..copy_len]);
    exprs.extend(expr_cmp(REG1, NFT_CMP_EQ, &padded));
    exprs
}

/// Expressions matching `udp dport 53` (DNS).
fn exprs_udp_dport_53() -> Vec<u8> {
    let mut exprs = Vec::new();
    // meta l4proto == UDP
    exprs.extend(expr_meta(REG1, NFT_META_L4PROTO));
    exprs.extend(expr_cmp(REG1, NFT_CMP_EQ, &[IP_PROTO_UDP]));
    // transport dport == 53
    exprs.extend(expr_payload(
        REG1,
        NFT_PAYLOAD_TRANSPORT_HEADER,
        DPORT_OFFSET,
        2,
    ));
    exprs.extend(expr_cmp(REG1, NFT_CMP_EQ, &53u16.to_be_bytes()));
    exprs
}

/// Expressions matching a TCP or UDP dport.
fn exprs_dport(host_port: u16, proto: u8) -> Vec<u8> {
    let mut exprs = Vec::new();
    exprs.extend(expr_meta(REG1, NFT_META_L4PROTO));
    exprs.extend(expr_cmp(REG1, NFT_CMP_EQ, &[proto]));
    exprs.extend(expr_payload(
        REG1,
        NFT_PAYLOAD_TRANSPORT_HEADER,
        DPORT_OFFSET,
        2,
    ));
    exprs.extend(expr_cmp(REG1, NFT_CMP_EQ, &host_port.to_be_bytes()));
    exprs
}

/// NAT DNAT expression (after IP loaded in REG1 and port in REG2).
fn expr_nat_dnat_v4() -> Vec<u8> {
    let mut d = Vec::new();
    nla_put_u32(&mut d, NFTA_NAT_TYPE, NFT_NAT_DNAT);
    nla_put_u32(&mut d, NFTA_NAT_FAMILY, NFPROTO_IPV4 as u32);
    nla_put_u32(&mut d, NFTA_NAT_REG_ADDR_MIN, REG1);
    nla_put_u32(&mut d, NFTA_NAT_REG_PROTO_MIN, REG2);
    nla_put_u32(&mut d, NFTA_NAT_FLAGS, NF_NAT_RANGE_PROTO_SPECIFIED);
    expr_wrap("nat", &d)
}

fn expr_nat_dnat_v6() -> Vec<u8> {
    let mut d = Vec::new();
    nla_put_u32(&mut d, NFTA_NAT_TYPE, NFT_NAT_DNAT);
    nla_put_u32(&mut d, NFTA_NAT_FAMILY, NFPROTO_IPV6 as u32);
    nla_put_u32(&mut d, NFTA_NAT_REG_ADDR_MIN, REG1);
    nla_put_u32(&mut d, NFTA_NAT_REG_PROTO_MIN, REG2);
    nla_put_u32(&mut d, NFTA_NAT_FLAGS, NF_NAT_RANGE_PROTO_SPECIFIED);
    expr_wrap("nat", &d)
}

// ── Message builders ──────────────────────────────────────────────────────────

fn msg_add_table(buf: &mut Vec<u8>, family: u8, table: &str, seq: u32) {
    let start = push_nft_header(
        buf,
        NFT_MSG_NEWTABLE,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_ACK,
        family,
        seq,
    );
    nla_put_str(buf, NFTA_TABLE_NAME, table);
    nla_put_u32(buf, NFTA_TABLE_FLAGS, 0);
    patch_nlmsg_len(buf, start);
}

fn msg_del_table(buf: &mut Vec<u8>, family: u8, table: &str, seq: u32) {
    let start = push_nft_header(
        buf,
        NFT_MSG_DELTABLE,
        NLM_F_REQUEST | NLM_F_ACK,
        family,
        seq,
    );
    nla_put_str(buf, NFTA_TABLE_NAME, table);
    patch_nlmsg_len(buf, start);
}

/// `hook`: (hooknum, priority_i32); policy is always NF_ACCEPT.
fn msg_add_base_chain(
    buf: &mut Vec<u8>,
    family: u8,
    table: &str,
    name: &str,
    chain_type: &str,
    hook: (u32, i32),
    seq: u32,
) {
    let policy = NF_ACCEPT;
    let start = push_nft_header(
        buf,
        NFT_MSG_NEWCHAIN,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_ACK,
        family,
        seq,
    );
    nla_put_str(buf, NFTA_CHAIN_TABLE, table);
    nla_put_str(buf, NFTA_CHAIN_NAME, name);
    nla_put_str(buf, NFTA_CHAIN_TYPE, chain_type);
    let hook_start = nla_nest_start(buf, NFTA_CHAIN_HOOK);
    nla_put_u32(buf, NFTA_HOOK_HOOKNUM, hook.0);
    nla_put(buf, NFTA_HOOK_PRIORITY, &hook.1.to_be_bytes());
    nla_nest_end(buf, hook_start);
    nla_put_u32(buf, NFTA_CHAIN_POLICY, policy);
    patch_nlmsg_len(buf, start);
}

fn msg_add_chain(buf: &mut Vec<u8>, family: u8, table: &str, name: &str, seq: u32) {
    let start = push_nft_header(
        buf,
        NFT_MSG_NEWCHAIN,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_ACK,
        family,
        seq,
    );
    nla_put_str(buf, NFTA_CHAIN_TABLE, table);
    nla_put_str(buf, NFTA_CHAIN_NAME, name);
    patch_nlmsg_len(buf, start);
}

fn msg_del_chain(buf: &mut Vec<u8>, family: u8, table: &str, name: &str, seq: u32) {
    let start = push_nft_header(
        buf,
        NFT_MSG_DELCHAIN,
        NLM_F_REQUEST | NLM_F_ACK,
        family,
        seq,
    );
    nla_put_str(buf, NFTA_CHAIN_TABLE, table);
    nla_put_str(buf, NFTA_CHAIN_NAME, name);
    patch_nlmsg_len(buf, start);
}

fn msg_flush_chain(buf: &mut Vec<u8>, family: u8, table: &str, chain: &str, seq: u32) {
    // Flush chain = NEWCHAIN with NLM_F_CREATE but no expressions; kernel clears rules.
    // Actual flush uses a dedicated NFT_MSG_DELRULE with no handle (del all).
    // The correct way: send NFT_MSG_DELRULE with NFTA_RULE_TABLE+CHAIN but no handle.
    // This deletes all rules in the chain atomically.
    let start = push_nft_header(buf, NFT_MSG_DELRULE, NLM_F_REQUEST | NLM_F_ACK, family, seq);
    nla_put_str(buf, NFTA_RULE_TABLE, table);
    nla_put_str(buf, NFTA_RULE_CHAIN, chain);
    patch_nlmsg_len(buf, start);
}

fn msg_add_rule(
    buf: &mut Vec<u8>,
    family: u8,
    table: &str,
    chain: &str,
    exprs_bytes: &[u8],
    seq: u32,
) {
    let start = push_nft_header(
        buf,
        NFT_MSG_NEWRULE,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_APPEND | NLM_F_ACK,
        family,
        seq,
    );
    nla_put_str(buf, NFTA_RULE_TABLE, table);
    nla_put_str(buf, NFTA_RULE_CHAIN, chain);
    // expressions list (nested)
    let exprs_start = nla_nest_start(buf, NFTA_RULE_EXPRESSIONS);
    buf.extend_from_slice(exprs_bytes);
    nla_nest_end(buf, exprs_start);
    patch_nlmsg_len(buf, start);
}

fn msg_del_rule_by_handle(
    buf: &mut Vec<u8>,
    family: u8,
    table: &str,
    chain: &str,
    handle: u64,
    seq: u32,
) {
    let start = push_nft_header(buf, NFT_MSG_DELRULE, NLM_F_REQUEST | NLM_F_ACK, family, seq);
    nla_put_str(buf, NFTA_RULE_TABLE, table);
    nla_put_str(buf, NFTA_RULE_CHAIN, chain);
    nla_put_u64(buf, NFTA_RULE_HANDLE, handle);
    patch_nlmsg_len(buf, start);
}

fn msg_get_rules(buf: &mut Vec<u8>, family: u8, table: &str, chain: &str, seq: u32) {
    let start = push_nft_header(
        buf,
        NFT_MSG_GETRULE,
        NLM_F_REQUEST | NLM_F_DUMP,
        family,
        seq,
    );
    nla_put_str(buf, NFTA_RULE_TABLE, table);
    nla_put_str(buf, NFTA_RULE_CHAIN, chain);
    patch_nlmsg_len(buf, start);
}

// ── Socket and batch execution ────────────────────────────────────────────────

fn open_nfnetlink() -> io::Result<RawFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            NETLINK_NETFILTER,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Bind to pid=0 so kernel assigns a unique nl_pid
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;
    let rc = unsafe {
        libc::bind(
            fd,
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };
    if rc < 0 {
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Send a batch of nfnetlink operations and consume all ACK responses.
/// Returns an error if any NLMSG_ERROR response has a non-zero error code.
///
/// `ops` should contain only the operation messages (not batch begin/end).
/// `num_ack_expected` is how many NLM_F_ACK responses to drain.
fn send_batch(fd: RawFd, ops: &[u8], num_ack_expected: usize) -> io::Result<()> {
    let mut batch = Vec::with_capacity(40 + ops.len() + 20);
    push_batch_ctrl(&mut batch, NFNL_MSG_BATCH_BEGIN, 0);
    batch.extend_from_slice(ops);
    push_batch_ctrl(
        &mut batch,
        NFNL_MSG_BATCH_END,
        (num_ack_expected + 1) as u32,
    );

    // Send
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as u16;
    let iov = libc::iovec {
        iov_base: batch.as_ptr() as *mut _,
        iov_len: batch.len(),
    };
    let msg = libc::msghdr {
        msg_name: &sa as *const _ as *mut _,
        msg_namelen: std::mem::size_of::<libc::sockaddr_nl>() as u32,
        msg_iov: &iov as *const _ as *mut _,
        msg_iovlen: 1,
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    let sent = unsafe { libc::sendmsg(fd, &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }

    // Drain ACK responses
    let mut recv_buf = vec![0u8; 32768];
    let mut remaining = num_ack_expected;
    while remaining > 0 {
        let n = unsafe {
            libc::recvmsg(
                fd,
                &mut libc::msghdr {
                    msg_name: std::ptr::null_mut(),
                    msg_namelen: 0,
                    msg_iov: &libc::iovec {
                        iov_base: recv_buf.as_mut_ptr() as *mut _,
                        iov_len: recv_buf.len(),
                    } as *const _ as *mut _,
                    msg_iovlen: 1,
                    msg_control: std::ptr::null_mut(),
                    msg_controllen: 0,
                    msg_flags: 0,
                },
                0,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        let mut offset = 0usize;
        let n = n as usize;
        while offset + 16 <= n {
            let msg_len =
                u32::from_le_bytes(recv_buf[offset..offset + 4].try_into().unwrap()) as usize;
            let msg_type = u16::from_le_bytes(recv_buf[offset + 4..offset + 6].try_into().unwrap());
            if msg_len < 16 || offset + msg_len > n {
                break;
            }
            if msg_type == NLMSG_ERROR {
                // nlmsghdr (16) + error i32 (4)
                if offset + 20 <= n {
                    let error =
                        i32::from_le_bytes(recv_buf[offset + 16..offset + 20].try_into().unwrap());
                    if error != 0 {
                        return Err(io::Error::from_raw_os_error(-error));
                    }
                }
                remaining = remaining.saturating_sub(1);
            } else if msg_type == NLMSG_DONE {
                remaining = 0;
                break;
            }
            offset += align4(msg_len);
        }
    }
    Ok(())
}

/// Like `send_batch` but treats non-zero errors as non-fatal (logs, returns Ok).
fn send_batch_quiet(fd: RawFd, ops: &[u8], num_ack: usize) -> io::Result<()> {
    match send_batch(fd, ops, num_ack) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::debug!("nfnetlink (non-fatal): {}", e);
            Ok(())
        }
    }
}

/// Convenience: open socket, run closure building ops buffer + ack count, close socket.
fn with_nfnl<F>(f: F) -> io::Result<()>
where
    F: FnOnce(RawFd) -> io::Result<()>,
{
    let fd = open_nfnetlink()?;
    let result = f(fd);
    unsafe { libc::close(fd) };
    result
}

fn with_nfnl_quiet<F>(f: F)
where
    F: FnOnce(RawFd) -> io::Result<()>,
{
    if let Err(e) = with_nfnl(f) {
        log::debug!("nfnetlink (non-fatal): {}", e);
    }
}

// ── High-level API ────────────────────────────────────────────────────────────

/// Create (or idempotently ensure) the per-network NAT table with:
/// - `ip TABLE postrouting` chain: masquerade rule for packets from CIDR not going to bridge
/// - `ip TABLE forward` chain: accept rules for CIDR src/dst
pub fn nft_create_nat_masquerade(
    table: &str,
    bridge: &str,
    net: Ipv4Addr,
    prefix: u8,
) -> io::Result<()> {
    with_nfnl(|fd| {
        let mut ops = Vec::new();
        let mut seq = 1u32;

        // add table ip TABLE
        msg_add_table(&mut ops, NFPROTO_IPV4, table, seq);
        seq += 1;

        // add chain ip TABLE postrouting { type nat hook postrouting priority 100 }
        msg_add_base_chain(
            &mut ops,
            NFPROTO_IPV4,
            table,
            "postrouting",
            "nat",
            (NF_INET_POST_ROUTING, 100),
            seq,
        );
        seq += 1;

        // masquerade rule: ip saddr CIDR oifname != bridge masquerade
        {
            let mut exprs = Vec::new();
            exprs.extend(exprs_match_ipv4_saddr(net, prefix));
            exprs.extend(exprs_oifname_neq(bridge));
            exprs.extend(expr_masq());
            msg_add_rule(&mut ops, NFPROTO_IPV4, table, "postrouting", &exprs, seq);
            seq += 1;
        }

        // add chain ip TABLE forward { type filter hook forward priority -100 }
        msg_add_base_chain(
            &mut ops,
            NFPROTO_IPV4,
            table,
            "forward",
            "filter",
            (NF_INET_FORWARD, -100),
            seq,
        );
        seq += 1;

        // accept rule: ip saddr CIDR accept
        {
            let mut exprs = Vec::new();
            exprs.extend(exprs_match_ipv4_saddr(net, prefix));
            exprs.extend(expr_verdict_accept());
            msg_add_rule(&mut ops, NFPROTO_IPV4, table, "forward", &exprs, seq);
            seq += 1;
        }

        // accept rule: ip daddr CIDR accept
        {
            let mut exprs = Vec::new();
            exprs.extend(exprs_match_ipv4_daddr(net, prefix));
            exprs.extend(expr_verdict_accept());
            msg_add_rule(&mut ops, NFPROTO_IPV4, table, "forward", &exprs, seq);
            seq += 1;
        }

        let num_acks = (seq - 1) as usize;
        send_batch(fd, &ops, num_acks)
    })
}

/// Flush the postrouting chain (remove MASQUERADE rules, keep port-forward DNAT chains).
pub fn nft_flush_postrouting(table: &str) -> io::Result<()> {
    with_nfnl(|fd| {
        let mut ops = Vec::new();
        msg_flush_chain(&mut ops, NFPROTO_IPV4, table, "postrouting", 1);
        send_batch(fd, &ops, 1)
    })
}

/// Delete an ip-family table entirely (non-fatal if not found).
pub fn nft_delete_ip_table(table: &str) {
    with_nfnl_quiet(|fd| {
        let mut ops = Vec::new();
        msg_del_table(&mut ops, NFPROTO_IPV4, table, 1);
        send_batch(fd, &ops, 1)
    });
}

/// Delete an ip6-family table entirely (non-fatal if not found).
pub fn nft_delete_ip6_table(table: &str) {
    with_nfnl_quiet(|fd| {
        let mut ops = Vec::new();
        msg_del_table(&mut ops, NFPROTO_IPV6, table, 1);
        send_batch(fd, &ops, 1)
    });
}

/// Add (or refresh) the DNS INPUT chain for a network:
/// - Creates the table and INPUT chain (priority -100) idempotently
/// - Flushes then re-adds: iifname bridge, udp dport 53, accept
pub fn nft_add_dns_input_chain(table: &str, bridge: &str) -> io::Result<()> {
    with_nfnl(|fd| {
        let mut ops = Vec::new();
        let mut seq = 1u32;

        msg_add_table(&mut ops, NFPROTO_IPV4, table, seq);
        seq += 1;
        msg_add_base_chain(
            &mut ops,
            NFPROTO_IPV4,
            table,
            "input",
            "filter",
            (NF_INET_LOCAL_IN, -100),
            seq,
        );
        seq += 1;
        msg_flush_chain(&mut ops, NFPROTO_IPV4, table, "input", seq);
        seq += 1;

        let mut exprs = Vec::new();
        exprs.extend(exprs_iifname_eq(bridge));
        exprs.extend(exprs_udp_dport_53());
        exprs.extend(expr_verdict_accept());
        msg_add_rule(&mut ops, NFPROTO_IPV4, table, "input", &exprs, seq);
        seq += 1;

        send_batch(fd, &ops, (seq - 1) as usize)
    })
}

/// Remove the DNS INPUT chain (non-fatal).
pub fn nft_remove_dns_input_chain(table: &str) {
    with_nfnl_quiet(|fd| {
        let mut ops = Vec::new();
        let mut seq = 1u32;
        msg_flush_chain(&mut ops, NFPROTO_IPV4, table, "input", seq);
        seq += 1;
        msg_del_chain(&mut ops, NFPROTO_IPV4, table, "input", seq);
        seq += 1;
        send_batch_quiet(fd, &ops, (seq - 1) as usize)
    });
}

/// Add iptables-nft compat FORWARD rules in `ip filter` for a network CIDR.
/// Creates a named chain `chain` inside `ip filter`, flushes it, adds saddr/daddr
/// accept rules, then adds a jump from `FORWARD` to this chain.
pub fn nft_add_filter_forward_compat(chain: &str, net: Ipv4Addr, prefix: u8) {
    with_nfnl_quiet(|fd| {
        let mut ops = Vec::new();
        let mut seq = 1u32;

        // Remove stale jump from FORWARD → chain first (idempotent)
        for handle in nft_find_jump_handles_fd(fd, NFPROTO_IPV4, "filter", "FORWARD", chain)? {
            msg_del_rule_by_handle(&mut ops, NFPROTO_IPV4, "filter", "FORWARD", handle, seq);
            seq += 1;
        }

        msg_add_chain(&mut ops, NFPROTO_IPV4, "filter", chain, seq);
        seq += 1;
        msg_flush_chain(&mut ops, NFPROTO_IPV4, "filter", chain, seq);
        seq += 1;

        let mut exprs = Vec::new();
        exprs.extend(exprs_match_ipv4_saddr(net, prefix));
        exprs.extend(expr_verdict_accept());
        msg_add_rule(&mut ops, NFPROTO_IPV4, "filter", chain, &exprs, seq);
        seq += 1;

        let mut exprs = Vec::new();
        exprs.extend(exprs_match_ipv4_daddr(net, prefix));
        exprs.extend(expr_verdict_accept());
        msg_add_rule(&mut ops, NFPROTO_IPV4, "filter", chain, &exprs, seq);
        seq += 1;

        let mut exprs = Vec::new();
        exprs.extend(expr_verdict_jump(chain));
        msg_add_rule(&mut ops, NFPROTO_IPV4, "filter", "FORWARD", &exprs, seq);
        seq += 1;

        send_batch_quiet(fd, &ops, (seq - 1) as usize)
    });
}

/// Remove iptables-nft compat FORWARD chain (non-fatal).
pub fn nft_remove_filter_forward_compat(chain: &str) {
    with_nfnl_quiet(|fd| {
        let handles = nft_find_jump_handles_fd(fd, NFPROTO_IPV4, "filter", "FORWARD", chain)?;
        let mut ops = Vec::new();
        let mut seq = 1u32;
        for handle in handles {
            msg_del_rule_by_handle(&mut ops, NFPROTO_IPV4, "filter", "FORWARD", handle, seq);
            seq += 1;
        }
        msg_flush_chain(&mut ops, NFPROTO_IPV4, "filter", chain, seq);
        seq += 1;
        msg_del_chain(&mut ops, NFPROTO_IPV4, "filter", chain, seq);
        seq += 1;
        send_batch_quiet(fd, &ops, (seq - 1) as usize)
    });
}

/// Add iptables-nft compat INPUT chain for DNS (non-fatal).
pub fn nft_add_filter_input_compat(chain: &str, bridge: &str) {
    with_nfnl_quiet(|fd| {
        let mut ops = Vec::new();
        let mut seq = 1u32;

        for handle in nft_find_jump_handles_fd(fd, NFPROTO_IPV4, "filter", "INPUT", chain)? {
            msg_del_rule_by_handle(&mut ops, NFPROTO_IPV4, "filter", "INPUT", handle, seq);
            seq += 1;
        }

        msg_add_chain(&mut ops, NFPROTO_IPV4, "filter", chain, seq);
        seq += 1;
        msg_flush_chain(&mut ops, NFPROTO_IPV4, "filter", chain, seq);
        seq += 1;

        let mut exprs = Vec::new();
        exprs.extend(exprs_iifname_eq(bridge));
        exprs.extend(exprs_udp_dport_53());
        exprs.extend(expr_verdict_accept());
        msg_add_rule(&mut ops, NFPROTO_IPV4, "filter", chain, &exprs, seq);
        seq += 1;

        let mut exprs = Vec::new();
        exprs.extend(expr_verdict_jump(chain));
        msg_add_rule(&mut ops, NFPROTO_IPV4, "filter", "INPUT", &exprs, seq);
        seq += 1;

        send_batch_quiet(fd, &ops, (seq - 1) as usize)
    });
}

/// Remove iptables-nft compat INPUT chain (non-fatal).
pub fn nft_remove_filter_input_compat(chain: &str) {
    with_nfnl_quiet(|fd| {
        let handles = nft_find_jump_handles_fd(fd, NFPROTO_IPV4, "filter", "INPUT", chain)?;
        let mut ops = Vec::new();
        let mut seq = 1u32;
        for handle in handles {
            msg_del_rule_by_handle(&mut ops, NFPROTO_IPV4, "filter", "INPUT", handle, seq);
            seq += 1;
        }
        msg_flush_chain(&mut ops, NFPROTO_IPV4, "filter", chain, seq);
        seq += 1;
        msg_del_chain(&mut ops, NFPROTO_IPV4, "filter", chain, seq);
        seq += 1;
        send_batch_quiet(fd, &ops, (seq - 1) as usize)
    });
}

/// Install (or rebuild) IPv4 DNAT rules in the prerouting chain.
pub fn nft_install_dnat(
    table: &str,
    entries: &[(Ipv4Addr, u16, u16, PortProto)],
) -> io::Result<()> {
    with_nfnl(|fd| {
        let mut ops = Vec::new();
        let mut seq = 1u32;

        msg_add_table(&mut ops, NFPROTO_IPV4, table, seq);
        seq += 1;
        msg_add_base_chain(
            &mut ops,
            NFPROTO_IPV4,
            table,
            "prerouting",
            "nat",
            (NF_INET_PRE_ROUTING, -100),
            seq,
        );
        seq += 1;
        msg_flush_chain(&mut ops, NFPROTO_IPV4, table, "prerouting", seq);
        seq += 1;

        for (ip, host_port, container_port, proto) in entries {
            let protos: &[u8] = match proto {
                PortProto::Tcp => &[IP_PROTO_TCP],
                PortProto::Udp => &[IP_PROTO_UDP],
                PortProto::Both => &[IP_PROTO_TCP, IP_PROTO_UDP],
            };
            for &p in protos {
                let mut exprs = Vec::new();
                exprs.extend(exprs_dport(*host_port, p));
                exprs.extend(expr_immediate_bytes(REG1, &ip.octets()));
                exprs.extend(expr_immediate_bytes(REG2, &container_port.to_be_bytes()));
                exprs.extend(expr_nat_dnat_v4());
                msg_add_rule(&mut ops, NFPROTO_IPV4, table, "prerouting", &exprs, seq);
                seq += 1;
            }
        }

        send_batch(fd, &ops, (seq - 1) as usize)
    })
}

/// Install (or rebuild) IPv6 DNAT rules in the prerouting chain.
pub fn nft_install_dnat6(
    table: &str,
    entries: &[(Ipv6Addr, u16, u16, PortProto)],
) -> io::Result<()> {
    with_nfnl(|fd| {
        let mut ops = Vec::new();
        let mut seq = 1u32;

        msg_add_table(&mut ops, NFPROTO_IPV6, table, seq);
        seq += 1;
        msg_add_base_chain(
            &mut ops,
            NFPROTO_IPV6,
            table,
            "prerouting",
            "nat",
            (NF_INET_PRE_ROUTING, -100),
            seq,
        );
        seq += 1;
        msg_flush_chain(&mut ops, NFPROTO_IPV6, table, "prerouting", seq);
        seq += 1;

        for (ip6, host_port, container_port, proto) in entries {
            let protos: &[u8] = match proto {
                PortProto::Tcp => &[IP_PROTO_TCP],
                PortProto::Udp => &[IP_PROTO_UDP],
                PortProto::Both => &[IP_PROTO_TCP, IP_PROTO_UDP],
            };
            for &p in protos {
                let mut exprs = Vec::new();
                exprs.extend(exprs_dport(*host_port, p));
                exprs.extend(expr_immediate_bytes(REG1, &ip6.octets()));
                exprs.extend(expr_immediate_bytes(REG2, &container_port.to_be_bytes()));
                exprs.extend(expr_nat_dnat_v6());
                msg_add_rule(&mut ops, NFPROTO_IPV6, table, "prerouting", &exprs, seq);
                seq += 1;
            }
        }

        send_batch(fd, &ops, (seq - 1) as usize)
    })
}

/// Flush the IPv4 prerouting chain (keep table/chain, remove all rules).
pub fn nft_flush_prerouting(table: &str) -> io::Result<()> {
    with_nfnl(|fd| {
        let mut ops = Vec::new();
        msg_flush_chain(&mut ops, NFPROTO_IPV4, table, "prerouting", 1);
        send_batch(fd, &ops, 1)
    })
}

/// Flush the IPv6 prerouting chain (non-fatal).
pub fn nft_flush_prerouting6(table: &str) {
    with_nfnl_quiet(|fd| {
        let mut ops = Vec::new();
        msg_flush_chain(&mut ops, NFPROTO_IPV6, table, "prerouting", 1);
        send_batch_quiet(fd, &ops, 1)
    });
}

// ── Rule listing (for iptables-nft compat cleanup) ────────────────────────────

/// Find handles of all rules in `family:table:chain` that contain a
/// `verdict jump <target>` expression.  Returns empty vec if the chain
/// or table doesn't exist (non-fatal).
pub fn nft_find_jump_handles(family: u8, table: &str, chain: &str, target: &str) -> Vec<u64> {
    match open_nfnetlink() {
        Ok(fd) => {
            let result =
                nft_find_jump_handles_fd(fd, family, table, chain, target).unwrap_or_default();
            unsafe { libc::close(fd) };
            result
        }
        Err(_) => vec![],
    }
}

/// Same as `nft_find_jump_handles` but reuses an existing fd.
fn nft_find_jump_handles_fd(
    fd: RawFd,
    family: u8,
    table: &str,
    chain: &str,
    target: &str,
) -> io::Result<Vec<u64>> {
    // Send GETRULE DUMP request
    let mut req = Vec::new();
    msg_get_rules(&mut req, family, table, chain, 1);

    let sent = unsafe {
        let mut sa: libc::sockaddr_nl = std::mem::zeroed();
        sa.nl_family = libc::AF_NETLINK as u16;
        let iov = libc::iovec {
            iov_base: req.as_ptr() as *mut _,
            iov_len: req.len(),
        };
        let msg = libc::msghdr {
            msg_name: &sa as *const _ as *mut _,
            msg_namelen: std::mem::size_of::<libc::sockaddr_nl>() as u32,
            msg_iov: &iov as *const _ as *mut _,
            msg_iovlen: 1,
            msg_control: std::ptr::null_mut(),
            msg_controllen: 0,
            msg_flags: 0,
        };
        libc::sendmsg(fd, &msg, 0)
    };
    if sent < 0 {
        let e = io::Error::last_os_error();
        // ENOENT means the chain/table doesn't exist — not an error for us
        if e.raw_os_error() == Some(libc::ENOENT) {
            return Ok(vec![]);
        }
        return Err(e);
    }

    // Read responses
    let mut handles = Vec::new();
    let mut recv_buf = vec![0u8; 65536];

    'outer: loop {
        let n = unsafe {
            libc::recvmsg(
                fd,
                &mut libc::msghdr {
                    msg_name: std::ptr::null_mut(),
                    msg_namelen: 0,
                    msg_iov: &libc::iovec {
                        iov_base: recv_buf.as_mut_ptr() as *mut _,
                        iov_len: recv_buf.len(),
                    } as *const _ as *mut _,
                    msg_iovlen: 1,
                    msg_control: std::ptr::null_mut(),
                    msg_controllen: 0,
                    msg_flags: 0,
                },
                0,
            )
        };
        if n <= 0 {
            break;
        }
        let n = n as usize;
        let mut offset = 0usize;
        while offset + 16 <= n {
            let msg_len =
                u32::from_le_bytes(recv_buf[offset..offset + 4].try_into().unwrap()) as usize;
            let msg_type = u16::from_le_bytes(recv_buf[offset + 4..offset + 6].try_into().unwrap());
            if msg_len < 16 || offset + msg_len > n {
                break;
            }
            if msg_type == NLMSG_DONE {
                break 'outer;
            }
            if msg_type == NLMSG_ERROR {
                // ENOENT = table/chain doesn't exist → return empty
                break 'outer;
            }
            let nfnl_subsys = msg_type >> 8;
            let nfnl_msg = msg_type & 0xff;
            if nfnl_subsys == NFNL_SUBSYS_NFTABLES && nfnl_msg == NFT_MSG_NEWRULE {
                // Parse attrs starting after nlmsghdr(16) + nfgenmsg(4)
                let attrs_start = offset + 16 + 4;
                let attrs_end = offset + msg_len;
                if let Some(handle) =
                    parse_rule_jump_handle(&recv_buf[attrs_start..attrs_end], target)
                {
                    handles.push(handle);
                }
            }
            offset += align4(msg_len);
        }
    }
    Ok(handles)
}

/// Delete a single rule by handle (non-fatal wrapper).
pub fn nft_delete_rule(family: u8, table: &str, chain: &str, handle: u64) {
    with_nfnl_quiet(|fd| {
        let mut ops = Vec::new();
        msg_del_rule_by_handle(&mut ops, family, table, chain, handle, 1);
        send_batch(fd, &ops, 1)
    });
}

// ── NLA response parser ───────────────────────────────────────────────────────

/// Parse the NLA attributes of a NEWRULE response.  If the rule contains a
/// `verdict jump <target>` immediate expression, return the rule's handle.
fn parse_rule_jump_handle(attrs: &[u8], target: &str) -> Option<u64> {
    let mut handle: Option<u64> = None;
    let mut has_jump_target = false;

    let mut pos = 0usize;
    while pos + 4 <= attrs.len() {
        let nla_len = u16::from_le_bytes(attrs[pos..pos + 2].try_into().ok()?) as usize;
        let nla_type = u16::from_le_bytes(attrs[pos + 2..pos + 4].try_into().ok()?) & !NLA_F_NESTED;
        if nla_len < 4 || pos + nla_len > attrs.len() {
            break;
        }
        let data = &attrs[pos + 4..pos + nla_len];
        match nla_type {
            t if t == NFTA_RULE_HANDLE => {
                if data.len() >= 8 {
                    handle = Some(u64::from_be_bytes(data[..8].try_into().ok()?));
                }
            }
            t if t == NFTA_RULE_EXPRESSIONS => {
                if rule_exprs_contain_jump(data, target) {
                    has_jump_target = true;
                }
            }
            _ => {}
        }
        pos += align4(nla_len);
    }

    if has_jump_target {
        handle
    } else {
        None
    }
}

/// Return true if the expressions NLA payload contains a jump to `target`.
fn rule_exprs_contain_jump(exprs: &[u8], target: &str) -> bool {
    let mut pos = 0usize;
    while pos + 4 <= exprs.len() {
        let nla_len = u16::from_le_bytes(exprs[pos..pos + 2].try_into().unwrap_or([0; 2])) as usize;
        if nla_len < 4 || pos + nla_len > exprs.len() {
            break;
        }
        let elem_data = &exprs[pos + 4..pos + nla_len];
        if expr_elem_is_jump(elem_data, target) {
            return true;
        }
        pos += align4(nla_len);
    }
    false
}

/// Check a single NFTA_LIST_ELEM payload for a jump to `target`.
fn expr_elem_is_jump(elem: &[u8], target: &str) -> bool {
    let mut expr_name: Option<&str> = None;
    let mut is_jump = false;
    let mut jump_chain: Option<&[u8]> = None;

    let mut pos = 0usize;
    while pos + 4 <= elem.len() {
        let nla_len = u16::from_le_bytes(elem[pos..pos + 2].try_into().unwrap_or([0; 2])) as usize;
        let nla_type =
            u16::from_le_bytes(elem[pos + 2..pos + 4].try_into().unwrap_or([0; 2])) & !NLA_F_NESTED;
        if nla_len < 4 || pos + nla_len > elem.len() {
            break;
        }
        let data = &elem[pos + 4..pos + nla_len];
        match nla_type {
            t if t == NFTA_EXPR_NAME => {
                expr_name = std::str::from_utf8(data)
                    .ok()
                    .map(|s| s.trim_end_matches('\0'));
            }
            t if t == NFTA_EXPR_DATA => {
                if matches!(expr_name, Some("immediate")) {
                    // Parse NFTA_EXPR_DATA of immediate for verdict
                    parse_immediate_for_jump(data, &mut is_jump, &mut jump_chain);
                }
            }
            _ => {}
        }
        pos += align4(nla_len);
    }

    if is_jump {
        if let Some(chain_bytes) = jump_chain {
            if let Ok(s) = std::str::from_utf8(chain_bytes) {
                return s.trim_end_matches('\0') == target;
            }
        }
    }
    false
}

fn parse_immediate_for_jump<'a>(data: &'a [u8], is_jump: &mut bool, chain: &mut Option<&'a [u8]>) {
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let nla_len = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap_or([0; 2])) as usize;
        let nla_type =
            u16::from_le_bytes(data[pos + 2..pos + 4].try_into().unwrap_or([0; 2])) & !NLA_F_NESTED;
        if nla_len < 4 || pos + nla_len > data.len() {
            break;
        }
        let d = &data[pos + 4..pos + nla_len];
        if nla_type == NFTA_IMMEDIATE_DATA {
            // data is NFTA_DATA_VERDICT nested
            parse_verdict_data(d, is_jump, chain);
        }
        pos += align4(nla_len);
    }
}

fn parse_verdict_data<'a>(data: &'a [u8], is_jump: &mut bool, chain: &mut Option<&'a [u8]>) {
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let nla_len = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap_or([0; 2])) as usize;
        let nla_type =
            u16::from_le_bytes(data[pos + 2..pos + 4].try_into().unwrap_or([0; 2])) & !NLA_F_NESTED;
        if nla_len < 4 || pos + nla_len > data.len() {
            break;
        }
        let d = &data[pos + 4..pos + nla_len];
        if nla_type == NFTA_DATA_VERDICT {
            parse_verdict_code(d, is_jump, chain);
        }
        pos += align4(nla_len);
    }
}

fn parse_verdict_code<'a>(data: &'a [u8], is_jump: &mut bool, chain: &mut Option<&'a [u8]>) {
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let nla_len = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap_or([0; 2])) as usize;
        let nla_type =
            u16::from_le_bytes(data[pos + 2..pos + 4].try_into().unwrap_or([0; 2])) & !NLA_F_NESTED;
        if nla_len < 4 || pos + nla_len > data.len() {
            break;
        }
        let d = &data[pos + 4..pos + nla_len];
        match nla_type {
            t if t == NFTA_VERDICT_CODE => {
                if d.len() >= 4 {
                    let code = u32::from_be_bytes(d[..4].try_into().unwrap_or([0; 4]));
                    if code == NFT_JUMP {
                        *is_jump = true;
                    }
                }
            }
            t if t == NFTA_VERDICT_CHAIN => {
                *chain = Some(d);
            }
            _ => {}
        }
        pos += align4(nla_len);
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align4() {
        assert_eq!(align4(0), 0);
        assert_eq!(align4(1), 4);
        assert_eq!(align4(4), 4);
        assert_eq!(align4(5), 8);
    }

    #[test]
    fn test_nla_put_str() {
        let mut buf = Vec::new();
        nla_put_str(&mut buf, 1, "nat");
        // len=8: 4 header + 4 data ("nat\0"), type=1
        assert_eq!(buf.len(), 8);
        assert_eq!(&buf[4..8], b"nat\0");
    }

    #[test]
    fn test_nla_put_u32_big_endian() {
        let mut buf = Vec::new();
        nla_put_u32(&mut buf, 1, 0x0102_0304);
        assert_eq!(&buf[4..8], &[0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn test_ipv4_prefix_mask() {
        assert_eq!(ipv4_prefix_mask(24), [255, 255, 255, 0]);
        assert_eq!(ipv4_prefix_mask(16), [255, 255, 0, 0]);
        assert_eq!(ipv4_prefix_mask(32), [255, 255, 255, 255]);
        assert_eq!(ipv4_prefix_mask(0), [0, 0, 0, 0]);
    }

    #[test]
    fn test_expr_payload_structure() {
        let e = expr_payload(1, NFT_PAYLOAD_NETWORK_HEADER, 12, 4);
        // Should contain "payload\0" string
        assert!(e.windows(8).any(|w| w == b"payload\0"));
    }

    #[test]
    fn test_expr_masq_structure() {
        let e = expr_masq();
        assert!(e.windows(5).any(|w| w == b"masq\0"));
    }

    #[test]
    fn test_msg_add_table_length() {
        let mut buf = Vec::new();
        msg_add_table(&mut buf, NFPROTO_IPV4, "pelagos-test", 1);
        let msg_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        assert_eq!(msg_len, buf.len());
    }

    #[test]
    fn test_batch_msg_has_correct_structure() {
        let mut ops = Vec::new();
        msg_add_table(&mut ops, NFPROTO_IPV4, "pelagos-test", 1);
        let mut batch = Vec::new();
        push_batch_ctrl(&mut batch, NFNL_MSG_BATCH_BEGIN, 0);
        batch.extend_from_slice(&ops);
        push_batch_ctrl(&mut batch, NFNL_MSG_BATCH_END, 2);
        // batch begin type
        let begin_type = u16::from_le_bytes(batch[4..6].try_into().unwrap());
        assert_eq!(begin_type, NFNL_MSG_BATCH_BEGIN);
    }
}
