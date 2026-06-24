---
name: icp-query
description: Use when querying China ICP filing records by company/subject name or domain, including batch ICP lookups, web/app/mapp/kapp/all query types, subsidiary expansion, checkpointed CSV output, and saving results into CyberStrikeAI file management.
version: 1.0.0
---

# ICP Query

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

Use this skill as the mandatory pre-check before calling the MCP tool `icp_query`. Apply it whenever the user asks for ICP filing, ICP备案, domain备案, 域名备案, 主体备案, 公司备案, APP备案, 小程序备案, 快应用备案, or subsidiary ICP lookup.

Do not use `quake_search`, `fofa_search`, `shodan_search`, `zoomeye_search`, or other cyberspace mapping tools as substitutes for ICP filing lookup. Those tools are for asset mapping, host/service discovery, and exposure search; they do not query official ICP filing records. Use them only when the user explicitly asks for asset mapping, exposure search, or when `icp_query` fails and the user agrees to use indirect clues.

## Mandatory Pre-Call Contract

Before calling `icp_query`, normalize parameters with the following rules:

- Always consult this skill first. Do not call `icp_query` directly from vague intuition.
- `items` and `input_file` are mutually exclusive. Provide exactly one of them.
- If the user typed company names or domains in chat, prefer `items`.
- Use `input_file` only when the user explicitly provided a file path or the batch is already stored in a file.
- Never pass an empty, blank, or placeholder `input_file`. In particular, never pass `""`, whitespace, or `"-"`.
- If `items` is used, omit `input_file` entirely.
- If `input_file` is used, omit `items` entirely.
- Omit `config` unless you know an exact valid config file path in the current environment.
- Never guess config paths such as `icp_quary111/config.json` or any other inferred relative path.
- If the environment default config is acceptable, omit `config` and let the wrapper choose the local default.
- Default `icp_path` to `professional_member`.
- Do not use `icp_path=default` unless the user explicitly asks for it or `professional_member` is unavailable in the current environment.
- If a first attempt fails because of argument construction, retry once by removing optional parameters that may have been guessed incorrectly, especially `input_file`, `config`, and nonessential advanced flags.
- Do not rely on the underlying CLI to retry forever. The wrapper defaults to `item_max_failures=10` and `total_max_failures=200`; keep those defaults unless the user explicitly asks for a different limit.

## Tool Choice

- Use MCP tool `icp_query` for real ICP filing lookups and CSV output.
- Use `query_mode=subject` for company or organization names.
- Use `query_mode=domain` for domain reverse lookup.
- In `domain` mode, always use `query_type=web`; other query types are not supported by the underlying CLI.
- In `subject` mode, use `query_type=web` by default. Use `all` only when the user explicitly needs websites, apps, mini programs, and quick apps together.

## Inputs

- For a small batch, pass `items` as an array of strings.
- For a large batch or an existing list, pass `input_file`; the file should contain one query item per line.
- If the source data comes from the current chat, default to `items`, not `input_file`.
- Use `output_name` as a meaningful batch prefix for `query_id`, for example `target_icp`; the user-facing CSV filename is fixed per conversation.
- If the current conversation ID is known, pass it as `conversation_id`; otherwise omit it and the wrapper will save under `_manual`.
- Use `relative_dir` to separate batches, for example `customer_a` or `domain_reverse`.

## Options

- Do not invent optional parameters unless the user asked for them or the environment requires them.
- Use `investment=true` only when subsidiary or outward-investment expansion is needed. When enabled, the wrapper first expands subsidiaries for the input parent companies, then queries ICP for the parent companies and subsidiaries as separate subjects. This does not depend on the parent company ICP lookup having a hit.
- Use `conprop=100` when only wholly-owned subsidiaries should be included.
- Use `conprop=51` when majority-owned subsidiaries should be included.
- Use `conprop=0` when subsidiary filtering by ownership percentage should be disabled.
- Use `smart_mode=true` only if the ICP CLI config has working MCP WebSearch and AI service settings.
- Use `icp_path=professional_member` by default.
- Use `icp_path=default` only as an explicit fallback when `professional_member` is unavailable or the user asks to switch.
- If a subject/domain repeatedly fails, let the wrapper record `query_status=failed` in the CSV and continue. Do not keep relaunching the same item manually after the per-item limit is reached.

## Output Handling

The tool appends user-facing results to one conversation-level CSV:

```text
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/icp_query/icp_query.results.csv
```

Temporary run files are stored under:

```text
/tmp/cyberstrike-ai/icp_query/
```

Return the generated `csv_file`, `query_id`, and `appended_rows` to the user. Do not surface checkpoint/log/debug paths unless troubleshooting or `save_debug_files=true` was requested.

When `investment=true`, the temporary subsidiary expansion CSV is also stored under `/tmp/cyberstrike-ai/icp_query/`. The user-facing CSV includes metadata columns such as `query_source`, `parent_company`, and `subsidiary_conprop` when available.

## Examples

Subject lookup:

```json
{
  "items": ["阿里巴巴（中国）有限公司", "腾讯科技（深圳）有限公司"],
  "query_mode": "subject",
  "query_type": "web",
  "output_name": "company_icp.csv"
}
```

Parent plus subsidiaries lookup:

```json
{
  "items": ["某某集团有限公司"],
  "query_mode": "subject",
  "query_type": "web",
  "investment": true,
  "conprop": 51,
  "output_name": "parent_and_subsidiaries_icp.csv"
}
```

Domain reverse lookup:

```json
{
  "items": ["aliyun.com", "qq.com"],
  "query_mode": "domain",
  "query_type": "web",
  "output_name": "domain_reverse_icp.csv"
}
```

Minimal safe example with environment-default config:

```json
{
  "items": ["宁波某某科技有限公司"],
  "query_mode": "subject",
  "query_type": "web",
  "icp_path": "professional_member",
  "output_name": "ningbo_icp.csv"
}
```
