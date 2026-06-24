---
name: penetration-testing-methodology
description: 渗透测试系统方法论技能，覆盖范围确认、信息收集、漏洞扫描、漏洞利用到报告编写的完整流程，符合PTES/OWASP测试指南标准
metadata:
  version: 1.0.0
---
# Penetration Testing

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

## Table of Contents

- [Overview](#overview)
- [When to Use](#when-to-use)
- [Quick Start](#quick-start)
- [Reference Guides](#reference-guides)
- [Best Practices](#best-practices)

## Overview

Systematic security testing to identify, exploit, and document vulnerabilities in applications, networks, and infrastructure through simulated attacks.

## When to Use

- Pre-production security validation
- Annual security assessments
- Compliance requirements (PCI-DSS, ISO 27001)
- Post-incident security review
- Third-party security audits
- Red team exercises

## Quick Start

Minimal working example:

```python
# pentest_framework.py
import requests
import socket
import subprocess
import json
from typing import List, Dict
from dataclasses import dataclass, asdict
from datetime import datetime

@dataclass
class Finding:
    severity: str
    category: str
    target: str
    vulnerability: str
    evidence: str
    remediation: str
    cvss_score: float

class PenetrationTester:
    def __init__(self, target: str):
        self.target = target
        self.findings: List[Finding] = []

    def test_sql_injection(self, url: str) -> None:
// ... (see reference guides for full implementation)
```

## Reference Guides

Detailed implementations in the `references/` directory:

| Guide | Contents |
|---|---|
| [Automated Penetration Testing Framework](references/automated-penetration-testing-framework.md) | Automated Penetration Testing Framework |
| [Burp Suite Automation Script](references/burp-suite-automation-script.md) | Burp Suite Automation Script |
| [Lazy-Loaded Code Testing](references/lazy-loaded-code-testing.md) | 懒加载JS发现与测试指南 |

## Multi-Target Execution Context（多目标执行上下文）

批量目标的预检、排序、分组和队列创建由 `目标预检调度` 角色负责。该角色会先使用 `nucleiPlus_precheck` 与 `whatweb` 对全部目标做状态/指纹统一预检，再通过 `batch_task_create` 创建一个或多个批量队列。预检阶段禁止用 `nucleiPlus` 打 POC；`nucleiPlus` 仅用于后续单目标渗透测试阶段的漏洞模板/POC 扫描。

当你在批量队列的子任务中加载本 skill 时，默认每个子任务只对应一个目标。此时不要重新调度整批目标，不要再创建新的批量队列；应专注于当前子任务给出的单一目标，结合子任务携带的预检证据执行完整渗透测试。

### 单目标完成标准

一个目标被视为"测试完成"必须满足：
- [ ] 信息收集阶段已执行（端口/服务/技术栈/JS/API/攻击面识别）
- [ ] 与目标类型匹配的漏洞扫描和手工验证已执行
- [ ] 发现的候选漏洞已尝试复现、误报排除和影响确认
- [ ] 输出了该目标的测试发现摘要

### 禁止行为

- ❌ 在子任务中把多个目标混在一起测试
- ❌ 跳过当前目标的信息收集、JS/API 发现或漏洞验证阶段
- ❌ 用批量扫描命中直接当作已复现漏洞
- ❌ 在报告中对未深入测试或未复现的风险给出确认结论
## Best Practices

### 测试降噪策略 (MANDATORY)

渗透测试必须按噪音级别从低到高执行，防止过早触发WAF/IPS封IP导致后续测试失败。

```
低噪声默认顺序（最低覆盖，可按现场证据调整）：

Phase 1 - JS分析与API发现（最高优先级，先于一切扫描）
  ✓ 优先加载 lazy-js-discovery skill（SPA/现代前端/JS 密集目标尤其需要）
  ✓ 默认按 lazy-js-discovery 的 L1 -> L2 -> L3 -> L4 -> L5 -> L6 -> L7 覆盖；可按现场证据调整或并行，但要回补 L1/L2 的证据和取舍说明
  ✓ L1 无认证静态提取：下载并分析 HTML、runtime/main bundle、chunk、source map，提取 JS 文件、API 端点、路由配置和硬编码凭证
  ✓ L2 前端路由守卫分析与浏览器触发：基于 L1 证据生成 initScript/Playwright/DevTools 方案，触发懒加载并收集新 script/network 请求
  ✓ L3/L4/L5/L6/L7 用于补充浏览器基线、交互触发、SPA 路由枚举、动态代码分析和 Runtime Hook
  ✓ `katana/gau/waybackurls/whatweb` 适合作为采集补充；不要把它们的单次结果直接等同于 L1 静态提取或 L2 浏览器触发已覆盖
  输出：完整的API端点列表 + 隐藏路由 + 泄露的凭证

Phase 2 - API接口测试（针对Phase 1发现的接口）
  ✓ 对每个发现的API端点进行认证/授权测试
  ✓ IDOR/越权测试（修改ID参数）
  ✓ SQL注入探测（参数类型变异）
  ✓ SSRF/XXE测试（URL类参数）
  ✓ 命令注入测试
  ✓ 文件上传/下载漏洞
  ✓ JWT/OAuth缺陷测试
  ✓ 业务逻辑漏洞（支付/转账/权限等）

Phase 3 - 补充扫描（中噪音）
  ✓ 端口扫描（nmap -T3）
  ✓ 目录枚举（小字典，补充JS未发现的路径）
  ✓ nucleiPlus指纹+PoC扫描
  ✓ CORS/CSP/安全头检查

Phase 4 - 爆破与高噪音测试（最后做！）
  ✓ 大字典目录爆破
  ✓ 密码爆破（hydra）
  ✓ 参数fuzz（大量请求）
  ✓ 子域名爆破（大字典）
```

**核心原则：JS分析和API发现是最高优先级。从前端代码中挺掘隐藏API是最高效的漏洞发现路径。**

**原因：**
- 现代Web应用的攻击面大多藏在JS中（懒加载模块、隐藏API、硬编码密钥）
- 从JS发现的API端点往往缺乏充分的安全检查（因为开发者认为“前端发现不了”）
- 爆破触发WAF封IP后所有后续测试全废，但JS分析不会触发任何防护
- Phase 1的发现直接指导Phase 2的精准测试，效率最高

### ✅ DO

- Get written authorization
- Define clear scope
- Use controlled environments
- Document all findings
- Follow responsible disclosure
- Provide remediation guidance
- Verify fixes after patching
- Maintain chain of custody

### ✘ DON'T

- Test production without approval
- Cause service disruption
- Exfiltrate sensitive data
- Share findings publicly
- Exceed authorized scope
- Use destructive payloads
- Batch-scan multiple targets without deep-diving each one
- 在测试早期执行爆破操作（密码爆破、大字典目录扫描、全端口扫描等）
- 使用过高扫描速率（nmap -T5、无延迟的ffuf等）

### ✅ DO

- Get written authorization
- Define clear scope
- Use controlled environments
- Document all findings
- Follow responsible disclosure
- Provide remediation guidance
- Verify fixes after patching
- Maintain chain of custody

### ❌ DON'T

- Test production without approval
- Cause service disruption
- Exfiltrate sensitive data
- Share findings publicly
- Exceed authorized scope
- Use destructive payloads
- Batch-scan multiple targets without deep-diving each one
