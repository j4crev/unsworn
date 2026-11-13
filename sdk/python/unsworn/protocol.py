"""The Unsworn handler protocol encodings. See docs/protocol.md."""

from __future__ import annotations

import json
import struct
from typing import Any, Optional

Frame = tuple[dict, Optional[bytes]]


def _with_body_len(message: dict, body: bytes | None) -> dict:
    if body:
        return {**message, "body_len": len(body)}
    return message


class JsonlCodec:
    """Tier 1: one JSON message per line, optionally followed by raw bytes."""

    def __init__(self) -> None:
        self._buf = bytearray()
        self._pending: tuple[dict, int] | None = None

    def encode(self, message: dict, body: bytes | None = None) -> bytes:
        line = json.dumps(_with_body_len(message, body), separators=(",", ":")).encode() + b"\n"
        return line + body if body else line

    def push(self, data: bytes) -> list[Frame]:
        self._buf.extend(data)
        frames: list[Frame] = []
        while True:
            if self._pending is not None:
                message, length = self._pending
                if len(self._buf) < length:
                    break
                payload = bytes(self._buf[:length])
                del self._buf[:length]
                frames.append((message, payload))
                self._pending = None
                continue
            nl = self._buf.find(b"\n")
            if nl < 0:
                break
            line = bytes(self._buf[:nl]).strip()
            del self._buf[: nl + 1]
            if not line:
                continue
            try:
                message = json.loads(line)
            except ValueError:
                continue
            length = message.get("body_len") or 0
            if length > 0:
                self._pending = (message, length)
            else:
                frames.append((message, None))
        return frames


class FramedCodec:
    """Tier 2: [frame_len u32][json_len u32][json][raw body], big-endian."""

    def __init__(self) -> None:
        self._buf = bytearray()

    def encode(self, message: dict, body: bytes | None = None) -> bytes:
        payload = json.dumps(_with_body_len(message, body), separators=(",", ":")).encode()
        body = body or b""
        return struct.pack(">II", 4 + len(payload) + len(body), len(payload)) + payload + body

    def push(self, data: bytes) -> list[Frame]:
        self._buf.extend(data)
        frames: list[Frame] = []
        while len(self._buf) >= 4:
            (frame_len,) = struct.unpack(">I", self._buf[:4])
            if len(self._buf) < 4 + frame_len:
                break
            (json_len,) = struct.unpack(">I", self._buf[4:8])
            payload = bytes(self._buf[8 : 8 + json_len])
            raw = bytes(self._buf[8 + json_len : 4 + frame_len])
            del self._buf[: 4 + frame_len]
            try:
                message: Any = json.loads(payload)
            except ValueError:
                continue
            frames.append((message, raw or None))
        return frames
