---
name: burpsuite-project-parser
description: Burp Suite项目文件解析与HTTP流量搜索技能，支持正则匹配请求/响应头和Body、提取安全审计发现、分析代理历史和站点地图数据
metadata:
  version: 1.0.0
---
# Burp Project Parser

## 使用边界与发散原则

- 本 skill 是最低覆盖要求、风险清单和证据标准，不是唯一测试路径或固定脚本。执行时必须根据目标技术栈、入口形态、认证状态、业务流程、已有证据、WAF/限速和授权范围调整方法。
- 文中的步骤、payload、命令、工具名和执行顺序默认是候选方法；除非本 skill、系统提示或后端门禁明确写为“必须/禁止/强制”，不要照抄固定列表，也不要因为示例没覆盖某个旁路、协议、参数位置、文件格式或业务入口就跳过。
- 发现阶段的字符串、端点、token、id、报错、组件名和扫描命中只作为候选信号；先记录和复核，不要仅凭单个关键词直接认定漏洞或加载多个无关专项流程。只有交互证据、可复现行为或明确进入对应专项测试时，才提升为专项验证。
- 每一步都应输出证据、判断理由和下一步分支；工具失败时先修参数、配置、环境或改用更合适的专业工具，确认无法覆盖时再使用通用命令兜底。
- 授权范围、低噪声、数据最小化、复现证据、误报排除和报告要求是硬边界；这些约束高于任何示例命令或默认流程。

Search and extract data from Burp Suite project files using the burpsuite-project-file-parser extension.

## When to Use

- Searching response headers or bodies with regex patterns
- Extracting security audit findings from Burp projects
- Dumping proxy history or site map data
- Analyzing HTTP traffic captured in a Burp project file

## Prerequisites

This skill **delegates parsing to Burp Suite Professional** - it does not parse .burp files directly.

