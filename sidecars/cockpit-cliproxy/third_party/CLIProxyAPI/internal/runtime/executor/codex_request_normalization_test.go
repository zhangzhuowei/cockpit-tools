package executor

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"github.com/router-for-me/CLIProxyAPI/v7/internal/config"
	"github.com/tidwall/gjson"
)

func TestNormalizeCodexInstructionsUsesModelBaseInstructions(t *testing.T) {
	for _, body := range [][]byte{
		[]byte(`{"model":"gpt-5.6-sol"}`),
		[]byte(`{"instructions":null}`),
		[]byte(`{"instructions":"  \n\t"}`),
	} {
		got := normalizeCodexInstructions(body, "gpt-5.6-sol")
		if instructions := gjson.GetBytes(got, "instructions").String(); len(instructions) < 100 {
			t.Fatalf("expected non-empty model base instructions, got %q from %s", instructions, body)
		}
	}

	body := []byte(`{"instructions":"keep me"}`)
	if got := gjson.GetBytes(normalizeCodexInstructions(body, "gpt-5.6-sol"), "instructions").String(); got != "keep me" {
		t.Fatalf("explicit instructions = %q, want keep me", got)
	}
}

func TestCodexHTTPAndCompactPreserveReplayNamespaces(t *testing.T) {
	for _, compact := range []bool{false, true} {
		for _, apiKey := range []bool{false, true} {
			t.Run(fmt.Sprintf("compact=%v/api-key=%v", compact, apiKey), func(t *testing.T) {
				captured := make(chan []byte, 1)
				server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
					body, err := io.ReadAll(r.Body)
					if err != nil {
						t.Error(err)
						return
					}
					captured <- body
					if compact {
						w.Header().Set("Content-Type", "application/json")
						_, _ = w.Write([]byte(`{"id":"resp-compact","output":[]}`))
					} else {
						w.Header().Set("Content-Type", "text/event-stream")
						_, _ = fmt.Fprintf(w, "data: %s\n\n", codexCompletedEventBody)
					}
				}))
				defer server.Close()
				auth := apiServiceTestAuth(server.URL)
				if apiKey {
					auth.Attributes["api_key"] = "test"
				}
				req, opts := codexTestRequest()
				req.Payload = namespaceReplayContractPayload()
				if compact {
					opts.Alt = "responses/compact"
				}
				ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
				defer cancel()
				if _, err := NewCodexExecutor(&config.Config{}).Execute(ctx, auth, req, opts); err != nil {
					t.Fatal(err)
				}
				assertNamespaceReplayContract(t, <-captured)
			})
		}
	}
}

func TestCodexWebsocketPreservesReplayNamespacesForAllAuthKinds(t *testing.T) {
	for _, apiKey := range []bool{false, true} {
		captured := make(chan []byte, 1)
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			conn, err := (&websocket.Upgrader{}).Upgrade(w, r, nil)
			if err != nil {
				t.Error(err)
				return
			}
			defer conn.Close()
			if err := conn.SetReadDeadline(time.Now().Add(5 * time.Second)); err != nil {
				t.Error(err)
				return
			}
			_, body, err := conn.ReadMessage()
			if err != nil {
				t.Error(err)
				return
			}
			captured <- body
			_ = conn.WriteMessage(websocket.TextMessage, []byte(codexCompletedEventBody))
		}))
		auth := apiServiceTestAuth(server.URL)
		if apiKey {
			auth.Attributes["api_key"] = "test"
		}
		req, opts := codexTestRequest()
		req.Payload = namespaceReplayContractPayload()
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_, err := NewCodexWebsocketsExecutor(&config.Config{}).Execute(ctx, auth, req, opts)
		cancel()
		server.Close()
		if err != nil {
			t.Fatal(err)
		}
		assertNamespaceReplayContract(t, <-captured)
	}
}

func namespaceReplayContractPayload() []byte {
	return []byte(`{"model":"gpt-5.6-terra","input":[{"type":"function_call","call_id":"c1","name":"lookup","namespace":"functions","arguments":"{}"},{"type":"function_call_output","call_id":"c1","output":"ok"},{"type":"message","role":"user","namespace":"client-extension","content":"hello"}]}`)
}

func assertNamespaceReplayContract(t *testing.T, body []byte) {
	t.Helper()
	if gjson.GetBytes(body, "input.0.namespace").String() != "functions" ||
		gjson.GetBytes(body, "input.2.namespace").String() != "client-extension" ||
		gjson.GetBytes(body, "input.1.call_id").String() != "c1" {
		t.Fatalf("replay metadata changed before upstream: %s", body)
	}
}
