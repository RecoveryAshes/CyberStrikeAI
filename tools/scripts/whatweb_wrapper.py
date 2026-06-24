#!/usr/bin/env python3
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, List
from urllib.parse import urlparse


REPO_ROOT = Path(__file__).resolve().parents[2]
CHAT_UPLOADS_DIR = REPO_ROOT / "chat_uploads"
TMP_OUTPUT_ROOT = Path(os.getenv("CYBERSTRIKE_TOOL_TMP_DIR", "/tmp/cyberstrike-ai-tools")) / "whatweb"
DEFAULT_BROWSER_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/120.0.0.0 Safari/537.36"
)
DEFAULT_UA = os.getenv("WHATWEB_DEFAULT_UA", DEFAULT_BROWSER_UA).strip() or DEFAULT_BROWSER_UA


def parse_items(raw: Any) -> List[str]:
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
    out: List[str] = []
    seen = set()
    for item in values:
        text = str(item or "").strip()
        if not text:
            continue
        if text not in seen:
            out.append(text)
            seen.add(text)
    return out


def normalize_url(raw: str) -> str:
    text = str(raw or "").strip()
    if not text:
        raise ValueError("empty target")
    if "://" not in text:
        text = "http://" + text
    parsed = urlparse(text)
    if parsed.scheme not in {"http", "https"} or not parsed.netloc:
        raise ValueError(f"invalid HTTP URL: {raw}")
    return text


def clean_segment(value: str, default: str) -> str:
    text = str(value or "").strip()
    if not text:
        return default
    text = re.sub(r"[^\w.\-\u4e00-\u9fff]+", "_", text, flags=re.UNICODE)
    text = text.strip("._-")
    return text[:80] or default


def build_output_dir(conversation_id: str, relative_dir: str) -> Path:
    date_segment = datetime.now().strftime("%Y%m%d")
    conversation_segment = clean_segment(conversation_id, "_manual")
    output_dir = CHAT_UPLOADS_DIR / date_segment / conversation_segment / "tool_outputs" / "whatweb"
    for part in Path(relative_dir or "").parts:
        segment = clean_segment(part, "")
        if segment:
            output_dir = output_dir / segment
    output_dir.mkdir(parents=True, exist_ok=True)
    return output_dir


def normalize_proxy(proxy: str) -> str:
    text = str(proxy or "").strip()
    if not text:
        return ""
    if "://" in text:
        parsed = urlparse(text)
        if not parsed.hostname:
            raise ValueError(f"invalid proxy: {proxy}")
        host = parsed.hostname
        port = parsed.port or 8080
        auth = ""
        if parsed.username:
            auth = parsed.username
            if parsed.password:
                auth += ":" + parsed.password
        return f"{host}:{port}", auth
    return text, ""


def plugin_values(raw: Dict[str, Any]) -> List[str]:
    values: List[str] = []
    for key in ("string", "module", "version", "account", "model", "os", "firmware", "filepath"):
        current = raw.get(key)
        if isinstance(current, list):
            values.extend(str(item) for item in current if str(item).strip())
        elif current:
            values.append(str(current))
    return values


def summarize_record(record: Dict[str, Any]) -> Dict[str, Any]:
    plugins = record.get("plugins") if isinstance(record.get("plugins"), dict) else {}
    plugin_names = sorted(plugins.keys())
    tech = []
    for name in plugin_names:
        values = plugin_values(plugins.get(name) or {})
        item: Dict[str, Any] = {"name": name}
        if values:
            item["values"] = values[:8]
        tech.append(item)
    title = ""
    if isinstance(plugins.get("Title"), dict):
        vals = plugin_values(plugins["Title"])
        title = vals[0] if vals else ""
    server = ""
    for key in ("HTTPServer", "Server"):
        if isinstance(plugins.get(key), dict):
            vals = plugin_values(plugins[key])
            if vals:
                server = vals[0]
                break
    return {
        "target": record.get("target", ""),
        "status_code": record.get("http_status"),
        "title": title,
        "server": server,
        "plugins": tech,
        "plugin_names": plugin_names,
    }


def load_json_records(path: Path) -> List[Dict[str, Any]]:
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8", errors="replace").strip()
    if not text:
        return []
    try:
        parsed = json.loads(text)
    except json.JSONDecodeError:
        records = []
        for line in text.splitlines():
            line = line.strip().rstrip(",")
            if not line or line in {"[", "]"}:
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError:
                continue
        return records
    return parsed if isinstance(parsed, list) else [parsed]


