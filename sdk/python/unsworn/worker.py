"""The Unsworn worker for Python handlers: ``python3 -u -m unsworn.worker``."""

from __future__ import annotations

import asyncio
import importlib.util
import inspect
import os
import sys
import traceback
from typing import Any, Callable

from . import PROTOCOL_VERSION, Request, __version__
from .protocol import FramedCodec, JsonlCodec

ROOT = os.environ.get("UNSWORN_ROOT", os.getcwd())
TRANSPORT = os.environ.get("UNSWORN_TRANSPORT", "stdio")
MAX_IN_FLIGHT = int(os.environ.get("UNSWORN_MAX_IN_FLIGHT", "32"))
DEV = os.environ.get("UNSWORN_DEV") == "1"

_modules: dict[str, Any] = {}


def _split(ref: str) -> tuple[str, str | None]:
    file, _, name = ref.partition("#")
    return file, (name or None)


def _load(file: str) -> Any:
    if file in _modules:
        return _modules[file]
    path = file if os.path.isabs(file) else os.path.join(ROOT, file)
    name = "unsworn_handler_" + str(abs(hash(path)))
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise ImportError(f"can't load {file}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    _modules[file] = module
    return module


def resolve(ref: str) -> Callable:
    file, name = _split(ref)
    module = _load(file)
    target = name or "handler"
    fn = getattr(module, target, None)
    if fn is None and name is None:
        fn = getattr(module, "default", None)
    if fn is None:
        raise LookupError(f'{file} has no function "{target}"')
    if not callable(fn):
        raise TypeError(f"{ref} isn't callable")
    return fn


class Worker:
    def __init__(self, codec: Any, write: Callable[[bytes], Any]) -> None:
        self.codec = codec
        self.write = write
        self.tasks: dict[str, asyncio.Task] = {}
        self.requests: dict[str, Request] = {}

    def send(self, message: dict, body: bytes | None = None) -> None:
        self.write(self.codec.encode(message, body))

    def dispatch(self, message: dict, raw: bytes | None) -> None:
        kind = message.get("type")
        if kind == "request":
            task = asyncio.ensure_future(self.handle(message, raw))
            self.tasks[message["id"]] = task
            task.add_done_callback(lambda _t, i=message["id"]: self._done(i))
        elif kind == "cancel":
            req = self.requests.get(message.get("id", ""))
            if req is not None:
                req.cancelled = True
            task = self.tasks.get(message.get("id", ""))
            if task is not None:
                task.cancel()
        elif kind == "verify":
            missing = []
            for ref in message.get("handlers", []):
                try:
                    resolve(ref)
                except Exception as e:  # noqa: BLE001 - reported back to Unsworn
                    missing.append({"handler": ref, "reason": str(e)})
            self.send({"type": "verified", "id": message["id"], "missing": missing})

    def _done(self, rid: str) -> None:
        self.tasks.pop(rid, None)
        self.requests.pop(rid, None)

    async def handle(self, msg: dict, raw: bytes | None) -> None:
        rid = msg["id"]
        req = Request(
            method=msg.get("method", ""),
            path=msg.get("path", ""),
            route=msg.get("route", ""),
            handler=msg.get("handler", ""),
            phase=msg.get("phase", "handle"),
            params=msg.get("params") or {},
            query=msg.get("query") or {},
            raw_query=msg.get("raw_query"),
            headers=msg.get("headers") or {},
            json=msg.get("json"),
            text=msg.get("text"),
            body=raw,
        )
        self.requests[rid] = req
        ctx = msg.get("context") or {}
        try:
            fn = resolve(req.handler)
            if inspect.iscoroutinefunction(fn):
                result = await fn(req, ctx)
            else:
                loop = asyncio.get_running_loop()
                result = await loop.run_in_executor(None, fn, req, ctx)
                if inspect.isawaitable(result):
                    result = await result
            await self.reply(rid, result, req)
        except asyncio.CancelledError:
            return
        except Exception as e:  # noqa: BLE001 - reported back to Unsworn
            if req.cancelled:
                return
            traceback.print_exc(file=sys.stderr)
            message = traceback.format_exc() if DEV else str(e)
            self.send({"type": "error", "id": rid, "message": message or type(e).__name__})

    async def reply(self, rid: str, result: Any, req: Request) -> None:
        if req.cancelled:
            return
        if result is None:
            self.send({"type": "response", "id": rid, "status": 204})
            return
        if not isinstance(result, dict):
            raise TypeError("a handler must return a reply, e.g. unsworn.json(...) or unsworn.text(...)")
        if "continue" in result:
            self.send({"type": "continue", "id": rid, "context": result["continue"] or {}})
            return
        unknown = set(result) - {"status", "headers", "json", "text", "body"}
        if unknown:
            raise TypeError(
                f"unexpected reply keys {sorted(unknown)}; wrap data in unsworn.json(...)"
            )
        base: dict[str, Any] = {"type": "response", "id": rid, "status": result.get("status", 200)}
        if result.get("headers"):
            base["headers"] = result["headers"]
        if "json" in result:
            self.send({**base, "json": result["json"]})
        elif "text" in result:
            self.send({**base, "text": str(result["text"])})
        elif "body" in result:
            payload = result["body"]
            if isinstance(payload, (bytes, bytearray, memoryview)):
                self.send(base, bytes(payload))
            elif isinstance(payload, str):
                self.send(base, payload.encode())
            elif hasattr(payload, "__aiter__"):
                self.send({**base, "stream": True})
                async for chunk in payload:
                    if req.cancelled:
                        return
                    self._chunk(rid, chunk)
                self.send({"type": "end", "id": rid})
            elif hasattr(payload, "__iter__"):
                self.send({**base, "stream": True})
                loop = asyncio.get_running_loop()
                it = iter(payload)
                done = object()
                while True:
                    chunk = await loop.run_in_executor(None, next, it, done)
                    if chunk is done or req.cancelled:
                        break
                    self._chunk(rid, chunk)
                if not req.cancelled:
                    self.send({"type": "end", "id": rid})
            else:
                raise TypeError("`body` must be bytes, str, or an iterator of them")
        else:
            self.send(base)

    def _chunk(self, rid: str, chunk: Any) -> None:
        data = chunk.encode() if isinstance(chunk, str) else bytes(chunk)
        if data:
            self.send({"type": "chunk", "id": rid}, data)


def _ready() -> dict:
    return {
        "type": "ready",
        "protocol": PROTOCOL_VERSION,
        "max_in_flight": MAX_IN_FLIGHT,
        "verify": True,
        "sdk": f"unsworn-python/{__version__}",
    }


async def _run_stdio() -> None:
    out = sys.stdout.buffer
    # stdout carries protocol messages, so print() goes to stderr.
    sys.stdout = sys.stderr

    def write(data: bytes) -> None:
        out.write(data)
        out.flush()

    worker = Worker(JsonlCodec(), write)
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader()
    await loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin.buffer)
    worker.send(_ready())
    while True:
        data = await reader.read(65536)
        if not data:
            break
        for message, raw in worker.codec.push(data):
            worker.dispatch(message, raw)


async def _run_unix(path: str) -> None:
    reader, writer = await asyncio.open_unix_connection(path)
    worker = Worker(FramedCodec(), writer.write)
    worker.send(_ready())
    while True:
        data = await reader.read(65536)
        if not data:
            break
        for message, raw in worker.codec.push(data):
            worker.dispatch(message, raw)


def main() -> None:
    try:
        if TRANSPORT.startswith("unix:"):
            asyncio.run(_run_unix(TRANSPORT[len("unix:") :]))
        else:
            asyncio.run(_run_stdio())
    except KeyboardInterrupt:
        pass
    os._exit(0)


if __name__ == "__main__":
    main()
