// The upstream for proxy benchmarks, the "Go on its own" baseline, and the
// Go server for the HTTP runtime benchmarks.
//
//	go build -o ../../target/bench/upstream . && ../../target/bench/upstream 127.0.0.1:9001
//
// With a second argument, it also listens on that Unix socket. Started by
// Unsworn as an HTTP runtime (no arguments), it listens where Unsworn says:
// UNSWORN_PORT, or else the socket in UNSWORN_SOCKET.
//
// GET /text answers like the conformance `text` handler (201, "hello",
// x-conformance: yes), so it compares directly with a handler behind
// Unsworn. Any other GET answers 200 "hello". POST echoes the body.
//
// Under Unsworn, in a project with functions or outbound services, it also
// answers like the conformance `relay` and `outbound` handlers, through
// Unsworn's local endpoint (UNSWORN_CALLS_SOCKET, or UNSWORN_CALLS_URL when
// BENCH_CALLS=url):
//
//	POST /relay?fn=<name>       calls the function with the body as input:
//	                            {"answer": <its JSON>} or {"failed": <status>}
//	POST /outbound?path=<path>  sends the body to the service "conformance" at that
//	                            path: its body, with x-status
//	POST /direct?path=<path>    the same request straight to the upstream on
//	                            127.0.0.1:9001, without Unsworn, to compare
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"time"
)

func main() {
	addr := "127.0.0.1:9001"
	if len(os.Args) > 1 {
		addr = os.Args[1]
	}
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodPost {
			body, err := io.ReadAll(r.Body)
			if err != nil {
				http.Error(w, err.Error(), http.StatusBadRequest)
				return
			}
			w.Header().Set("content-type", "application/octet-stream")
			w.Write(body)
			return
		}
		w.Header().Set("content-type", "text/plain; charset=utf-8")
		if r.URL.Path == "/text" {
			w.Header().Set("x-conformance", "yes")
			w.WriteHeader(http.StatusCreated)
		}
		io.WriteString(w, "hello")
	})
	if len(os.Args) == 1 {
		if serveCalls() {
			http.HandleFunc("POST /relay", relay)
			http.HandleFunc("POST /outbound", outbound)
			http.HandleFunc("POST /direct", direct)
		}
		if port := os.Getenv("UNSWORN_PORT"); port != "" {
			log.Fatal(http.ListenAndServe("127.0.0.1:"+port, nil))
		}
		if socket := os.Getenv("UNSWORN_SOCKET"); socket != "" {
			log.Fatal(serveUnix(socket))
		}
	}
	if len(os.Args) > 2 {
		go func() { log.Fatal(serveUnix(os.Args[2])) }()
	}
	log.Fatal(http.ListenAndServe(addr, nil))
}

func serveUnix(path string) error {
	os.Remove(path)
	socket, err := net.Listen("unix", path)
	if err != nil {
		return err
	}
	return http.Serve(socket, nil)
}

// Unsworn's local endpoint, and a client for it that keeps its connections.
var (
	calls       string
	callsClient *http.Client
	upstream    = &http.Client{Transport: &http.Transport{MaxIdleConnsPerHost: 1024}}
)

func serveCalls() bool {
	transport := &http.Transport{MaxIdleConnsPerHost: 1024, IdleConnTimeout: time.Minute}
	if os.Getenv("BENCH_CALLS") == "url" {
		calls = os.Getenv("UNSWORN_CALLS_URL")
	} else if socket := os.Getenv("UNSWORN_CALLS_SOCKET"); socket != "" {
		calls = "http://unsworn"
		transport.DialContext = func(ctx context.Context, _, _ string) (net.Conn, error) {
			var d net.Dialer
			return d.DialContext(ctx, "unix", socket)
		}
	}
	callsClient = &http.Client{Transport: transport}
	return calls != ""
}

func relay(w http.ResponseWriter, r *http.Request) {
	req, _ := http.NewRequestWithContext(r.Context(), "POST", calls+"/call/"+r.URL.Query().Get("fn"), r.Body)
	req.Header.Set("unsworn-call", r.Header.Get("unsworn-call"))
	req.Header.Set("content-type", "application/json")
	res, err := callsClient.Do(req)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadGateway)
		return
	}
	defer res.Body.Close()
	answer, err := io.ReadAll(res.Body)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadGateway)
		return
	}
	w.Header().Set("content-type", "application/json")
	if res.StatusCode >= 400 {
		fmt.Fprintf(w, `{"failed":%d}`, res.StatusCode)
		return
	}
	out, _ := json.Marshal(map[string]json.RawMessage{"answer": answer})
	w.Write(out)
}

func outbound(w http.ResponseWriter, r *http.Request) {
	forward(w, r, callsClient, calls+"/fetch/conformance"+r.URL.Query().Get("path"), true)
}

func direct(w http.ResponseWriter, r *http.Request) {
	forward(w, r, upstream, "http://127.0.0.1:9001"+r.URL.Query().Get("path"), false)
}

func forward(w http.ResponseWriter, r *http.Request, client *http.Client, url string, token bool) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	req, _ := http.NewRequestWithContext(r.Context(), "POST", url, bytes.NewReader(body))
	req.Header.Set("x-from", "handler")
	if token {
		req.Header.Set("unsworn-call", r.Header.Get("unsworn-call"))
	}
	res, err := client.Do(req)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadGateway)
		return
	}
	defer res.Body.Close()
	answer, err := io.ReadAll(res.Body)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadGateway)
		return
	}
	w.Header().Set("content-type", "application/octet-stream")
	w.Header().Set("x-status", fmt.Sprint(res.StatusCode))
	w.Write(answer)
}
