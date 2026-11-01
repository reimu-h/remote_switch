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
  tools/rsctl.py keygen -o remote_switch.local.conf   # once, before building
  tools/rsctl.py send on --host 10.0.0.1              # over the 1NCE OpenVPN tunnel
  tools/rsctl.py payload status                        # paste into a shell or the 1NCE API
"""

import argparse
import hashlib
import hmac
import math
import os
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
MAX_COUNTER = (1 << 64) - 1
MAX_COUNTER_DIGITS = 20
HEX_DIGITS = frozenset("0123456789abcdefABCDEF")
KEY_RE = re.compile(r'^\s*CONFIG_REMOTE_SWITCH_KEY\s*=\s*"([0-9a-fA-F]{64})"\s*$', re.M)
RAW_KEY_RE = re.compile(r"^[0-9a-fA-F]{64}$")


def parse_key_text(text: str) -> bytes:
    match = KEY_RE.search(text)
    if match:
        return bytes.fromhex(match.group(1))
    stripped = text.strip().strip("\"'")
    if RAW_KEY_RE.fullmatch(stripped):
        return bytes.fromhex(stripped)
    raise ValueError('expected CONFIG_REMOTE_SWITCH_KEY="<64 hex digits>" or 64 hex digits')


def load_key(path: pathlib.Path) -> bytes:
    try:
        text = path.read_text()
    except OSError as e:
        sys.exit(f"error: cannot read key file {path}: {e.strerror}\n"
                 f"hint: create one with `{sys.argv[0]} keygen -o {path}`")
    match = KEY_RE.search(text)
    if not match:
        sys.exit(f'error: {path} has no CONFIG_REMOTE_SWITCH_KEY="<64 hex digits>" line')
    return bytes.fromhex(match.group(1))


def tag(key: bytes, domain: bytes, word: str, counter: int) -> str:
    body = f"{word} {counter}".encode()
    return hmac.new(key, domain + body, hashlib.sha256).hexdigest()[:TAG_HEX_LEN]


def make_request(key: bytes, command: str, counter: int) -> str:
    return f"{command} {counter} {tag(key, REQUEST_DOMAIN, command, counter)}"


def parse_counter_token(token: str) -> int | None:
    """Parses a canonical decimal u64 counter in 1..=2**64-1 (no leading zeros)."""
    if (not token
            or len(token) > MAX_COUNTER_DIGITS
            or token[0] == "0"
            or not token.isascii()
            or not token.isdigit()):
        return None
    value = int(token)
    return value if 1 <= value <= MAX_COUNTER else None


def parse_reply(key: bytes, text: str):
    """Returns (status, counter) for an authentic reply, or None."""
    if not text.isascii():
        return None
    parts = text.strip().split(" ")
    if len(parts) != 3 or parts[0] not in REPLY_STATUSES:
        return None
    counter = parse_counter_token(parts[1])
    if counter is None:
        return None
    reply_tag = parts[2]
    if len(reply_tag) != TAG_HEX_LEN or any(c not in HEX_DIGITS for c in reply_tag):
        return None
    if not hmac.compare_digest(reply_tag.lower(), tag(key, REPLY_DOMAIN, parts[0], counter)):
        return None
    return parts[0], counter


def new_counter(minimum: int = 1) -> int:
    """Milliseconds since the Unix epoch, which increases across runs."""
    counter = max(time.time_ns() // 1_000_000, minimum, 1)
    if counter > MAX_COUNTER:
        raise ValueError(f"counter {counter} exceeds u64 maximum ({MAX_COUNTER})")
    return counter


def parse_port(raw: str) -> int:
    try:
        port = int(raw)
    except ValueError as e:
        raise argparse.ArgumentTypeError(f"invalid int value: {raw!r}") from e
    if not 1 <= port <= 65535:
        raise argparse.ArgumentTypeError("port must be in 1..65535")
    return port


def parse_timeout(raw: str) -> float:
    try:
        timeout = float(raw)
    except ValueError as e:
        raise argparse.ArgumentTypeError(f"invalid float value: {raw!r}") from e
    if not math.isfinite(timeout) or timeout <= 0:
        raise argparse.ArgumentTypeError("timeout must be a finite number > 0")
    return timeout


def parse_retries(raw: str) -> int:
    try:
        retries = int(raw)
    except ValueError as e:
        raise argparse.ArgumentTypeError(f"invalid int value: {raw!r}") from e
    if retries < 1:
        raise argparse.ArgumentTypeError("retries must be >= 1")
    return retries


def cmd_keygen(args) -> int:
    content = (
        "# Remote switch key. Keep this file private and out of version control.\n"
        f'CONFIG_REMOTE_SWITCH_KEY="{secrets.token_hex(32)}"\n'
    )
    if args.output is None:
        sys.stdout.write(content)
        return 0
    flags = os.O_WRONLY | os.O_CREAT | (os.O_TRUNC if args.force else os.O_EXCL)
    try:
        fd = os.open(args.output, flags, 0o600)
    except FileExistsError:
        print(f"error: {args.output} already exists (pass --force to overwrite)",
              file=sys.stderr)
        return 1
    except OSError as e:
        detail = e.strerror or str(e)
        print(f"error: cannot write {args.output}: {detail}", file=sys.stderr)
        return 1
    try:
        with os.fdopen(fd, "w", encoding="ascii") as f:
            if args.force and hasattr(os, "fchmod"):
                os.fchmod(f.fileno(), 0o600)
            f.write(content)
    except OSError as e:
        detail = e.strerror or str(e)
        print(f"error: failed writing {args.output}: {detail}", file=sys.stderr)
        return 1
    return 0


def cmd_payload(args) -> None:
    key = load_key(args.config)
    try:
        counter = new_counter()
    except ValueError as e:
        sys.exit(f"error: {e}")
    print(make_request(key, args.command, counter))


def exchange(sock, key, request: str, counter: int, host: str, port: int, timeout: float,
             retries: int):
    """Sends `request` until an authentic reply for `counter` arrives (or retries run out)."""
    for attempt in range(1, retries + 1):
        sock.sendto(request.encode(), (host, port))
        deadline = time.monotonic() + timeout
        while (remaining := deadline - time.monotonic()) > 0:
            sock.settimeout(remaining)
            try:
                data, _ = sock.recvfrom(256)
            except socket.timeout:
                break
            reply = parse_reply(key, data.decode("ascii", errors="replace"))
            if reply is None:
                print(f"ignoring unauthenticated reply: {data!r}", file=sys.stderr)
            elif (reply[0] == "stale" and reply[1] >= counter) or (
                reply[0] != "stale" and reply[1] == counter
            ):
                return reply
        print(f"no reply (attempt {attempt}/{retries})", file=sys.stderr)
    return None


def send_command(
    key: bytes,
    command: str,
    host: str,
    port: int = DEFAULT_PORT,
    timeout: float = 10.0,
    retries: int = 3,
    minimum: int = 1,
) -> tuple[str, int] | None:
    """Sends an authenticated command over UDP, resynchronizing once on `stale`."""
    counter = new_counter(minimum)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        reply = exchange(sock, key, make_request(key, command, counter), counter, host, port,
                         timeout, retries)
        if reply and reply[0] == "stale" and reply[1] < MAX_COUNTER:
            counter = new_counter(max(counter + 1, reply[1] + 1))
            reply = exchange(sock, key, make_request(key, command, counter), counter, host, port,
                             timeout, retries)
    return reply


def cmd_send(args) -> int:
    key = load_key(args.config)
    try:
        reply = send_command(key, args.command, args.host, args.port, args.timeout, args.retries)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    except OSError as e:
        detail = e.strerror or str(e)
        print(f"error: UDP communication with {args.host}:{args.port} failed: {detail}",
              file=sys.stderr)
        return 1
    if reply and reply[0] == "stale" and reply[1] >= MAX_COUNTER:
        print("error: device replay counter is exhausted (u64::MAX)", file=sys.stderr)
        return 2
    if reply is None:
        print("error: no authentic reply; check the host/port, VPN tunnel, and key",
              file=sys.stderr)
        return 1
    print(reply[0])
    if args.command == "status":
        return 0 if reply[0] in ("on", "off") else 2
    return 0 if reply[0] == args.command else 2


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=pathlib.Path, default=DEFAULT_CONFIG,
                        help="Kconfig fragment holding CONFIG_REMOTE_SWITCH_KEY "
                             "(default: %(default)s)")
    sub = parser.add_subparsers(dest="action", required=True)

    keygen = sub.add_parser("keygen", help="generate a new random key as a Kconfig fragment")
    keygen.add_argument(
        "-o",
        "--output",
        type=pathlib.Path,
        help="write key file with mode 0600 (refuses to overwrite without --force)",
    )
    keygen.add_argument(
        "-f",
        "--force",
        action="store_true",
        help="overwrite --output if it already exists",
    )

    payload = sub.add_parser("payload", help="print a signed request without sending it")
    payload.add_argument("command", choices=COMMANDS)

    send = sub.add_parser("send", help="send a request over UDP and print the reply")
    send.add_argument("command", choices=COMMANDS)
    send.add_argument("--host", required=True, help="device IP address (e.g. from the 1NCE portal)")
    send.add_argument("--port", type=parse_port, default=DEFAULT_PORT)
    send.add_argument("--timeout", type=parse_timeout, default=10.0, help="seconds per attempt")
    send.add_argument("--retries", type=parse_retries, default=3)

    args = parser.parse_args()
    if args.action == "keygen":
        return cmd_keygen(args)
    if args.action == "payload":
        cmd_payload(args)
        return 0
    return cmd_send(args)


if __name__ == "__main__":
    sys.exit(main())

