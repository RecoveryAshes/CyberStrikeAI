---
name: semgrep-rule-creator
description: 自定义Semgrep规则创建技能，用于编写检测安全漏洞、Bug模式和代码规范的Semgrep规则，支持污点模式和数据流分析
metadata:
  version: 1.0.0
---
# Semgrep Rule Creator

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

Create production-quality Semgrep rules with proper testing and validation.

## When to Use

**Ideal scenarios:**
- Writing Semgrep rules for specific bug patterns
- Writing rules to detect security vulnerabilities in your codebase
- Writing taint mode rules for data flow vulnerabilities
- Writing rules to enforce coding standards

## When NOT to Use

Do NOT use this skill for:
- Running existing Semgrep rulesets
- General static analysis without custom rules (use `static-analysis` skill)

## Rationalizations to Reject

When writing Semgrep rules, reject these common shortcuts:

- **"The pattern looks complete"** → Still run `semgrep --test --config <rule-id>.yaml <rule-id>.<ext>` to verify. Untested rules have hidden false positives/negatives.
- **"It matches the vulnerable case"** → Matching vulnerabilities is half the job. Verify safe cases don't match (false positives break trust).
- **"Taint mode is overkill for this"** → If data flows from user input to a dangerous sink, taint mode gives better precision than pattern matching.
- **"One test is enough"** → Include edge cases: different coding styles, sanitized inputs, safe alternatives, and boundary conditions.
- **"I'll optimize the patterns first"** → Write correct patterns first, optimize after all tests pass. Premature optimization causes regressions.
- **"The AST dump is too complex"** → The AST reveals exactly how Semgrep sees code. Skipping it leads to patterns that miss syntactic variations.

## Anti-Patterns

**Too broad** - matches everything, useless for detection:
```yaml
# BAD: Matches any function call
pattern: $FUNC(...)

# GOOD: Specific dangerous function
pattern: eval(...)
```

**Missing safe cases in tests** - leads to undetected false positives:
```python
# BAD: Only tests vulnerable case
# ruleid: my-rule
dangerous(user_input)

# GOOD: Include safe cases to verify no false positives
# ruleid: my-rule
dangerous(user_input)

# ok: my-rule
dangerous(sanitize(user_input))

# ok: my-rule
dangerous("hardcoded_safe_value")
```

**Overly specific patterns** - misses variations:
```yaml
# BAD: Only matches exact format
pattern: os.system("rm " + $VAR)

# GOOD: Matches all os.system calls with taint tracking
mode: taint
pattern-sources:
  - pattern: input(...)
pattern-sinks:
  - pattern: os.system(...)
```

## Strictness Level

This workflow is **strict** - do not skip steps:
- **Read documentation first**: See [Documentation](#documentation) before writing Semgrep rules
- **Test-first is mandatory**: Never write a rule without tests
- **100% test pass is required**: "Most tests pass" is not acceptable
- **Optimization comes last**: Only simplify patterns after all tests pass
- **Avoid generic patterns**: Rules must be specific, not match broad patterns
- **Prioritize taint mode**: For data flow vulnerabilities
- **One YAML file - one Semgrep rule**: Each YAML file must contain only one Semgrep rule; don't combine multiple rules in a single file
- **No generic rules**: When targeting a specific language for Semgrep rules - avoid generic pattern matching (`languages: generic`)
- **Forbidden `todook` and `todoruleid` test annotations**: `todoruleid: <rule-id>` and `todook: <rule-id>` annotations in tests files for future rule improvements are forbidden

## Overview

This skill guides creation of Semgrep rules that detect security vulnerabilities and code patterns. Rules are created iteratively: analyze the problem, write tests first, analyze AST structure, write the rule, iterate until all tests pass, optimize the rule.

**Approach selection:**
- **Taint mode** (prioritize): Data flow issues where untrusted input reaches dangerous sinks
- **Pattern matching**: Simple syntactic patterns without data flow requirements

**Why prioritize taint mode?** Pattern matching finds syntax but misses context. A pattern `eval($X)` matches both `eval(user_input)` (vulnerable) and `eval("safe_literal")` (safe). Taint mode tracks data flow, so it only alerts when untrusted data actually reaches the sink—dramatically reducing false positives for injection vulnerabilities.

**Iterating between approaches:** It's okay to experiment. If you start with taint mode and it's not working well (e.g., taint doesn't propagate as expected, too many false positives/negatives), switch to pattern matching. Conversely, if pattern matching produces too many false positives on safe cases, try taint mode instead. The goal is a working rule—not rigid adherence to one approach.

**Output structure** - exactly 2 files in a directory named after the rule-id:
```
<rule-id>/
├── <rule-id>.yaml     # Semgrep rule
└── <rule-id>.<ext>    # Test file with ruleid/ok annotations
```

## Quick Start

```yaml
rules:
  - id: insecure-eval
    languages: [python]
    severity: HIGH
    message: User input passed to eval() allows code execution
    mode: taint
    pattern-sources:
      - pattern: request.args.get(...)
    pattern-sinks:
      - pattern: eval(...)
```

Test file (`insecure-eval.py`):
```python
# ruleid: insecure-eval
eval(request.args.get('code'))

# ok: insecure-eval
eval("print('safe')")
```

Run tests (from rule directory): `semgrep --test --config <rule-id>.yaml <rule-id>.<ext>`

## Quick Reference

- For commands, pattern operators, and taint mode syntax, see [quick-reference.md]({baseDir}/references/quick-reference.md).
- For detailed workflow and examples, you MUST see [workflow.md]({baseDir}/references/workflow.md)

## Workflow

Copy this checklist and track progress:

```
Semgrep Rule Progress:
- [ ] Step 1: Analyze the Problem
- [ ] Step 2: Write Tests First
- [ ] Step 3: Analyze AST structure
- [ ] Step 4: Write the rule
- [ ] Step 5: Iterate until all tests pass (semgrep --test)
- [ ] Step 6: Optimize the rule (remove redundancies, re-test)
- [ ] Step 7: Final Run
```

## Documentation

**REQUIRED**: Before writing any rule, use WebFetch to read **all** of these 7 links with Semgrep documentation:

1. [Rule Syntax](https://raw.githubusercontent.com/semgrep/semgrep-docs/refs/heads/main/docs/writing-rules/rule-syntax.md)
2. [Pattern Syntax](https://raw.githubusercontent.com/semgrep/semgrep-docs/refs/heads/main/docs/writing-rules/pattern-syntax.mdx)
3. [Testing Rules](https://raw.githubusercontent.com/semgrep/semgrep-docs/refs/heads/main/docs/writing-rules/testing-rules.md)
4. [Taint analysis](https://raw.githubusercontent.com/semgrep/semgrep-docs/refs/heads/main/docs/writing-rules/data-flow/taint-mode/overview.md)
5. [Advanced techniques for taint analysis](https://raw.githubusercontent.com/semgrep/semgrep-docs/refs/heads/main/docs/writing-rules/data-flow/taint-mode/advanced.md)
6. [Constant propagation](https://raw.githubusercontent.com/semgrep/semgrep-docs/refs/heads/main/docs/writing-rules/data-flow/constant-propagation.md)
7. [Trail of Bits Testing Handbook - Semgrep chapter](https://raw.githubusercontent.com/trailofbits/testing-handbook/refs/heads/main/content/docs/static-analysis/semgrep/10-advanced.md)
