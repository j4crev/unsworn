"""The Unsworn worker for Python handlers: ``python3 -u -m unsworn.worker``."""

from __future__ import annotations

import asyncio
import contextvars
import importlib.util
import inspect
import json
import logging
import os
import queue
import sys
import threading
import time
import traceback
from typing import Any, Callable

from . import _UNSET, PROTOCOL_VERSION, CallError, Context, Fetched, FetchError, Request, __version__
from .protocol import FramedCodec, JsonlCodec

ROOT = os.environ.get("UNSWORN_ROOT", os.getcwd())
TRANSPORT = os.environ.get("UNSWORN_TRANSPORT", "stdio")
MAX_IN_FLIGHT = int(os.environ.get("UNSWORN_MAX_IN_FLIGHT", "32"))
DEV = os.environ.get("UNSWORN_DEV") == "1"
# Set by Unsworn versions that take a JSON body as the raw bytes after a
# message (``body_json``), in both directions.
RAW_JSON = os.environ.get("UNSWORN_RAW_JSON") == "1"
# Set by Unsworn versions that answer `call` messages, for `ctx.call`.
CALLS = os.environ.get("UNSWORN_CALLS") == "1"
# And `fetch` messages, for `ctx.fetch`.
FETCH = os.environ.get("UNSWORN_FETCH") == "1"
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


class BadJson(ValueError):
    """The request's body was sent as JSON and isn't."""


class _RawJsonRequest(Request):
    """A request whose JSON body arrived as bytes. ``json`` is parsed from
    them when a handler first reads it."""

    @property
    def json(self) -> Any:
        raw = self.__dict__.get("_raw_json")
        if raw is not None:
            try:
                self.__dict__["_json"] = json.loads(raw)
            except ValueError as e:
                raise BadJson(str(e)) from None
            self.__dict__["_raw_json"] = None
        return self.__dict__.get("_json")

    @json.setter
    def json(self, value: Any) -> None:
        self.__dict__["_json"] = value
        self.__dict__["_raw_json"] = None


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


