package helps

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"testing"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v7/internal/config"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v7/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v7/sdk/cliproxy/executor"
	"github.com/tidwall/gjson"
)

func TestCodexCapacityClassification(t *testing.T) {
	for _, tc := range []struct {
		body string
		want bool
	}{
		{`{"error":{"code":"server_is_overloaded"}}`, true},
		{`{"response":{"error":{"code":"slow_down"}}}`, true},
		{`{"error":{"type":"invalid_request_error","message":"Selected model is at capacity. Please try a different model."}}`, true},
		{`{"error":{"message":"Our servers are currently overloaded. Please try again later."}}`, true},
		{`{"error":{"type":"usage_limit_reached","message":"Selected model is at capacity"}}`, false},
		{`{"error":{"code":"rate_limit_exceeded","message":"Selected model is at capacity"}}`, false},
		{`{"error":{"code":"context_length_exceeded"},"echo":"Selected model is at capacity"}`, false},
		{`{"error":{"code":"cyber_policy","message":"Selected model is at capacity"}}`, false},
		{`{"error":{"type":"authentication_error","message":"Selected model is at capacity"}}`, false},
		{`{"error":{"code":"invalid_value"},"echo":"server_is_overloaded"}`, false},
		{`{"message":"Selected model is at capacity"}`, false},
		{`Selected model is at capacity`, false},
	} {
		t.Run(tc.body, func(t *testing.T) {
			if got := IsCodexCapacityFailure([]byte(tc.body)); got != tc.want {
				t.Fatalf("classification = %v, want %v", got, tc.want)
			}
		})
	}
}

type capacityTestError struct{ status int }

func (e capacityTestError) Error() string {
	return `{"error":{"code":"server_is_overloaded","message":"original overload message","request_id":"test-id"}}`
}
func (e capacityTestError) StatusCode() int { return e.status }
func (e capacityTestError) Headers() http.Header {
	return http.Header{"Retry-After": {"1"}}
}

func TestCodexCapacityNormalizationPreservesCauseAndMessage(t *testing.T) {
	original := capacityTestError{502}
	err := NormalizeCodexCapacityError(original)
	var capacity *codexCapacityError
	if !errors.As(err, &capacity) || !errors.Is(err, original) {
		t.Fatalf("original cause or capacity classification lost: %v", err)
	}
	if capacity.StatusCode() != 503 || !capacity.IsTransientRequestScoped() {
		t.Fatal("capacity error must remain retryable without credential cooldown")
	}
	if gjson.Get(err.Error(), "error.code").String() != "server_error" ||
		gjson.Get(err.Error(), "error.message").String() != "original overload message" ||
		gjson.Get(err.Error(), "error.request_id").String() != "test-id" ||
		capacity.Headers().Get("Retry-After") != "1" {
		t.Fatalf("client error lost fields: %v", err)
	}
	if NormalizeCodexCapacityError(err) != err {
		t.Fatal("normalization must be idempotent")
	}
	for _, rawDelay := range []string{"5", time.Now().Add(time.Minute).UTC().Format(http.TimeFormat)} {
		err := NormalizeCodexCapacityError(original, http.Header{"Retry-After": {rawDelay}})
		delay := err.(interface{ RetryAfter() *time.Duration }).RetryAfter()
		if delay == nil || *delay < 5*time.Second || *delay > time.Minute {
			t.Fatalf("Retry-After %q was not preserved: %v", rawDelay, delay)
		}
	}
	for _, status := range []int{401, 403} {
		original := capacityTestError{status}
		if NormalizeCodexCapacityError(original) != original {
			t.Fatalf("must not reinterpret HTTP %d authentication failures", status)
		}
	}
	for _, original := range []error{context.Canceled, context.DeadlineExceeded, fmt.Errorf("cancel: %w", context.Canceled)} {
		if NormalizeCodexCapacityError(original) != original {
			t.Fatal("cancellation must not become a retryable failure")
		}
	}
}

func TestCodexAPIServiceCompatibilityIsOptInAndOAuthOnly(t *testing.T) {
	cfg := &config.Config{Codex: config.CodexConfig{APIServiceCompatibility: true}}
	oauth := &cliproxyauth.Auth{Metadata: map[string]any{"access_token": "test"}}
	if !CodexAPIServiceCompatibilityEnabled(cfg, oauth) {
		t.Fatal("expected API Service OAuth compatibility")
	}
	for _, auth := range []*cliproxyauth.Auth{nil, {Attributes: map[string]string{"api_key": "test"}}} {
		if CodexAPIServiceCompatibilityEnabled(cfg, auth) {
			t.Fatal("must not enable compatibility for API keys or missing auth")
		}
	}
	if CodexAPIServiceCompatibilityEnabled(nil, oauth) || CodexAPIServiceCompatibilityEnabled(&config.Config{}, oauth) {
		t.Fatal("instance gateways must keep the old default")
	}
}

func TestCodexCapacityStreamPreservesOrderAndCancels(t *testing.T) {
	input := make(chan cliproxyexecutor.StreamChunk, 2)
	input <- cliproxyexecutor.StreamChunk{Payload: []byte("already generated")}
	input <- cliproxyexecutor.StreamChunk{Err: capacityTestError{502}}
	close(input)
	result := NormalizeCodexCapacityStream(context.Background(), &cliproxyexecutor.StreamResult{Chunks: input})
	if chunk := <-result.Chunks; string(chunk.Payload) != "already generated" {
		t.Fatal("must preserve generated output")
	}
	if chunk := <-result.Chunks; chunk.Err == nil || gjson.Get(chunk.Err.Error(), "error.code").String() != "server_error" {
		t.Fatal("must deliver the terminal error without replay")
	}
	if _, ok := <-result.Chunks; ok {
		t.Fatal("stream must close")
	}
	ctx, cancel := context.WithCancel(context.Background())
	result = NormalizeCodexCapacityStream(ctx, &cliproxyexecutor.StreamResult{Chunks: make(chan cliproxyexecutor.StreamChunk)})
	cancel()
	select {
	case _, ok := <-result.Chunks:
		if ok {
			t.Fatal("canceled stream must not emit data")
		}
	case <-time.After(time.Second):
		t.Fatal("stream adapter did not stop on cancellation")
	}
}
