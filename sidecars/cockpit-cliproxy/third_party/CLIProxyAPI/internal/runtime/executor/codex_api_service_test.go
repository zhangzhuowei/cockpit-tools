package executor

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v7/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v7/internal/registry"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v7/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v7/sdk/cliproxy/executor"
	"github.com/tidwall/gjson"
)

func apiServiceTestAuth(url string) *cliproxyauth.Auth {
	return &cliproxyauth.Auth{
		ID: "api-service-capacity-test", Provider: "codex", Status: cliproxyauth.StatusActive,
		Attributes: map[string]string{"base_url": url},
		Metadata:   map[string]any{"access_token": "test"},
	}
}

func apiServiceTestExecutor(ws bool, cfg *config.Config) cliproxyauth.ProviderExecutor {
	if ws {
		return NewCodexWebsocketsExecutor(cfg)
	}
	return NewCodexExecutor(cfg)
}

func TestCodexAPIServiceCapacityBeforeAndAfterOutput(t *testing.T) {
	for _, ws := range []bool{false, true} {
		for _, generated := range []bool{false, true} {
			for _, failure := range []string{
				codexOverloadEvent,
				`{"type":"error","status":503,"error":{"code":"server_is_overloaded","message":"busy"}}`,
				`{"type":"response.failed","response":{"error":{"code":"slow_down","message":"busy"}}}`,
				`{"type":"response.failed","response":{"error":{"type":"invalid_request_error","message":"Selected model is at capacity. Please try a different model."}}}`,
			} {
				events := []string{codexCreatedEvent}
				if generated {
					events = append(events, codexOutputAddedEvent)
				}
				events = append(events, failure)
				server := codexSSEServer(events...)
				if ws {
					server.Close()
					server = codexWebsocketServer(t, events...)
				}
				cfg := codexBufferingConfig(true)
				cfg.Codex.APIServiceCompatibility = true
				req, opts := codexTestRequest()
				ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
				result, err := apiServiceTestExecutor(ws, cfg).ExecuteStream(ctx, apiServiceTestAuth(server.URL), req, opts)
				if generated {
					if err != nil || result == nil {
						t.Fatalf("ws=%v: must not retry after generated output: %v", ws, err)
					}
					var output string
					output, err = drainChunks(result)
					if !strings.Contains(output, "response.output_item.added") {
						t.Fatalf("ws=%v: output lost: %s", ws, output)
					}
				} else if result != nil || err == nil {
					t.Fatalf("ws=%v: capacity failure must precede any downstream stream", ws)
				}
				var transient interface{ IsTransientRequestScoped() bool }
				if !errors.As(err, &transient) || !transient.IsTransientRequestScoped() || statusCodeFromTestError(t, err) != 503 {
					t.Fatalf("ws=%v: missing retryable request-scoped capacity error: %v", ws, err)
				}
				if gjson.Get(err.Error(), "error.code").String() != "server_error" {
					t.Fatalf("unexpected client error: %v", err)
				}
				cancel()
				server.Close()
			}
		}
	}
}

func TestCodexAPIServiceWebsocketCapacityDeliversErrorBeforeDisconnect(t *testing.T) {
	for _, generated := range []bool{false, true} {
		for _, failure := range []string{codexOverloadEvent, `{"type":"error","status":503,"error":{"code":"slow_down"}}`} {
			events := []string{codexCreatedEvent}
			if generated {
				events = append(events, codexOutputAddedEvent)
			}
			events = append(events, failure)
			server := codexWebsocketServerHoldingConnection(t, events...)
			cfg := codexBufferingConfig(true)
			cfg.Codex.APIServiceCompatibility = true
			exec := NewCodexWebsocketsExecutor(cfg)
			exec.store = &codexWebsocketSessionStore{sessions: make(map[string]*codexWebsocketSession)}
			const sessionID = "api-service-capacity-session"
			disconnect := exec.UpstreamDisconnectChan(sessionID)
			req, opts := codexWebsocketRequest()
			opts.Metadata = map[string]any{cliproxyexecutor.ExecutionSessionMetadataKey: sessionID}
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			result, err := exec.ExecuteStream(ctx, apiServiceTestAuth(server.URL), req, opts)
			if result != nil {
				_, err = drainChunks(result)
			}
			if err == nil || statusCodeFromTestError(t, err) != 503 {
				t.Fatalf("capacity error lost: %v", err)
			}
			select {
			case <-disconnect:
				t.Fatal("capacity teardown must not race the downstream error frame")
			default:
			}
			cancel()
			exec.CloseExecutionSession(sessionID)
			server.Close()
		}
	}
}

