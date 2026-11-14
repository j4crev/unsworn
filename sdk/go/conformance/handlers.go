// Conformance handlers for the Go SDK. See docs/conformance.md.
package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/j4crev/unsworn/sdk/go/unsworn"
)

func main() {
	unsworn.Serve(echo, text, binary, status, stream, fail, slow, crash, allow, deny, noisy)
}

func echo(req *unsworn.Request) (unsworn.Reply, error) {
	var header any
	if v, ok := req.Headers["x-test"]; ok {
		header = v
	}
	return unsworn.JSON(map[string]any{
		"method":  req.Method,
		"params":  req.Params,
		"query":   req.Query,
		"context": req.Ctx,
		"json":    req.JSON,
		"header":  header,
	}), nil
}

func text(req *unsworn.Request) (unsworn.Reply, error) {
	return unsworn.Text("hello").WithStatus(201).WithHeader("x-conformance", "yes"), nil
}

func binary(req *unsworn.Request) (unsworn.Reply, error) {
	return unsworn.Bytes(req.Body), nil
}

func status(req *unsworn.Request) (unsworn.Reply, error) {
	code, ok := req.QueryInt("code")
	if !ok {
		return unsworn.Reply{}, errors.New("query.code isn't a number")
	}
	return unsworn.Status(int(code)), nil
}

func stream(req *unsworn.Request) (unsworn.Reply, error) {
	return unsworn.Stream(func(w io.Writer) error {
		for _, s := range []string{"a", "b", "c"} {
			if _, err := io.WriteString(w, s); err != nil {
				return err
			}
		}
		return nil
	}), nil
}

func fail(req *unsworn.Request) (unsworn.Reply, error) {
	return unsworn.Reply{}, errors.New("boom")
}

func slow(req *unsworn.Request) (unsworn.Reply, error) {
	select {
	case <-time.After(500 * time.Millisecond):
		return unsworn.Text("done"), nil
	case <-req.Context().Done():
		return unsworn.Reply{}, req.Context().Err()
	}
}

func crash(req *unsworn.Request) (unsworn.Reply, error) {
	os.Exit(3)
	return unsworn.Reply{}, nil
}

func allow(req *unsworn.Request) (unsworn.Reply, error) {
	return unsworn.Next(map[string]any{"checked": true}), nil
}

func deny(req *unsworn.Request) (unsworn.Reply, error) {
	return unsworn.JSON(map[string]any{"error": "denied"}).WithStatus(403), nil
}

func noisy(req *unsworn.Request) (unsworn.Reply, error) {
	fmt.Println("this goes to stderr, not the protocol stream")
	return unsworn.Text("ok"), nil
}
