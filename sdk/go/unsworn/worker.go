package unsworn

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"runtime/debug"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Serve registers handlers and runs the worker until Unsworn closes the
// connection. Call it from main. It exits the process if the worker can't
// start.
func Serve(fns ...HandlerFunc) {
	Register(fns...)
	if err := Run(); err != nil {
		fmt.Fprintln(os.Stderr, "unsworn:", err)
		os.Exit(1)
	}
}

// Run runs the worker for the registered handlers until Unsworn closes the
// connection. It reads UNSWORN_TRANSPORT to pick Tier 1 (stdio) or Tier 2
// (a Unix socket).
//
// On Tier 1, stdout carries protocol messages, so Run points os.Stdout at
// stderr: fmt.Println output ends up in Unsworn's logs.
func Run() error {
	transport := os.Getenv("UNSWORN_TRANSPORT")
	if path, ok := strings.CutPrefix(transport, "unix:"); ok {
		conn, err := net.Dial("unix", path)
		if err != nil {
			return fmt.Errorf("can't connect to %s: %w", path, err)
		}
		defer conn.Close()
		return run(&framedCodec{r: bufio.NewReaderSize(conn, 64<<10)}, conn)
	}
	out := os.Stdout
	os.Stdout = os.Stderr
	return run(&jsonlCodec{r: bufio.NewReaderSize(os.Stdin, 64<<10)}, out)
}

type worker struct {
	codec codec
	dev   bool

	writeMu sync.Mutex
	out     io.Writer
	buf     []byte

	mu       sync.Mutex
	inFlight map[string]*call
}

type call struct {
	cancel    context.CancelFunc
	cancelled bool
}

func run(c codec, out io.Writer) error {
	w := &worker{
		codec:    c,
		out:      out,
		dev:      os.Getenv("UNSWORN_DEV") == "1",
		inFlight: map[string]*call{},
	}
	maxInFlight := 64
	if n, err := strconv.Atoi(os.Getenv("UNSWORN_MAX_IN_FLIGHT")); err == nil && n > 0 {
		maxInFlight = n
	}
	if err := w.send(readyMessage{
		Type:        "ready",
		Protocol:    ProtocolVersion,
		MaxInFlight: maxInFlight,
		Verify:      true,
		SDK:         "unsworn-go/" + Version,
	}, nil); err != nil {
		return err
	}
	for {
		m, body, err := c.read()
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return err
		}
		switch m.Type {
		case "request":
			w.start(m, body)
		case "cancel":
			w.cancel(m.ID)
		case "verify":
			w.verify(m)
		}
	}
}

func (w *worker) send(message any, body []byte) error {
	data, err := marshal(message)
	if err != nil {
		return err
	}
	w.writeMu.Lock()
	defer w.writeMu.Unlock()
	w.buf = w.codec.encode(w.buf[:0], data, body)
	_, err = w.out.Write(w.buf)
	if cap(w.buf) > 1<<20 {
		w.buf = nil
	}
	return err
}

func (w *worker) verify(m *inMessage) {
	missing := []missingHandler{}
	for _, ref := range m.Handlers {
		if _, err := resolve(ref); err != nil {
			missing = append(missing, missingHandler{Handler: ref, Reason: err.Error()})
		}
	}
	w.send(verifiedMessage{Type: "verified", ID: m.ID, Missing: missing}, nil)
}

func (w *worker) cancel(id string) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if c, ok := w.inFlight[id]; ok {
		c.cancelled = true
		c.cancel()
	}
}

func (w *worker) isCancelled(id string) bool {
	w.mu.Lock()
	defer w.mu.Unlock()
	c, ok := w.inFlight[id]
	return ok && c.cancelled
}

