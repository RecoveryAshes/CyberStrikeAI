package handler

import (
	"strings"

	"cyberstrike-ai/internal/config"
)

const defaultRoleName = "默认"

func normalizeConfiguredRoleName(roleName string) string {
	name := strings.TrimSpace(roleName)
	if name == "" {
		return defaultRoleName
	}
	return name
}

func resolveConfiguredRole(cfg *config.Config, roleName string) (config.RoleConfig, string, bool) {
	name := normalizeConfiguredRoleName(roleName)
	if cfg == nil || cfg.Roles == nil {
		return config.RoleConfig{}, name, false
	}
	role, ok := cfg.Roles[name]
	if !ok || !role.Enabled {
		return config.RoleConfig{}, name, false
	}
	return role, name, true
}

func applyConfiguredRole(cfg *config.Config, roleName, message string) (string, []string, string, bool) {
	role, name, ok := resolveConfiguredRole(cfg, roleName)
	if !ok {
		return message, nil, name, false
	}
	finalMessage := message
	if strings.TrimSpace(role.UserPrompt) != "" {
		finalMessage = role.UserPrompt + "\n\n" + message
	}
	return finalMessage, role.Tools, name, true
}
