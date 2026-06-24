---
name: asset-mapping
description: Mandatory pre-check before calling MCP tool asset_mapping for controlled Quake + ZoomEye asset mapping from bare domains and IPs only.
version: 1.0.0
---

# asset-mapping

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

Use this skill before calling the MCP tool `asset_mapping`.

## Tool Role

`asset_mapping` is a restricted Quake + ZoomEye aggregation wrapper. It accepts only bare domains and single IP addresses, then internally builds the mapping-engine DSL, queries both engines, normalizes results, deduplicates assets, and writes conversation-level output files.

Use it for:

- domain/IP based cyberspace asset mapping
- collecting URLs for later `nucleiPlus` tool use after loading the `nuclei-plus` skill
- collecting host/ip:port services for later port/service verification
- merging Quake and ZoomEye results without duplicate assets

Do not use it for:

- direct port scanning
- ICP filing lookup
- vulnerability scanning
- arbitrary Quake or ZoomEye DSL searches

## Mandatory Pre-Call Contract

- Always consult this skill first. Do not call `asset_mapping` directly from vague intuition.
- `domains` and `ips` may be used together, but at least one must be non-empty.
- Pass bare domains in `domains`, for example `example.com`.
- Pass single IP addresses in `ips`, for example `1.1.1.1`.
- Never pass URLs, paths, ports, CIDR ranges, IP ranges, company names, or raw engine DSL.
- Do not invent domains from a company name. Use ICP/subdomain/domain discovery first if the user has only provided an organization name.
- Keep `size` modest unless the user explicitly asks for a broader pull. Default is 20 per input per engine.
- Use `engines` only to narrow scope when needed; default `quake,zoomeye` is preferred for dedupe coverage.

## Output Contract

The wrapper returns JSON with summary counts and artifact paths. Important files:

```text
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/asset_mapping/asset_mapping.assets.jsonl
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/asset_mapping/asset_mapping.urls.txt
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/asset_mapping/asset_mapping.http_services.txt
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/asset_mapping/asset_mapping.raw.json
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/asset_mapping/asset_mapping.summary.json
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/asset_mapping/asset_mapping.report.html
```

Use `asset_mapping.urls.txt` as the preferred follow-up input for `nucleiPlus.urls` after loading the `nuclei-plus` skill.
Use `asset_mapping.http_services.txt` as candidate service input for direct validation or nmap fingerprinting before vulnerability scanning.
