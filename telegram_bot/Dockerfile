# SPDX-License-Identifier: MIT

# Stage 1: Minimal OpenVPN client image for carrier private network tunnels (e.g. 1NCE).
FROM alpine:3.21 AS openvpn
RUN apk add --no-cache iproute2 openvpn

# Stage 2: Stdlib-only Python Telegram bot running as an unprivileged user.
FROM python:3.12-slim AS rsbot
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1
RUN groupadd --gid 10001 rsbot && \
    useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin rsbot
WORKDIR /app
COPY tools/rsctl.py tools/rsbot.py /app/
RUN chmod 0555 /app/rsctl.py /app/rsbot.py
USER rsbot:rsbot
ENTRYPOINT ["python3", "/app/rsbot.py"]
