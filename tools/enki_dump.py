#!/usr/bin/env python3
"""Receive the Enki Link camera stream and write the raw H.264 (Annex-B) to stdout or a file.

Test tool: it needs only the Python standard library. It gives the phone a fresh session token through adb
(`adb shell content call ...`, the only way the phone accepts one) and proves it on connect, like the desktop app.

    adb forward tcp:28200 tcp:28200          # phone connected by USB, Enki app started
    python3 tools/enki_dump.py | ffplay -f h264 -fflags nobuffer -flags low_delay -framedrop -i -
    python3 tools/enki_dump.py -o camera.h264 --seconds 10
"""
import argparse
import hashlib
import hmac
import os
import socket
import struct
import subprocess
import sys
import time

MAGIC = b"ENKI"
VERSION = 1
CH_MEDIA = 0
PROVISION_URI = "content://io.github.oopnm.enki.provision"
AUTH_CONTEXT = b"ENKI-AUTH-1"
AUTH_STATUS = {1: "the phone rejected the session token", 2: "the phone has no session token (provisioning failed?)"}
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


def provision(token, adb="adb"):
    """Hand the phone the session token. Only the adb shell may do this."""
    out = subprocess.run(
        [adb, "shell", "content", "call", "--uri", PROVISION_URI, "--method", "provision", "--arg", token.hex()],
        capture_output=True, text=True,
    )
    text = out.stdout + out.stderr
    if out.returncode != 0 or "ok=true" not in text:
        raise SystemExit(f"could not give the phone a session token (is the Enki app from the same version installed?): {text.strip()}")


def handshake(sock, token):
    sock.sendall(MAGIC + bytes([VERSION, CH_MEDIA, 0, 0]))
    reply = read_exact(sock, 8)
    if reply[:4] != MAGIC or reply[5] != CH_MEDIA:
        raise ValueError(f"unexpected preamble from phone: {reply.hex()}")
    if reply[4] != VERSION:
        raise ValueError(f"the phone app speaks protocol version {reply[4]}, this tool speaks {VERSION}")
    nonce = read_exact(sock, 16)
    sock.sendall(hmac.new(token, AUTH_CONTEXT + bytes([CH_MEDIA]) + nonce, hashlib.sha256).digest())
    status = read_exact(sock, 1)[0]
    if status != 0:
        raise ValueError(AUTH_STATUS.get(status, f"authentication failed ({status})"))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=28200)
    ap.add_argument("-o", "--output", help="write to this file instead of stdout")
    ap.add_argument("--seconds", type=float, help="stop after this many seconds")
    ap.add_argument("-v", "--verbose", action="store_true", help="log every frame to stderr")
    ap.add_argument("--adb", default="adb", help="adb program used to give the phone the session token")
    ap.add_argument("--token", help="use this 64-digit hex token instead of provisioning a new one (the phone must already have it)")
    args = ap.parse_args()
    token = bytes.fromhex(args.token) if args.token else os.urandom(32)
    if not args.token:
        provision(token, args.adb)

    out = open(args.output, "wb") if args.output else sys.stdout.buffer
    sock = socket.create_connection((args.host, args.port), timeout=5)
    sock.settimeout(10)
    deadline = time.monotonic() + args.seconds if args.seconds else None
    frames = key_frames = total = 0
    try:
        handshake(sock, token)
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
    except (EOFError, BrokenPipeError, ConnectionResetError, ValueError) as e:
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
