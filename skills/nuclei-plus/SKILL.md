---
name: nuclei-plus
description: Use before calling nucleiPlus or nucleiPlus_precheck for constrained ddddPro status/fingerprint precheck or fingerprint-linked PoC scanning on known URLs or nmap-identified HTTP services.
version: 1.0.0
---

# nuclei-plus

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

Use this skill as the mandatory pre-check before calling the MCP tools `nucleiPlus_precheck` or `nucleiPlus`.

Apply it when the user already has either:

- explicit web targets as `http://` or `https://` URLs, or
- concrete `ip:port` services that have already been identified as HTTP by `nmap`, `ddddPro`, or another trusted service-fingerprinting step.

Do not use this tool for asset discovery, cyberspace mapping, subdomain expansion, or broad unknown-port exploration. It is a constrained follow-up scanner, not an asset collection tool.

## Tool Roles

There are two separate tool names for different phases:

- `nucleiPlus_precheck`: target precheck only. It checks status/response and passive fingerprints/technology hints, and hard-disables active Web fingerprint probing with `-nd` plus nuclei POC / vulnerability detection with `-npoc`. Use this in the `目标预检调度` role.
- `nucleiPlus`: penetration-testing phase scanning. It keeps the fingerprint -> workflow -> nuclei template linkage and may run POC/templates. Use this only when the task has moved past precheck into vulnerability scanning/verification.

`nucleiPlus` is a restricted wrapper around ddddPro. Its vulnerability-scan flow is:

`known URL or known HTTP service -> passive web probe -> active directory/fingerprint probe -> fingerprint identification -> workflow match -> nuclei templates`

Both wrappers hard-disable:

- GoPoc
- brute-force
- host-bind expansion
- mapping-engine parameters
- subdomain enumeration parameters
- masscan / SYN scan parameters

The wrapper runs targets in batches by default. Each batch has its own ddddPro process and timeout, and batch results are appended into conversation-level files for later linkage.

## Finding Confirmation Rule

`nucleiPlus` scan hits are candidate findings, not confirmed vulnerabilities.

After `nucleiPlus` reports a possible vulnerability, you must perform an independent reproduction step before saying the target has that vulnerability or before calling `record_vulnerability`.

For every meaningful hit:

- Extract the target URL/service, template or POC name, severity, matched path, matcher/extractor evidence, and any request data shown in the output.
- Reproduce with a direct, minimal verification request or PoC against the same target. Prefer a raw HTTP request that can be replayed in Burp Repeater for Web/API findings.
- Compare the response or side effect with the expected vulnerable behavior from the template. Status code alone is not enough.
- Check at least one false-positive control when practical, such as a non-vulnerable path, altered payload, missing required parameter, unauthenticated/authenticated contrast, or expected response marker absence.
- If the finding depends on DNS/OOB behavior, confirm the callback or interaction log and record the correlation token.
- If reproduction is blocked by auth, instability, missing context, WAF/rate limits, or unavailable OOB infrastructure, label it as `unconfirmed` and describe the blocker. Do not present it as confirmed.

Only mark a finding as confirmed when the reproduction demonstrates the vulnerable behavior with concrete evidence. Confirmed Web/API proof must include a replayable raw HTTP request, key response excerpt or observable side effect, and the reason this rules out the main false-positive path.

## Mandatory Pre-Call Contract

Before calling `nucleiPlus_precheck` or `nucleiPlus`, normalize inputs with the following rules:

- Always consult this skill first. Do not call `nucleiPlus_precheck` or `nucleiPlus` directly from vague intuition.
- `urls` and `http_services` may be used together, but at least one must be non-empty.
- If the target is already a full URL, pass it in `urls`.
- If the target is an `ip:port` and it is known to be an HTTP service, pass it in `http_services`.
- Never pass a non-HTTP service into `http_services`.
- Never pass a URL into `http_services`.
- Never pass a bare domain, bare IP, CIDR, range, or unknown open port into this tool.
- `urls` entries must explicitly include `http://` or `https://`.
- `http_services` entries must strictly be `ip:port` or `host:port`.
- If the current conversation already contains the targets, pass them directly as arrays. Do not create a temporary input file first.

