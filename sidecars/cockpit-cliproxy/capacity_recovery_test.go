package main

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v7/internal/runtime/executor/helps"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v7/sdk/translator"
	"github.com/tidwall/gjson"
)

func TestCapacityFailureReturnsRetryableHTTPAndStreamErrors(t *testing.T) {
	err := helps.NormalizeCodexCapacityError(relayStatusError{
		status:  502,
		message: `{"error":{"code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later."}}`,
	})
	w := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(w)
	c.Request = httptest.NewRequest(http.MethodPost, "/v1/responses", nil)
	(&relayServer{}).writeExecutorError(c, err)
	if w.Code != 503 || gjson.Get(w.Body.String(), "error.code").String() != "server_error" ||
		gjson.Get(w.Body.String(), "error.message").String() != "Our servers are currently overloaded. Please try again later." {
		t.Fatalf("unexpected downstream capacity response: %d %s", w.Code, w.Body.String())
	}
	w = httptest.NewRecorder()
	c, _ = gin.CreateTestContext(w)
	writeStreamTerminalErrorForFormat(c, err, sdktranslator.FormatOpenAIResponse)
	if !strings.Contains(w.Body.String(), "server_error") || strings.Contains(w.Body.String(), "server_is_overloaded") {
		t.Fatalf("unexpected downstream capacity stream: %s", w.Body.String())
	}
}
