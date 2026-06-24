#!/usr/bin/env python3
import argparse
import html
import json
import os
import re
import shutil
import subprocess
import sys
from datetime import datetime
from pathlib import Path
from typing import Optional


REPO_ROOT = Path(__file__).resolve().parents[2]
CHAT_UPLOADS_DIR = REPO_ROOT / "chat_uploads"
TMP_OUTPUT_ROOT = Path(os.getenv("CYBERSTRIKE_TOOL_TMP_DIR", "/tmp/cyberstrike-ai-tools")) / "nucleiPlus"
DDDD_BINARY_CANDIDATES = [
    "/Users/recovery/opt/tools/scan/dddd/ddddPro/dddd_darwin_arm64",
    "/home/user/tools/scan/dddd/ddddPro/dddd_linux_arm64",
    "/home/user/tools/scan/dddd/ddddPro/dddd_linux_amd64",
    "/home/user/tools/scan/dddd/ddddPro/dddd",
    "dddd",
]


def clean_segment(value: str, default: str) -> str:
    text = str(value or "").strip()
    if not text:
        return default
    text = re.sub(r"[^\w.\-\u4e00-\u9fff]+", "_", text, flags=re.UNICODE)
    text = text.strip("._-")
    return text[:80] or default


def parse_items(raw):
    if raw is None:
        return []
    if isinstance(raw, list):
        values = raw
    else:
        text = str(raw).strip()
        if not text:
            return []
        try:
            parsed = json.loads(text)
            values = parsed if isinstance(parsed, list) else [text]
        except json.JSONDecodeError:
            values = re.split(r"[\n,]+", text)
    return [str(item).strip() for item in values if str(item).strip()]


def validate_url(value: str) -> str:
    text = str(value or "").strip()
    if not text:
        raise ValueError("URL 不能为空")
    if not (text.startswith("http://") or text.startswith("https://")):
        raise ValueError(f"URL 必须显式包含 http:// 或 https://: {text}")
    return text


def validate_http_service(value: str) -> str:
    text = str(value or "").strip()
    if not text:
        raise ValueError("HTTP 服务不能为空")
    if "://" in text or "/" in text:
        raise ValueError(f"http_services 只允许 ip:port，不允许 URL: {text}")
    host, sep, port = text.rpartition(":")
    if not sep or not host or not port.isdigit():
        raise ValueError(f"http_services 必须是 ip:port 格式: {text}")
    port_num = int(port)
    if port_num < 1 or port_num > 65535:
        raise ValueError(f"端口超出范围: {text}")
    return f"{host}:{port_num}"


def find_dddd_binary() -> str:
    for candidate in DDDD_BINARY_CANDIDATES:
        resolved = shutil.which(candidate) if "/" not in candidate else candidate
        if resolved and Path(resolved).exists():
            return resolved
    raise RuntimeError("未找到 ddddPro 可执行文件，请检查 DDDD_BINARY_CANDIDATES 或 PATH")


def build_chat_output_dir(conversation_id: str, relative_dir: str) -> Path:
    date_segment = datetime.now().strftime("%Y%m%d")
    conversation_segment = clean_segment(conversation_id, "_manual")
    output_dir = CHAT_UPLOADS_DIR / date_segment / conversation_segment / "tool_outputs" / "nucleiPlus"
    for part in Path(relative_dir or "").parts:
        segment = clean_segment(part, "")
        if segment:
            output_dir = output_dir / segment
    output_dir.mkdir(parents=True, exist_ok=True)
    return output_dir


def build_tmp_run_dir(conversation_id: str, relative_dir: str, run_id: str) -> Path:
    date_segment = datetime.now().strftime("%Y%m%d")
    conversation_segment = clean_segment(conversation_id, "_manual")
    rel_segment = clean_segment(relative_dir, "run")
    output_dir = TMP_OUTPUT_ROOT / date_segment / conversation_segment / rel_segment / run_id
    output_dir.mkdir(parents=True, exist_ok=True)
    return output_dir


def copy_if_exists(src: Path, dst_dir: Path):
    if not src.exists():
        return None
    dst_dir.mkdir(parents=True, exist_ok=True)
    dst = dst_dir / src.name
    if src.resolve() != dst.resolve():
        shutil.copy2(src, dst)
    return dst


def append_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as f:
        f.write(text)


def append_jsonl(path: Path, item: dict) -> None:
    append_text(path, json.dumps(item, ensure_ascii=False) + "\n")


def ensure_text(value) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return str(value)