class Threads:
    """Runs sync handlers on threads, so one that blocks doesn't hold up the
    worker's other requests.

    It starts with one thread, which is the cheapest way to run handlers
    that return quickly or compute: Python runs one thread at a time, so
    with a thread per request the threads spend more time waking up and
    passing the interpreter lock around than a small handler takes to run.

    More threads only help when the ones there are wait on something, such
    as a database. So while calls are queued, it checks every ``CHECK_EVERY``
    seconds what happened since the last check. If no thread ran out of
    calls, and the worker's threads together spent less than half that time
    running or ready to run, then calls sat queued while Python was free to
    run them: the threads were waiting inside handlers. Every queued call
    then gets a thread of its own, up to ``limit`` threads. The kernel says
    how long each thread ran (``schedstat``, Linux only); where it can't be
    asked, that half of the test is skipped.

    ``RESET_EVERY`` seconds after it grows, the extra threads are retired,
    so a pool that grew for a burst of slow calls goes back to one thread.
    If the calls still need threads it grows again within two checks, and
    then waits twice as long before the next reset, up to ``RESET_MAX``."""

    # Long enough for the kernel's figures to mean something: it updates a
    # thread's running time at scheduler ticks, a few milliseconds apart.
    CHECK_EVERY = 0.02
    RESET_EVERY = 30.0
    RESET_MAX = 3600.0
    # Growing again this soon after a reset means the threads were in use.
    REGROWN_WITHIN = 5.0

    def __init__(self, loop: asyncio.AbstractEventLoop, limit: int) -> None:
        self.loop = loop
        self.limit = max(limit, 1)
        self.queue: queue.SimpleQueue = queue.SimpleQueue()
        # Only the loop's thread touches what follows.
        self.threads = 0
        self.started = 0
        self.unfinished = 0  # calls queued or running
        self.checking = False
        self.resetting = False
        self.reset_every = self.RESET_EVERY
        self.reset_at = float("-inf")
        # An open schedstat file for the loop's thread and each handler thread.
        self.clocks: dict[int, int] = {}
        self.clocked = True
        self._joined(threading.get_native_id())
        # How often a thread found nothing queued. Threads add to it without
        # a lock: a lost update still leaves it changed, which is all
        # `_check` looks at.
        self.idled = 0

    def run(self, fn: Callable, *args: Any) -> asyncio.Future:
        """Calls ``fn(*args)`` on a thread, in a copy of the caller's context.
        If the returned future is cancelled before a thread gets to the
        call, the call isn't made."""
        future = self.loop.create_future()
        self.unfinished += 1
        self.queue.put((future, contextvars.copy_context(), fn, args))
        if self.threads == 0:
            self._start(1)
        elif self.unfinished > self.threads and not self.checking and self.threads < self.limit:
            self.checking = True
            self.loop.call_later(self.CHECK_EVERY, self._check, None)
        return future

    def _start(self, n: int) -> None:
        for _ in range(n):
            self.threads += 1
            self.started += 1
            threading.Thread(target=self._thread, name=f"unsworn-handler-{self.started}", daemon=True).start()
        if self.threads > 1 and not self.resetting:
            regrown = self.loop.time() - self.reset_at < self.REGROWN_WITHIN
            self.reset_every = min(self.reset_every * 2, self.RESET_MAX) if regrown else self.RESET_EVERY
            self.resetting = True
            self.loop.call_later(self.reset_every, self._reset)

    def _check(self, before: tuple[int, int, int] | None) -> None:
        self.checking = False
        waiting = self.queue.qsize()
        if waiting == 0 or self.threads >= self.limit:
            return
        now, busy, idled = time.monotonic_ns(), self._busy(), self.idled
        if before is not None and before[2] == idled:
            if busy is None or (busy - before[1]) * 2 < now - before[0]:
                self._start(min(waiting, self.limit - self.threads))
                return
        self.checking = True
        self.loop.call_later(self.CHECK_EVERY, self._check, (now, busy or 0, idled))

    def _busy(self) -> int | None:
        """Nanoseconds the worker's threads have spent running or ready to
        run, or None if the kernel doesn't say."""
        if not self.clocked:
            return None
        total = 0
        try:
            for fd in self.clocks.values():
                ran, ready = os.pread(fd, 64, 0).split()[:2]
                total += int(ran) + int(ready)
        except (OSError, ValueError):
            self.clocked = False
            return None
        return total

    def _reset(self) -> None:
        self.resetting = False
        self.reset_at = self.loop.time()
        for _ in range(self.threads - 1):
            self.threads -= 1
            self.queue.put(None)

    def _joined(self, thread_id: int) -> None:
        if not self.clocked:
            return
        try:
            self.clocks[thread_id] = os.open(f"/proc/self/task/{thread_id}/schedstat", os.O_RDONLY)
        except OSError:
            self.clocked = False

    def _left(self, thread_id: int) -> None:
        fd = self.clocks.pop(thread_id, None)
        if fd is not None:
            os.close(fd)

    def _thread(self) -> None:
        thread_id = threading.get_native_id()
        try:
            self.loop.call_soon_threadsafe(self._joined, thread_id)
            while True:
                try:
                    item = self.queue.get_nowait()
                except queue.Empty:
                    self.idled += 1
                    item = self.queue.get()
                if item is None:
                    self.loop.call_soon_threadsafe(self._left, thread_id)
                    return
                future, context, fn, args = item
                result = error = None
                if not future.cancelled():
                    try:
                        result = context.run(fn, *args)
                    except BaseException as e:  # noqa: BLE001 - raised where the future is awaited
                        error = e
                self.loop.call_soon_threadsafe(self._finish, future, result, error)
                del item, future, context, fn, args, result, error
        except RuntimeError:  # the loop has closed
            return

    def _finish(self, future: asyncio.Future, result: Any, error: BaseException | None) -> None:
        self.unfinished -= 1
        if future.done():
            return
        if error is None:
            future.set_result(result)
        elif isinstance(error, StopIteration):
            # A future can't raise StopIteration.
            future.set_exception(RuntimeError("StopIteration raised on a handler thread"))
        else:
            future.set_exception(error)


