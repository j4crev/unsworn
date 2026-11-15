// The upstream for proxy benchmarks, and the "Go on its own" baseline.
//
//	go build -o ../../target/bench/upstream . && ../../target/bench/upstream 127.0.0.1:9001
//
// GET /text answers like the conformance `text` handler (201, "hello",
// x-conformance: yes), so it compares directly with a handler behind
// Unsworn. Any other GET answers 200 "hello". POST echoes the body.
package main

import (
	"io"
	"log"
	"net/http"
	"os"
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
	log.Fatal(http.ListenAndServe(addr, nil))
}