def write_html_report_index(path: Path, summary: dict, merged_results_file: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    merged_preview = ""
    if merged_results_file.exists():
        merged_preview = merged_results_file.read_text(encoding="utf-8", errors="replace")[-20000:]

    rows = []
    for batch in summary.get("batches", []):
        artifacts = batch.get("artifacts", {})
        html_report = artifacts.get("chat_html_file") or artifacts.get("html_file") or ""
        html_link = (
            f'<a href="{html.escape(Path(html_report).name)}">HTML</a>'
            if html_report and Path(html_report).parent == path.parent
            else html.escape(html_report or "")
        )
        rows.append(
            "<tr>"
            f"<td>{html.escape(str(batch.get('batch_id', '')))}</td>"
            f"<td>{html.escape(str(batch.get('status', '')))}</td>"
            f"<td>{html.escape(str(batch.get('target_count', '')))}</td>"
            f"<td>{html.escape(str(batch.get('return_code', '')))}</td>"
            f"<td>{html_link}</td>"
            f"<td>{html.escape(str(artifacts.get('result_file', '')))}</td>"
            "</tr>"
        )

    document = f"""<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <title>nucleiPlus Report</title>
  <style>
    body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 24px; color: #17202a; }}
    h1 {{ font-size: 22px; margin: 0 0 12px; }}
    h2 {{ font-size: 16px; margin-top: 28px; }}
    table {{ border-collapse: collapse; width: 100%; font-size: 13px; }}
    th, td {{ border: 1px solid #d7dde5; padding: 8px; text-align: left; vertical-align: top; }}
    th {{ background: #f5f7fa; }}
    code, pre {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }}
    pre {{ background: #f7f8fa; border: 1px solid #d7dde5; padding: 12px; overflow: auto; max-height: 640px; }}
    .meta {{ color: #52606d; font-size: 13px; line-height: 1.6; }}
  </style>
</head>
<body>
  <h1>nucleiPlus Report</h1>
  <div class="meta">
    <div>Status: <strong>{html.escape(str(summary.get("status", "")))}</strong></div>
    <div>Total targets: {html.escape(str(summary.get("inputs", {}).get("total_count", "")))}</div>
    <div>Batch count: {html.escape(str(summary.get("batching", {}).get("batch_count", "")))}</div>
    <div>Merged text: {html.escape(str(merged_results_file))}</div>
  </div>
  <h2>Batches</h2>
  <table>
    <thead><tr><th>Batch</th><th>Status</th><th>Targets</th><th>Return</th><th>HTML</th><th>Text Result</th></tr></thead>
    <tbody>{''.join(rows)}</tbody>
  </table>
  <h2>Merged Text Preview</h2>
  <pre>{html.escape(merged_preview)}</pre>
</body>
</html>
"""
    path.write_text(document, encoding="utf-8")


def chunked(items, size: int):
    for index in range(0, len(items), size):
        yield items[index:index + size]


def str_to_bool(value) -> bool:
    if isinstance(value, bool):
        return value
    if value is None:
        return False
    return str(value).strip().lower() in {"1", "true", "yes", "y", "on"}


def append_scan_options(cmd: list[str], args: argparse.Namespace, audit_log_path: Optional[Path] = None) -> list[str]:
    if args.severity:
        cmd.extend(["-s", args.severity])
    if args.exclude_tags:
        cmd.extend(["-et", args.exclude_tags])
    if args.poc_name:
        cmd.extend(["-poc", args.poc_name])
    if args.template_dir:
        cmd.extend(["-nt", args.template_dir])
    if args.workflow_yaml:
        cmd.extend(["-wy", args.workflow_yaml])
    if args.finger_yaml:
        cmd.extend(["-fy", args.finger_yaml])
    if args.dir_yaml:
        cmd.extend(["-dy", args.dir_yaml])
    if args.web_threads > 0:
        cmd.extend(["-wt", str(args.web_threads)])
    if args.web_timeout > 0:
        cmd.extend(["-wto", str(args.web_timeout)])
    if args.nmap_threads > 0:
        cmd.extend(["-tc", str(args.nmap_threads)])
    if args.nmap_timeout > 0:
        cmd.extend(["-nto", str(args.nmap_timeout)])
    if args.disable_interactsh:
        cmd.append("-ni")
    if args.audit_log and audit_log_path is not None:
        cmd.extend(["-a", "-alf", str(audit_log_path)])
    return cmd


def build_dddd_command(
    dddd_bin: str,
    args: argparse.Namespace,
    target_file: Path,
    result_file: Path,
    html_file: Path,
    audit_log_path: Optional[Path] = None,
) -> list[str]:
    cmd = [
        dddd_bin,
        "-active",
        "-t", str(target_file),
        "-Pn",
    ]
    if args.scan_mode == "precheck":
        cmd.extend(["-npoc", "-nd"])
    cmd.extend([
        "-ngp",
        "-nb",
        "-nhb",
        "-o", str(result_file),
        "-ho", str(html_file),
    ])
    append_scan_options(cmd, args, audit_log_path)
    return cmd


def run_dddd_batch(
    dddd_bin: str,
    args: argparse.Namespace,
    batch_dir: Path,
    batch_id: str,
    batch_items: list[str],
    timeout_seconds: int,
    env: dict[str, str],
    debug_chat_dir: Path,
    html_reports_dir: Path,
    merged_results_file: Path,
    runs_index_file: Path,
) -> dict:
    batch_dir.mkdir(parents=True, exist_ok=True)
    stdout_file = batch_dir / "dddd.stdout.log"
    stderr_file = batch_dir / "dddd.stderr.log"
    target_file = batch_dir / "targets.txt"
    result_file = batch_dir / f"{batch_id}.txt"
    html_file = batch_dir / f"{batch_id}.html"
    target_file.write_text("\n".join(batch_items) + "\n", encoding="utf-8")

    cmd = build_dddd_command(dddd_bin, args, target_file, result_file, html_file, batch_dir / "audit.log")

    try:
        completed = subprocess.run(
            cmd,
            cwd=batch_dir,
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout_seconds,
        )
        stdout = completed.stdout or ""
        stderr = completed.stderr or ""
        stdout_file.write_text(stdout, encoding="utf-8")
        stderr_file.write_text(stderr, encoding="utf-8")
        timed_out = False
        return_code = completed.returncode
        status = "success" if return_code == 0 else "error"
        message = "ddddPro 扫描完成" if return_code == 0 else "ddddPro 扫描返回非零状态"
    except subprocess.TimeoutExpired as exc:
        stdout = ensure_text(exc.stdout)
        stderr = ensure_text(exc.stderr)
        stdout_file.write_text(stdout, encoding="utf-8")
        stderr_file.write_text(stderr, encoding="utf-8")
        timed_out = True
        return_code = None
        status = "timeout"
        message = f"ddddPro 执行超时，超过 {timeout_seconds} 秒"
        cmd = exc.cmd if isinstance(exc.cmd, list) else cmd

    copied_targets = copied_stdout = copied_stderr = copied_result = copied_html = None
    if args.save_debug_files:
        batch_debug_dir = debug_chat_dir / batch_id
        copied_targets = copy_if_exists(target_file, batch_debug_dir)
        copied_stdout = copy_if_exists(stdout_file, batch_debug_dir)
        copied_stderr = copy_if_exists(stderr_file, batch_debug_dir)
        copied_result = copy_if_exists(result_file, batch_debug_dir) if result_file.exists() else None
        copied_html = copy_if_exists(html_file, batch_debug_dir) if html_file.exists() else None

    appended_result_bytes = 0
    if result_file.exists() and result_file.stat().st_size > 0:
        result_text = result_file.read_text(encoding="utf-8", errors="replace")
        if result_text.strip():
            block = [
                f"\n===== {batch_id} =====\n",
                f"status: {status}\n",
                f"targets: {len(batch_items)}\n",
                result_text.rstrip("\n") + "\n",
            ]
            payload = "".join(block)
            append_text(merged_results_file, payload)
            appended_result_bytes = len(payload.encode("utf-8", errors="replace"))

    chat_html_file = None
    if html_file.exists() and html_file.stat().st_size > 0:
        chat_html_file = copy_if_exists(html_file, html_reports_dir)

    index_item = {
        "batch_id": batch_id,
        "status": status,
        "return_code": return_code,
        "timed_out": timed_out,
        "target_count": len(batch_items),
        "target_file": str(target_file),
        "result_file": str(result_file) if result_file.exists() else "",
        "html_file": str(html_file) if html_file.exists() else "",
        "chat_html_file": str(chat_html_file) if chat_html_file else "",
        "stdout_log": str(stdout_file),
        "stderr_log": str(stderr_file),
        "appended_result_bytes": appended_result_bytes,
    }
    append_jsonl(runs_index_file, index_item)

    return {
        "batch_id": batch_id,
        "status": status,
        "message": message,
        "return_code": return_code,
        "timed_out": timed_out,
        "target_count": len(batch_items),
        "appended_result_bytes": appended_result_bytes,
        "command": cmd,
        "artifacts": {
            "batch_dir": str(batch_dir),
            "target_file": str(target_file),
            "result_file": str(result_file) if result_file.exists() else "",
            "html_file": str(html_file) if html_file.exists() else "",
            "chat_html_file": str(chat_html_file) if chat_html_file else "",
            "stdout_log": str(stdout_file),
            "stderr_log": str(stderr_file),
        },
        "debug_copies": {
            "target_file": str(copied_targets) if copied_targets else "",
            "stdout_log": str(copied_stdout) if copied_stdout else "",
            "stderr_log": str(copied_stderr) if copied_stderr else "",
            "result_file": str(copied_result) if copied_result else "",
            "html_file": str(copied_html) if copied_html else "",
        },
        "stdout_tail": stdout[-4000:],
        "stderr_tail": stderr[-2500:],
    }


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Constrained ddddPro wrapper. Defaults to vuln_scan for penetration testing; pass scan_mode=precheck for status/fingerprint only."
    )
    parser.add_argument("--urls", help="URL 列表；支持 JSON 数组、逗号分隔或换行分隔。")
    parser.add_argument("--http-services", help="HTTP 服务 ip:port 列表；支持 JSON 数组、逗号分隔或换行分隔。")
    parser.add_argument(
        "--scan-mode",
        choices=["precheck", "vuln_scan"],
        default="vuln_scan",
        help="precheck 仅做状态/被动指纹并加 -npoc -nd；vuln_scan 允许 nuclei 模板/POC 联动。默认 vuln_scan，保持渗透测试角色原有能力。",
    )
    parser.add_argument("--severity", default="", help="限制严重程度，如 critical,high。")
    parser.add_argument("--exclude-tags", default="", help="排除的 nuclei tags，逗号分隔。")
    parser.add_argument("--poc-name", default="", help="按名称模糊匹配 POC。")
    parser.add_argument("--template-dir", default="", help="自定义 nuclei 模板目录。")
    parser.add_argument("--workflow-yaml", default="", help="自定义 workflow.yaml 路径。")
    parser.add_argument("--finger-yaml", default="", help="自定义 finger.yaml 路径。")
    parser.add_argument("--dir-yaml", default="", help="自定义 dir.yaml 路径。")
    parser.add_argument("--web-threads", type=int, default=0, help="Web 探针线程数。")
    parser.add_argument("--web-timeout", type=int, default=0, help="Web 探针超时秒数。")
    parser.add_argument("--nmap-threads", type=int, default=0, help="协议识别线程数。")
    parser.add_argument("--nmap-timeout", type=int, default=0, help="协议识别超时秒数。")
    parser.add_argument("--disable-interactsh", action="store_true", help="禁用 interactsh。")
    parser.add_argument("--audit-log", action="store_true", help="开启 dddd 审计日志。")
    parser.add_argument("--output-name", default="nucleiPlus", help="输出文件名前缀。")
    parser.add_argument("--conversation-id", default="", help="会话 ID。")
    parser.add_argument("--relative-dir", default="", help="输出子目录。")
    parser.add_argument("--save-debug-files", action="store_true", help="把临时调试文件复制到 chat_uploads。")
    parser.add_argument("--process-timeout", type=int, default=0, help="wrapper 对 dddd 子进程的超时秒数。")
    parser.add_argument("--batch-size", type=int, default=30, help="每批目标数量，默认 30。")
    parser.add_argument("--max-targets", type=int, default=500, help="单次调用允许的最大目标数，默认 500。")
    parser.add_argument("--continue-on-error", default="true", help="某批失败后是否继续后续批次，默认 true。")
    return parser


