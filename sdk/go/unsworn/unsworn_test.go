package unsworn

import (
	"bufio"
	"bytes"
	"strings"
	"testing"
)

func GetUser(req *Request) (Reply, error)   { return Text("user"), nil }
func ListUsers(req *Request) (Reply, error) { return Text("users"), nil }

func TestResolveByNameAndFile(t *testing.T) {
	resetRegistry()
	defer resetRegistry()
	Register(GetUser, ListUsers)
	RegisterAs("routes/health.get.go", func(req *Request) (Reply, error) { return Text("ok"), nil })
	RegisterAs("Ping", func(req *Request) (Reply, error) { return Text("pong"), nil })

	for ref, want := range map[string]string{
		"sdk/go/unsworn/unsworn_test.go#GetUser": "user",
		"unsworn/unsworn_test.go#ListUsers":      "users",
		"./unsworn_test.go#GetUser":              "user",
		"routes/health.get.go":                   "ok",
		"anything.go#Ping":                       "pong",
	} {
		fn, err := resolve(ref)
		if err != nil {
			t.Fatalf("%s: %v", ref, err)
		}
		r, _ := fn(&Request{})
		if r.text != want {
			t.Errorf("%s: got %q, want %q", ref, r.text, want)
		}
	}

	for ref, reason := range map[string]string{
		"unsworn_test.go#GetUsr":        `no handler named "GetUsr"`,
		"users.go#GetUser":              "not users.go",
		"unsworn_test.go":               "several handlers (GetUser, ListUsers)",
		"routes/missing.get.go":         "no handler is registered from routes/missing.get.go",
		"other_unsworn_test.go#GetUser": "not other_unsworn_test.go",
	} {
		_, err := resolve(ref)
		if err == nil || !strings.Contains(err.Error(), reason) {
			t.Errorf("%s: got %v, want an error containing %q", ref, err, reason)
		}
	}
}

func TestRegisterRejectsClosures(t *testing.T) {
	resetRegistry()
	defer resetRegistry()
	defer func() {
		if recover() == nil {
			t.Fatal("expected a panic for an anonymous function")
		}
	}()
	Register(func(req *Request) (Reply, error) { return Reply{}, nil })
}

func TestFuncName(t *testing.T) {
	for full, want := range map[string]string{
		"main.GetUser":                   "GetUser",
		"example.com/app/routes.GetUser": "GetUser",
		"gopkg.in/x.v3.Handler":          "Handler",
		"main.(*API).Users-fm":           "Users",
		"main.main.func1":                "",
		"main.main.func1.2":              "",
	} {
		if got := funcName(full); got != want {
			t.Errorf("funcName(%q) = %q, want %q", full, got, want)
		}
	}
}

func TestCodecsRoundTrip(t *testing.T) {
	msg := []byte(`{"type":"request","id":"r-1","handler":"a.go#B","params":{"id":42},"json":null,"body_len":3}`)
	for name, c := range map[string]func(r *bytes.Reader) codec{
		"jsonl":  func(r *bytes.Reader) codec { return &jsonlCodec{r: bufio.NewReader(r)} },
		"framed": func(r *bytes.Reader) codec { return &framedCodec{r: bufio.NewReader(r)} },
	} {
		var wire []byte
		wire = c(nil).encode(wire, msg, []byte("abc"))
		wire = c(nil).encode(wire, []byte(`{"type":"cancel","id":"r-1"}`), nil)
		dec := c(bytes.NewReader(wire))
		m, body, err := dec.read()
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		if m.Type != "request" || string(body) != "abc" || m.JSON != nil {
			t.Fatalf("%s: got %+v body %q", name, m, body)
		}
		if id, ok := asInt(m.Params["id"]); !ok || id != 42 {
			t.Fatalf("%s: params.id = %v", name, m.Params["id"])
		}
		m, _, err = dec.read()
		if err != nil || m.Type != "cancel" {
			t.Fatalf("%s: second message %+v, %v", name, m, err)
		}
		if _, _, err = dec.read(); err == nil {
			t.Fatalf("%s: expected EOF", name)
		}
	}
}

func TestMarshalKeepsHTML(t *testing.T) {
	data, err := marshal(map[string]string{"a": "<b>&</b>"})
	if err != nil || string(data) != `{"a":"<b>&</b>"}` {
		t.Fatalf("got %s, %v", data, err)
	}
}

func TestWithHeaderDoesNotShareMaps(t *testing.T) {
	base := Text("x").WithHeader("Set-Cookie", "a=1")
	one := base.WithHeader("set-cookie", "b=2")
	if len(base.headers["set-cookie"]) != 1 || len(one.headers["set-cookie"]) != 2 {
		t.Fatalf("base %v, one %v", base.headers, one.headers)
	}
}
