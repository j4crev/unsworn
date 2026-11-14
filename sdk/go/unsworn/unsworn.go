// Package unsworn lets you write Unsworn handlers in Go.
//
// A handler is a function that takes a request and returns a reply:
//
//	func GetUser(req *unsworn.Request) (unsworn.Reply, error) {
//		id, _ := req.ParamInt("id")
//		return unsworn.JSON(map[string]any{"id": id, "name": "Ada"}), nil
//	}
//
// Go can't load a function from a file by name at runtime, so handlers are
// compiled into one worker program that lists them:
//
//	func main() {
//		unsworn.Serve(GetUser, ListUsers)
//	}
//
// and the runtime builds and runs it, from unsworn.toml:
//
//	[runtimes.go]
//	extensions = [".go"]
//	build      = "go build -o .unsworn/go-worker ."
//	command    = ".unsworn/go-worker"
//	protocol   = "binary"
//
// A route's handler reference, such as "users.go#GetUser", finds the
// function named GetUser that is defined in users.go. See [Register].
package unsworn

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
)

// Version is the SDK version.
const Version = "0.1.0"

// ProtocolVersion is the Unsworn handler protocol version this SDK speaks.
const ProtocolVersion = 1

// HandlerFunc handles one request. Before handlers have the same signature
// and return [Next] to let the request continue.
//
// Returning an error sends Unsworn an error message, and the client gets a
// 500. To answer with another status, return a reply such as
// JSON(body).WithStatus(404) and a nil error.
type HandlerFunc func(req *Request) (Reply, error)

// Request is a request that Unsworn has already matched, authenticated and
// validated.
type Request struct {
	// ID is Unsworn's id for this request, unique per worker connection.
	ID string
	// Phase is "handle" for the route's handler and "before" for a before
	// handler.
	Phase string
	// Handler is the handler reference, such as "users.go#GetUser".
	Handler string
	// Route is the matched route pattern, such as "/users/{id}".
	Route  string
	Method string
	Path   string
	// Params and Query hold values already converted to their declared
	// types. Numbers are json.Number; see ParamInt, BindParams and BindQuery.
	Params map[string]any
	Query  map[string]any
	// RawQuery is the query string as sent, without the "?".
	RawQuery string
	// Headers has lowercase names.
	Headers map[string]string
	// Ctx holds the verified credentials under "auth", plus anything before
	// handlers added with Next.
	Ctx map[string]any
	// JSON is the request's JSON body, or nil. See Bind.
	JSON json.RawMessage
	// Text is the request's text body, when it had one.
	Text string
	// Body is the request's raw body, when it had one.
	Body []byte

	ctx context.Context
}

// Context is cancelled when Unsworn cancels the request: the client went
// away or the route's timeout passed. Pass it to database and HTTP calls.
func (r *Request) Context() context.Context {
	if r.ctx == nil {
		return context.Background()
	}
	return r.ctx
}

// Header returns a request header. name is matched case-insensitively.
func (r *Request) Header(name string) string {
	return r.Headers[lower(name)]
}

// Param returns a path parameter as a string, or "" if it's missing.
func (r *Request) Param(name string) string {
	return asString(r.Params[name])
}

// ParamInt returns a path parameter as an integer.
func (r *Request) ParamInt(name string) (int64, bool) {
	return asInt(r.Params[name])
}

// QueryValue returns a query parameter as a string, or "" if it's missing.
func (r *Request) QueryValue(name string) string {
	return asString(r.Query[name])
}

// QueryInt returns a query parameter as an integer.
func (r *Request) QueryInt(name string) (int64, bool) {
	return asInt(r.Query[name])
}

// Bind decodes the JSON body into v.
func (r *Request) Bind(v any) error {
	if len(r.JSON) == 0 {
		return errors.New("unsworn: the request has no JSON body")
	}
	return json.Unmarshal(r.JSON, v)
}

// BindParams decodes the path parameters into v, a pointer to a struct or map.
func (r *Request) BindParams(v any) error { return rebind(r.Params, v) }

// BindQuery decodes the query parameters into v, a pointer to a struct or map.
func (r *Request) BindQuery(v any) error { return rebind(r.Query, v) }

func rebind(from map[string]any, v any) error {
	data, err := json.Marshal(from)
	if err != nil {
		return err
	}
	return json.Unmarshal(data, v)
}

func asString(v any) string {
	switch t := v.(type) {
	case nil:
		return ""
	case string:
		return t
	case json.Number:
		return t.String()
	default:
		return fmt.Sprint(t)
	}
}

func asInt(v any) (int64, bool) {
	switch t := v.(type) {
	case json.Number:
		n, err := t.Int64()
		return n, err == nil
	case string:
		n, err := strconv.ParseInt(t, 10, 64)
		return n, err == nil
	case float64:
		return int64(t), float64(int64(t)) == t
	case int64:
		return t, true
	case int:
		return int64(t), true
	default:
		return 0, false
	}
}

func lower(s string) string {
	for i := 0; i < len(s); i++ {
		if 'A' <= s[i] && s[i] <= 'Z' {
			b := []byte(s)
			for j := i; j < len(b); j++ {
				if 'A' <= b[j] && b[j] <= 'Z' {
					b[j] += 'a' - 'A'
				}
			}
			return string(b)
		}
	}
	return s
}

type replyKind int

const (
	kindEmpty replyKind = iota
	kindJSON
	kindText
	kindBytes
	kindStream
	kindNext
	kindHTTP
)

// Reply is what a handler returns. Build one with JSON, Text, Bytes, Stream,
// Status or Next. The zero Reply is a 204 with no body.
type Reply struct {
	kind    replyKind
	status  int
	headers map[string][]string
	json    any
	text    string
	body    []byte
	stream  func(w io.Writer) error
	context map[string]any
	http    http.Handler
}

// JSON replies 200 with v encoded as JSON.
func JSON(v any) Reply { return Reply{kind: kindJSON, status: 200, json: v} }

// Text replies 200 with a text body.
func Text(s string) Reply { return Reply{kind: kindText, status: 200, text: s} }

// Bytes replies 200 with a raw body.
func Bytes(b []byte) Reply { return Reply{kind: kindBytes, status: 200, body: b} }

// Status replies with a status and no body.
func Status(code int) Reply { return Reply{kind: kindEmpty, status: code} }

// Stream replies 200 and streams the body: every Write sends a chunk to the
// client right away. Writes fail once the request is cancelled. Use it for
// server-sent events, large downloads and generated output.
func Stream(fn func(w io.Writer) error) Reply {
	return Reply{kind: kindStream, status: 200, stream: fn}
}

// Next is returned by a before handler to let the request continue, adding
// values to its context. Later handlers see them in Request.Ctx.
func Next(ctx map[string]any) Reply { return Reply{kind: kindNext, context: ctx} }

// WithStatus sets the reply's status.
func (r Reply) WithStatus(code int) Reply {
	r.status = code
	return r
}

// WithHeader adds a response header. Adding the same name again sends it
// several times, as Set-Cookie needs.
func (r Reply) WithHeader(name, value string) Reply {
	h := make(map[string][]string, len(r.headers)+1)
	for k, v := range r.headers {
		h[k] = v
	}
	name = lower(name)
	h[name] = append(append([]string(nil), h[name]...), value)
	r.headers = h
	return r
}
