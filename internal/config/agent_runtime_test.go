package config

import (
	"path/filepath"
	"testing"
)

func TestNormalizeAgentModeAgentRuntime(t *testing.T) {
	for _, input := range []string{"codex", "codex_runtime", "agent_runtime"} {
		if got := NormalizeAgentMode(input); got != "agent_runtime" {
			t.Fatalf("NormalizeAgentMode(%q) = %q, want agent_runtime", input, got)
		}
	}
}

func TestNormalizeRobotAgentModeAllowsAgentRuntime(t *testing.T) {
	got := NormalizeRobotAgentMode(MultiAgentConfig{RobotDefaultAgentMode: "agent_runtime"})
	if got != "agent_runtime" {
		t.Fatalf("NormalizeRobotAgentMode(agent_runtime) = %q, want agent_runtime", got)
	}
}

func TestNormalizeBatchAgentModeAllowsAgentRuntime(t *testing.T) {
	for _, input := range []string{"agent_runtime", "codex", "codex_runtime"} {
		if got := NormalizeBatchAgentMode(input); got != "agent_runtime" {
			t.Fatalf("NormalizeBatchAgentMode(%q) = %q, want agent_runtime", input, got)
		}
	}
	if got := NormalizeBatchAgentMode("plan_execute"); got != "plan_execute" {
		t.Fatalf("NormalizeBatchAgentMode(plan_execute) = %q", got)
	}
}

func TestAgentRuntimeBinaryPathEffective(t *testing.T) {
	cfg := AgentRuntimeConfig{BinaryPath: "agent-runtime/bin"}
	got := cfg.BinaryPathEffective("/tmp/project")
	want := filepath.Join("/tmp/project", "agent-runtime/bin")
	if got != want {
		t.Fatalf("BinaryPathEffective = %q, want %q", got, want)
	}
}

func TestAgentRuntimeCompactionDefaults(t *testing.T) {
	cfg := AgentRuntimeConfig{}
	if got := cfg.CompactionThresholdCharsEffective(); got != 40000 {
		t.Fatalf("CompactionThresholdCharsEffective = %d, want 40000", got)
	}
	if got := cfg.CompactionKeepRecentMessagesEffective(); got != 8 {
		t.Fatalf("CompactionKeepRecentMessagesEffective = %d, want 8", got)
	}
	cfg.CompactionThresholdChars = 123
	cfg.CompactionKeepRecentMessages = 4
	if got := cfg.CompactionThresholdCharsEffective(); got != 123 {
		t.Fatalf("CompactionThresholdCharsEffective override = %d, want 123", got)
	}
	if got := cfg.CompactionKeepRecentMessagesEffective(); got != 4 {
		t.Fatalf("CompactionKeepRecentMessagesEffective override = %d, want 4", got)
	}
}

func TestAgentRuntimeTransportDefaults(t *testing.T) {
	cfg := AgentRuntimeConfig{}
	if got := cfg.TransportEffective(); got != "jsonl" {
		t.Fatalf("TransportEffective = %q, want jsonl", got)
	}
	if got := cfg.GRPCListenEffective(); got != "127.0.0.1:0" {
		t.Fatalf("GRPCListenEffective = %q, want 127.0.0.1:0", got)
	}
	if got := cfg.RedisPrefixEffective(); got != "csai:agent_runtime:" {
		t.Fatalf("RedisPrefixEffective = %q, want csai:agent_runtime:", got)
	}

	cfg.Transport = "grpc"
	cfg.GRPCListen = "127.0.0.1:19090"
	cfg.RedisPrefix = "custom:"
	if got := cfg.TransportEffective(); got != "grpc" {
		t.Fatalf("TransportEffective override = %q, want grpc", got)
	}
	if got := cfg.GRPCListenEffective(); got != "127.0.0.1:19090" {
		t.Fatalf("GRPCListenEffective override = %q, want 127.0.0.1:19090", got)
	}
	if got := cfg.RedisPrefixEffective(); got != "custom:" {
		t.Fatalf("RedisPrefixEffective override = %q, want custom:", got)
	}
}

func TestAgentRuntimeSkillsSourceDefaultsToRustDir(t *testing.T) {
	cfg := AgentRuntimeConfig{}
	if got := cfg.SkillsSourceEffective(); got != "rust_dir" {
		t.Fatalf("SkillsSourceEffective = %q, want rust_dir", got)
	}
	cfg.SkillsSource = "go_context"
	if got := cfg.SkillsSourceEffective(); got != "go_context" {
		t.Fatalf("SkillsSourceEffective override = %q, want go_context", got)
	}
	cfg.SkillsSource = "invalid"
	if got := cfg.SkillsSourceEffective(); got != "rust_dir" {
		t.Fatalf("SkillsSourceEffective invalid = %q, want rust_dir", got)
	}
}

func TestAgentRuntimeEffectiveFallsBackToDeprecatedCodexAlias(t *testing.T) {
	cfg := Config{CodexRuntime: AgentRuntimeConfig{Enabled: true, MaxSteps: 12}}
	got := cfg.AgentRuntimeEffective()
	if !got.Enabled || got.MaxSteps != 12 {
		t.Fatalf("AgentRuntimeEffective fallback = %#v", got)
	}
	cfg.AgentRuntime = AgentRuntimeConfig{Enabled: true, MaxSteps: 99}
	got = cfg.AgentRuntimeEffective()
	if got.MaxSteps != 99 {
		t.Fatalf("AgentRuntimeEffective primary = %#v", got)
	}
}

func TestAuthDisabledEffective(t *testing.T) {
	t.Setenv("CYBERSTRIKE_AUTH_DISABLED", "")
	if (AuthConfig{}).DisabledEffective() {
		t.Fatalf("auth should be enabled by default")
	}

	if (AuthConfig{Disabled: true}).DisabledEffective() != true {
		t.Fatalf("auth.disabled should disable auth")
	}

	t.Setenv("CYBERSTRIKE_AUTH_DISABLED", "1")
	if !(AuthConfig{}).DisabledEffective() {
		t.Fatalf("CYBERSTRIKE_AUTH_DISABLED=1 should disable auth")
	}
}