func TestCodexAPIServiceSameAccountCanRecoverOnRetry(t *testing.T) {
	var calls atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		terminal := codexCompletedEventBody
		if calls.Add(1) == 1 {
			terminal = codexOverloadEvent
		}
		_, _ = fmt.Fprintf(w, "data: %s\n\ndata: %s\n\n", codexCreatedEvent, terminal)
	}))
	defer server.Close()
	cfg := codexBufferingConfig(true)
	cfg.Codex.APIServiceCompatibility = true
	m := cliproxyauth.NewManager(nil, nil, nil)
	m.SetConfig(cfg)
	m.SetRetryConfig(1, 3*time.Second, 1)
	auth := apiServiceTestAuth(server.URL)
	req, opts := codexTestRequest()
	registry.GetGlobalRegistry().RegisterClient(auth.ID, "codex", []*registry.ModelInfo{{ID: req.Model}})
	defer registry.GetGlobalRegistry().UnregisterClient(auth.ID)
	if _, err := m.Register(context.Background(), auth); err != nil {
		t.Fatal(err)
	}
	m.RegisterExecutor(NewCodexExecutor(cfg))
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	result, err := m.ExecuteStream(ctx, []string{"codex"}, req, opts)
	if err != nil {
		t.Fatal(err)
	}
	body, err := drainChunks(result)
	if err != nil || calls.Load() != 2 || !strings.Contains(body, "hello") || strings.Contains(body, "server_is_overloaded") || strings.Count(body, "response.created") != 1 {
		t.Fatalf("retry must expose only the successful attempt: calls=%d err=%v body=%s", calls.Load(), err, body)
	}
}

func TestCodexAPIServiceLeavesAPIKeyStreamsAndQuotaFailuresUnchanged(t *testing.T) {
	for _, ws := range []bool{false, true} {
		for _, apiKey := range []bool{false, true} {
			failure := `{"type":"error","error":{"type":"usage_limit_reached","resets_in_seconds":60,"message":"quota exhausted"}}`
			if apiKey {
				failure = codexOverloadEvent
			}
			server := codexSSEServer(codexCreatedEvent, failure)
			if ws {
				server.Close()
				server = codexWebsocketServer(t, codexCreatedEvent, failure)
			}
			cfg := codexBufferingConfig(true)
			cfg.Codex.APIServiceCompatibility = true
			auth := apiServiceTestAuth(server.URL)
			if apiKey {
				auth.Attributes["api_key"] = "test"
			}
			req, opts := codexTestRequest()
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			result, err := apiServiceTestExecutor(ws, cfg).ExecuteStream(ctx, auth, req, opts)
			if err != nil || result == nil {
				t.Fatalf("ws=%v apiKey=%v: legacy stream semantics changed: %v", ws, apiKey, err)
			}
			_, err = drainChunks(result)
			wantStatus := 429
			if apiKey {
				wantStatus = 502
			}
			if err == nil || statusCodeFromTestError(t, err) != wantStatus {
				t.Fatalf("ws=%v apiKey=%v: expected status %d, got %v", ws, apiKey, wantStatus, err)
			}
			var transient interface{ IsTransientRequestScoped() bool }
			if errors.As(err, &transient) && transient.IsTransientRequestScoped() {
				t.Fatal("quota and passthrough errors must not become request-scoped capacity failures")
			}
			cancel()
			server.Close()
		}
	}
}

type countingCapacityExecutor struct {
	cliproxyauth.ProviderExecutor
	calls atomic.Int32
}

