---
name: port-scanning
description: Use when the user asks for TCP port discovery, host port scanning, service/version enumeration, Nmap default scripts, or choosing between masscan and nmap in CyberStrikeAI. Default to MCP tool streaming_port_scan for any end-to-end port scan that needs both open-port discovery and service/version enumeration.
version: 1.0.0
---

# Port Scanning

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

Use this skill when the task is about port discovery, open-port confirmation, service fingerprinting, version detection, Nmap default scripts, or scan strategy. The normal CyberStrikeAI flow is:

1. Use MCP tool `streaming_port_scan` by default for normal port scanning tasks, because it confirms open ports first and runs `nmap` only after a specific `host:port` is confirmed open.
2. Use MCP tool `masscan` alone only when you determine the task is pure fast TCP open-port discovery and service/version enumeration would be unnecessary or wasteful.
3. Use MCP tool `nmap` alone only when the input already contains known live ports or the task is specifically deeper enumeration of known services.
4. Summarize open ports, detected services, versions, scripts, uncertainty, and recommended next enumeration steps.

Do not use cyberspace mapping tools such as `quake_search`, `fofa_search`, `shodan_search`, or `zoomeye_search` as substitutes for direct port scanning. Those tools are for exposure search and external asset mapping, not live confirmation.

## Tool Roles

- `streaming_port_scan`: preferred orchestrator for normal end-to-end port scanning. In `auto` mode, small explicit port lists use TCP connect discovery; larger port ranges/CIDR use masscan. It starts `nmap` only for ports that connect/masscan has confirmed open.
- `masscan`: fast TCP port liveness check. Use it directly only when the right answer is a port list, not service details.
- `nmap`: authoritative follow-up enumeration. Use it directly only when ports are already known or when deeper protocol/script enumeration is the main task.
- `rustscan`: disabled in this project to avoid duplicate scan paths. Do not select it.

## Mandatory Flow

Before scanning, check that the user has provided an authorized target or that the scope is clearly part of an approved assessment. If scope is ambiguous, ask for the target and permitted range.

For a normal TCP scan, use `streaming_port_scan`:

- Pass `target`, `ports`, `discovery_mode=auto`, `small_port_threshold`, `rate`, `batch_size`, and `nmap_timing`.
- Use `ports=1-65535` only when the user asks for full-port coverage or the target count is small enough to justify it.
- For a single host with a small explicit port list, let `streaming_port_scan` auto-select TCP connect discovery instead of masscan.
- Do not call standalone `nmap` to discover unknown open ports. Unknown port discovery must happen through `streaming_port_scan`, which chooses connect or masscan.
- Only run `nmap` against concrete `host:port` pairs that are already confirmed open by connect/masscan, supplied by the user, or proven by prior results in the current task.
- Keep `rate` conservative in VM/NAT environments, usually `500` to `1000`.
- Keep `nmap_additional_args=-Pn` unless host discovery behavior is specifically needed.
- Use `batch_size=10` by default. Lower it to `1` to start `nmap` immediately for each discovery; raise it for large noisy scans.

Manual two-step flow is valid only when `streaming_port_scan` is unavailable or when you need tighter control than the wrapper exposes:

- Call `masscan` first with `target`, `ports`, and a conservative `rate`.
- Use `ports=1-65535` only when the user asks for full-port coverage or the target count is small enough to justify it.
- Prefer common ports for broad scopes unless the user requests full coverage.
- Keep `banners=false` in `masscan` by default; let `nmap` perform banner, service, and script enumeration.
- If `masscan` returns no open ports but the target is expected to be alive, retry once at a lower `rate` or with a narrower port set before concluding no TCP ports were found.
- After `masscan`, call `nmap` only for confirmed open ports. Pass the confirmed comma-separated port list via `ports`. Never use `nmap -p 1-65535` as the first discovery step.
- Use the `nmap` default behavior unless there is a reason to override it. The tool default is `-sT -sV -sC`, which works without root.
- Add `timing=3` or `timing=4` for normal authorized internal work. Avoid `-T5` unless the user explicitly wants speed over reliability.
- Use `additional_args=-Pn` for hosts that block ping or where host discovery could cause false negatives.

## Masscan Rate Guidance

The project `masscan` tool exposes `rate` as `--rate`, measured in packets per second. Do not treat it as bandwidth in Mbps.

Default safe choices:

- Single host or a few hosts in a VM/NAT environment: `rate=500` to `1000`.
- Small internal range: `rate=1000` to `3000`.
- Large authorized range: start at `rate=1000`, increase only if results are stable.
- Unstable network, UTM VM, NAT, VPN, or inconsistent results: reduce to `rate=300` to `800`.

When accuracy matters, run a confirmation pass instead of increasing speed. If two `masscan` runs disagree, trust the slower or repeated-confirmed result and say the fast result is unstable.

## Nmap Follow-Up

For confirmed ports, call `nmap` like this conceptually:

```json
{
  "target": "192.0.2.10",
  "ports": "22,80,443",
  "timing": "4",
  "additional_args": "-Pn"
}
```

Do not set `scan_type` unless you intentionally replace the default `-sT -sV -sC`. If you set `scan_type`, include every needed option explicitly.

Avoid `os_detection=true` and `aggressive=true` unless root privileges are available and the user wants OS detection or aggressive probing. These can fail without root and are noisier than the default service pass.

## Result Handling

Report results in this order:

1. Scope scanned and port range.
2. `masscan` confirmed open TCP ports.
3. `nmap` service/version/default-script findings for each confirmed port.
4. Inconsistencies, timeouts, filtered behavior, or confidence limits.
5. Targeted next steps, such as HTTP enumeration for web ports, SMB enumeration for 445, SSH hardening checks for 22, or TLS checks for 443.

Keep raw command details brief unless the user asks for exact commands. If a tool generated files, return the relevant file paths and explain which output is the canonical one.

## Examples

Single host full TCP discovery:

```json
{
  "tool": "streaming_port_scan",
  "arguments": {
    "target": "192.0.2.10",
    "ports": "1-65535",
    "discovery_mode": "auto",
    "rate": 800,
    "batch_size": 5,
    "nmap_timing": "4",
    "nmap_additional_args": "-Pn"
  }
}
```

Manual masscan-only discovery:

```json
{
  "tool": "masscan",
  "arguments": {
    "target": "192.0.2.10",
    "ports": "1-65535",
    "rate": 800,
    "banners": false
  }
}
```

Manual nmap confirmation after open ports are found:

```json
{
  "tool": "nmap",
  "arguments": {
    "target": "192.0.2.10",
    "ports": "22,80,443",
    "timing": "4",
    "additional_args": "-Pn"
  }
}
```

Small CIDR with conservative rate:

```json
{
  "tool": "masscan",
  "arguments": {
    "target": "192.0.2.0/28",
    "ports": "1-10000",
    "rate": 1000,
    "banners": false
  }
}
```
