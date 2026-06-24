---
name: cyberstrike-parrot-workflow
description: CyberStrikeAI project workflow. Use after any code, config, script, Web, Go/Python tool, test, build/runtime, or project skills change in this repository; local work may use only LSP/static editor diagnostics, while all tests, builds, service runs, curl/browser checks, and runtime validation must run on the Parrot VM via rsync + ssh.
---

# CyberStrikeAI Parrot Workflow

This is a project-scoped Codex skill for CyberStrikeAI. It defines the delivery bar after modifying this repository: edit locally, use only local LSP/static diagnostics for quick feedback, then synchronize to the Parrot VM and perform all executable validation there.

## When To Use

Use this skill whenever working in the CyberStrikeAI repository and any of these are true:

- You modify Go, Python, Shell, HTML, JS, CSS, templates, config, tests, build scripts, runtime scripts, tools, MCP code, or project `skills/`.
- The user asks to implement, fix, refactor, build, test, verify, run, debug, deploy, synchronize, or deliver code.
- The task mentions Parrot, `rsync`, `ssh`, `sshpass`, `192.168.64.2`, or `/home/user/CyberStrikeAI/`.

Do not force this workflow for pure read-only explanation, pure planning, or code review where the user explicitly does not want changes or validation.

## Local Rules

Local filesystem edits happen in the current workspace. Before changing code, inspect related files and call sites.

Local validation is limited to non-runtime editor/static feedback:

- Allowed locally: reading files, `rg`, `git diff`, `git status`, formatting inspection, LSP diagnostics if available, type/navigation queries provided by the editor.
- Not allowed locally for delivery validation: `go test`, `go vet`, `go build`, `go run`, app/server startup, curl/API checks, browser UI checks, Python script execution, shell integration tests, npm test/build, or any runtime behavior check.

If a command would compile, execute tests, start services, hit APIs, or otherwise validate runtime behavior, run it only on Parrot after syncing.

## Remote Environment

- Host: `user@192.168.64.2`
- Remote project directory: `/home/user/CyberStrikeAI/`
- Authoritative validation location: the remote project directory on Parrot
- Web/UI validation URL from the local machine: `https://192.168.64.2:51282/`

Prefer SSH keys or an existing ssh-agent. If password login is unavoidable, use `sshpass -e` with a runtime environment variable. Never write passwords or tokens into repository files, scripts, logs, or final replies.

## Sync Step

After local edits and before executable validation, synchronize from the repository root:

```bash
rsync -az --delete \
  --exclude '.git/' \
  --exclude 'venv/' \
  --exclude 'tmp/' \
  --exclude 'data/' \
  --exclude 'chat_uploads/' \
  --exclude '.upgrade-backup/' \
  --exclude '.sisyphus/' \
  --exclude 'cyberstrike-ai' \
  --exclude 'cyberstrike-ai.linux-arm64' \
  --exclude 'config.yaml' \
  ./ user@192.168.64.2:/home/user/CyberStrikeAI/
```

`config.yaml` is excluded by default because it may contain local credentials and environment-specific ports. If a config change is required for remote validation, warn the user that it may contain sensitive or machine-specific values and get explicit confirmation before syncing it.

## Parrot Validation

Choose validation based on the change, from narrow to broad. These commands are examples to run through SSH in `/home/user/CyberStrikeAI/`:

```bash
ssh user@192.168.64.2 'cd /home/user/CyberStrikeAI && go test ./...'
ssh user@192.168.64.2 'cd /home/user/CyberStrikeAI && go vet ./...'
ssh user@192.168.64.2 'cd /home/user/CyberStrikeAI && go build -o /tmp/cyberstrike-ai cmd/server/main.go'
```

Additional validation:

- Go dependency changes: run `go mod download`, then relevant `go test` and build commands.
- MCP entrypoint changes: build `cmd/mcp-stdio/main.go`.
- Python tools or scripts: run syntax or unit-level checks on Parrot, for example `python3 -m compileall <changed-paths>`.
- Web, handler, template, or frontend changes: start or use the service on Parrot, then validate affected pages/APIs. API checks should run against the Parrot service; browser UI checks may be driven from the local machine by opening `https://192.168.64.2:51282/`. Record the port/log path and clean up any process started only for validation.
- Project `skills/` changes: verify each changed `SKILL.md` frontmatter, folder/name consistency, and readability on Parrot.

If a remote command fails, decide whether it is caused by the current changes. Fix current-change failures, re-sync, and re-run the remote validation. For remote environment gaps or pre-existing failures, report the exact command and key error.

## Final Response Requirements

When code or runtime-affecting files changed, the final response must include:

- What changed.
- Whether `rsync` to Parrot was completed.
- The Parrot commands run and their results.
- Any validation that could not be run and why.
- Whether failures were fixed, still blocking, or judged pre-existing.

Do not present a code change as delivered based only on local checks.