class Worker:
    def __init__(self, codec: Any, write: Callable[[bytes], Any]) -> None:
        self.codec = codec
        self.write = write
        self.threads = Threads(asyncio.get_running_loop(), MAX_IN_FLIGHT)
        self.tasks: dict[str, asyncio.Task] = {}
        self.requests: dict[str, Request] = {}
        self.bodies: dict[str, BodyStream] = {}
        self.loop = asyncio.get_running_loop()
        self.loop_thread = threading.get_ident()
        # Calls to functions that Unsworn hasn't answered: the request each
        # is for, and the future its answer goes to.
        self.calls: dict[str, tuple[str, asyncio.Future]] = {}
        self.next_call = 0

    def send(self, message: dict, body: bytes | None = None) -> None:
        self.write(self.codec.encode(message, body))

    def call(self, rid: str, name: str, value: Any) -> Any:
        """``ctx.call``: asks Unsworn to run a function for the request
        ``rid``. On the loop's thread, which is where an ``async def``
        handler runs, it returns something to await. On a handler thread
        it waits for the answer."""
        if threading.get_ident() == self.loop_thread:
            return self._call(rid, name, value)
        return asyncio.run_coroutine_threadsafe(self._call(rid, name, value), self.loop).result()

    async def _call(self, rid: str, name: str, value: Any) -> Any:
        if not CALLS:
            raise CallError(501, "this Unsworn doesn't run functions for handlers: update it")
        self.next_call += 1
        call_id = f"c-{self.next_call}"
        answer: asyncio.Future = self.loop.create_future()
        self.calls[call_id] = (rid, answer)
        self.send({"type": "call", "id": call_id, "request": rid, "function": name, "json": value})
        try:
            return await answer
        finally:
            self.calls.pop(call_id, None)

    def fetch(self, rid: str, *args: Any) -> Any:
        """``ctx.fetch``: asks Unsworn for a request to an outbound service.
        Awaitable on the loop's thread, and waits on a handler thread, as
        :meth:`call` does."""
        if threading.get_ident() == self.loop_thread:
            return self._fetch(rid, *args)
        return asyncio.run_coroutine_threadsafe(self._fetch(rid, *args), self.loop).result()

    async def _fetch(
        self, rid: str, service: str, path: str, method: str | None, headers: dict | None, value: Any, body: Any
    ) -> Fetched:
        if not FETCH:
            raise FetchError(501, "this Unsworn doesn't make requests for handlers: update it")
        sent = {str(k).lower(): str(v) for k, v in (headers or {}).items()}
        raw: bytes | None = None
        if value is not _UNSET:
            raw = json.dumps(value, separators=(",", ":")).encode()
            sent.setdefault("content-type", "application/json")
        elif body is not None:
            raw = body.encode() if isinstance(body, str) else bytes(body)
        self.next_call += 1
        fetch_id = f"f-{self.next_call}"
        answer: asyncio.Future = self.loop.create_future()
        self.calls[fetch_id] = (rid, answer)
        self.send(
            {
                "type": "fetch",
                "id": fetch_id,
                "request": rid,
                "service": service,
                "method": (method or ("POST" if raw else "GET")).upper(),
                "path": path,
                "headers": sent,
            },
            raw or None,
        )
        try:
            return await answer
        finally:
            self.calls.pop(fetch_id, None)

    def _fetched(self, message: dict, raw: bytes | None) -> None:
        entry = self.calls.pop(message.get("id", ""), None)
        if entry is None or entry[1].done():
            return
        if message.get("error") is not None:
            entry[1].set_exception(FetchError(message.get("status", 502), message["error"]))
        else:
            entry[1].set_result(Fetched(message.get("status", 0), message.get("headers") or {}, raw or b""))

    def _result(self, message: dict) -> None:
        entry = self.calls.pop(message.get("id", ""), None)
        if entry is None or entry[1].done():
            return
        status = message.get("status", 500)
        if status < 400:
            entry[1].set_result(message.get("json"))
        else:
            reason = message.get("error") or f"the function answered {status}"
            entry[1].set_exception(CallError(status, reason, message.get("json")))

    def dispatch(self, message: dict, raw: bytes | None) -> None:
        kind = message.get("type")
        if kind == "result":
            self._result(message)
            return
        if kind == "fetched":
            self._fetched(message, raw)
            return
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
        # Unsworn drops a request's calls when the request ends, so nothing
        # will answer the ones still waiting.
        if self.calls:
            for call_id, (request, answer) in list(self.calls.items()):
                if request == rid:
                    del self.calls[call_id]
                    answer.cancel()

    async def handle(self, msg: dict, raw: bytes | None) -> None:
        rid = msg["id"]
        body_json = bool(msg.get("body_json")) and raw is not None
        req = (_RawJsonRequest if body_json else Request)(
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
            body=None if body_json else raw,
            stream=self.bodies.get(rid),
        )
        if body_json:
            req.__dict__["_raw_json"] = raw
        self.requests[rid] = req
        ctx = Context(msg.get("context") or {})
        ctx._call = lambda name, value=None: self.call(rid, name, value)
        ctx._fetch = lambda *args: self.fetch(rid, *args)
        # Each task has its own context, so this only tags this request's logs.
        _current_id.set(rid)
        try:
            fn = resolve(req.handler)
            if inspect.iscoroutinefunction(fn):
                result = await fn(req, ctx)
            else:
                result = await self.threads.run(fn, req, ctx)
                if inspect.isawaitable(result):
                    result = await result
            await self.reply(rid, result, req)
        except asyncio.CancelledError:
            return
        except BadJson as e:
            if req.cancelled:
                return
            # Unsworn left the body for the handler to read, so this is the
            # 400 it would have answered with.
            self.send(
                {
                    "type": "response",
                    "id": rid,
                    "status": 400,
                    "headers": {"content-type": "application/problem+json"},
                    "json": {
                        "type": "about:blank",
                        "title": "Bad Request",
                        "status": 400,
                        "detail": f"the body isn't valid JSON: {e}",
                    },
                }
            )
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
        unknown = set(result) - {"status", "headers", "json", "text", "body", "mcp_content"}
        if unknown:
            raise TypeError(
                f"unexpected reply keys {sorted(unknown)}; wrap data in unsworn.json(...)"
            )
        base: dict[str, Any] = {"type": "response", "id": rid, "status": result.get("status", 200)}
        if result.get("headers"):
            base["headers"] = result["headers"]
        if "mcp_content" in result:
            base["mcp_content"] = result["mcp_content"]
        if "json" in result and RAW_JSON:
            # Written out here and sent as the raw body, which Unsworn
            # passes on without reading it.
            text = json.dumps(result["json"], separators=(",", ":")).encode()
            self.send({**base, "body_json": True}, text)
        elif "json" in result:
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
                it = iter(payload)
                done = object()
                while True:
                    chunk = await self.threads.run(next, it, done)
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
        "raw_json": RAW_JSON,
        "raw_body": True,
        "calls": CALLS,
        "fetch": FETCH,
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
