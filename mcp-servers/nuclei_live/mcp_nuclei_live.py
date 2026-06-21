#!/usr/bin/env python3
"""
Nuclei Live MCP Server - 交互式 Nuclei 扫描 MCP 服务

通过 MCP 协议提供交互式 Nuclei 漏洞扫描能力：
- 按标签/模板/严重级别选择扫描策略
- 实时流式输出扫描结果
- 自定义模板生成
- 扫描结果管理

依赖：pip install mcp（或使用项目 venv）
需要：nuclei 已安装并在 PATH 中
运行：python mcp_nuclei_live.py
"""

import asyncio
import json
import os
import subprocess
import tempfile
import time
from typing import Any

from mcp.server.fastmcp import FastMCP

# ---------------------------------------------------------------------------
# 状态管理
# ---------------------------------------------------------------------------

_SCANS: dict[str, dict] = {}  # scan_id -> scan info
_SCAN_COUNTER = 0
_NUCLEI_BIN = os.environ.get("NUCLEI_BIN", "nuclei")
_DEFAULT_TIMEOUT = 300  # 5分钟超时


def _find_nuclei() -> str:
    """查找nuclei二进制路径"""
    # 优先环境变量
    if os.path.isfile(_NUCLEI_BIN):
        return _NUCLEI_BIN
    # 检查PATH
    result = subprocess.run(["which", "nuclei"], capture_output=True, text=True)
    if result.returncode == 0:
        return result.stdout.strip()
    return "nuclei"  # 默认，可能失败


def _generate_scan_id() -> str:
    global _SCAN_COUNTER
    _SCAN_COUNTER += 1
    return f"scan_{int(time.time())}_{_SCAN_COUNTER}"


# ---------------------------------------------------------------------------
# MCP Server
# ---------------------------------------------------------------------------

mcp = FastMCP("nuclei-live")


@mcp.tool()
def nuclei_scan(
    targets: str,
    tags: str = "",
    templates: str = "",
    severity: str = "",
    custom_flags: str = "",
    timeout: int = _DEFAULT_TIMEOUT,
) -> dict[str, Any]:
    """
    执行 Nuclei 扫描。

    Args:
        targets: 扫描目标，多个用逗号分隔（URL或IP）
        tags: Nuclei模板标签过滤，如 "cve,rce,sqli"
        templates: 指定模板路径，多个用逗号分隔
        severity: 严重级别过滤，如 "critical,high"
        custom_flags: 额外nuclei命令行参数
        timeout: 超时秒数（默认300秒）

    Returns:
        扫描结果字典，包含发现的漏洞列表
    """
    scan_id = _generate_scan_id()
    nuclei_path = _find_nuclei()

    # 构建命令
    cmd = [nuclei_path, "-jsonl", "-silent"]

    # 处理目标
    target_list = [t.strip() for t in targets.split(",") if t.strip()]
    if len(target_list) == 1:
        cmd.extend(["-u", target_list[0]])
    else:
        # 多目标写入临时文件
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False)
        tmp.write("\n".join(target_list))
        tmp.close()
        cmd.extend(["-l", tmp.name])

    # 添加过滤选项
    if tags:
        cmd.extend(["-tags", tags])
    if templates:
        cmd.extend(["-t", templates])
    if severity:
        cmd.extend(["-severity", severity])
    if custom_flags:
        cmd.extend(custom_flags.split())

    # 执行扫描
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )

        # 解析JSONL输出
        findings = []
        for line in result.stdout.strip().split("\n"):
            if line.strip():
                try:
                    finding = json.loads(line)
                    findings.append({
                        "template_id": finding.get("template-id", ""),
                        "name": finding.get("info", {}).get("name", ""),
                        "severity": finding.get("info", {}).get("severity", ""),
                        "host": finding.get("host", ""),
                        "matched_at": finding.get("matched-at", ""),
                        "matcher_name": finding.get("matcher-name", ""),
                        "extracted_results": finding.get("extracted-results", []),
                        "curl_command": finding.get("curl-command", ""),
                    })
                except json.JSONDecodeError:
                    continue

        scan_result = {
            "scan_id": scan_id,
            "status": "completed",
            "targets": target_list,
            "total_findings": len(findings),
            "findings": findings,
            "command": " ".join(cmd),
        }

        _SCANS[scan_id] = scan_result

        # 清理临时文件
        if len(target_list) > 1:
            os.unlink(tmp.name)

        return scan_result

    except subprocess.TimeoutExpired:
        return {
            "scan_id": scan_id,
            "status": "timeout",
            "error": f"扫描超时（{timeout}秒）",
            "command": " ".join(cmd),
        }
    except FileNotFoundError:
        return {
            "scan_id": scan_id,
            "status": "error",
            "error": f"nuclei未找到: {nuclei_path}。请确保nuclei已安装。",
        }
    except Exception as e:
        return {
            "scan_id": scan_id,
            "status": "error",
            "error": str(e),
        }


@mcp.tool()
def nuclei_scan_with_template(
    targets: str,
    template_content: str,
    timeout: int = _DEFAULT_TIMEOUT,
) -> dict[str, Any]:
    """
    使用自定义模板执行 Nuclei 扫描。

    Args:
        targets: 扫描目标URL
        template_content: Nuclei模板YAML内容
        timeout: 超时秒数

    Returns:
        扫描结果
    """
    scan_id = _generate_scan_id()

    # 写入临时模板文件
    tmp_template = tempfile.NamedTemporaryFile(
        mode="w", suffix=".yaml", delete=False
    )
    tmp_template.write(template_content)
    tmp_template.close()

    try:
        result = nuclei_scan(
            targets=targets,
            templates=tmp_template.name,
            timeout=timeout,
        )
        result["custom_template"] = True
        return result
    finally:
        os.unlink(tmp_template.name)


