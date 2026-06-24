# Nuclei Live MCP

[English](README.md)

通过MCP协议为CyberStrikeAI添加**交互式Nuclei扫描**能力：按标签/模板/严重级别动态选择扫描策略、生成自定义模板、实时获取结果——无需修改后端代码。

## 工具列表

| 工具 | 描述 |
|------|------|
| `nuclei_scan` | 执行Nuclei扫描，支持按标签/模板/严重级别过滤 |
| `nuclei_scan_with_template` | 使用自定义YAML模板执行扫描 |
| `nuclei_list_templates` | 列出可用的Nuclei模板，支持过滤 |
| `nuclei_generate_template` | 生成自定义Nuclei模板 |
| `nuclei_get_scan_result` | 获取历史扫描结果 |
| `nuclei_status` | 查看Nuclei状态（版本、模板数量等） |

## 需求

- Python 3.10+
- `mcp` 包（项目venv已包含，或 `pip install mcp`）
- **nuclei** 已安装并在PATH中（https://github.com/projectdiscovery/nuclei）

## CyberStrikeAI中配置

1. **路径示例**
   项目根目录：`/path/to/CyberStrikeAI`
   脚本：`/path/to/CyberStrikeAI/mcp-servers/nuclei_live/mcp_nuclei_live.py`
   Python：`/path/to/CyberStrikeAI/venv/bin/python3`（或系统Python）

2. **Web UI 配置**
   打开 **设置 → 外部MCP** → **添加外部MCP**

   **JSON配置**：
   ```json
   {
     "name": "nuclei-live",
     "type": "stdio",
     "command": "/path/to/CyberStrikeAI/venv/bin/python3",
     "args": [
       "/path/to/CyberStrikeAI/mcp-servers/nuclei_live/mcp_nuclei_live.py"
     ],
     "env": {}
   }
   ```

3. **保存并启动**
   点击 **启动**。工具列表中将出现 `nuclei_scan`, `nuclei_list_templates` 等。

## 使用示例

### 1. 按标签扫描

```
AI: 使用nuclei扫描 https://example.com，查找SQL注入漏洞

执行：nuclei_scan(
  targets="https://example.com",
  tags="sqli",
  severity="high,critical"
)

结果：发现2个SQL注入漏洞
- CVE-2023-1234 (critical)
- Generic SQLi (high)
```

### 2. 生成自定义模板并扫描

```
AI: 生成一个检测 /api/debug 是否返回敏感信息的模板

1. nuclei_generate_template(
     template_id="custom-debug-leak",
     name="Debug Endpoint Information Disclosure",
     severity="medium",
     method="GET",
     path="/api/debug",
     matchers="status:200 AND body:password"
   )

2. nuclei_scan_with_template(
     targets="https://target.com",
     template_content="<生成的YAML>"
   )
```

### 3. 查询可用模板

```
AI: 列出所有关于JWT的高危模板

nuclei_list_templates(
  keyword="jwt",
  severity="high,critical"
)

返回：
- jwt-weak-secret
- jwt-none-algorithm
- ...
```

## 优势

- **无需重启后端**：MCP stdio方式，动态加载
- **交互式选择**：AI根据对话上下文动态选择模板/标签
- **自定义模板**：即时生成专用PoC模板
- **结果管理**：扫描结果保存在内存，可随时查询

## 故障排查

### 问题：`nuclei未找到`

**解决**：
```bash
# 安装nuclei
go install -v github.com/projectdiscovery/nuclei/v3/cmd/nuclei@latest

# 或使用包管理器
brew install nuclei  # macOS
apt install nuclei   # Debian/Ubuntu

# 验证
nuclei -version
```

### 问题：MCP启动失败

**排查**：
1. 检查Python路径是否正确
2. 检查脚本路径是否正确
3. 查看CyberStrikeAI日志：`logs/app.log`

### 问题：扫描超时

**解决**：调整timeout参数
```
nuclei_scan(
  targets="...",
  timeout=600  # 增加到10分钟
)
```

## 参考

- [Nuclei 文档](https://docs.projectdiscovery.io/tools/nuclei)
- [Nuclei 模板库](https://github.com/projectdiscovery/nuclei-templates)
- [MCP 协议](https://modelcontextprotocol.io/)
