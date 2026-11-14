package unsworn

import (
	"bytes"
	"fmt"
	"net/http"
	"strings"
)

// HTTPHandler turns an http.Handler, such as an http.ServeMux or a
// router built with chi, gin or echo, into a handler. Use it to put an
// existing Go app behind a catch-all route:
//
//	func main() {
//		unsworn.RegisterAs("app.go", unsworn.HTTPHandler(newMux()))
//		if err := unsworn.Run(); err != nil {
//			log.Fatal(err)
//		}
//	}
//
// The handler sees the original method, path, query, headers and body.
// Responses are buffered unless the handler calls Flush, which starts
// streaming, as server-sent events need.
func HTTPHandler(h http.Handler) HandlerFunc {
	return func(req *Request) (Reply, error) {
		return Reply{kind: kindHTTP, http: h}, nil
	}
}

func (w *worker) serveHTTP(req *Request, h http.Handler) (err error) {
	hr, err := toHTTPRequest(req)
	if err != nil {
		return err
	}
	rw := &responseWriter{w: w, req: req, header: http.Header{}}
	defer func() {
		if p := recover(); p != nil {
			if p == http.ErrAbortHandler {
				err = fmt.Errorf("handler aborted the response")
			} else {
				err = fmt.Errorf("panic: %v", p)
			}
		}
	}()
	h.ServeHTTP(rw, hr)
	return rw.finish()
}

func toHTTPRequest(req *Request) (*http.Request, error) {
	var body []byte
	switch {
	case req.JSON != nil:
		body = req.JSON
	case req.Body != nil:
		body = req.Body
	default:
		body = []byte(req.Text)
	}
	target := req.Path
	if req.RawQuery != "" {
		target += "?" + req.RawQuery
	}
	hr, err := http.NewRequestWithContext(req.Context(), req.Method, target, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	hr.RequestURI = target
	hr.ContentLength = int64(len(body))
	for k, v := range req.Headers {
		hr.Header.Set(k, v)
	}
	hr.Header.Del("Content-Length")
	hr.Host = req.Headers["host"]
	return hr, nil
}

// responseWriter buffers a response, or streams it after Flush.
type responseWriter struct {
	w      *worker
	req    *Request
	header http.Header
	status int
	buf    []byte
	// streaming is set once the response head has been sent.
	streaming bool
	err       error
}

func (r *responseWriter) Header() http.Header { return r.header }

func (r *responseWriter) WriteHeader(code int) {
	if r.status == 0 {
		r.status = code
	}
}

func (r *responseWriter) Write(p []byte) (int, error) {
	r.WriteHeader(http.StatusOK)
	if r.err != nil {
		return 0, r.err
	}
	if !r.streaming {
		r.buf = append(r.buf, p...)
		return len(p), nil
	}
	if err := r.req.Context().Err(); err != nil {
		r.err = err
		return 0, err
	}
	if len(p) == 0 {
		return 0, nil
	}
	if err := r.w.send(chunkMessage{Type: "chunk", ID: r.req.ID, BodyLen: len(p)}, p); err != nil {
		r.err = err
		return 0, err
	}
	return len(p), nil
}

// Flush sends the response head, if it hasn't been sent, and anything
// buffered.
func (r *responseWriter) Flush() {
	r.WriteHeader(http.StatusOK)
	if r.err != nil {
		return
	}
	if !r.streaming {
		head := r.head()
		head.Stream = true
		if r.err = r.w.send(head, nil); r.err != nil {
			return
		}
		r.streaming = true
		buf := r.buf
		r.buf = nil
		if len(buf) > 0 {
			r.Write(buf)
		}
	}
}

func (r *responseWriter) head() responseMessage {
	if r.header.Get("Content-Type") == "" && len(r.buf) > 0 {
		r.header.Set("Content-Type", http.DetectContentType(r.buf))
	}
	res := responseMessage{Type: "response", ID: r.req.ID, Status: r.status}
	if len(r.header) > 0 {
		res.Headers = make(map[string]any, len(r.header))
		for k, v := range r.header {
			k = strings.ToLower(k)
			if k == "content-length" || k == "transfer-encoding" || k == "connection" {
				continue
			}
			if len(v) == 1 {
				res.Headers[k] = v[0]
			} else {
				res.Headers[k] = v
			}
		}
	}
	return res
}

func (r *responseWriter) finish() error {
	r.WriteHeader(http.StatusOK)
	if r.err != nil {
		return r.err
	}
	if r.streaming {
		return r.w.send(idMessage{Type: "end", ID: r.req.ID}, nil)
	}
	head := r.head()
	head.BodyLen = len(r.buf)
	return r.w.send(head, r.buf)
}