## When To Use

Use `nucleiPlus_precheck` when the task is:

- multi-target scheduling precheck
- status code / response reachability checks
- passive fingerprinting / technology hints on known web targets
- grouping targets before creating batch task queues

Do not use `nucleiPlus_precheck` for vulnerability verification or POC scanning.

Use `nucleiPlus` when the task is:

- fingerprint-driven nuclei scanning
- focused template scanning after service identification
- follow-up scanning on `nmap`-identified HTTP ports

Prefer this tool over raw `nuclei` when the user explicitly wants:

- active fingerprint probing
- passive + active fingerprint linkage
- workflow-based template selection

Prefer raw `nuclei` instead when the task is purely:

- direct template execution on already-normalized URLs
- no active probing
- exact template/tag execution without ddddPro fingerprint workflow

## Parameter Guidance

- Use `severity` when the user wants to limit scan scope, such as `critical,high`.
- Use `exclude_tags` when the user wants to suppress noisy or irrelevant template categories.
- Use `poc_name` only when the user explicitly wants a named or fuzzy-matched subset of templates.
- Use `template_dir`, `workflow_yaml`, `finger_yaml`, or `dir_yaml` only when the user explicitly wants custom rule sources or the environment requires them.
- Use `web_threads`, `web_timeout`, `nmap_threads`, and `nmap_timeout` only when there is a clear tuning need.
- Use `disable_interactsh=true` in restricted environments or when out-of-band callbacks are not allowed.
- Use `audit_log=true` for sensitive environments where scan trace retention matters.
- Use `batch_size` for large inputs. Default is 30. Lower it when targets are slow or unstable.
- Use `process_timeout` as the per-batch timeout. Default is 1800 seconds.
- Use `max_targets` to explicitly cap scope. Default is 500.
- Keep `continue_on_error=true` unless the user wants the whole scan to stop on the first failed batch.
- Do not invent tuning parameters without a reason.

## Input Examples

Known URLs:

```json
{
  "urls": ["https://example.com", "https://app.example.com"],
  "conversation_id": "<conversation_id>"
}
```

Precheck-only known URLs:

Call `nucleiPlus_precheck` with:

```json
{
  "urls": ["https://example.com", "https://app.example.com"],
  "conversation_id": "<conversation_id>"
}
```

Penetration-test POC scan:

Call `nucleiPlus` with:

```json
{
  "urls": ["https://example.com"],
  "severity": "critical,high",
  "conversation_id": "<conversation_id>"
}
```

Known HTTP services from prior nmap results:

```json
{
  "http_services": ["192.0.2.10:80", "192.0.2.10:8080"]
}
```

Mixed input:

```json
{
  "urls": ["https://portal.example.com"],
  "http_services": ["192.0.2.20:8443"],
  "severity": "high,critical",
  "exclude_tags": "default-login,bruteforce"
}
```

Named POC subset:

```json
{
  "urls": ["https://nacos.example.com"],
  "poc_name": "nacos"
}
```

## Output Handling

The wrapper returns structured JSON including:

- accepted inputs
- the constrained command that was executed
- temp artifact paths
- stdout/stderr tails
- per-batch statuses
- the conversation-level merged result path

The user-facing merged files are:

```text
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/nucleiPlus/nucleiPlus.results.txt
chat_uploads/YYYYMMDD/<conversation_id>/tool_outputs/nucleiPlus/nucleiPlus.runs.jsonl
```

Treat the wrapper output as execution evidence and the dddd/nuclei findings as scan evidence.

Do not convert scan evidence directly into a vulnerability record. Use it to drive reproduction first:

- confirmed: reproduction succeeded and evidence is sufficient for `record_vulnerability`
- unconfirmed: scan hit exists but reproduction is incomplete or blocked
- false positive: reproduction failed and the observed behavior does not match the template claim

If the scan returns no findings, do not assume the target is safe. Report that fingerprinting/scanning completed with no hits under the selected workflow/templates, and mention any scope constraints such as severity filters or named-template restrictions.
