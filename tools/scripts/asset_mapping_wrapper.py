#!/usr/bin/env python3
import argparse
import base64
import html
import ipaddress
import json
import os
import re
import sys
from datetime import datetime
from pathlib import Path
from typing import Any, Optional

import requests


REPO_ROOT = Path(__file__).resolve().parents[2]
CHAT_UPLOADS_DIR = REPO_ROOT / "chat_uploads"
DEFAULT_QUAKE_URL = "https://quake.360.cn/api/v3/search/quake_service"
DEFAULT_ZOOMEYE_URL = "https://api.zoomeye.org/v2/search"


def clean_segment(value: str, default: str) -> str:
    text = str(value or "").strip()
    if not text:
        return default
    text = re.sub(r"[^\w.\-\u4e00-\u9fff]+", "_", text, flags=re.UNICODE).strip("._-")
    return text[:80] or default


def parse_items(raw: Any) -> list[str]:
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
            values = re.split(r"[\s,;]+", text)
    seen = set()
    output = []
    for item in values:
        value = str(item).strip()
        if value and value not in seen:
            seen.add(value)
            output.append(value)
    return output


def validate_domain(value: str) -> str:
    text = str(value or "").strip().lower().rstrip(".")
    if not text or "://" in text or "/" in text or ":" in text:
        raise ValueError(f"domains 只允许裸域名，不允许 URL、路径或端口: {value}")
    if len(text) > 253 or "." not in text:
        raise ValueError(f"非法域名: {value}")
    label_re = re.compile(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
    if not all(label_re.match(part) for part in text.split(".")):
        raise ValueError(f"非法域名: {value}")
    return text


def validate_ip(value: str) -> str:
    text = str(value or "").strip()
    if "/" in text or ":" in text:
        raise ValueError(f"ips 只允许单个 IP，不允许 CIDR、端口或范围: {value}")
    try:
        return str(ipaddress.ip_address(text))
    except ValueError as exc:
        raise ValueError(f"非法 IP: {value}") from exc


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def config_lookup(path: list[str]) -> str:
    config_path = REPO_ROOT / "config.yaml"
    text = read_text(config_path)
    if not text:
        return ""
    try:
        import yaml  # type: ignore

        data = yaml.safe_load(text) or {}
        current: Any = data
        for key in path:
            if not isinstance(current, dict):
                return ""
            current = current.get(key)
        return str(current or "").strip()
    except Exception:
        # Minimal fallback for simple two-level YAML values used by config.yaml.
        if len(path) == 2:
            section, key = path
            m = re.search(rf"(?ms)^{re.escape(section)}:\s*\n(?P<body>(?:^[ \t]+.*\n?)*)", text)
            if m:
                km = re.search(rf"(?m)^[ \t]+{re.escape(key)}:\s*[\"']?([^\"'\n#]+)", m.group("body"))
                if km:
                    return km.group(1).strip()
    return ""


def extract_inline_key(tool_file: str, var_name: str) -> str:
    text = read_text(REPO_ROOT / "tools" / tool_file)
    if not text:
        return ""
    m = re.search(rf'{re.escape(var_name)}\s*=\s*["\']([^"\']+)["\']', text)
    return m.group(1).strip() if m else ""


def build_chat_output_dir(conversation_id: str, relative_dir: str) -> Path:
    date_segment = datetime.now().strftime("%Y%m%d")
    conv = clean_segment(conversation_id, "_manual")
    output_dir = CHAT_UPLOADS_DIR / date_segment / conv / "tool_outputs" / "asset_mapping"
    for part in Path(relative_dir or "").parts:
        segment = clean_segment(part, "")
        if segment:
            output_dir = output_dir / segment
    output_dir.mkdir(parents=True, exist_ok=True)
    return output_dir


def safe_get(data: Any, path: list[Any], default: Any = "") -> Any:
    current = data
    for key in path:
        if isinstance(current, dict):
            current = current.get(key, default)
        elif isinstance(current, list) and isinstance(key, int) and 0 <= key < len(current):
            current = current[key]
        else:
            return default
    return current if current is not None else default


def first_value(*values: Any) -> str:
    for value in values:
        if value is None:
            continue
        if isinstance(value, (list, tuple)):
            for item in value:
                text = first_value(item)
                if text:
                    return text
            continue
        text = str(value).strip()
        if text:
            return text
    return ""


def to_int(value: Any) -> Optional[int]:
    try:
        if value is None or str(value).strip() == "":
            return None
        port = int(value)
        if 1 <= port <= 65535:
            return port
    except (TypeError, ValueError):
        return None
    return None


def infer_scheme(raw: dict[str, Any], port: Optional[int]) -> str:
    scheme = first_value(
        raw.get("scheme"),
        raw.get("protocol"),
        raw.get("transport"),
        safe_get(raw, ["service", "http", "scheme"]),
        safe_get(raw, ["service", "name"]),
        raw.get("service"),
    ).lower()
    if scheme in {"https", "ssl", "tls"}:
        return "https"
    if scheme in {"http", "http-proxy"}:
        return "http"
    if port in {443, 8443, 9443}:
        return "https"
    if port in {80, 8080, 8000, 8888}:
        return "http"
    return ""


def normalize_url(url: str) -> str:
    text = str(url or "").strip()
    if not text:
        return ""
    return text.rstrip("/")


def normalize_quake_record(raw: dict[str, Any], input_type: str, input_value: str) -> dict[str, Any]:
    port = to_int(first_value(raw.get("port"), safe_get(raw, ["service", "port"])))
    ip = first_value(raw.get("ip"), raw.get("host"), safe_get(raw, ["service", "ip"]))
    domain = first_value(
        raw.get("domain"),
        raw.get("hostname"),
        safe_get(raw, ["service", "http", "host"]),
        safe_get(raw, ["service", "http", "hosts"]),
    )
    host = domain or ip
    title = first_value(raw.get("title"), safe_get(raw, ["service", "http", "title"]))
    service = first_value(raw.get("service_name"), safe_get(raw, ["service", "name"]), raw.get("service"))
    url = normalize_url(first_value(raw.get("url"), safe_get(raw, ["service", "http", "url"])))
    scheme = infer_scheme(raw, port)
    if not url and scheme and host and port:
        default_port = (scheme == "http" and port == 80) or (scheme == "https" and port == 443)
        url = f"{scheme}://{host}" if default_port else f"{scheme}://{host}:{port}"
    return {
        "sources": ["quake"],
        "input_type": input_type,
        "input": input_value,
        "host": host,
        "ip": ip,
        "port": port,
        "url": url,
        "scheme": scheme,
        "service": service,
        "title": title,
        "domain": domain,
        "raw_refs": [{"source": "quake", "raw": raw}],
    }


def normalize_zoomeye_record(raw: dict[str, Any], input_type: str, input_value: str) -> dict[str, Any]:
    port = to_int(first_value(raw.get("port"), safe_get(raw, ["portinfo", "port"])))
    ip = first_value(raw.get("ip"), raw.get("ip_str"), raw.get("host"), safe_get(raw, ["ipinfo", "ip"]))
    domain = first_value(raw.get("domain"), raw.get("hostname"), raw.get("site"), raw.get("rdns"))
    host = domain or ip
    title = first_value(raw.get("title"), safe_get(raw, ["webapp", 0, "name"]), safe_get(raw, ["portinfo", "title"]))
    service = first_value(raw.get("service"), raw.get("app"), raw.get("product"), safe_get(raw, ["portinfo", "service"]))
    url = normalize_url(first_value(raw.get("url"), raw.get("site")))
    scheme = infer_scheme(raw, port)
    if not url and scheme and host and port:
        default_port = (scheme == "http" and port == 80) or (scheme == "https" and port == 443)
        url = f"{scheme}://{host}" if default_port else f"{scheme}://{host}:{port}"
    return {
        "sources": ["zoomeye"],
        "input_type": input_type,
        "input": input_value,
        "host": host,
        "ip": ip,
        "port": port,
        "url": url,
        "scheme": scheme,
        "service": service,
        "title": title,
        "domain": domain,
        "raw_refs": [{"source": "zoomeye", "raw": raw}],
    }


def asset_key(asset: dict[str, Any]) -> str:
    url = normalize_url(asset.get("url", "")).lower()
    if url:
        return f"url:{url}"
    ip = str(asset.get("ip") or "").lower()
    host = str(asset.get("host") or "").lower()
    port = asset.get("port")
    if (ip or host) and port:
        return f"svc:{ip or host}:{port}"
    if ip:
        return f"ip:{ip}"
    if host:
        return f"host:{host}"
    return "raw:" + json.dumps(asset.get("raw_refs", []), ensure_ascii=False, sort_keys=True)[:500]


def merge_asset(existing: dict[str, Any], incoming: dict[str, Any]) -> dict[str, Any]:
    for source in incoming.get("sources", []):
        if source not in existing["sources"]:
            existing["sources"].append(source)
    for key in ("host", "ip", "port", "url", "scheme", "service", "title", "domain"):
        if not existing.get(key) and incoming.get(key):
            existing[key] = incoming[key]
    existing.setdefault("inputs", [])
    pair = {"input_type": incoming.get("input_type"), "input": incoming.get("input")}
    if pair not in existing["inputs"]:
        existing["inputs"].append(pair)
    existing.setdefault("raw_refs", []).extend(incoming.get("raw_refs", []))
    return existing


def quake_query(api_key: str, query: str, size: int, timeout: int, base_url: str) -> dict[str, Any]:
    data = {
        "query": query,
        "size": size,
        "start": 0,
        "latest": True,
        "include": [
            "ip",
            "port",
            "domain",
            "hostname",
            "service.name",
            "service.http.title",
            "service.http.host",
        ],
    }
    response = requests.post(
        base_url,
        json=data,
        headers={"X-QuakeToken": api_key, "Content-Type": "application/json"},
        timeout=timeout,
    )
    try:
        body = response.json()
    except ValueError:
        response.raise_for_status()
        raise RuntimeError(f"Quake API返回非 JSON 响应: HTTP {response.status_code}")
    response.raise_for_status()
    if body.get("code") != 0:
        raise RuntimeError(f"Quake API错误: {body.get('message', 'unknown')} (code={body.get('code')})")
    return body


def zoomeye_query(api_key: str, query: str, size: int, timeout: int, base_url: str, sub_type: str) -> dict[str, Any]:
    data = {
        "qbase64": base64.b64encode(query.encode("utf-8")).decode("utf-8"),
        "page": 1,
        "pagesize": size,
        "fields": "ip,port,domain,hostname,url,service,title,app,product,rdns,update_time",
        "sub_type": sub_type,
    }
    response = requests.post(
        base_url,
        json=data,
        headers={"API-KEY": api_key, "Content-Type": "application/json"},
        timeout=timeout,
    )
    try:
        body = response.json()
    except ValueError:
        response.raise_for_status()
        raise RuntimeError(f"ZoomEye API返回非 JSON 响应: HTTP {response.status_code}")
    if response.status_code >= 400:
        message = body.get("message") or body.get("error") or response.text[:200]
        raise RuntimeError(f"ZoomEye API错误: {message} (HTTP {response.status_code}, code={body.get('code', 'unknown')})")
    if body.get("code") != 60000:
        raise RuntimeError(f"ZoomEye API错误: {body.get('message', 'unknown')} (code={body.get('code')})")
    return body


def write_json(path: Path, data: Any) -> None:
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def write_outputs(output_dir: Path, assets: list[dict[str, Any]], raw_runs: list[dict[str, Any]], summary: dict[str, Any]) -> dict[str, str]:
    assets_jsonl = output_dir / "asset_mapping.assets.jsonl"
    urls_txt = output_dir / "asset_mapping.urls.txt"
    http_services_txt = output_dir / "asset_mapping.http_services.txt"
    raw_json = output_dir / "asset_mapping.raw.json"
    summary_json = output_dir / "asset_mapping.summary.json"
    html_report = output_dir / "asset_mapping.report.html"

    with assets_jsonl.open("w", encoding="utf-8") as f:
        for asset in assets:
            f.write(json.dumps(asset, ensure_ascii=False) + "\n")

    urls = sorted({asset["url"] for asset in assets if asset.get("url")})
    services = sorted(
        {
            f"{asset.get('ip') or asset.get('host')}:{asset.get('port')}"
            for asset in assets
            if (asset.get("ip") or asset.get("host")) and asset.get("port")
        }
    )
    urls_txt.write_text("\n".join(urls) + ("\n" if urls else ""), encoding="utf-8")
    http_services_txt.write_text("\n".join(services) + ("\n" if services else ""), encoding="utf-8")
    write_json(raw_json, raw_runs)
    write_json(summary_json, summary)

    rows = []
    for asset in assets[:1000]:
        rows.append(
            "<tr>"
            f"<td>{html.escape(','.join(asset.get('sources', [])))}</td>"
            f"<td>{html.escape(str(asset.get('host') or ''))}</td>"
            f"<td>{html.escape(str(asset.get('ip') or ''))}</td>"
            f"<td>{html.escape(str(asset.get('port') or ''))}</td>"
            f"<td>{html.escape(str(asset.get('service') or ''))}</td>"
            f"<td>{html.escape(str(asset.get('title') or ''))}</td>"
            f"<td>{html.escape(str(asset.get('url') or ''))}</td>"
            "</tr>"
        )
    html_report.write_text(
        f"""<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <title>Asset Mapping Report</title>
  <style>
    body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 24px; color: #17202a; }}
    h1 {{ font-size: 22px; margin: 0 0 12px; }}
    .meta {{ color: #52606d; font-size: 13px; line-height: 1.6; margin-bottom: 16px; }}
    table {{ border-collapse: collapse; width: 100%; font-size: 13px; }}
    th, td {{ border: 1px solid #d7dde5; padding: 8px; text-align: left; vertical-align: top; }}
    th {{ background: #f5f7fa; }}
  </style>
</head>
<body>
  <h1>Asset Mapping Report</h1>
  <div class="meta">
    assets={summary.get('asset_count', 0)} urls={summary.get('url_count', 0)} http_services={summary.get('http_service_count', 0)}
  </div>
  <table>
    <thead><tr><th>sources</th><th>host</th><th>ip</th><th>port</th><th>service</th><th>title</th><th>url</th></tr></thead>
    <tbody>{''.join(rows)}</tbody>
  </table>
</body>
</html>
""",
        encoding="utf-8",
    )
    return {
        "assets_jsonl": str(assets_jsonl),
        "urls_txt": str(urls_txt),
        "http_services_txt": str(http_services_txt),
        "raw_json": str(raw_json),
        "summary_json": str(summary_json),
        "html_report": str(html_report),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Quake + ZoomEye asset mapping wrapper with restricted domain/IP inputs.")
    parser.add_argument("--domains", default="", help="裸域名列表，支持 JSON 数组、逗号或空白分隔。")
    parser.add_argument("--ips", default="", help="IP 列表，支持 JSON 数组、逗号或空白分隔。不接受 CIDR/端口/范围。")
    parser.add_argument("--size", type=int, default=20, help="每个输入、每个引擎最多返回的结果数，默认 20，最大 100。")
    parser.add_argument("--engines", default="auto", help="启用引擎：auto、quake、zoomeye。默认 auto：跳过未配置 API Key 的引擎。")
    parser.add_argument("--timeout", type=int, default=30, help="单个 API 请求超时秒数。")
    parser.add_argument("--conversation-id", default="", help="会话 ID，用于归档输出文件。")
    parser.add_argument("--relative-dir", default="", help="归档输出子目录。")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        domains = [validate_domain(item) for item in parse_items(args.domains)]
        ips = [validate_ip(item) for item in parse_items(args.ips)]
        if not domains and not ips:
            raise ValueError("domains 和 ips 至少提供一个")
        size = max(1, min(int(args.size or 20), 100))
        timeout = max(5, min(int(args.timeout or 30), 120))
        engines_raw = str(args.engines or "auto").strip().lower()
        engines_explicit = engines_raw not in {"", "auto"}
        engines = {item.strip().lower() for item in engines_raw.split(",") if item.strip() and item.strip().lower() != "auto"}
        allowed_engines = {"quake", "zoomeye"}
        unknown = engines - allowed_engines
        if unknown:
            raise ValueError(f"不支持的 engines: {', '.join(sorted(unknown))}")
        if not engines:
            engines = set(allowed_engines)

        quake_key = os.getenv("QUAKE_API_KEY", "").strip() or config_lookup(["quake", "api_key"])
        quake_url = os.getenv("QUAKE_BASE_URL", "").strip() or config_lookup(["quake", "base_url"]) or DEFAULT_QUAKE_URL
        zoomeye_key = os.getenv("ZOOMEYE_API_KEY", "").strip() or config_lookup(["zoomeye", "api_key"]) or extract_inline_key("zoomeye_search.yaml", "ZOOMEYE_API_KEY")
        zoomeye_url = os.getenv("ZOOMEYE_BASE_URL", "").strip() or config_lookup(["zoomeye", "base_url"]) or DEFAULT_ZOOMEYE_URL

        if "quake" in engines and not quake_key:
            if engines_explicit:
                raise ValueError("缺少 Quake API Key：请配置 QUAKE_API_KEY 或 config.yaml: quake.api_key")
            engines.discard("quake")
        if "zoomeye" in engines and not zoomeye_key:
            if engines_explicit:
                raise ValueError("缺少 ZoomEye API Key：请配置 ZOOMEYE_API_KEY、config.yaml: zoomeye.api_key 或 zoomeye_search.yaml")
            engines.discard("zoomeye")
        if not engines:
            raise ValueError("没有可用测绘引擎：请配置 QUAKE_API_KEY 或 ZOOMEYE_API_KEY，或在 config.yaml 配置 quake/zoomeye.api_key")

        output_dir = build_chat_output_dir(args.conversation_id, args.relative_dir)
        merged: dict[str, dict[str, Any]] = {}
        raw_runs: list[dict[str, Any]] = []
        errors: list[dict[str, str]] = []

        query_plan: list[tuple[str, str, str, str]] = []
        for domain in domains:
            query_plan.append(("domain", domain, f'domain:"{domain}"', f'domain="{domain}"'))
        for ip in ips:
            query_plan.append(("ip", ip, f'ip:"{ip}"', f'ip="{ip}"'))

        for input_type, input_value, quake_dsl, zoomeye_dsl in query_plan:
            if "quake" in engines:
                try:
                    body = quake_query(quake_key, quake_dsl, size, timeout, quake_url)
                    records = body.get("data", []) if isinstance(body, dict) else []
                    raw_runs.append({"engine": "quake", "input_type": input_type, "input": input_value, "query": quake_dsl, "status": "success", "count": len(records), "raw": body})
                    for raw in records:
                        if isinstance(raw, dict):
                            asset = normalize_quake_record(raw, input_type, input_value)
                            key = asset_key(asset)
                            if key in merged:
                                merge_asset(merged[key], asset)
                            else:
                                asset["inputs"] = [{"input_type": input_type, "input": input_value}]
                                merged[key] = asset
                except Exception as exc:
                    errors.append({"engine": "quake", "input": input_value, "message": str(exc)})
                    raw_runs.append({"engine": "quake", "input_type": input_type, "input": input_value, "query": quake_dsl, "status": "error", "error": str(exc)})

            if "zoomeye" in engines:
                for sub_type in (["web"] if input_type == "domain" else ["v4"]):
                    try:
                        body = zoomeye_query(zoomeye_key, zoomeye_dsl, size, timeout, zoomeye_url, sub_type)
                        records = body.get("data", []) if isinstance(body, dict) else []
                        raw_runs.append({"engine": "zoomeye", "sub_type": sub_type, "input_type": input_type, "input": input_value, "query": zoomeye_dsl, "status": "success", "count": len(records), "raw": body})
                        for raw in records:
                            if isinstance(raw, dict):
                                asset = normalize_zoomeye_record(raw, input_type, input_value)
                                key = asset_key(asset)
                                if key in merged:
                                    merge_asset(merged[key], asset)
                                else:
                                    asset["inputs"] = [{"input_type": input_type, "input": input_value}]
                                    merged[key] = asset
                    except Exception as exc:
                        errors.append({"engine": "zoomeye", "input": input_value, "sub_type": sub_type, "message": str(exc)})
                        raw_runs.append({"engine": "zoomeye", "sub_type": sub_type, "input_type": input_type, "input": input_value, "query": zoomeye_dsl, "status": "error", "error": str(exc)})

        assets = sorted(merged.values(), key=lambda item: (str(item.get("host") or ""), str(item.get("ip") or ""), int(item.get("port") or 0), str(item.get("url") or "")))
        url_count = len({asset["url"] for asset in assets if asset.get("url")})
        service_count = len({f"{asset.get('ip') or asset.get('host')}:{asset.get('port')}" for asset in assets if (asset.get("ip") or asset.get("host")) and asset.get("port")})
        summary = {
            "status": "success" if not errors else ("partial_success" if assets else "error"),
            "message": "asset_mapping 查询完成" if not errors else "asset_mapping 查询完成，但部分引擎/输入失败",
            "domains": domains,
            "ips": ips,
            "engines": sorted(engines),
            "per_engine_size": size,
            "asset_count": len(assets),
            "url_count": url_count,
            "http_service_count": service_count,
            "error_count": len(errors),
            "errors": errors,
            "output_dir": str(output_dir),
        }
        artifacts = write_outputs(output_dir, assets, raw_runs, summary)
        summary["artifacts"] = artifacts
        print(json.dumps({**summary, "assets_preview": assets[:20]}, ensure_ascii=False, indent=2))
        return 0 if summary["status"] in {"success", "partial_success"} else 1
    except Exception as exc:
        print(json.dumps({"status": "error", "message": str(exc), "type": type(exc).__name__}, ensure_ascii=False, indent=2))
        return 1


if __name__ == "__main__":
    sys.exit(main())
