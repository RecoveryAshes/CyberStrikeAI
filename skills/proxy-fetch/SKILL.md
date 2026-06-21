---
name: proxy-fetch
description: Use during authorized penetration testing for low-concurrency HTTP/browser
  proxy acquisition, including JS discovery, API endpoint testing, WAF/interception
  confirmation, key request replay, minimal Web/API vulnerability verification,
  small default-credential checks, path/parameter enumeration, and port/service
  confirmation when blocking or source-IP restrictions affect progress. Guides safe
  use of the low-bandwidth, 1-5 minute TTL `proxy_fetch` MCP tool and forbids only
  high-concurrency proxy use, not broad category names.
metadata:
  version: 1.0.0
---
# Proxy Fetch

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

Use this skill before calling `proxy_fetch` in an authorized penetration test where the current source IP appears blocked or restricted, or where the user explicitly asks to continue a low-concurrency web/API check through a proxy.

The current proxy source is low-bandwidth and short-lived: roughly 2 Mbps with an IP validity window of about 1-5 minutes.

## When To Use

- Use `proxy_fetch` when the user explicitly asks to get a proxy, rotate IP, use 快代理, or continue through a proxy.
- Use it when authorized testing hits clear blocking signals: WAF block pages, repeated 403/429 caused by source IP, IP ban messages, CAPTCHA/risk-control challenges, connection resets after repeated probes, or rate-limit behavior that blocks progress.
- Use it for low-concurrency HTTP/browser actions, including JS discovery, lazy-loaded JS confirmation, API endpoint validation/testing, confirming an intercepted/blocked page, retrying key requests, and narrowly scoped manual verification.
- Use it for minimal Web/API vulnerability verification or false-positive review when the current source IP is blocked or restricted. The proxy boundary in this skill is concurrency: do not use the proxy for high-concurrency activity, and do not reject a task solely because its category name includes "brute force", "port test", "scan", "crawl", or "replay".
- Use it for low-concurrency checks such as default-credential attempts, path/parameter wordlists, or host/port service confirmation when useful for the authorized task.
- Use it when a browser, endpoint test, or small request workflow has produced evidence that the current IP is restricted and direct retry would likely keep failing.
- Do not use it for high-concurrency crawls, high-concurrency batch HTTP checks, high-concurrency credential attacks, high-concurrency directory checks, or high-concurrency port/service sweeps. If a scanner, crawler, brute-force tool, or port tool is configured for low concurrency, evaluate it by actual concurrency rather than by the tool/category name.
- Do not use it merely to increase concurrent request volume or hide activity. Prefer direct, low-rate, low-noise testing until there is evidence of blocking.

## Tool Contract

- Call MCP tool `proxy_fetch`; do not run the wrapper script through shell unless debugging the tool itself.
- Default `provider` is `kuaidaili_dps`.
- Prefer environment configuration (`KDL_SECRET_ID`, `KDL_SECRET_KEY`, etc.). Do not guess config file paths.
- If the user supplied a known config path, pass it as `config`.
- Use `num=1` unless the next step needs multiple proxies.
- Set `save_to_file=true` only when the user wants a proxy acquisition record in file management.
- Keep `include_secret_in_response=true` by default. The tool is intended to provide a short-lived full proxy URL to the AI and follow-up tools.
- If the returned `valid_seconds` is too short for the next action, do not start the action through that proxy. Fetch a fresh proxy or continue without proxy.

## Output Handling

The tool returns:

- `proxy_display`: safe URL without credentials
- `proxy_masked`: credential-masked URL
- `proxy_host`: host:port
- `valid_seconds` and `expires_at` when the provider returns TTL
- `proxy`: full URL by default when `include_secret_in_response=true`

If saving is enabled, the CSV includes the full short-lived proxy URL so AI/follow-up tools can read it:

```text
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/proxy_fetch/proxy_fetch.results.csv
```

Raw JSON is written under:

```text
/tmp/cyberstrike-ai/proxy_fetch/
```

## Safe Usage Rules

- Never paste full credential-bearing proxy URLs into the final answer.
- When passing a full proxy URL to a follow-up tool, keep it inside tool arguments only.
- Check `valid_seconds` before use. If the proxy is close to expiry, fetch a new one.
- Do not pass this proxy to high-concurrency scanners, crawlers, brute-force tools, or batch jobs. Use it only for low-concurrency HTTP/browser checks.
- For tools that accept only unauthenticated local proxies, this tool currently returns upstream proxy details; add a local-forwarding wrapper before using it for those tools.

## Examples

Get one proxy for AI/follow-up tool use:

```json
{
  "provider": "kuaidaili_dps",
  "num": 1
}
```

Get one full proxy URL for immediate follow-up use:

```json
{
  "provider": "kuaidaili_dps",
  "num": 1,
  "include_secret_in_response": true
}
```

Save a proxy record to file management:

```json
{
  "provider": "kuaidaili_dps",
  "num": 1,
  "conversation_id": "current_conversation_id",
  "relative_dir": "proxy",
  "save_to_file": true
}
```