func (e *countingCapacityExecutor) ExecuteStream(ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (*cliproxyexecutor.StreamResult, error) {
	e.calls.Add(1)
	return e.ProviderExecutor.ExecuteStream(ctx, auth, req, opts)
}

func TestCodexAPIServiceCapacityRetriesAreBoundedWithoutCooling(t *testing.T) {
	for _, ws := range []bool{false, true} {
		server := codexSSEServer(codexCreatedEvent, codexOverloadEvent)
		if ws {
			server.Close()
			server = codexWebsocketServer(t, codexCreatedEvent, codexOverloadEvent)
		}
		cfg := codexBufferingConfig(true)
		cfg.Codex.APIServiceCompatibility = true
		manager := cliproxyauth.NewManager(nil, nil, nil)
		manager.SetConfig(cfg)
		manager.SetRetryConfig(1, 3*time.Second, 1)
		auth := apiServiceTestAuth(server.URL)
		req, opts := codexTestRequest()
		registry.GetGlobalRegistry().RegisterClient(auth.ID, "codex", []*registry.ModelInfo{{ID: req.Model}})
		if _, err := manager.Register(context.Background(), auth); err != nil {
			t.Fatal(err)
		}
		executor := &countingCapacityExecutor{ProviderExecutor: apiServiceTestExecutor(ws, cfg)}
		manager.RegisterExecutor(executor)
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		result, err := manager.ExecuteStream(ctx, []string{"codex"}, req, opts)
		if result != nil || err == nil || statusCodeFromTestError(t, err) != 503 {
			t.Fatalf("ws=%v: exhausted capacity retries must return 503: %v", ws, err)
		}
		if executor.calls.Load() != 2 {
			t.Fatalf("ws=%v: attempts=%d, want initial + one retry", ws, executor.calls.Load())
		}
		persisted, _ := manager.GetByID(auth.ID)
		if persisted.Unavailable || persisted.Quota.Exceeded || !persisted.NextRetryAfter.IsZero() {
			t.Fatalf("ws=%v: capacity failure cooled the account", ws)
		}
		for _, state := range persisted.ModelStates {
			if state.Unavailable || state.Quota.Exceeded || !state.NextRetryAfter.IsZero() {
				t.Fatalf("ws=%v: capacity failure cooled the model", ws)
			}
		}
		cancel()
		registry.GetGlobalRegistry().UnregisterClient(auth.ID)
		server.Close()
	}
}

func TestCodexAPIServicePairsVersionWithoutChangingAPIKeysOrInstanceGateways(t *testing.T) {
	for _, enabled := range []bool{false, true} {
		for _, apiKey := range []bool{false, true} {
			cfg := &config.Config{Codex: config.CodexConfig{APIServiceCompatibility: enabled}}
			for _, ws := range []bool{false, true} {
				headers := http.Header{"User-Agent": {"codex_cli_rs/0.140.0"}, "Version": {"0.140.0"}, "Originator": {"codex_cli_rs"}}
				auth := apiServiceTestAuth("")
				if apiKey {
					auth.Attributes["api_key"] = "test"
				}
				var got http.Header
				if ws {
					got = applyCodexWebsocketHeaders(context.Background(), http.Header{}, auth, "test", cfg, headers)
				} else {
					req, _ := http.NewRequest(http.MethodPost, "http://localhost/responses", nil)
					applyCodexHeadersFromSources(req, auth, "test", true, cfg, headers)
					got = req.Header
				}
				wantVersion := "0.140.0"
				if enabled && !apiKey {
					_, rest, _ := strings.Cut(codexUserAgent, "/")
					wantVersion, _, _ = strings.Cut(rest, " ")
				}
				if got.Get("Version") != wantVersion {
					t.Fatalf("ws=%v enabled=%v apiKey=%v: version=%q want %q", ws, enabled, apiKey, got.Get("Version"), wantVersion)
				}
				if apiKey && (got.Get("User-Agent") != headers.Get("User-Agent") || got.Get("Originator") != headers.Get("Originator")) {
					t.Fatal("API key identity must remain unchanged")
				}
			}
		}
	}
}
