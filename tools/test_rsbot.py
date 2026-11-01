#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Unit tests for `tools/rsctl.py` and `tools/rsbot.py`."""

import argparse
import io
import os
import pathlib
import socket
import stat
import sys
import tempfile
import threading
import unittest
from unittest import mock
import urllib.error

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import rsbot  # noqa: E402
import rsctl  # noqa: E402

KEY_HEX = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
KEY = bytes.fromhex(KEY_HEX)
TOKEN = "123456:FakeSecretBotToken"


def make_reply(status: str, counter: int) -> bytes:
    return f"{status} {counter} {rsctl.tag(KEY, rsctl.REPLY_DOMAIN, status, counter)}".encode()


class BotAndClientTests(unittest.TestCase):
    def test_hmac_vectors_and_key_parsing(self) -> None:
        self.assertEqual(rsctl.make_request(KEY, "on", 1_761_840_000_123),
                         "on 1761840000123 16d82a013add92ef")
        self.assertEqual(rsctl.parse_reply(KEY, "on 1761840000123 50d3e76858638e40"),
                         ("on", 1_761_840_000_123))
        self.assertEqual(rsctl.parse_key_text(KEY_HEX), KEY)
        self.assertEqual(rsctl.parse_key_text(f'CONFIG_REMOTE_SWITCH_KEY="{KEY_HEX}"\n'), KEY)

    def test_send_command_retries_and_resyncs_stale(self) -> None:
        seen: list[int] = []
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as srv:
            srv.bind(("127.0.0.1", 0))
            port = srv.getsockname()[1]

            def responder() -> None:
                d1, _ = srv.recvfrom(256)
                seen.append(int(d1.decode().split()[1]))  # drop 1st packet
                d2, addr = srv.recvfrom(256)
                c2 = int(d2.decode().split()[1])
                seen.append(c2)
                srv.sendto(make_reply("stale", c2 + 1000), addr)
                d3, addr = srv.recvfrom(256)
                c3 = int(d3.decode().split()[1])
                seen.append(c3)
                srv.sendto(make_reply("on", c3), addr)

            t = threading.Thread(target=responder, daemon=True)
            t.start()
            reply = rsctl.send_command(KEY, "on", "127.0.0.1", port, timeout=0.1, retries=2)
            t.join(timeout=2.0)

        self.assertIsNotNone(reply)
        self.assertEqual(reply[0], "on")
        self.assertEqual(seen[0], seen[1])
        self.assertGreater(seen[2], seen[1] + 1000)

    def test_tg_call_redacts_token_and_ignores_not_modified(self) -> None:
        def fail_open(req, timeout=0):
            raise urllib.error.URLError(f"boom {req.full_url}")

        with self.assertRaises(RuntimeError) as ctx:
            rsbot.tg_call(TOKEN, "getMe", urlopen=fail_open)
        self.assertNotIn(TOKEN, str(ctx.exception))

        def not_modified(req, timeout=0):
            raise urllib.error.HTTPError(
                req.full_url, 400, "Bad Request", None,  # type: ignore[arg-type]
                io.BytesIO(b'{"ok":false,"description":"Bad Request: message is not modified"}'),
            )

        self.assertIsNone(rsbot.tg_call(TOKEN, "editMessageText", urlopen=not_modified))

        def bad_json(_req, timeout=0):
            return io.BytesIO(f"not-json {TOKEN}".encode())

        with self.assertRaises(RuntimeError) as bad_ctx:
            rsbot.tg_call(TOKEN, "getUpdates", urlopen=bad_json)
        self.assertNotIn(TOKEN, str(bad_ctx.exception))

    def test_authorization_stale_rejection_and_commands(self) -> None:
        calls: list[tuple[str, dict]] = []
        sent_cmds: list[str] = []

        def fake_tg(_token: str, method: str, **params):
            calls.append((method, params))
            return {"message_id": 42}

        def fake_send(_key, cmd, *_args):
            sent_cmds.append(cmd)
            return ("on", 500)

        cfg = {"token": TOKEN, "allowed_users": {10}, "host": "10.0.0.1", "port": 4242,
               "timeout": 1.0, "retries": 1, "key": KEY}
        state = {"username": "MyBot", "min_counter": 1}

        # Unauthorized user is dropped silently:
        rsbot.handle_update(
            {"message": {"date": 1000, "from": {"id": 99}, "chat": {"id": 99}, "text": "/on"}},
            cfg, state, send_fn=fake_send, tg_fn=fake_tg, now_fn=lambda: 1000,
        )
        self.assertEqual(sent_cmds, [])
        self.assertEqual(calls, [])

        # Stale /on (>60s old) does not actuate:
        rsbot.handle_update(
            {"message": {"date": 900, "from": {"id": 10}, "chat": {"id": 10}, "text": "/on"}},
            cfg, state, send_fn=fake_send, tg_fn=fake_tg, now_fn=lambda: 1000,
        )
        self.assertEqual(sent_cmds, [])
        self.assertIn("Ignored delayed /on", calls.pop()[1]["text"])

        # Authorized /on and inline button tap succeed and advance min_counter:
        rsbot.handle_update(
            {"message": {"date": 1000, "from": {"id": 10}, "chat": {"id": 10}, "text": "/on@mybot"}},
            cfg, state, send_fn=fake_send, tg_fn=fake_tg, now_fn=lambda: 1000,
        )
        rsbot.handle_update(
            {"callback_query": {"id": "cb1", "from": {"id": 10},
                                "message": {"message_id": 42, "chat": {"id": 10}}, "data": "status"}},
            cfg, state, send_fn=fake_send, tg_fn=fake_tg, now_fn=lambda: 1000,
        )
        self.assertEqual(sent_cmds, ["on", "status"])
        self.assertEqual(state["min_counter"], 501)

        # Group chats are rejected by default unless listed in allowed_chats:
        calls.clear()
        sent_cmds.clear()
        rsbot.handle_update(
            {"message": {"date": 1000, "from": {"id": 10},
                         "chat": {"id": -100555, "type": "supergroup"}, "text": "/on"}},
            cfg, state, send_fn=fake_send, tg_fn=fake_tg, now_fn=lambda: 1000,
        )
        self.assertEqual(sent_cmds, [])
        self.assertEqual(calls, [])

        group_cfg = {**cfg, "allowed_chats": {-100555}}
        rsbot.handle_update(
            {"message": {"date": 1000, "from": {"id": 10},
                         "chat": {"id": -100555, "type": "supergroup"}, "text": "/status"}},
            group_cfg, state, send_fn=fake_send, tg_fn=fake_tg, now_fn=lambda: 1000,
        )
        self.assertEqual(sent_cmds, ["status"])

        # Expired callback_query ("query is too old" -> None) blocks /on actuation but allows /status:
        calls.clear()
        sent_cmds.clear()

        def expired_cb_tg(_token: str, method: str, **params):
            calls.append((method, params))
            return None if method == "answerCallbackQuery" else {"message_id": 42}

        rsbot.handle_update(
            {"callback_query": {"id": "cb_old", "from": {"id": 10},
                                "message": {"message_id": 42, "chat": {"id": 10}}, "data": "on"}},
            cfg, state, send_fn=fake_send, tg_fn=expired_cb_tg, now_fn=lambda: 1000,
        )
        self.assertEqual(sent_cmds, [])
        self.assertIn("Ignored delayed /on button tap", calls[-1][1]["text"])

        rsbot.handle_update(
            {"callback_query": {"id": "cb_stat", "from": {"id": 10},
                                "message": {"message_id": 42, "chat": {"id": 10}}, "data": "status"}},
            cfg, state, send_fn=fake_send, tg_fn=expired_cb_tg, now_fn=lambda: 1000,
        )
        self.assertEqual(sent_cmds, ["status"])

        # Deleted intermediate message falls back from editMessageText to sendMessage:
        calls.clear()

        def deleted_msg_tg(_token: str, method: str, **params):
            calls.append((method, params))
            if method == "editMessageText":
                raise RuntimeError("editMessageText HTTP 400: Bad Request: message to edit not found")
            return {"message_id": 99}

        rsbot.handle_update(
            {"message": {"date": 1000, "from": {"id": 10}, "chat": {"id": 10}, "text": "/status"}},
            cfg, state, send_fn=fake_send, tg_fn=deleted_msg_tg, now_fn=lambda: 1000,
        )
        self.assertEqual([m for m, _ in calls], ["sendMessage", "editMessageText", "sendMessage"])

    def test_keygen_permissions_and_overwrite_guard(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            key_path = pathlib.Path(tmp) / "remote_switch.local.conf"
            rc = rsctl.cmd_keygen(argparse.Namespace(output=key_path, force=False))
            self.assertEqual(rc, 0)
            self.assertEqual(stat.S_IMODE(key_path.stat().st_mode), 0o600)
            first_key = rsctl.load_key(key_path)
            self.assertEqual(len(first_key), 32)

            with mock.patch("sys.stderr", new_callable=io.StringIO):
                self.assertEqual(
                    rsctl.cmd_keygen(argparse.Namespace(output=key_path, force=False)), 1
                )
            self.assertEqual(rsctl.load_key(key_path), first_key)

            os.chmod(key_path, 0o644)
            self.assertEqual(
                rsctl.cmd_keygen(argparse.Namespace(output=key_path, force=True)), 0
            )
            self.assertEqual(stat.S_IMODE(key_path.stat().st_mode), 0o600)

    def test_load_config_fails_closed_without_allowed_users(self) -> None:
        env = {"TELEGRAM_BOT_TOKEN": TOKEN, "RS_HOST": "10.0.0.1", "RS_KEY": KEY_HEX}
        with mock.patch.dict(os.environ, env, clear=True):
            with self.assertRaises(SystemExit):
                rsbot.load_config()


if __name__ == "__main__":
    unittest.main()
