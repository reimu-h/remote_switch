#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Minimal Telegram bot for controlling the remote switch over UDP."""

from datetime import datetime, timezone
import json
import os
import pathlib
import re
import signal
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import rsctl  # noqa: E402

API_BASE = "https://api.telegram.org"
MAX_COMMAND_AGE_SEC = 60
MAX_POLL_FAILURES = 20
KEYBOARD = {
    "inline_keyboard": [
        [
            {"text": "\U0001f7e2 Turn ON", "callback_data": "on"},
            {"text": "\U0001f534 Turn OFF", "callback_data": "off"},
        ],
        [{"text": "\U0001f504 Refresh Status", "callback_data": "status"}],
    ]
}
STATUS_LABELS = {
    "on": "\U0001f7e2 ON",
    "off": "\U0001f534 OFF",
    "unavailable": "\u26a0\ufe0f Hardware unavailable (forced OFF)",
    "busy": "\u23f3 Busy (queue full, retry shortly)",
    "stale": "\u26a0\ufe0f Counter sync failed (stale)",
}


def tg_call(token: str, method: str, *, urlopen=urllib.request.urlopen, **params):
    body = json.dumps(params).encode()
    req = urllib.request.Request(
        f"{API_BASE}/bot{token}/{method}",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    http_timeout = float(params.get("timeout", 0)) + 15.0
    try:
        with urlopen(req, timeout=http_timeout) as resp:
            data = json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        raw = e.read(4096).decode(errors="replace")
        if "message is not modified" in raw or "query is too old" in raw:
            return None
        raise RuntimeError(f"{method} HTTP {e.code}: {raw.replace(token, '<REDACTED>')}") from None
    except (OSError, ValueError) as e:
        raise RuntimeError(f"{method} failed: {str(e).replace(token, '<REDACTED>')}") from None
    if not isinstance(data, dict) or not data.get("ok"):
        desc = str(data.get("description", "error") if isinstance(data, dict) else data)
        raise RuntimeError(f"{method}: {desc.replace(token, '<REDACTED>')}")
    return data.get("result")


def parse_command(text: str, bot_username: str) -> str | None:
    text = text.strip()
    if not text.startswith("/"):
        return None
    cmd, _, mention = text.split(None, 1)[0][1:].partition("@")
    if mention and bot_username and mention.lower() != bot_username.lower():
        return None
    cmd = cmd.lower()
    if cmd in ("start", "help"):
        return "help"
    return cmd if cmd in rsctl.COMMANDS else "unknown"


def format_status(reply: tuple[str, int] | None, elapsed: float) -> str:
    label = STATUS_LABELS.get(reply[0], reply[0]) if reply else "\u274c Unreachable (no reply)"
    ts = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")
    return f"Switch: {label}\nChecked: {ts} ({elapsed:.1f}s)"


def _is_authorized(user_id, chat: dict, cfg: dict) -> bool:
    if type(user_id) is not int or user_id not in cfg["allowed_users"]:
        return False
    chat_id = chat.get("id")
    if type(chat_id) is not int or chat_id == 0:
        return False
    allowed_chats = cfg.get("allowed_chats")
    if allowed_chats is not None:
        return chat_id in allowed_chats
    return chat_id == user_id and chat.get("type", "private") == "private"


def handle_update(update: dict, cfg: dict, state: dict, *, send_fn=rsctl.send_command,
                  tg_fn=tg_call, now_fn=time.time) -> None:
    if "message" in update:
        msg = update.get("message") or {}
        user_id = (msg.get("from") or {}).get("id")
        chat = msg.get("chat") or {}
        chat_id = chat.get("id")
        if not _is_authorized(user_id, chat, cfg):
            print(f"ignored unauthorized message from user_id={user_id} chat_id={chat_id}",
                  file=sys.stderr)
            return
        action = parse_command(msg.get("text") or "", state.get("username", ""))
        if action is None:
            return
        if action == "help":
            tg_fn(cfg["token"], "sendMessage", chat_id=chat_id,
                  text="Commands: /status, /on, /off", reply_markup=KEYBOARD)
            return
        if action == "unknown":
            tg_fn(cfg["token"], "sendMessage", chat_id=chat_id,
                  text="Unknown command. Use /status, /on, or /off.", reply_markup=KEYBOARD)
            return
        msg_date = msg.get("date")
        if action in ("on", "off") and (
            type(msg_date) is not int or now_fn() - msg_date > MAX_COMMAND_AGE_SEC
        ):
            tg_fn(cfg["token"], "sendMessage", chat_id=chat_id,
                  text=f"Ignored delayed /{action} command. Try again.", reply_markup=KEYBOARD)
            return
        sent = tg_fn(cfg["token"], "sendMessage", chat_id=chat_id, text=f"Sending {action}...")
        msg_id = sent.get("message_id") if isinstance(sent, dict) else None
    elif "callback_query" in update:
        cb = update.get("callback_query") or {}
        user_id = (cb.get("from") or {}).get("id")
        msg = cb.get("message") or {}
        chat = msg.get("chat") or {}
        chat_id = chat.get("id")
        msg_id = msg.get("message_id")
        if not _is_authorized(user_id, chat, cfg):
            print(f"ignored unauthorized button tap from user_id={user_id} chat_id={chat_id}",
                  file=sys.stderr)
            return
        action = cb.get("data") or ""
        if action not in rsctl.COMMANDS or "id" not in cb:
            return
        answered = tg_fn(cfg["token"], "answerCallbackQuery", callback_query_id=cb["id"],
                         text=f"Sending {action}...")
        if answered is None and action in ("on", "off"):
            tg_fn(cfg["token"], "sendMessage", chat_id=chat_id,
                  text=f"Ignored delayed /{action} button tap. Try again.", reply_markup=KEYBOARD)
            return
    else:
        return

    t0 = time.monotonic()
    try:
        reply = send_fn(cfg["key"], action, cfg["host"], cfg["port"], cfg["timeout"],
                        cfg["retries"], state["min_counter"])
        if reply and reply[0] != "busy" and reply[1] < rsctl.MAX_COUNTER:
            state["min_counter"] = max(state["min_counter"], reply[1] + 1)
        text = format_status(reply, time.monotonic() - t0)
    except (OSError, ValueError) as e:
        detail = e.strerror if isinstance(e, OSError) and e.strerror else e
        text = f"Switch: \u274c UDP error ({detail})"

    if msg_id:
        try:
            tg_fn(cfg["token"], "editMessageText", chat_id=chat_id, message_id=msg_id,
                  text=text, reply_markup=KEYBOARD)
            return
        except RuntimeError as e:
            if "message to edit not found" not in str(e) and "message can't be edited" not in str(e):
                raise
    tg_fn(cfg["token"], "sendMessage", chat_id=chat_id, text=text, reply_markup=KEYBOARD)


def load_config() -> dict:
    token = os.environ.get("TELEGRAM_BOT_TOKEN", "").strip()
    if not token:
        sys.exit("error: TELEGRAM_BOT_TOKEN is required")
    try:
        users = {
            int(x) for x in re.split(r"[\s,]+", os.environ.get("TELEGRAM_ALLOWED_USERS", "").strip())
            if x
        }
    except ValueError as e:
        sys.exit(f"error: invalid TELEGRAM_ALLOWED_USERS: {e}")
    if not users or any(u <= 0 for u in users):
        sys.exit("error: TELEGRAM_ALLOWED_USERS must list one or more positive user IDs")
    raw_chats = os.environ.get("TELEGRAM_ALLOWED_CHATS", "").strip()
    allowed_chats = None
    if raw_chats:
        try:
            allowed_chats = {int(x) for x in re.split(r"[\s,]+", raw_chats) if x}
        except ValueError as e:
            sys.exit(f"error: invalid TELEGRAM_ALLOWED_CHATS: {e}")
        if not allowed_chats or 0 in allowed_chats:
            sys.exit("error: TELEGRAM_ALLOWED_CHATS must list non-zero chat IDs")
    host = os.environ.get("RS_HOST", "").strip()
    if not host:
        sys.exit("error: RS_HOST is required")
    raw_key = os.environ.get("RS_KEY", "").strip()
    key_file = pathlib.Path(os.environ.get("RS_KEY_FILE", "").strip() or rsctl.DEFAULT_CONFIG)
    try:
        key = rsctl.parse_key_text(raw_key) if raw_key else rsctl.load_key(key_file)
        port = rsctl.parse_port(os.environ.get("RS_PORT", "").strip() or str(rsctl.DEFAULT_PORT))
        timeout = rsctl.parse_timeout(os.environ.get("RS_TIMEOUT", "").strip() or "10.0")
        retries = rsctl.parse_retries(os.environ.get("RS_RETRIES", "").strip() or "3")
    except Exception as e:
        sys.exit(f"error: {e}")
    return {
        "token": token,
        "allowed_users": users,
        "allowed_chats": allowed_chats,
        "host": host,
        "port": port,
        "timeout": timeout,
        "retries": retries,
        "key": key,
    }


def main() -> int:
    cfg = load_config()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))

    me = tg_call(cfg["token"], "getMe") or {}
    state = {"username": me.get("username", ""), "min_counter": 1}
    tg_call(cfg["token"], "setMyCommands", commands=[
        {"command": "status", "description": "Query switch state"},
        {"command": "on", "description": "Turn switch ON"},
        {"command": "off", "description": "Turn switch OFF"},
    ])

    # Drop any queued offline updates so old taps never actuate the switch on startup.
    queued = tg_call(cfg["token"], "getUpdates", offset=-1, timeout=0) or []
    offset = queued[-1]["update_id"] + 1 if queued else None
    print(f"listening as @{state['username']} -> {cfg['host']}:{cfg['port']}/udp", file=sys.stderr)

    backoff = 1.0
    failures = 0
    while True:
        try:
            updates = tg_call(cfg["token"], "getUpdates", offset=offset, timeout=25,
                              allowed_updates=["message", "callback_query"]) or []
            backoff = 1.0
            failures = 0
        except KeyboardInterrupt:
            return 0
        except RuntimeError as e:
            failures += 1
            if failures >= MAX_POLL_FAILURES:
                print(f"poll error ({failures}/{MAX_POLL_FAILURES}): {e}; exiting to restart",
                      file=sys.stderr)
                return 1
            print(f"poll error: {e}; retrying in {backoff:.0f}s", file=sys.stderr)
            time.sleep(backoff)
            backoff = min(backoff * 2.0, 30.0)
            continue
        for upd in updates:
            offset = upd["update_id"] + 1
            try:
                handle_update(upd, cfg, state)
            except RuntimeError as e:
                print(f"telegram error: {e}", file=sys.stderr)


if __name__ == "__main__":
    sys.exit(main())