def main() -> int:
    parser = build_arg_parser()
    args = parser.parse_args()

    if args.scan_mode == "precheck":
        forbidden = {
            "severity": args.severity,
            "exclude_tags": args.exclude_tags,
            "poc_name": args.poc_name,
            "template_dir": args.template_dir,
            "workflow_yaml": args.workflow_yaml,
        }
        used = [name for name, value in forbidden.items() if str(value or "").strip()]
        if used:
            print(json.dumps({
                "status": "error",
                "message": "precheck 模式只允许状态/指纹识别，禁止传入 nuclei POC/模板扫描参数；如确需漏洞模板扫描，请显式设置 scan_mode=vuln_scan。",
                "forbidden_parameters": used,
            }, ensure_ascii=False, indent=2))
            return 2

    raw_urls = parse_items(args.urls)
    raw_http_services = parse_items(args.http_services)
    if not raw_urls and not raw_http_services:
        parser.error("urls 和 http_services 至少要提供一项")

    try:
        urls = [validate_url(item) for item in raw_urls]
        http_services = [validate_http_service(item) for item in raw_http_services]
    except ValueError as exc:
        print(json.dumps({"status": "error", "message": str(exc)}, ensure_ascii=False, indent=2))
        return 2

    urls = list(dict.fromkeys(urls))
    http_services = list(dict.fromkeys(http_services))
    target_lines = urls + http_services
    max_targets = args.max_targets if args.max_targets > 0 else 500
    if len(target_lines) > max_targets:
        print(json.dumps({
            "status": "error",
            "message": f"目标数量 {len(target_lines)} 超过 max_targets={max_targets}，请拆分任务或显式调大 max_targets",
            "inputs": {
                "url_count": len(urls),
                "http_service_count": len(http_services),
            },
        }, ensure_ascii=False, indent=2))
        return 2
    batch_size = args.batch_size if args.batch_size > 0 else 30
    continue_on_error = str_to_bool(args.continue_on_error)

    dddd_bin = find_dddd_binary()
    run_id = clean_segment(args.output_name, "nucleiPlus") + "_" + datetime.now().strftime("%H%M%S")
    tmp_dir = build_tmp_run_dir(args.conversation_id, args.relative_dir, run_id)
    chat_dir = build_chat_output_dir(args.conversation_id, args.relative_dir)
    debug_chat_dir = chat_dir / "debug"
    merged_results_file = chat_dir / "nucleiPlus.results.txt"
    runs_index_file = chat_dir / "nucleiPlus.runs.jsonl"
    html_index_file = chat_dir / "nucleiPlus.report.html"
    html_reports_dir = chat_dir / "html_reports"
    env = os.environ.copy()
    timeout_seconds = args.process_timeout if args.process_timeout > 0 else 1800

    batches = []
    overall_status = "success"
    for batch_index, batch_items in enumerate(chunked(target_lines, batch_size), start=1):
        batch_id = f"{run_id}_batch_{batch_index:03d}"
        batch_result = run_dddd_batch(
            dddd_bin=dddd_bin,
            args=args,
            batch_dir=tmp_dir / batch_id,
            batch_id=batch_id,
            batch_items=batch_items,
            timeout_seconds=timeout_seconds,
            env=env,
            debug_chat_dir=debug_chat_dir,
            html_reports_dir=html_reports_dir,
            merged_results_file=merged_results_file,
            runs_index_file=runs_index_file,
        )
        batches.append(batch_result)
        if batch_result["status"] != "success":
            overall_status = "partial_error" if continue_on_error else "error"
            if not continue_on_error:
                break

    summary = {
        "status": overall_status,
        "message": "nucleiPlus 分批扫描完成" if overall_status == "success" else "nucleiPlus 分批扫描存在失败批次",
        "binary": dddd_bin,
        "inputs": {
            "url_count": len(urls),
            "http_service_count": len(http_services),
            "total_count": len(target_lines),
            "urls": urls,
            "http_services": http_services,
        },
        "batching": {
            "enabled": True,
            "batch_size": batch_size,
            "batch_count": len(batches),
            "process_timeout_per_batch": timeout_seconds,
            "continue_on_error": continue_on_error,
            "max_targets": max_targets,
        },
        "constraints": {
            "scan_mode": args.scan_mode,
            "active_finger": args.scan_mode != "precheck",
            "passive_finger": True,
            "nuclei_linked_scan": args.scan_mode == "vuln_scan",
            "disabled_nuclei_poc": args.scan_mode == "precheck",
            "disabled_active_web_fingerprint": args.scan_mode == "precheck",
            "disabled_gopoc": True,
            "disabled_bruteforce": True,
            "disabled_host_bind": True,
            "notes": [
                "不暴露测绘引擎参数",
                "不暴露子域名枚举参数",
                "不暴露 masscan/SYN 扫描参数",
                "对 http_services 仅接受 ip:port，实际由 ddddPro 协议识别后补 http/https",
            ],
        },
        "artifacts": {
            "tmp_dir": str(tmp_dir),
            "merged_results_file": str(merged_results_file),
            "runs_index_file": str(runs_index_file),
            "html_index_file": str(html_index_file),
            "html_reports_dir": str(html_reports_dir),
            "merged_results_exists": merged_results_file.exists(),
            "merged_results_bytes": merged_results_file.stat().st_size if merged_results_file.exists() else 0,
        },
        "batches": batches,
    }
    write_html_report_index(html_index_file, summary, merged_results_file)
    summary["artifacts"]["html_index_exists"] = html_index_file.exists()
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0 if overall_status == "success" or (overall_status == "partial_error" and continue_on_error) else 1


if __name__ == "__main__":
    sys.exit(main())
