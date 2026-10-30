#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Send authenticated commands to the remote switch.

Messages are one line of text, "<command> <counter> <tag>", where <tag> is the
first 8 bytes (16 hex digits) of HMAC-SHA256(key, "rs1 req <command> <counter>").
Replies use the same format with the "rs1 rsp " domain prefix. A request and
its reply cost about 120 bytes of cellular data, including IPv4 and UDP headers.

The key is read from the Kconfig fragment the firmware is built with
(CONFIG_REMOTE_SWITCH_KEY). Uses only the Python standard library.

Examples:
  tools/rsctl.py keygen > remote_switch.local.conf      # once, before building
  tools/rsctl.py send on --host 10.0.0.1               # over the 1NCE OpenVPN tunnel
  tools/rsctl.py payload status                         # paste into a shell or the 1NCE API
"""

import argparse
import hashlib
import hmac
import pathlib
import re
import secrets
import socket
import sys
import time

DEFAULT_CONFIG = pathlib.Path(__file__).resolve().parent.parent / "remote_switch.local.conf"
DEFAULT_PORT = 4242
COMMANDS = ("on", "off", "status")
REPLY_STATUSES = ("on", "off", "stale", "unavailable", "busy")
REQUEST_DOMAIN = b"rs1 req "
REPLY_DOMAIN = b"rs1 rsp "
TAG_HEX_LEN = 16
KEY_RE = re.compile(r'^\s*CONFIG_REMOTE_SWITCH_KEY\s*=\s*"([0-9a-fA-F]{64})"\s*$', re.M)


def load_key(path: pathlib.Path) -> bytes:
    try:
        text = path.read_text()
    except OSError as e:
        sys.exit(f"error: cannot read key file {path}: {e.strerror}\n"
                 f"hint: create one with `{sys.argv[0]} keygen > {path}`")
    match = KEY_RE.search(text)
    if not match:
        sys.exit(f'error: {path} has no CONFIG_REMOTE_SWITCH_KEY="<64 hex digits>" line')
    return bytes.fromhex(match.group(1))


def tag(key: bytes, domain: bytes, word: str, counter: int) -> str:
    body = f"{word} {counter}".encode()
    return hmac.new(key, domain + body, hashlib.sha256).hexdigest()[:TAG_HEX_LEN]


def make_request(key: bytes, command: str, counter: int) -> str:
    return f"{command} {counter} {tag(key, REQUEST_DOMAIN, command, counter)}"


def parse_reply(key: bytes, text: str):
    """Returns (status, counter) for an authentic reply, or None."""
    parts = text.strip().split(" ")
    if len(parts) != 3 or parts[0] not in REPLY_STATUSES or not parts[1].isdigit():
        return None
    status, counter = parts[0], int(parts[1])
    if not hmac.compare_digest(parts[2].lower(), tag(key, REPLY_DOMAIN, status, counter)):
        return None
    return status, counter


def new_counter(minimum: int = 0) -> int:
    """Milliseconds since the Unix epoch, which increases across runs."""
    return max(time.time_ns() // 1_000_000, minimum)


def cmd_keygen(_args) -> None:
    print("# Remote switch key. Keep this file private and out of version control.")
    print(f'CONFIG_REMOTE_SWITCH_KEY="{secrets.token_hex(32)}"')


def cmd_payload(args) -> None:
    key = load_key(args.config)
    print(make_request(key, args.command, new_counter()))


def exchange(sock, key, request: str, counter: int, args):
    """Sends `request` until an authentic reply for `counter` arrives (or retries run out)."""
    for attempt in range(1, args.retries + 1):
        sock.sendto(request.encode(), (args.host, args.port))
        deadline = time.monotonic() + args.timeout
        while (remaining := deadline - time.monotonic()) > 0:
            sock.settimeout(remaining)
            try:
                data, _ = sock.recvfrom(256)
            except socket.timeout:
                break
            reply = parse_reply(key, data.decode(errors="replace"))
            if reply is None:
                print(f"ignoring unauthenticated reply: {data!r}", file=sys.stderr)
            elif reply[0] == "stale" or reply[1] == counter:
                return reply
        print(f"no reply (attempt {attempt}/{args.retries})", file=sys.stderr)
    return None


def cmd_send(args) -> int:
    key = load_key(args.config)
    counter = new_counter()
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        reply = exchange(sock, key, make_request(key, args.command, counter), counter, args)
        if reply and reply[0] == "stale":
            # The device has seen a newer counter (e.g. clock skew); retry just above it.
            counter = new_counter(reply[1] + 1)
            reply = exchange(sock, key, make_request(key, args.command, counter), counter, args)
    if reply is None:
        print("error: no authentic reply; check the host/port, VPN tunnel, and key",
              file=sys.stderr)
        return 1
    print(reply[0])
    return 0 if reply[0] in ("on", "off") else 2


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=pathlib.Path, default=DEFAULT_CONFIG,
                        help="Kconfig fragment holding CONFIG_REMOTE_SWITCH_KEY "
                             "(default: %(default)s)")
    sub = parser.add_subparsers(dest="action", required=True)

    sub.add_parser("keygen", help="print a new random key as a Kconfig fragment")

    payload = sub.add_parser("payload", help="print a signed request without sending it")
    payload.add_argument("command", choices=COMMANDS)

    send = sub.add_parser("send", help="send a request over UDP and print the reply")
    send.add_argument("command", choices=COMMANDS)
    send.add_argument("--host", required=True, help="device IP address (e.g. from the 1NCE portal)")
    send.add_argument("--port", type=int, default=DEFAULT_PORT)
    send.add_argument("--timeout", type=float, default=10.0, help="seconds per attempt")
    send.add_argument("--retries", type=int, default=3)

    args = parser.parse_args()
    if args.action == "keygen":
        cmd_keygen(args)
    elif args.action == "payload":
        cmd_payload(args)
    else:
        return cmd_send(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())

