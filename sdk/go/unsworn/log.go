package unsworn

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"sync/atomic"
	"time"
)

// Logging: Run makes slog's default logger send records to Unsworn, unless
// the program already set its own default. Records logged with a request's
// context are tagged with that request:
//
//	slog.InfoContext(req.Context(), "charged card", "amount", 1200)
//
// The standard log package goes through slog's default logger too, so
// log.Printf output arrives at the info level.

// active is the running worker, for LogHandler.
var active atomic.Pointer[worker]

type requestIDKey struct{}

// LogHandler returns an slog.Handler that sends records to Unsworn, for
// programs that build their own logger:
//
//	logger := slog.New(unsworn.LogHandler())
//
// Before Run starts, or under an Unsworn version without log records, it
// writes text to stderr.
func LogHandler() slog.Handler {
	return &logHandler{fields: map[string]any{}}
}

type logMessage struct {
	Type    string         `json:"type"`
	ID      string         `json:"id,omitempty"`
	Level   string         `json:"level"`
	Message string         `json:"message"`
	Fields  map[string]any `json:"fields,omitempty"`
}

type logHandler struct {
	fields map[string]any
	groups []string
}

var stderrHandler = slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelDebug})

// minLevel is the least severe level Unsworn wants, from UNSWORN_LOG_LEVEL.
func minLevel() (slog.Level, bool) {
	switch os.Getenv("UNSWORN_LOG_LEVEL") {
	case "":
		return 0, false
	case "error":
		return slog.LevelError, true
	case "warn":
		return slog.LevelWarn, true
	case "info":
		return slog.LevelInfo, true
	case "debug":
		return slog.LevelDebug, true
	case "trace":
		return slog.LevelDebug - 4, true
	default: // "off"
		return slog.LevelError + 100, true
	}
}

func (h *logHandler) Enabled(ctx context.Context, level slog.Level) bool {
	min, ok := minLevel()
	if !ok || active.Load() == nil {
		return stderrHandler.Enabled(ctx, level)
	}
	return level >= min
}

func (h *logHandler) Handle(ctx context.Context, r slog.Record) error {
	w := active.Load()
	if _, ok := minLevel(); !ok || w == nil {
		return h.toStderr(ctx, r)
	}
	fields := clone(h.fields)
	r.Attrs(func(a slog.Attr) bool {
		insert(fields, h.groups, a)
		return true
	})
	m := logMessage{Type: "log", Level: levelName(r.Level), Message: r.Message}
	if id, ok := ctx.Value(requestIDKey{}).(string); ok {
		m.ID = id
	}
	if len(fields) > 0 {
		m.Fields = fields
	}
	return w.send(m, nil)
}

func (h *logHandler) toStderr(ctx context.Context, r slog.Record) error {
	var out slog.Handler = stderrHandler
	var attrs []slog.Attr
	for k, v := range h.fields {
		attrs = append(attrs, slog.Any(k, v))
	}
	out = out.WithAttrs(attrs)
	for _, g := range h.groups {
		out = out.WithGroup(g)
	}
	return out.Handle(ctx, r)
}

func (h *logHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	fields := clone(h.fields)
	for _, a := range attrs {
		insert(fields, h.groups, a)
	}
	return &logHandler{fields: fields, groups: h.groups}
}

func (h *logHandler) WithGroup(name string) slog.Handler {
	if name == "" {
		return h
	}
	groups := append(append([]string{}, h.groups...), name)
	return &logHandler{fields: h.fields, groups: groups}
}

func levelName(l slog.Level) string {
	switch {
	case l >= slog.LevelError:
		return "error"
	case l >= slog.LevelWarn:
		return "warn"
	case l >= slog.LevelInfo:
		return "info"
	case l >= slog.LevelDebug:
		return "debug"
	default:
		return "trace"
	}
}

// insert adds an attribute to fields, inside the given groups.
func insert(fields map[string]any, groups []string, a slog.Attr) {
	a.Value = a.Value.Resolve()
	if a.Equal(slog.Attr{}) {
		return
	}
	for _, g := range groups {
		next, ok := fields[g].(map[string]any)
		if !ok {
			next = map[string]any{}
			fields[g] = next
		}
		fields = next
	}
	if a.Value.Kind() == slog.KindGroup {
		inner := fields
		if a.Key != "" {
			next, ok := fields[a.Key].(map[string]any)
			if !ok {
				next = map[string]any{}
				fields[a.Key] = next
			}
			inner = next
		}
		for _, ga := range a.Value.Group() {
			insert(inner, nil, ga)
		}
		return
	}
	fields[a.Key] = plainValue(a.Value)
}

func plainValue(v slog.Value) any {
	switch v.Kind() {
	case slog.KindTime:
		return v.Time().Format(time.RFC3339Nano)
	case slog.KindDuration:
		return v.Duration().String()
	case slog.KindAny:
		x := v.Any()
		if err, ok := x.(error); ok {
			return err.Error()
		}
		if _, err := json.Marshal(x); err != nil {
			return fmt.Sprint(x)
		}
		return x
	default:
		return v.Any()
	}
}

func clone(m map[string]any) map[string]any {
	out := make(map[string]any, len(m))
	for k, v := range m {
		if inner, ok := v.(map[string]any); ok {
			v = clone(inner)
		}
		out[k] = v
	}
	return out
}

// installLogging makes slog's default logger send to Unsworn, unless the
// program already replaced it.
func installLogging(w *worker) {
	active.Store(w)
	if _, ok := minLevel(); !ok {
		return
	}
	if fmt.Sprintf("%T", slog.Default().Handler()) == "*slog.defaultHandler" {
		slog.SetDefault(slog.New(LogHandler()))
	}
}
