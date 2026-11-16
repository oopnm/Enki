#!/usr/bin/env python3
"""Receive the Enki Link camera stream and write the raw H.264 (Annex-B) to stdout or a file.

Phase 1 test tool: it needs only the Python standard library.

    adb forward tcp:28200 tcp:28200          # phone connected by USB, Enki app started
    python3 tools/enki_dump.py | ffplay -f h264 -fflags nobuffer -flags low_delay -framedrop -i -
    python3 tools/enki_dump.py -o camera.h264 --seconds 10
"""
import argparse
import socket
import struct
import sys
import time

MAGIC = b"ENKI"
VERSION = 0
CH_MEDIA = 0
KIND_NAMES = {0: "config", 1: "key", 2: "delta", 3: "audio-config", 4: "audio"}
VIDEO_KINDS = (0, 1, 2)


def read_exact(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError("connection closed")
        buf += chunk
    return bytes(buf)


def parse_header(b):
    stream, kind, flags, reserved, length, pts = struct.unpack(">BBBBIQ", b)
    if reserved != 0:
        raise ValueError("reserved byte must be zero")
    if length > 16 * 1024 * 1024:
        raise ValueError(f"payload too large: {length}")
    return stream, kind, flags, length, pts


def handshake(sock):
    sock.sendall(MAGIC + bytes([VERSION, CH_MEDIA, 0, 0]))
    reply = read_exact(sock, 8)
    if reply[:4] != MAGIC or reply[4] != VERSION or reply[5] != CH_MEDIA:
        raise ValueError(f"unexpected preamble from phone: {reply.hex()}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=28200)
    ap.add_argument("-o", "--output", help="write to this file instead of stdout")
    ap.add_argument("--seconds", type=float, help="stop after this many seconds")
    ap.add_argument("-v", "--verbose", action="store_true", help="log every frame to stderr")
    args = ap.parse_args()

    out = open(args.output, "wb") if args.output else sys.stdout.buffer
    sock = socket.create_connection((args.host, args.port), timeout=5)
    sock.settimeout(10)
    deadline = time.monotonic() + args.seconds if args.seconds else None
    frames = key_frames = total = 0
    try:
        handshake(sock)
        print("connected, receiving camera stream", file=sys.stderr)
        while deadline is None or time.monotonic() < deadline:
            stream, kind, flags, length, pts = parse_header(read_exact(sock, 16))
            payload = read_exact(sock, length)
            if args.verbose:
                print(f"stream={stream} kind={KIND_NAMES.get(kind, kind)} flags={flags} len={length} pts={pts}us", file=sys.stderr)
            if stream == 0 and kind in VIDEO_KINDS:
                out.write(payload)
                out.flush()
                frames += 1
                key_frames += kind == 1
                total += length
    except (EOFError, BrokenPipeError, ConnectionResetError) as e:
        print(f"stopped: {e}", file=sys.stderr)
    except socket.timeout:
        print("stopped: no data from the phone for 10 s", file=sys.stderr)
    finally:
        sock.close()
        if args.output:
            out.close()
        print(f"{frames} video frames ({key_frames} key), {total / 1024:.0f} KiB", file=sys.stderr)


if __name__ == "__main__":
    main()
