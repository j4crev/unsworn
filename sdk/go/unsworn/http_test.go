package unsworn

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"testing"
)

// serve runs h for req through a Tier 1 worker and returns what it sent.
func serve(t *testing.T, h http.Handler, req *Request) []map[string]any {
	t.Helper()
	var out bytes.Buffer
	w := &worker{codec: &jsonlCodec{}, out: &out, inFlight: map[string]*call{}}
	req.ctx = context.Background()
	if err := w.serveHTTP(req, h); err != nil {
		t.Fatal(err)
	}
	var msgs []map[string]any
	r := bufio.NewReader(&out)
	for {
		line, err := r.ReadBytes('\n')
		if err == io.EOF {
			return msgs
		}
		var m map[string]any
		if err := json.Unmarshal(line, &m); err != nil {
			t.Fatalf("bad message %q: %v", line, err)
		}
		if n, ok := m["body_len"].(float64); ok {
			body := make([]byte, int(n))
			io.ReadFull(r, body)
			m["body"] = string(body)
		}
		msgs = append(msgs, m)
	}
}

func TestHTTPHandlerBuffered(t *testing.T) {
	mux := http.NewServeMux()
	mux.HandleFunc("POST /greet/{name}", func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		w.Header().Add("Set-Cookie", "a=1")
		w.Header().Add("Set-Cookie", "b=2")
		w.WriteHeader(201)
		fmt.Fprintf(w, "hi %s, q=%s, host=%s, body=%s", r.PathValue("name"), r.URL.Query().Get("q"), r.Host, body)
	})
	msgs := serve(t, mux, &Request{
		ID: "r-1", Method: "POST", Path: "/greet/ada", RawQuery: "q=1",
		Headers: map[string]string{"host": "example.com", "content-type": "application/json"},
		JSON:    json.RawMessage(`{"x":1}`),
	})
	if len(msgs) != 1 {
		t.Fatalf("got %d messages: %v", len(msgs), msgs)
	}
	m := msgs[0]
	if m["type"] != "response" || m["status"] != 201.0 || m["stream"] != nil {
		t.Fatalf("got %v", m)
	}
	if m["body"] != `hi ada, q=1, host=example.com, body={"x":1}` {
		t.Fatalf("body %q", m["body"])
	}
	headers := m["headers"].(map[string]any)
	if cookies, ok := headers["set-cookie"].([]any); !ok || len(cookies) != 2 {
		t.Fatalf("headers %v", headers)
	}
	if !strings.HasPrefix(headers["content-type"].(string), "text/plain") {
		t.Fatalf("content-type %v", headers["content-type"])
	}
}

func TestHTTPHandlerStreamsAfterFlush(t *testing.T) {
	h := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		for i := 0; i < 2; i++ {
			fmt.Fprintf(w, "data: %d\n\n", i)
			w.(http.Flusher).Flush()
		}
	})
	msgs := serve(t, h, &Request{ID: "r-2", Method: "GET", Path: "/events"})
	var kinds []string
	for _, m := range msgs {
		kinds = append(kinds, m["type"].(string))
	}
	if strings.Join(kinds, ",") != "response,chunk,chunk,end" {
		t.Fatalf("got %v", msgs)
	}
	if msgs[0]["stream"] != true || msgs[1]["body"] != "data: 0\n\n" || msgs[2]["body"] != "data: 1\n\n" {
		t.Fatalf("got %v", msgs)
	}
}
