package unsworn

import (
	"context"
	"errors"
	"io"
	"sync"
)

// bodyReader is a streamed request body. Each chunk is acknowledged to
// Unsworn once the handler has read all of it, which lets Unsworn send more.
type bodyReader struct {
	w   *worker
	id  string
	ctx context.Context

	mu      sync.Mutex
	ready   chan struct{} // signalled when chunks arrive or the body ends
	chunks  [][]byte
	current []byte
	size    int // length of the chunk being read
	done    bool
	err     error
}

func newBodyReader(w *worker, id string, ctx context.Context) *bodyReader {
	return &bodyReader{w: w, id: id, ctx: ctx, ready: make(chan struct{}, 1)}
}

func (b *bodyReader) push(chunk []byte) {
	b.mu.Lock()
	b.chunks = append(b.chunks, chunk)
	b.mu.Unlock()
	b.signal()
}

func (b *bodyReader) end(err error) {
	b.mu.Lock()
	b.done = true
	if b.err == nil {
		b.err = err
	}
	b.mu.Unlock()
	b.signal()
}

func (b *bodyReader) signal() {
	select {
	case b.ready <- struct{}{}:
	default:
	}
}

func (b *bodyReader) Read(p []byte) (int, error) {
	for {
		b.mu.Lock()
		if len(b.current) == 0 && len(b.chunks) > 0 {
			b.current, b.chunks = b.chunks[0], b.chunks[1:]
			b.size = len(b.current)
		}
		if len(b.current) > 0 {
			n := copy(p, b.current)
			b.current = b.current[n:]
			finished := len(b.current) == 0
			size := b.size
			b.mu.Unlock()
			if finished {
				b.w.send(ackMessage{Type: "ack", ID: b.id, Bytes: size}, nil)
			}
			return n, nil
		}
		if b.done {
			err := b.err
			b.mu.Unlock()
			if err == nil {
				err = io.EOF
			}
			return 0, err
		}
		b.mu.Unlock()
		select {
		case <-b.ready:
		case <-b.ctx.Done():
			return 0, errors.Join(errors.New("unsworn: the request was cancelled"), b.ctx.Err())
		}
	}
}