func (w *worker) start(m *inMessage, body []byte) {
	var (
		ctx    context.Context
		cancel context.CancelFunc
	)
	if m.DeadlineMs != nil {
		ctx, cancel = context.WithTimeout(context.Background(), time.Duration(*m.DeadlineMs)*time.Millisecond)
	} else {
		ctx, cancel = context.WithCancel(context.Background())
	}
	w.mu.Lock()
	w.inFlight[m.ID] = &call{cancel: cancel}
	w.mu.Unlock()

	req := &Request{
		ID:       m.ID,
		Phase:    m.Phase,
		Handler:  m.Handler,
		Route:    m.Route,
		Method:   m.Method,
		Path:     m.Path,
		Params:   orEmpty(m.Params),
		Query:    orEmpty(m.Query),
		RawQuery: m.RawQuery,
		Headers:  m.Headers,
		Ctx:      orEmpty(m.Context),
		JSON:     m.JSON,
		Body:     body,
		ctx:      ctx,
	}
	if req.Phase == "" {
		req.Phase = "handle"
	}
	if req.Headers == nil {
		req.Headers = map[string]string{}
	}
	if m.Text != nil {
		req.Text = *m.Text
	}

	go func() {
		defer func() {
			w.mu.Lock()
			delete(w.inFlight, m.ID)
			w.mu.Unlock()
			cancel()
		}()
		reply, err := w.call(req)
		// Past the deadline Unsworn answers 504 and cancels the request,
		// so anything sent now would be discarded.
		if w.isCancelled(req.ID) || errors.Is(ctx.Err(), context.DeadlineExceeded) {
			return
		}
		if err == nil {
			err = w.reply(req, reply)
		}
		if err != nil && !w.isCancelled(req.ID) && ctx.Err() == nil {
			fmt.Fprintf(os.Stderr, "unsworn: %s: %v\n", req.Handler, err)
			w.send(idMessage{Type: "error", ID: req.ID, Message: err.Error()}, nil)
		}
	}()
}

// call runs the handler, turning a panic into an error.
func (w *worker) call(req *Request) (reply Reply, err error) {
	fn, err := resolve(req.Handler)
	if err != nil {
		return Reply{}, err
	}
	defer func() {
		if p := recover(); p != nil {
			if w.dev {
				err = fmt.Errorf("panic: %v\n%s", p, debug.Stack())
			} else {
				err = fmt.Errorf("panic: %v", p)
			}
		}
	}()
	return fn(req)
}

func (w *worker) reply(req *Request, r Reply) error {
	if r.kind == kindNext {
		ctx := r.context
		if ctx == nil {
			ctx = map[string]any{}
		}
		return w.send(continueMessage{Type: "continue", ID: req.ID, Context: ctx}, nil)
	}
	res := responseMessage{Type: "response", ID: req.ID, Status: r.status}
	if res.Status == 0 {
		res.Status = 204
	}
	if len(r.headers) > 0 {
		res.Headers = make(map[string]any, len(r.headers))
		for k, v := range r.headers {
			if len(v) == 1 {
				res.Headers[k] = v[0]
			} else {
				res.Headers[k] = v
			}
		}
	}
	switch r.kind {
	case kindJSON:
		data, err := marshal(r.json)
		if err != nil {
			return fmt.Errorf("can't encode the JSON reply: %w", err)
		}
		res.JSON = data
		return w.send(res, nil)
	case kindText:
		res.Text = &r.text
		return w.send(res, nil)
	case kindBytes:
		res.BodyLen = len(r.body)
		return w.send(res, r.body)
	case kindStream:
		res.Stream = true
		if err := w.send(res, nil); err != nil {
			return err
		}
		if err := r.stream(&chunkWriter{w: w, req: req}); err != nil {
			return err
		}
		return w.send(idMessage{Type: "end", ID: req.ID}, nil)
	case kindHTTP:
		return w.serveHTTP(req, r.http)
	default:
		return w.send(res, nil)
	}
}

type chunkWriter struct {
	w   *worker
	req *Request
}

func (c *chunkWriter) Write(p []byte) (int, error) {
	if err := c.req.Context().Err(); err != nil {
		return 0, err
	}
	if len(p) == 0 {
		return 0, nil
	}
	if err := c.w.send(chunkMessage{Type: "chunk", ID: c.req.ID, BodyLen: len(p)}, p); err != nil {
		return 0, err
	}
	return len(p), nil
}

func orEmpty(m map[string]any) map[string]any {
	if m == nil {
		return map[string]any{}
	}
	return m
}
