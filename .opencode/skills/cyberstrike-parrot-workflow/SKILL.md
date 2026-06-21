---
name: cyberstrike-parrot-workflow
description: "CyberStrikeAI 项目专用工作流技能；必须在本仓库的代码、配置、脚本、Web、Go/Python 工具、测试或 skills 变更后使用。完成代码后先用 rsync 同步到 user@192.168.64.2:/home/user/CyberStrikeAI/，再通过 ssh 到 Parrot 系统运行 go test/build/lint/手工验证；不要只在本机测试。"
---

# CyberStrikeAI Parrot 同步与远程验证工作流

本技能只服务当前 CyberStrikeAI 项目。它把“代码写完”后的交付门槛固定下来：本地实现可以发生在当前工作区，但最终验证必须在 Parrot VM 上完成，因为用户要求以该系统作为真实测试环境。

## 适用范围

使用本技能处理以下任务：

- 修改 Go、Python、Shell、HTML、JS、CSS、配置、测试、项目内 `skills/` 或构建/运行脚本。
- 用户要求实现、修复、重构、构建、测试、验证、同步、部署到 Parrot、远程运行或交付代码。
- 用户提到 `rsync`、`ssh`、`sshpass`、`Parrot`、`192.168.64.2`、`/home/user/CyberStrikeAI/`。

不要为纯只读解释、纯代码审查、纯计划、纯文档润色且用户明确不要求运行验证的任务强行同步。任何可执行代码或运行配置一旦改变，都必须走同步和远程验证。

## 固定远程环境

- SSH 主机：`user@192.168.64.2`
- 远程项目目录：`/home/user/CyberStrikeAI/`
- 远程系统：Parrot
- 权威测试位置：远程项目目录内

优先使用 SSH key 或已有 ssh-agent。若只能密码登录，可用 `sshpass -e` 从环境变量读取密码，但不要把密码写进仓库文件、脚本、日志或最终回复。命令示例里的 `<password>` 代表运行时输入的凭据，不是要持久化的文本。

## 工作流门槛

### 1. 本地实现门槛

先按项目现有风格完成代码修改。实现前后都要检查相关文件与调用链，不要只改表面症状。可以在本地运行快速诊断或小范围测试来加速反馈，但这些不能替代远程 Parrot 测试。

### 2. 同步到 Parrot 门槛

代码完成后，先从本地仓库根目录同步到远程项目目录。默认同步源代码和项目资源，排除运行态、大体积缓存和本机私有状态：

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
  --exclude 'config.yaml' \
  ./ user@192.168.64.2:/home/user/CyberStrikeAI/
```

如果本次确实修改了 `config.yaml` 并且远程测试必须使用该变更，先提醒用户该文件可能含有本地凭据或远程私有配置，再按用户确认执行定向同步。

密码登录时使用环境变量而不是硬编码：

```bash
SSHPASS='<password>' sshpass -e rsync -az --delete ... ./ user@192.168.64.2:/home/user/CyberStrikeAI/
SSHPASS='<password>' sshpass -e ssh user@192.168.64.2 'cd /home/user/CyberStrikeAI && go test ./...'
```

### 3. 远程测试门槛

同步后，所有交付前测试都通过 SSH 在 Parrot 上运行。先识别本次改动影响范围，再从小到大验证。

基础命令优先级。若远程环境缺少某项工具或既有问题导致失败，记录命令、退出结果和关键错误，不要把未运行项写成通过：

```bash
ssh user@192.168.64.2 'cd /home/user/CyberStrikeAI && go test ./...'
ssh user@192.168.64.2 'cd /home/user/CyberStrikeAI && go vet ./...'
ssh user@192.168.64.2 'cd /home/user/CyberStrikeAI && go build -o /tmp/cyberstrike-ai cmd/server/main.go'
```

按改动追加验证：

- Go 依赖或模块变化：先运行 `go mod download`，再运行 `go test ./...`。
- MCP 入口变化：运行 `go build -o /tmp/cyberstrike-ai-mcp cmd/mcp-stdio/main.go`。
- Python 工具、`requirements.txt` 或脚本变化：在远程检查 Python 版本、依赖安装路径，并至少运行相关脚本的语法/单元级验证，例如 `python3 -m compileall <changed-paths>`。
- Web、HTTP handler、模板或前端资源变化：在远程启动服务，使用 `curl` 或浏览器验证受影响页面/API。若使用 `./run.sh --http`，记录日志路径和端口，验证后清理后台进程。
- 项目内 `skills/` 变化：检查 `SKILL.md` frontmatter、目录名与 `name` 是否一致，并验证相关 skill 文件能被读取。

远程命令失败时，先判断是否由本次改动引起。本次改动导致的失败要修复、重新 rsync、重新远程验证。明显的远程环境缺失或历史失败要在最终回复中标为阻塞，附上命令和关键错误。

## 交付回复要求

最终回复必须包含以下证据：

- 修改内容摘要。
- 已执行的 `rsync` 同步说明，敏感凭据必须省略。
- Parrot 上执行的测试/构建/手工验证命令及结果。
- 未能执行的验证项及原因。
- 如有失败，说明是已修复、仍阻塞，还是判定为既有问题。

不要说“应该可以”“本地通过所以完成”。没有远程 Parrot 验证证据时，不要宣称代码交付完成。