def main() -> int:
    parser = argparse.ArgumentParser(description="CyberStrikeAI WhatWeb wrapper.")
    parser.add_argument("--urls", required=True, help="JSON array, comma-separated, or newline-separated URL list.")
    parser.add_argument("--aggression", type=int, default=1)
    parser.add_argument("--threads", type=int, default=10)
    parser.add_argument("--open-timeout", type=int, default=10)
    parser.add_argument("--read-timeout", type=int, default=15)
    parser.add_argument("--proxy", default="")
    parser.add_argument("--user-agent", default=DEFAULT_UA)
    parser.add_argument("--conversation-id", default="")
    parser.add_argument("--relative-dir", default="")
    parser.add_argument("--output-name", default="whatweb")
    parser.add_argument("--save-debug-files", action="store_true", default=False)
    args = parser.parse_args()

    whatweb = shutil.which("whatweb")
    if not whatweb:
        raise RuntimeError("whatweb executable not found in PATH")

    targets = [normalize_url(item) for item in parse_items(args.urls)]
    if not targets:
        raise RuntimeError("urls cannot be empty")
    if len(targets) > 2000:
        raise RuntimeError("too many targets for one whatweb call; split the precheck")

    TMP_OUTPUT_ROOT.mkdir(parents=True, exist_ok=True)
    tmp_dir = Path(tempfile.mkdtemp(prefix="whatweb_", dir=str(TMP_OUTPUT_ROOT)))
    tmp_dir.mkdir(parents=True, exist_ok=True)
    input_file = tmp_dir / "targets.txt"
    json_file = tmp_dir / "whatweb.json"
    stdout_file = tmp_dir / "stdout.txt"
    stderr_file = tmp_dir / "stderr.txt"
    input_file.write_text("\n".join(targets) + "\n", encoding="utf-8")

    cmd = [
        whatweb,
        "--no-errors",
        "--colour=never",
        f"--aggression={max(1, min(args.aggression, 4))}",
        f"--max-threads={max(1, min(args.threads, 50))}",
        f"--open-timeout={max(1, args.open_timeout)}",
        f"--read-timeout={max(1, args.read_timeout)}",
        f"--user-agent={args.user_agent or DEFAULT_UA}",
        f"--log-json={json_file}",
        f"--input-file={input_file}",
    ]
    proxy_auth = ""
    if args.proxy:
        proxy_host, proxy_auth = normalize_proxy(args.proxy)
        cmd.append(f"--proxy={proxy_host}")
        if proxy_auth:
            cmd.append(f"--proxy-user={proxy_auth}")

    completed = subprocess.run(
        cmd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=max(30, len(targets) * max(args.open_timeout + args.read_timeout, 5)),
        check=False,
    )
    stdout_file.write_text(completed.stdout, encoding="utf-8", errors="replace")
    stderr_file.write_text(completed.stderr, encoding="utf-8", errors="replace")

    records = load_json_records(json_file)
    summaries = [summarize_record(record) for record in records]
    status_counts: Dict[str, int] = {}
    plugin_counts: Dict[str, int] = {}
    for item in summaries:
        code = str(item.get("status_code") or "unknown")
        status_counts[code] = status_counts.get(code, 0) + 1
        for plugin in item.get("plugin_names") or []:
            plugin_counts[plugin] = plugin_counts.get(plugin, 0) + 1

    output: Dict[str, Any] = {
        "status": "success" if completed.returncode == 0 else "partial",
        "tool": "whatweb",
        "target_count": len(targets),
        "result_count": len(records),
        "status_counts": dict(sorted(status_counts.items())),
        "plugin_counts": dict(sorted(plugin_counts.items(), key=lambda kv: (-kv[1], kv[0].lower()))[:80]),
        "summaries": summaries,
        "return_code": completed.returncode,
        "user_agent": args.user_agent or DEFAULT_UA,
        "stdout_tail": completed.stdout[-4000:],
        "stderr_tail": completed.stderr[-2000:],
        "command": [part if not proxy_auth or proxy_auth not in part else "--proxy-user=<redacted>" for part in cmd],
    }

    if args.save_debug_files:
        output_dir = build_output_dir(args.conversation_id, args.relative_dir)
        output_name = clean_segment(args.output_name, "whatweb")
        out_json = output_dir / f"{output_name}.results.json"
        out_raw = output_dir / f"{output_name}.raw.json"
        out_stdout = output_dir / f"{output_name}.stdout.txt"
        out_stderr = output_dir / f"{output_name}.stderr.txt"
        out_json.write_text(json.dumps(output, ensure_ascii=False, indent=2), encoding="utf-8")
        out_raw.write_text(json.dumps(records, ensure_ascii=False, indent=2), encoding="utf-8")
        out_stdout.write_text(completed.stdout, encoding="utf-8", errors="replace")
        out_stderr.write_text(completed.stderr, encoding="utf-8", errors="replace")
        output["artifacts"] = {
            "summary_json": str(out_json),
            "raw_json": str(out_raw),
            "stdout": str(out_stdout),
            "stderr": str(out_stderr),
        }

    print(json.dumps(output, ensure_ascii=False, indent=2))
    return 0 if completed.returncode == 0 else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(json.dumps({"status": "error", "error": str(exc)}, ensure_ascii=False), file=sys.stderr)
        raise SystemExit(1)
