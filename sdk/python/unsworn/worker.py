"""The Unsworn worker for Python handlers: ``python3 -u -m unsworn.worker``."""

from __future__ import annotations

import asyncio
import contextvars
import functools
import importlib.util
import inspect
import json
import logging
import os
import sys
import threading
import traceback
from typing import Any, Callable

from . import PROTOCOL_VERSION, Request, __version__
from .protocol import FramedCodec, JsonlCodec

ROOT = os.environ.get("UNSWORN_ROOT", os.getcwd())
TRANSPORT = os.environ.get("UNSWORN_TRANSPORT", "stdio")
MAX_IN_FLIGHT = int(os.environ.get("UNSWORN_MAX_IN_FLIGHT", "32"))
DEV = os.environ.get("UNSWORN_DEV") == "1"
# Set by Unsworn versions that accept `log` messages. Without it, logging
# keeps Python's defaults (stderr).
LOG_LEVEL = os.environ.get("UNSWORN_LOG_LEVEL")

_current_id: contextvars.ContextVar[str | None] = contextvars.ContextVar("unsworn_request_id", default=None)

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


_LEVELS = {"error": logging.ERROR, "warn": logging.WARNING, "info": logging.INFO, "debug": logging.DEBUG, "trace": 5}
# Attributes every LogRecord has; anything else came from `extra=`.
_RECORD_ATTRS = set(vars(logging.LogRecord("", 0, "", 0, "", None, None))) | {"message", "asctime", "taskName"}


def _level_name(levelno: int) -> str:
    if levelno >= logging.ERROR:
        return "error"
    if levelno >= logging.WARNING:
        return "warn"
    if levelno >= logging.INFO:
        return "info"
    if levelno >= logging.DEBUG:
        return "debug"
    return "trace"


class LogHandler(logging.Handler):
    """Sends ``logging`` records to Unsworn, tagged with the current request."""

    def __init__(self, worker: "Worker", loop: asyncio.AbstractEventLoop) -> None:
        super().__init__()
        self.worker = worker
        self.loop = loop
        self.loop_thread = threading.get_ident()

    def emit(self, record: logging.LogRecord) -> None:
        try:
            message = record.getMessage()
            if record.exc_info:
                message += "\n" + self.formatter_exception(record)
            fields = {k: v for k, v in vars(record).items() if k not in _RECORD_ATTRS}
            if record.name != "root":
                fields["logger"] = record.name
            out: dict[str, Any] = {"type": "log", "level": _level_name(record.levelno), "message": message}
            rid = _current_id.get()
            if rid is not None:
                out["id"] = rid
            if fields:
                out["fields"] = json.loads(json.dumps(fields, default=str))
            if threading.get_ident() == self.loop_thread:
                self.worker.send(out)
            else:
                self.loop.call_soon_threadsafe(self.worker.send, out)
        except Exception:  # noqa: BLE001 - logging must never raise
            self.handleError(record)

    @staticmethod
    def formatter_exception(record: logging.LogRecord) -> str:
        return logging.Formatter().formatException(record.exc_info)  # type: ignore[arg-type]


def _install_logging(worker: "Worker") -> None:
    if LOG_LEVEL is None:
        return
    root = logging.getLogger()
    root.addHandler(LogHandler(worker, asyncio.get_running_loop()))
    root.setLevel(_LEVELS.get(LOG_LEVEL, logging.CRITICAL + 1))


class BodyStream:
    """A streamed request body. Iterate it with ``async for`` or, in a sync
    handler, ``for``. Each chunk is acknowledged as the handler reads it."""

    def __init__(self, worker: "Worker", rid: str, loop: asyncio.AbstractEventLoop) -> None:
        self.worker = worker
        self.rid = rid
        self.loop = loop
        self.queue: asyncio.Queue[bytes | BaseException | None] = asyncio.Queue()

    def push(self, chunk: bytes) -> None:
        self.queue.put_nowait(chunk)

    def end(self, error: BaseException | None = None) -> None:
        self.queue.put_nowait(error)

    def __aiter__(self) -> "BodyStream":
        return self

    async def __anext__(self) -> bytes:
        item = await self.queue.get()
        if item is None:
            self.queue.put_nowait(None)  # stay finished
            raise StopAsyncIteration
        if isinstance(item, BaseException):
            self.queue.put_nowait(item)
            raise item
        self.worker.send({"type": "ack", "id": self.rid, "bytes": len(item)})
        return item

    def __iter__(self) -> "BodyStream":
        return self

    def __next__(self) -> bytes:
        try:
            return asyncio.run_coroutine_threadsafe(self.__anext__(), self.loop).result()
        except StopAsyncIteration:
            raise StopIteration from None


class Worker:
    def __init__(self, codec: Any, write: Callable[[bytes], Any]) -> None:
        self.codec = codec
        self.write = write
        self.tasks: dict[str, asyncio.Task] = {}
        self.requests: dict[str, Request] = {}
        self.bodies: dict[str, BodyStream] = {}

    def send(self, message: dict, body: bytes | None = None) -> None:
        self.write(self.codec.encode(message, body))

    def dispatch(self, message: dict, raw: bytes | None) -> None:
        kind = message.get("type")
        if kind == "request":
            if message.get("stream"):
                loop = asyncio.get_running_loop()
                self.bodies[message["id"]] = BodyStream(self, message["id"], loop)
            task = asyncio.ensure_future(self.handle(message, raw))
            self.tasks[message["id"]] = task
            task.add_done_callback(lambda _t, i=message["id"]: self._done(i))
        elif kind == "chunk":
            stream = self.bodies.get(message.get("id", ""))
            data = raw if raw is not None else (message.get("text") or "").encode()
            if stream is not None and data:
                stream.push(data)
        elif kind == "end":
            stream = self.bodies.get(message.get("id", ""))
            if stream is not None:
                stream.end()
        elif kind == "cancel":
            stream = self.bodies.get(message.get("id", ""))
            if stream is not None:
                stream.end(ConnectionAbortedError("the request was cancelled"))
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
        self.bodies.pop(rid, None)

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
            stream=self.bodies.get(rid),
        )
        self.requests[rid] = req
        ctx = msg.get("context") or {}
        # Each task has its own context, so this only tags this request's logs.
        _current_id.set(rid)
        try:
            fn = resolve(req.handler)
            if inspect.iscoroutinefunction(fn):
                result = await fn(req, ctx)
            else:
                loop = asyncio.get_running_loop()
                call = functools.partial(contextvars.copy_context().run, fn, req, ctx)
                result = await loop.run_in_executor(None, call)
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
        "stream_body": True,
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
    _install_logging(worker)
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
    _install_logging(worker)
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
