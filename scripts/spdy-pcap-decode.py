#!/usr/bin/env python3
"""Minimal SPDY/3.1 frame decoder for a pcap of a single loopback exec stream.
Reassembles one TCP direction and walks SPDY frames (DATA + control). Header
blocks of SYN_STREAM/REPLY are zlib-compressed and left undecoded — we only
care about DATA (stdout/stdin/error) and GOAWAY/RST_STREAM control frames.

Usage: spdy-pcap-decode.py <pcap> <server_port>
Prints server->client and client->server frames in capture order.
"""
import struct, sys, zlib

CTRL = {1:"SYN_STREAM",2:"SYN_REPLY",3:"RST_STREAM",4:"SETTINGS",
        6:"PING",7:"GOAWAY",8:"HEADERS",9:"WINDOW_UPDATE"}

def frames(buf):
    i = 0
    while i + 8 <= len(buf):
        first = buf[i]
        if first & 0x80:  # control
            ver = struct.unpack(">H", buf[i:i+2])[0] & 0x7fff
            typ = struct.unpack(">H", buf[i+2:i+4])[0]
            flags = buf[i+4]
            length = int.from_bytes(buf[i+5:i+8], "big")
            body = buf[i+8:i+8+length]
            yield ("CTRL", CTRL.get(typ, f"type{typ}"), flags, length, body, ver)
            i += 8 + length
        else:  # data
            sid = struct.unpack(">I", buf[i:i+4])[0] & 0x7fffffff
            flags = buf[i+4]
            length = int.from_bytes(buf[i+5:i+8], "big")
            data = buf[i+8:i+8+length]
            yield ("DATA", sid, flags, length, data, None)
            i += 8 + length

def reassemble(pcap_path, server_port):
    with open(pcap_path, "rb") as f:
        gh = f.read(24)
        magic, = struct.unpack("<I", gh[:4])
        le = "<" if magic in (0xa1b2c3d4, 0xa1b2cd34, 0xa1b23c4d) else ">"
        linktype = struct.unpack(le+"I", gh[20:24])[0]
        s2c, c2s = {}, {}  # seq->payload
        while True:
            rh = f.read(16)
            if len(rh) < 16: break
            _, _, caplen, _ = struct.unpack(le+"IIII", rh)
            pkt = f.read(caplen)
            # link layer
            if linktype == 1:       off = 14            # EN10MB
            elif linktype == 0:     off = 4             # NULL/loopback
            elif linktype == 113:   off = 16            # SLL
            else:                   off = 14
            ip = pkt[off:]
            if len(ip) < 20: continue
            ihl = (ip[0] & 0x0f) * 4
            proto = ip[9]
            if proto != 6: continue
            tcp = ip[ihl:]
            if len(tcp) < 20: continue
            sport, dport = struct.unpack(">HH", tcp[:4])
            seq = struct.unpack(">I", tcp[4:8])[0]
            doff = (tcp[12] >> 4) * 4
            payload = tcp[doff:]
            if not payload: continue
            if sport == server_port: s2c[seq] = payload
            elif dport == server_port: c2s[seq] = payload
        def cat(d):
            out = b""
            for seq in sorted(d):
                out += d[seq]
            return out
        return cat(s2c), cat(c2s)

def dump(label, buf):
    # Skip the HTTP upgrade preamble (request or 101 response) that precedes
    # the SPDY frames on each direction.
    sep = buf.find(b"\r\n\r\n")
    if sep != -1 and (buf[:20].upper().startswith(b"POST") or b"HTTP/1.1" in buf[:sep]):
        buf = buf[sep+4:]
    print(f"\n===== {label} ({len(buf)} bytes after HTTP preamble) =====")
    for kind, a, flags, length, body, ver in frames(buf):
        fin = " FIN" if (kind=="DATA" and flags & 0x01) else ""
        if kind == "DATA":
            preview = body[:32]
            print(f"  DATA stream={a} flags=0x{flags:02x}{fin} len={length} data={preview!r}")
        else:
            extra = ""
            if a == "GOAWAY" and len(body) >= 4:
                lg = struct.unpack(">I", body[:4])[0] & 0x7fffffff
                extra = f" last_good_stream_id={lg}"
            if a == "RST_STREAM" and len(body) >= 8:
                sid = struct.unpack(">I", body[:4])[0] & 0x7fffffff
                st = struct.unpack(">I", body[4:8])[0]
                extra = f" stream={sid} status={st}"
            print(f"  CTRL {a} flags=0x{flags:02x} len={length}{extra}")

if __name__ == "__main__":
    pcap, port = sys.argv[1], int(sys.argv[2])
    s2c, c2s = reassemble(pcap, port)
    dump("SERVER -> CLIENT", s2c)
    dump("CLIENT -> SERVER", c2s)