**Required:**
1. **Burp Suite Professional** - Must be installed ([portswigger.net](https://portswigger.net/burp/pro))
2. **burpsuite-project-file-parser extension** - Provides CLI functionality

**Install the extension:**
1. Download from [github.com/BuffaloWill/burpsuite-project-file-parser](https://github.com/BuffaloWill/burpsuite-project-file-parser)
2. In Burp Suite: Extender → Extensions → Add
3. Select the downloaded JAR file

## Quick Reference

Use the wrapper script:
```bash
{baseDir}/scripts/burp-search.sh /path/to/project.burp [FLAGS]
```

The script uses environment variables for platform compatibility:
- `BURP_JAVA`: Path to Java executable
- `BURP_JAR`: Path to burpsuite_pro.jar

See [Platform Configuration](#platform-configuration) for setup instructions.

## Sub-Component Filters (USE THESE)

**ALWAYS use sub-component filters instead of full dumps.** Full `proxyHistory` or `siteMap` can return gigabytes of data. Sub-component filters return only what you need.

### Available Filters

| Filter | Returns | Typical Size |
|--------|---------|--------------|
| `proxyHistory.request.headers` | Request line + headers only | Small (< 1KB/record) |
| `proxyHistory.request.body` | Request body only | Variable |
| `proxyHistory.response.headers` | Status + headers only | Small (< 1KB/record) |
| `proxyHistory.response.body` | Response body only | **LARGE - avoid** |
| `siteMap.request.headers` | Same as above for site map | Small |
| `siteMap.request.body` | | Variable |
| `siteMap.response.headers` | | Small |
| `siteMap.response.body` | | **LARGE - avoid** |

### Default Approach

**Start with headers, not bodies:**

```bash
# GOOD - headers only, safe to retrieve
{baseDir}/scripts/burp-search.sh project.burp proxyHistory.request.headers | head -c 50000
{baseDir}/scripts/burp-search.sh project.burp proxyHistory.response.headers | head -c 50000

# BAD - full records include bodies, can be gigabytes
{baseDir}/scripts/burp-search.sh project.burp proxyHistory  # NEVER DO THIS
```

**Only fetch bodies for specific URLs after reviewing headers, and ALWAYS truncate:**

```bash
# 1. First, find interesting URLs from headers
{baseDir}/scripts/burp-search.sh project.burp proxyHistory.response.headers | \
  jq -r 'select(.headers | test("text/html")) | .url' | head -n 20

# 2. Then search bodies with targeted regex - MUST truncate body to 1000 chars
{baseDir}/scripts/burp-search.sh project.burp "responseBody='.*specific-pattern.*'" | \
  head -n 10 | jq -c '.body = (.body[:1000] + "...[TRUNCATED]")'
```

**HARD RULE: Body content > 1000 chars must NEVER enter context.** If the user needs full body content, they must view it in Burp Suite's UI.

## Regex Search Operations

### Search Response Headers
```bash
responseHeader='.*regex.*'
```
Searches all response headers. Output: `{"url":"...", "header":"..."}`

Example - find server signatures:
```bash
responseHeader='.*(nginx|Apache|Servlet).*' | head -c 50000
```

### Search Response Bodies
```bash
responseBody='.*regex.*'
```
**MANDATORY: Always truncate body content to 1000 chars max.** Response bodies can be megabytes each.

```bash
# REQUIRED format - always truncate .body field
{baseDir}/scripts/burp-search.sh project.burp "responseBody='.*<form.*action.*'" | \
  head -n 10 | jq -c '.body = (.body[:1000] + "...[TRUNCATED]")'
```

**Never retrieve full body content.** If you need to see more of a specific response, ask the user to open it in Burp Suite's UI.

## Other Operations

### Extract Audit Items
```bash
auditItems
```
Returns all security findings. Output includes: name, severity, confidence, host, port, protocol, url.

**Note:** Audit items are small (no bodies) - safe to retrieve with `head -n 100`.

### Dump Proxy History (AVOID)
```bash
proxyHistory
```
**NEVER use this directly.** Use sub-component filters instead:
- `proxyHistory.request.headers`
- `proxyHistory.response.headers`

### Dump Site Map (AVOID)
```bash
siteMap
```
**NEVER use this directly.** Use sub-component filters instead.

## Output Limits (REQUIRED)

**CRITICAL: Always check result size BEFORE retrieving data.** A broad search can return thousands of records, each potentially megabytes. This will overflow the context window.

### Step 1: Always Check Size First

Before any search, check BOTH record count AND byte size:

```bash
# Check record count AND total bytes - never skip this step
{baseDir}/scripts/burp-search.sh project.burp proxyHistory | wc -cl
{baseDir}/scripts/burp-search.sh project.burp "responseHeader='.*Server.*'" | wc -cl
{baseDir}/scripts/burp-search.sh project.burp auditItems | wc -cl
```

The `wc -cl` output shows: `<bytes> <lines>` (e.g., `524288 42` means 512KB across 42 records).

**Interpret the results - BOTH must pass:**

| Metric | Safe | Narrow search | Too broad | STOP |
|--------|------|---------------|-----------|------|
| **Lines** | < 50 | 50-200 | 200+ | 1000+ |
| **Bytes** | < 50KB | 50-200KB | 200KB+ | 1MB+ |

**A single 10MB response on one line will show high byte count but only 1 line - the byte check catches this.**

### Step 2: Refine Broad Searches

If count/size is too high:

1. **Use sub-component filters** (see table above):
   ```bash
   # Instead of: proxyHistory (gigabytes)
   # Use: proxyHistory.request.headers (kilobytes)
   ```

2. **Narrow regex patterns:**
   ```bash
   # Too broad (matches everything):
   responseHeader='.*'

   # Better - target specific headers:
   responseHeader='.*X-Frame-Options.*'
   responseHeader='.*Content-Security-Policy.*'
   ```

3. **Filter with jq before retrieving:**
   ```bash
   # Get only specific content types
   {baseDir}/scripts/burp-search.sh project.burp proxyHistory.response.headers | \
     jq -c 'select(.url | test("/api/"))' | head -n 50
   ```

### Step 3: Always Truncate Output

Even after narrowing, always pipe through truncation:

```bash
# ALWAYS use head -c to limit total bytes (max 50KB)
{baseDir}/scripts/burp-search.sh project.burp proxyHistory.request.headers | head -c 50000

# For body searches, truncate each JSON object's body field:
{baseDir}/scripts/burp-search.sh project.burp "responseBody='pattern'" | \
  head -n 20 | jq -c '.body = (.body | if length > 1000 then .[:1000] + "...[TRUNCATED]" else . end)'

# Limit both record count AND byte size:
{baseDir}/scripts/burp-search.sh project.burp auditItems | head -n 50 | head -c 50000
```

**Hard limits to enforce:**
- `head -c 50000` (50KB max) on ALL output
- **Truncate `.body` fields to 1000 chars - MANDATORY, no exceptions**
  ```bash
  jq -c '.body = (.body[:1000] + "...[TRUNCATED]")'
  ```

**Never run these without counting first AND truncating:**
- `proxyHistory` / `siteMap` (full dumps - always use sub-component filters)
- `responseBody='...'` searches (bodies can be megabytes each)
- Any broad regex like `.*` or `.+`

## Investigation Workflow

1. **Identify scope** - What are you looking for? (specific vuln type, endpoint, header pattern)

2. **Search audit items first** - Start with Burp's findings:
   ```bash
   {baseDir}/scripts/burp-search.sh project.burp auditItems | jq 'select(.severity == "High")'
   ```

3. **Check confidence scores** - Filter for actionable findings:
   ```bash
   ... | jq 'select(.confidence == "Certain" or .confidence == "Firm")'
   ```

4. **Extract affected URLs** - Get the attack surface:
   ```bash
   ... | jq -r '.url' | sort -u
   ```

5. **Search raw traffic for context** - Examine actual requests/responses:
   ```bash
   {baseDir}/scripts/burp-search.sh project.burp "responseBody='pattern'"
   ```

6. **Validate manually** - Burp findings are indicators, not proof. Verify each one.

## Understanding Results

### Severity vs Confidence

Burp reports both **severity** (High/Medium/Low) and **confidence** (Certain/Firm/Tentative). Use both when triaging:

| Combination | Meaning |
|-------------|---------|
| High + Certain | Likely real vulnerability, prioritize investigation |
| High + Tentative | Often a false positive, verify before reporting |
| Medium + Firm | Worth investigating, may need manual validation |

A "High severity, Tentative confidence" finding is frequently a false positive. Don't report findings based on severity alone.

### When Proxy History is Incomplete

Proxy history only contains what Burp captured. It may be missing traffic due to:
- **Scope filters** excluding domains
- **Intercept settings** dropping requests
- **Browser traffic** not routed through Burp proxy

If you don't find expected traffic, check Burp's scope and proxy settings in the original project.

### HTTP Body Encoding

Response bodies may be gzip compressed, chunked, or use non-UTF8 encoding. Regex patterns that work on plaintext may silently fail on encoded responses. If searches return fewer results than expected:
- Check if responses are compressed
- Try broader patterns or search headers first
- Use Burp's UI to inspect raw vs rendered response

## Rationalizations to Reject

Common shortcuts that lead to missed vulnerabilities or false reports:

| Shortcut | Why It's Wrong |
|----------|----------------|
| "This regex looks good" | Verify on sample data first—encoding and escaping cause silent failures |
| "High severity = must fix" | Check confidence score too; Burp has false positives |
| "All audit items are relevant" | Filter by actual threat model; not every finding matters for every app |
| "Proxy history is complete" | May be filtered by Burp scope/intercept settings; you see only what Burp captured |
| "Burp found it, so it's a vuln" | Burp findings require manual verification—they indicate potential issues, not proof |

## Output Format

All output is JSON, one object per line. Pipe to `jq` for formatting:
```bash
{baseDir}/scripts/burp-search.sh project.burp auditItems | jq .
```

Filter with grep:
```bash
{baseDir}/scripts/burp-search.sh project.burp auditItems | grep -i "sql injection"
```

## Examples

Search for CORS headers (with byte limit):
```bash
{baseDir}/scripts/burp-search.sh project.burp "responseHeader='.*Access-Control.*'" | head -c 50000
```

Get all high-severity findings (audit items are small, but still limit):
```bash
{baseDir}/scripts/burp-search.sh project.burp auditItems | jq -c 'select(.severity == "High")' | head -n 100
```

Extract just request URLs from proxy history:
```bash
{baseDir}/scripts/burp-search.sh project.burp proxyHistory.request.headers | jq -r '.request.url' | head -n 200
```

Search response bodies (MUST truncate body to 1000 chars):
```bash
{baseDir}/scripts/burp-search.sh project.burp "responseBody='.*password.*'" | \
  head -n 10 | jq -c '.body = (.body[:1000] + "...[TRUNCATED]")'
```

## Platform Configuration

The wrapper script requires two environment variables to locate Burp Suite's bundled Java and JAR file.

### macOS

```bash
export BURP_JAVA="/Applications/Burp Suite Professional.app/Contents/Resources/jre.bundle/Contents/Home/bin/java"
export BURP_JAR="/Applications/Burp Suite Professional.app/Contents/Resources/app/burpsuite_pro.jar"
```

### Windows

```powershell
$env:BURP_JAVA = "C:\Program Files\BurpSuiteProfessional\jre\bin\java.exe"
$env:BURP_JAR = "C:\Program Files\BurpSuiteProfessional\burpsuite_pro.jar"
```

### Linux

```bash
export BURP_JAVA="/opt/BurpSuiteProfessional/jre/bin/java"
export BURP_JAR="/opt/BurpSuiteProfessional/burpsuite_pro.jar"
```

Add these exports to your shell profile (`.bashrc`, `.zshrc`, etc.) for persistence.

### Manual Invocation

If not using the wrapper script, invoke directly:
```bash
"$BURP_JAVA" -jar -Djava.awt.headless=true "$BURP_JAR" \
  --project-file=/path/to/project.burp [FLAGS]
```
