package unsworn

import (
	"fmt"
	"path"
	"reflect"
	"runtime"
	"strings"
	"sync"
)

type entry struct {
	// name is the function's name, such as "GetUser".
	name string
	// file is the function's source file as the Go toolchain recorded it:
	// absolute, or module-relative when built with -trimpath.
	file string
	// ref is set for handlers registered with RegisterAs.
	ref string
	fn  HandlerFunc
}

var (
	registryMu sync.RWMutex
	registry   []entry
	resolved   = map[string]HandlerFunc{}
)

// Register adds handlers so that references can find them. A reference
// "users.go#GetUser" finds the function named GetUser defined in a file
// whose path ends in users.go. A reference without a name, such as a route
// file "routes/users.[id].get.go", finds the only handler registered from
// that file, or the one named Handler.
//
// Register panics on anonymous functions; give those a name with
// RegisterAs.
func Register(fns ...HandlerFunc) {
	registryMu.Lock()
	defer registryMu.Unlock()
	for _, fn := range fns {
		if fn == nil {
			panic("unsworn: Register got a nil handler")
		}
		f := runtime.FuncForPC(reflect.ValueOf(fn).Pointer())
		if f == nil {
			panic("unsworn: can't find the name of a registered handler; use RegisterAs")
		}
		name := funcName(f.Name())
		if name == "" {
			panic(fmt.Sprintf("unsworn: %s is an anonymous function; register it with RegisterAs", f.Name()))
		}
		file, _ := f.FileLine(f.Entry())
		registry = append(registry, entry{name: name, file: file, fn: fn})
	}
	resolved = map[string]HandlerFunc{}
}

// RegisterAs adds a handler under an explicit reference: a full reference
// such as "users.go#GetUser", a bare name such as "GetUser" that matches
// "<any file>#GetUser", or a file such as "routes/health.get.go".
func RegisterAs(ref string, fn HandlerFunc) {
	if fn == nil {
		panic("unsworn: RegisterAs got a nil handler")
	}
	registryMu.Lock()
	defer registryMu.Unlock()
	registry = append(registry, entry{ref: ref, fn: fn})
	resolved = map[string]HandlerFunc{}
}

// funcName turns "example.com/app/routes.GetUser" into "GetUser", and
// returns "" for closures such as "main.main.func1".
func funcName(full string) string {
	full = strings.TrimSuffix(full, "-fm") // method values
	if i := strings.LastIndexByte(full, '/'); i >= 0 {
		full = full[i+1:]
	}
	name := full[strings.LastIndexByte(full, '.')+1:]
	if isDigits(name) || (strings.HasPrefix(name, "func") && isDigits(name[4:])) {
		return "" // a closure: "main.main.func1", or "main.main.func1.2"
	}
	return name
}

func isDigits(s string) bool {
	if s == "" {
		return false
	}
	for _, c := range s {
		if c < '0' || c > '9' {
			return false
		}
	}
	return true
}

func splitRef(ref string) (file, name string) {
	if i := strings.IndexByte(ref, '#'); i >= 0 {
		return ref[:i], ref[i+1:]
	}
	return ref, ""
}

// sameFile reports whether a source file recorded by the toolchain is the
// file a reference names, relative to the project root.
func sameFile(src, ref string) bool {
	src = path.Clean(src)
	ref = strings.TrimPrefix(path.Clean(ref), "./")
	return src == ref || strings.HasSuffix(src, "/"+ref)
}

// resolve finds the handler for a reference, or explains why it can't.
func resolve(ref string) (HandlerFunc, error) {
	registryMu.RLock()
	fn, ok := resolved[ref]
	registryMu.RUnlock()
	if ok {
		return fn, nil
	}
	fn, err := lookup(ref)
	if err != nil {
		return nil, err
	}
	registryMu.Lock()
	resolved[ref] = fn
	registryMu.Unlock()
	return fn, nil
}

func lookup(ref string) (HandlerFunc, error) {
	registryMu.RLock()
	defer registryMu.RUnlock()
	file, name := splitRef(ref)

	for _, e := range registry {
		if e.ref != "" && (e.ref == ref || (name != "" && e.ref == name) || (name == "" && sameFile(e.ref, file))) {
			return e.fn, nil
		}
	}

	if name != "" {
		var elsewhere []string
		for _, e := range registry {
			if e.ref != "" || e.name != name {
				continue
			}
			if sameFile(e.file, file) {
				return e.fn, nil
			}
			elsewhere = append(elsewhere, e.file)
		}
		if len(elsewhere) > 0 {
			return nil, fmt.Errorf("%s is registered from %s, not %s", name, strings.Join(elsewhere, ", "), file)
		}
		return nil, fmt.Errorf("no handler named %q is registered; add it to unsworn.Serve or unsworn.Register", name)
	}

	var inFile []entry
	for _, e := range registry {
		if e.ref == "" && sameFile(e.file, file) {
			if e.name == "Handler" {
				return e.fn, nil
			}
			inFile = append(inFile, e)
		}
	}
	switch len(inFile) {
	case 1:
		return inFile[0].fn, nil
	case 0:
		return nil, fmt.Errorf("no handler is registered from %s", file)
	default:
		names := make([]string, len(inFile))
		for i, e := range inFile {
			names[i] = e.name
		}
		return nil, fmt.Errorf("%s registers several handlers (%s); add #<name> to the reference", file, strings.Join(names, ", "))
	}
}

func resetRegistry() {
	registryMu.Lock()
	defer registryMu.Unlock()
	registry = nil
	resolved = map[string]HandlerFunc{}
}
