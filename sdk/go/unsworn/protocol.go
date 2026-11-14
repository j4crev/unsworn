package unsworn

import (
	"bufio"
	"bytes"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
)

// maxFrame matches Unsworn's default frame limit.
const maxFrame = 64 << 20

// inMessage is any message Unsworn sends a worker.
type inMessage struct {
	Type       string            `json:"type"`
	ID         string            `json:"id"`
	Phase      string            `json:"phase"`
	Handler    string            `json:"handler"`
	Route      string            `json:"route"`
	Method     string            `json:"method"`
	Path       string            `json:"path"`
	Params     map[string]any    `json:"params"`
	Query      map[string]any    `json:"query"`
	RawQuery   string            `json:"raw_query"`
	Headers    map[string]string `json:"headers"`
	Context    map[string]any    `json:"context"`
	JSON       json.RawMessage   `json:"json"`
	Text       *string           `json:"text"`
	BodyLen    int64             `json:"body_len"`
	DeadlineMs *int64            `json:"deadline_ms"`
	Handlers   []string          `json:"handlers"`
}

type readyMessage struct {
	Type        string `json:"type"`
	Protocol    int    `json:"protocol"`
	MaxInFlight int    `json:"max_in_flight"`
	Verify      bool   `json:"verify"`
	SDK         string `json:"sdk"`
}

type responseMessage struct {
	Type    string          `json:"type"`
	ID      string          `json:"id"`
	Status  int             `json:"status"`
	Headers map[string]any  `json:"headers,omitempty"`
	JSON    json.RawMessage `json:"json,omitempty"`
	Text    *string         `json:"text,omitempty"`
	BodyLen int             `json:"body_len,omitempty"`
	Stream  bool            `json:"stream,omitempty"`
}

type continueMessage struct {
	Type    string         `json:"type"`
	ID      string         `json:"id"`
	Context map[string]any `json:"context"`
}

type chunkMessage struct {
	Type    string `json:"type"`
	ID      string `json:"id"`
	BodyLen int    `json:"body_len"`
}

type idMessage struct {
	Type    string `json:"type"`
	ID      string `json:"id"`
	Message string `json:"message,omitempty"`
}

type missingHandler struct {
	Handler string `json:"handler"`
	Reason  string `json:"reason"`
}

type verifiedMessage struct {
	Type    string           `json:"type"`
	ID      string           `json:"id"`
	Missing []missingHandler `json:"missing"`
}

// marshal encodes JSON without escaping <, > and &, so bodies arrive as
// the handler wrote them.
func marshal(v any) ([]byte, error) {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buf.Bytes(), []byte("\n")), nil
}

func unmarshal(data []byte) (*inMessage, error) {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber()
	var m inMessage
	if err := dec.Decode(&m); err != nil {
		return nil, err
	}
	if string(m.JSON) == "null" {
		m.JSON = nil
	}
	return &m, nil
}

// codec reads and writes one tier's encoding.
type codec interface {
	// read returns the next message and its raw body, or io.EOF.
	read() (*inMessage, []byte, error)
	// encode appends a message and its raw body to dst.
	encode(dst []byte, message []byte, body []byte) []byte
}

// jsonlCodec is Tier 1: one JSON message per line, then body_len raw bytes.
type jsonlCodec struct{ r *bufio.Reader }

func (c *jsonlCodec) read() (*inMessage, []byte, error) {
	for {
		line, err := c.r.ReadBytes('\n')
		if err != nil {
			if errors.Is(err, io.EOF) {
				return nil, nil, io.EOF
			}
			return nil, nil, err
		}
		line = bytes.TrimSpace(line)
		if len(line) == 0 {
			continue
		}
		m, err := unmarshal(line)
		if err != nil {
			return nil, nil, fmt.Errorf("invalid message: %w", err)
		}
		var body []byte
		if m.BodyLen > 0 {
			if m.BodyLen > maxFrame {
				return nil, nil, fmt.Errorf("body of %d bytes is too large", m.BodyLen)
			}
			body = make([]byte, m.BodyLen)
			if _, err := io.ReadFull(c.r, body); err != nil {
				return nil, nil, err
			}
		}
		return m, body, nil
	}
}

func (c *jsonlCodec) encode(dst, message, body []byte) []byte {
	dst = append(dst, message...)
	dst = append(dst, '\n')
	return append(dst, body...)
}

// framedCodec is Tier 2: [frame_len u32][json_len u32][json][raw body],
// both lengths big-endian.
type framedCodec struct{ r *bufio.Reader }

func (c *framedCodec) read() (*inMessage, []byte, error) {
	var head [4]byte
	if _, err := io.ReadFull(c.r, head[:]); err != nil {
		if errors.Is(err, io.ErrUnexpectedEOF) {
			return nil, nil, io.EOF
		}
		return nil, nil, err
	}
	frameLen := binary.BigEndian.Uint32(head[:])
	if frameLen < 4 || frameLen > maxFrame {
		return nil, nil, fmt.Errorf("invalid frame length %d", frameLen)
	}
	frame := make([]byte, frameLen)
	if _, err := io.ReadFull(c.r, frame); err != nil {
		return nil, nil, err
	}
	jsonLen := binary.BigEndian.Uint32(frame[:4])
	if uint64(jsonLen)+4 > uint64(frameLen) {
		return nil, nil, fmt.Errorf("invalid JSON length %d", jsonLen)
	}
	m, err := unmarshal(frame[4 : 4+jsonLen])
	if err != nil {
		return nil, nil, fmt.Errorf("invalid message: %w", err)
	}
	var body []byte
	if rest := frame[4+jsonLen:]; len(rest) > 0 {
		body = rest
	}
	return m, body, nil
}

func (c *framedCodec) encode(dst, message, body []byte) []byte {
	dst = binary.BigEndian.AppendUint32(dst, uint32(4+len(message)+len(body)))
	dst = binary.BigEndian.AppendUint32(dst, uint32(len(message)))
	dst = append(dst, message...)
	return append(dst, body...)
}
