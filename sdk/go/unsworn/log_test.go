package unsworn

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"testing"
)

func TestLogHandlerSendsRecords(t *testing.T) {
	t.Setenv("UNSWORN_LOG_LEVEL", "info")
	var out bytes.Buffer
	w := &worker{codec: &jsonlCodec{r: bufio.NewReader(&bytes.Buffer{})}, out: &out}
	active.Store(w)
	defer active.Store(nil)

	logger := slog.New(LogHandler()).With("svc", "billing").WithGroup("card")
	ctx := context.WithValue(context.Background(), requestIDKey{}, "r-7")
	logger.InfoContext(ctx, "charged", "amount", 1200, slog.Group("meta", "retry", false), "err", errors.New("slow"))
	logger.Debug("dropped below the level")

	var m logMessage
	if err := json.Unmarshal(bytes.TrimSpace(out.Bytes()), &m); err != nil {
		t.Fatalf("want exactly one message, got %q: %v", out.String(), err)
	}
	if m.Type != "log" || m.ID != "r-7" || m.Level != "info" || m.Message != "charged" {
		t.Fatalf("unexpected message: %+v", m)
	}
	fields, _ := json.Marshal(m.Fields)
	want := `{"card":{"amount":1200,"err":"slow","meta":{"retry":false}},"svc":"billing"}`
	if string(fields) != want {
		t.Errorf("fields: got %s, want %s", fields, want)
	}
}