@mcp.tool()
def nuclei_list_templates(
    tags: str = "",
    severity: str = "",
    keyword: str = "",
) -> dict[str, Any]:
    """
    列出可用的 Nuclei 模板。

    Args:
        tags: 按标签过滤，如 "cve,rce"
        severity: 按严重级别过滤，如 "critical,high"
        keyword: 按关键词搜索模板名称

    Returns:
        匹配的模板列表
    """
    nuclei_path = _find_nuclei()
    cmd = [nuclei_path, "-tl", "-jsonl"]

    if tags:
        cmd.extend(["-tags", tags])
    if severity:
        cmd.extend(["-severity", severity])

    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)

        templates = []
        for line in result.stdout.strip().split("\n"):
            if line.strip():
                try:
                    tmpl = json.loads(line)
                    name = tmpl.get("name", "") or tmpl.get("template-id", "")
                    if keyword and keyword.lower() not in name.lower():
                        continue
                    templates.append({
                        "id": tmpl.get("template-id", ""),
                        "name": tmpl.get("name", ""),
                        "severity": tmpl.get("severity", ""),
                        "tags": tmpl.get("tags", []),
                    })
                except json.JSONDecodeError:
                    # 非JSON格式，按行处理
                    if keyword and keyword.lower() not in line.lower():
                        continue
                    templates.append({"id": line.strip()})

        return {
            "total": len(templates),
            "templates": templates[:100],  # 限制返回数量
        }
    except subprocess.TimeoutExpired:
        return {"error": "查询超时"}
    except FileNotFoundError:
        return {"error": f"nuclei未找到: {nuclei_path}"}
    except Exception as e:
        return {"error": str(e)}


@mcp.tool()
def nuclei_generate_template(
    template_id: str,
    name: str,
    severity: str,
    method: str,
    path: str,
    matchers: str,
    description: str = "",
) -> dict[str, Any]:
    """
    生成自定义 Nuclei 模板。

    Args:
        template_id: 模板唯一ID，如 "custom-sqli-login"
        name: 模板名称
        severity: 严重级别 (info/low/medium/high/critical)
        method: HTTP方法 (GET/POST)
        path: 请求路径，如 "/api/login"
        matchers: 匹配器描述，如 "status:200 AND body:admin"
        description: 漏洞描述

    Returns:
        生成的模板YAML内容
    """
    # 解析matchers
    matcher_list = []
    for m in matchers.split(" AND "):
        m = m.strip()
        if m.startswith("status:"):
            matcher_list.append({
                "type": "status",
                "status": [int(m.split(":")[1])],
            })
        elif m.startswith("body:"):
            matcher_list.append({
                "type": "word",
                "words": [m.split(":", 1)[1]],
                "part": "body",
            })
        elif m.startswith("header:"):
            matcher_list.append({
                "type": "word",
                "words": [m.split(":", 1)[1]],
                "part": "header",
            })
        elif m.startswith("regex:"):
            matcher_list.append({
                "type": "regex",
                "regex": [m.split(":", 1)[1]],
            })

    # 构建模板
    template = f"""id: {template_id}

info:
  name: {name}
  author: cyberstrike-ai
  severity: {severity}
  description: {description or name}

http:
  - method: {method}
    path:
      - "{{{{BaseURL}}}}{path}"
"""

    if matcher_list:
        template += "\n    matchers-condition: and\n    matchers:\n"
        for matcher in matcher_list:
            template += f"      - type: {matcher['type']}\n"
            if matcher["type"] == "status":
                template += f"        status:\n          - {matcher['status'][0]}\n"
            elif matcher["type"] == "word":
                template += f"        part: {matcher.get('part', 'body')}\n"
                template += f"        words:\n          - \"{matcher['words'][0]}\"\n"
            elif matcher["type"] == "regex":
                template += f"        regex:\n          - \"{matcher['regex'][0]}\"\n"

    return {
        "template_id": template_id,
        "template_content": template,
        "usage": f"使用 nuclei_scan_with_template(targets='...', template_content='...') 执行",
    }


@mcp.tool()
def nuclei_get_scan_result(scan_id: str) -> dict[str, Any]:
    """
    获取历史扫描结果。

    Args:
        scan_id: 扫描ID

    Returns:
        扫描结果
    """
    if scan_id in _SCANS:
        return _SCANS[scan_id]
    return {"error": f"扫描 {scan_id} 不存在", "available_scans": list(_SCANS.keys())}


@mcp.tool()
def nuclei_status() -> dict[str, Any]:
    """
    获取 Nuclei 状态信息。

    Returns:
        Nuclei版本、模板数量等信息
    """
    nuclei_path = _find_nuclei()

    try:
        # 获取版本
        version_result = subprocess.run(
            [nuclei_path, "-version"], capture_output=True, text=True, timeout=10
        )
        version = version_result.stdout.strip() or version_result.stderr.strip()

        # 获取模板统计
        stats_result = subprocess.run(
            [nuclei_path, "-stats", "-silent"],
            capture_output=True,
            text=True,
            timeout=10,
        )

        return {
            "nuclei_path": nuclei_path,
            "version": version,
            "stats": stats_result.stdout.strip(),
            "scans_in_memory": len(_SCANS),
        }
    except FileNotFoundError:
        return {"error": f"nuclei未找到: {nuclei_path}", "installed": False}
    except Exception as e:
        return {"error": str(e)}


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    mcp.run()
