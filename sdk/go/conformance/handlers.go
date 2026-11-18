// Conformance handlers for the Go SDK. See docs/conformance.md.
package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"time"

	"github.com/j4crev/unsworn/sdk/go/unsworn"
)

func main() {
	unsworn.Serve(echo, text, binary, status, stream, fail, slow, crash, allow, deny, noisy, log, upload, content, mcp, relay, outbound)
}

func echo(req *unsworn.Request) (unsworn.Reply, error) {
	var header any
	if v, ok := req.Headers["x-test"]; ok {
		header = v
	}
	// Bind, not req.JSON as it is: nothing has read the body before the
	// handler, so this is where one that isn't JSON is found out.
	var body json.RawMessage
	if req.JSON != nil {
		if err := req.Bind(&body); err != nil {
			return unsworn.Reply{}, err
		}
	}
	return unsworn.JSON(map[string]any{
		"method":  req.Method,
		"params":  req.Params,
		"query":   req.Query,
		"context": req.Ctx,
		"json":    body,
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

func upload(req *unsworn.Request) (unsworn.Reply, error) {
	data, err := io.ReadAll(req.BodyReader)
	if err != nil {
		return unsworn.Reply{}, err
	}
	return unsworn.Bytes(data), nil
}

func log(req *unsworn.Request) (unsworn.Reply, error) {
	slog.InfoContext(req.Context(), "conformance log", "n", 1)
	return unsworn.Status(200), nil
}

func content(req *unsworn.Request) (unsworn.Reply, error) {
	blocks := []map[string]any{{"type": "text", "text": "hi"}}
	return unsworn.JSON(map[string]any{"ok": true}).WithContent(blocks), nil
}

func relay(req *unsworn.Request) (unsworn.Reply, error) {
	var input, answer json.RawMessage
	if req.JSON != nil {
		if err := req.Bind(&input); err != nil {
			return unsworn.Reply{}, err
		}
	}
	err := req.Call(req.QueryValue("fn"), input, &answer)
	var failed *unsworn.CallError
	if errors.As(err, &failed) {
		return unsworn.JSON(map[string]any{"failed": failed.Status}), nil
	}
	if err != nil {
		return unsworn.Reply{}, err
	}
	return unsworn.JSON(map[string]any{"answer": answer}), nil
}

func outbound(req *unsworn.Request) (unsworn.Reply, error) {
	answer, err := req.Fetch("conformance", req.QueryValue("path"), &unsworn.Outbound{
		Method:  "POST",
		Headers: map[string]string{"x-from": "handler"},
		Body:    req.Body,
	})
	var failed *unsworn.FetchError
	if errors.As(err, &failed) {
		return unsworn.JSON(map[string]any{"failed": failed.Status}), nil
	}
	if err != nil {
		return unsworn.Reply{}, err
	}
	return unsworn.Bytes(answer.Body).WithHeader("x-status", fmt.Sprint(answer.Status)), nil
}

func mcp(req *unsworn.Request) (unsworn.Reply, error) {
	call := req.MCP()
	if call == nil {
		return unsworn.Reply{}, errors.New("not an MCP call")
	}
	var client any
	if call.Client != nil {
		client = call.Client.Name
	}
	return unsworn.JSON(map[string]any{
		"tool":             call.Tool,
		"protocol_version": call.ProtocolVersion,
		"client":           client,
	}), nil
}
