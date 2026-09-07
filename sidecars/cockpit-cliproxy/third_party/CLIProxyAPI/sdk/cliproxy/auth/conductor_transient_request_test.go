package auth

import (
	"context"
	"testing"
	"time"

	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v7/sdk/cliproxy/executor"
)

type transientRequestTestError struct{ customStatusError }

func (transientRequestTestError) IsTransientRequestScoped() bool { return true }

func TestTransientRequestRetryHonorsRoundAndWaitBudgets(t *testing.T) {
	m := NewManager(nil, nil, nil)
	ids := registerOverloadAuths(t, m, 1)
	err := transientRequestTestError{overloadStatusError()}
	if isRequestInvalidError(err) || !shouldSkipCredentialCooldown(resultErrorFromError(err)) {
		t.Fatal("transient request error must retry without cooling credentials")
	}
	for _, tc := range []struct {
		rounds   int
		attempt  int
		maxWait  time.Duration
		wantWait time.Duration
		want     bool
	}{
		{1, 0, 3 * time.Second, 300 * time.Millisecond, true},
		{1, 1, 3 * time.Second, 0, false},
		{0, 0, 3 * time.Second, 0, false},
		{1, 0, 0, 0, false},
		{1, 0, 100 * time.Millisecond, 0, false},
		{4, 2, 3 * time.Second, 1200 * time.Millisecond, true},
		{5, 3, 3 * time.Second, 1500 * time.Millisecond, true},
	} {
		m.SetRetryConfig(tc.rounds, tc.maxWait, 1)
		wait, retry := m.shouldRetryAfterErrorWithAttempted(context.Background(), cliproxyexecutor.Options{}, err, tc.attempt,
			[]string{"codex"}, "gpt-5.6-terra", tc.maxWait, 0, tc.rounds, map[string]struct{}{ids[0]: {}})
		if retry != tc.want || wait != tc.wantWait {
			t.Fatalf("rounds=%d attempt=%d maxWait=%s: got (%s,%v), want (%s,%v)", tc.rounds, tc.attempt, tc.maxWait, wait, retry, tc.wantWait, tc.want)
		}
	}
}
