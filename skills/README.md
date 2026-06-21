# Skills 目录（Agent Skills / Provider Runtime）

- 每个技能为**子目录**，根上必须有 **`SKILL.md`**（YAML front matter：`name`、`description` + Markdown 正文），见 [agentskills.io](https://agentskills.io/specification.md)。
- **目录名须与 `name` 一致**。
- **运行时加载**：在 Provider Runtime 会话中由内置 **`skill` / `load_skill`** 工具渐进披露：工具目录和 `action=list` 只暴露各 skill 的 name/description，模型显式调用 load 后才拉取对应 `SKILL.md` 正文。
- **Web 管理**：HTTP `/api/skills/*` 仍用于列表、编辑、上传包内文件（实现为 `internal/skillpackage`，非 MCP）。
- **Provider Runtime 配置**：运行侧配置位于 `multi_agent.runtime_skills`；旧配置键不作为当前 skill 加载路径。
