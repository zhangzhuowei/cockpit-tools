package helps

import (
	"context"
	"errors"
	"net/http"
	"strings"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v7/internal/config"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v7/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v7/sdk/cliproxy/executor"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

func CodexAPIServiceCompatibilityEnabled(cfg *config.Config, auth *cliproxyauth.Auth) bool {
	return cfg != nil && cfg.Codex.APIServiceCompatibility && auth != nil &&
		auth.AuthKind() != cliproxyauth.AuthKindAPIKey && strings.TrimSpace(auth.Attributes["api_key"]) == ""
}

// Only explicit error fields are authoritative; echoed request text must never
// turn quota, authentication, or policy failures into capacity retries.
func IsCodexCapacityFailure(body []byte) bool {
	if !gjson.ValidBytes(body) {
		return false
	}
	for _, path := range []string{"error", "response.error"} {
		node := gjson.GetBytes(body, path)
		if !node.IsObject() {
			continue
		}
		code := strings.ToLower(strings.TrimSpace(node.Get("code").String()))
		kind := strings.ToLower(strings.TrimSpace(node.Get("type").String()))
		if code != "" && code != "server_is_overloaded" && code != "slow_down" {
			return false
		}
		switch kind {
		case "usage_limit_reached", "rate_limit_error", "rate_limit_exceeded", "authentication_error", "permission_error":
			return false
		}
		if code == "server_is_overloaded" || code == "slow_down" {
			return true
		}
		message := strings.ToLower(node.Get("message").String())
		if strings.Contains(message, "selected model is at capacity") ||
			strings.Contains(message, "servers are currently overloaded") ||
			strings.Contains(message, "servers are overloaded") ||
			strings.Contains(message, "server is overloaded") {
			return true
		}
	}
	return false
}

type codexCapacityError struct {
	cause      error
	clientBody string
	headers    http.Header
}

func (e *codexCapacityError) Error() string                  { return e.clientBody }
func (e *codexCapacityError) Unwrap() error                  { return e.cause }
func (e *codexCapacityError) StatusCode() int                { return http.StatusServiceUnavailable }
func (e *codexCapacityError) IsTransientRequestScoped() bool { return true }
func (e *codexCapacityError) RetryAfter() *time.Duration {
	var source interface{ RetryAfter() *time.Duration }
	if errors.As(e.cause, &source) {
		if delay := source.RetryAfter(); delay != nil {
			return delay
		}
	}
	now := time.Now()
	if deadline, ok := parseRetryAfterHeader(e.Headers().Get("Retry-After"), now); ok && deadline.After(now) {
		delay := deadline.Sub(now)
		return &delay
	}
	return nil
}
func (e *codexCapacityError) Headers() http.Header {
	if e.headers != nil {
		return e.headers.Clone()
	}
	var source interface{ Headers() http.Header }
	if errors.As(e.cause, &source) {
		return source.Headers().Clone()
	}
	return nil
}

func NormalizeCodexCapacityError(err error, headers ...http.Header) error {
	if err == nil || errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return err
	}
	var existing *codexCapacityError
	if errors.As(err, &existing) {
		return err
	}
	var status interface{ StatusCode() int }
	if errors.As(err, &status) {
		switch status.StatusCode() {
		case http.StatusUnauthorized, http.StatusForbidden:
			return err
		}
	}
	body := []byte(err.Error())
	if !IsCodexCapacityFailure(body) {
		return err
	}
	// Keep the original failure reachable for diagnostics. The client still gets
	// an error and the original message, but not a terminal capacity error code.
	for _, path := range []string{"error", "response.error"} {
		if gjson.GetBytes(body, path).IsObject() {
			var editErr error
			body, editErr = sjson.SetBytes(body, path+".code", "server_error")
			if editErr != nil {
				return err
			}
			body, editErr = sjson.SetBytes(body, path+".type", "server_error")
			if editErr != nil {
				return err
			}
		}
	}
	capacity := &codexCapacityError{cause: err, clientBody: string(body)}
	if len(headers) > 0 {
		capacity.headers = headers[0].Clone()
	}
	return capacity
}

// This adapter only changes terminal errors. It never replays a stream after
// any chunk has been emitted, and preserves payload ordering and backpressure.
func NormalizeCodexCapacityStream(ctx context.Context, result *cliproxyexecutor.StreamResult) *cliproxyexecutor.StreamResult {
	if result == nil || result.Chunks == nil {
		return result
	}
	if ctx == nil {
		ctx = context.Background()
	}
	out := make(chan cliproxyexecutor.StreamChunk)
	wrapped := *result
	wrapped.Chunks = out
	go func() {
		defer close(out)
		for {
			select {
			case <-ctx.Done():
				return
			case chunk, ok := <-result.Chunks:
				if !ok {
					return
				}
				chunk.Err = NormalizeCodexCapacityError(chunk.Err)
				select {
				case <-ctx.Done():
					return
				case out <- chunk:
				}
			}
		}
	}()
	return &wrapped
}
