#!/usr/bin/env python3
import argparse
import base64
import csv
import hashlib
import hmac
import json
import os
import time
from datetime import datetime
from pathlib import Path
from urllib import error, parse, request


REPO_ROOT = Path(__file__).resolve().parents[2]
CHAT_UPLOADS_DIR = REPO_ROOT / "chat_uploads"
TMP_OUTPUT_ROOT = Path(os.getenv("CYBERSTRIKE_TOOL_TMP_DIR", "/tmp/cyberstrike-ai-tools")) / "proxy_fetch"
GET_DPS_ENDPOINT = "https://dps.kdlapi.com/api/getdps"
GET_SECRET_TOKEN_ENDPOINT = "https://auth.kdlapi.com/api/get_secret_token"
GET_PROXY_AUTHORIZATION_ENDPOINT = "https://dev.kdlapi.com/api/getproxyauthorization"
SUPPORTED_SIGN_TYPES = {"token", "hmacsha1"}


def clean_segment(value, default):
    text = str(value or "").strip()
    if not text:
        return default
    text = "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in text)
    return text.strip("._-")[:100] or default


def parse_bool(value, default=False):
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return value != 0
    text = str(value).strip().lower()
    if text in {"1", "true", "yes", "on", "y"}:
        return True
    if text in {"0", "false", "no", "off", "n", ""}:
        return False
    raise RuntimeError(f"invalid boolean value: {value}")


def parse_int(value, field_name, default=0):
    if value is None or value == "":
        return default
    try:
        return int(value)
    except (TypeError, ValueError) as exc:
        raise RuntimeError(f"{field_name} must be an integer: {value}") from exc


def load_config(config_path):
    raw = {}
    if config_path:
        path = Path(config_path).expanduser()
        if not path.is_absolute():
            path = (REPO_ROOT / path).resolve()
        if not path.exists():
            raise RuntimeError(f"proxy config not found: {path}")
        with path.open("r", encoding="utf-8") as f:
            raw = json.load(f)
        if not isinstance(raw, dict):
            raise RuntimeError("proxy config must be a JSON object")

    def pick(env_name, key, default=""):
        env_value = os.getenv(env_name)
        if env_value is not None:
            return env_value
        return raw.get(key, default)

    cfg = {
        "secret_id": str(pick("KDL_SECRET_ID", "secret_id", "")).strip(),
        "secret_key": str(pick("KDL_SECRET_KEY", "secret_key", "")).strip(),
        "sign_type": str(pick("KDL_SIGN_TYPE", "sign_type", "token") or "token").strip().lower(),
        "area": str(pick("KDL_AREA", "area", "")).strip(),
        "area_ex": str(pick("KDL_AREA_EX", "area_ex", "")).strip(),
        "carrier": parse_int(pick("KDL_CARRIER", "carrier", 0), "carrier", 0),
        "dedup": parse_bool(pick("KDL_DEDUP", "dedup", True), True),
        "timeout": parse_int(pick("KDL_TIMEOUT", "timeout", 10), "timeout", 10),
        "proxy_username": str(pick("KDL_PROXY_USERNAME", "proxy_username", "")).strip(),
        "proxy_password": str(pick("KDL_PROXY_PASSWORD", "proxy_password", "")).strip(),
        "include_proxy_auth_in_url": parse_bool(pick("KDL_INCLUDE_PROXY_AUTH_IN_URL", "include_proxy_auth_in_url", False), False),
    }

    if cfg["sign_type"] not in SUPPORTED_SIGN_TYPES:
        raise RuntimeError("KDL_SIGN_TYPE only supports token or hmacsha1")
    if not cfg["secret_id"] or not cfg["secret_key"]:
        raise RuntimeError("missing KDL_SECRET_ID / KDL_SECRET_KEY or config secret_id / secret_key")
    if cfg["carrier"] < 0 or cfg["carrier"] > 3:
        raise RuntimeError("carrier must be 0-3")
    if cfg["timeout"] <= 0:
        raise RuntimeError("timeout must be > 0")
    return cfg


def request_json(method, endpoint, params, timeout):
    method = method.upper()
    headers = {"Accept": "application/json"}
    data = None
    url = endpoint
    encoded = parse.urlencode({k: stringify(v) for k, v in params.items()})
    if method == "GET":
        url = f"{endpoint}?{encoded}"
    elif method == "POST":
        headers["Content-Type"] = "application/x-www-form-urlencoded"
        data = encoded.encode("utf-8")
    else:
        raise RuntimeError(f"unsupported HTTP method: {method}")

    req = request.Request(url=url, data=data, method=method, headers=headers)
    try:
        with request.urlopen(req, timeout=timeout) as response:
            body = response.read().decode("utf-8")
    except error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"Kuaidaili API HTTP error: {extract_error_message(body) or exc.reason}") from exc
    except error.URLError as exc:
        raise RuntimeError(f"Kuaidaili API connection error: {exc.reason}") from exc

    try:
        payload = json.loads(body)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Kuaidaili API returned invalid JSON: {body[:500]}") from exc
    if not isinstance(payload, dict):
        raise RuntimeError("Kuaidaili API returned non-object JSON")
    if payload.get("code") != 0:
        raise RuntimeError(f"Kuaidaili API error: {payload.get('msg') or 'unknown error'}")
    return payload


def stringify(value):
    if isinstance(value, bool):
        return "1" if value else "0"
    return str(value)


def extract_error_message(body):
    text = str(body or "").strip()
    if not text:
        return ""
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return text
    if isinstance(payload, dict):
        return str(payload.get("msg") or payload.get("error") or "").strip()
    return text


def proxy_record_without_secret(item):
    masked = dict(item)
    masked.pop("proxy", None)
    return masked


def write_private_json(path, payload):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
    try:
        path.chmod(0o600)
    except OSError:
        pass


def get_secret_token(cfg):
    payload = request_json(
        "POST",
        GET_SECRET_TOKEN_ENDPOINT,
        {"secret_id": cfg["secret_id"], "secret_key": cfg["secret_key"]},
        cfg["timeout"],
    )
    data = payload.get("data")
    if not isinstance(data, dict):
        raise RuntimeError("Kuaidaili token response format error")
    token = str(data.get("secret_token") or "").strip()
    if not token:
        raise RuntimeError("Kuaidaili secret_token is empty")
    return token


def sign_hmacsha1(method, endpoint, params, secret_key):
    path = parse.urlsplit(endpoint).path
    query_text = "&".join(f"{key}={stringify(params[key])}" for key in sorted(params.keys()))
    raw_text = f"{method.upper()}{path}?{query_text}"
    digest = hmac.new(secret_key.encode("utf-8"), raw_text.encode("utf-8"), hashlib.sha1).digest()
    return base64.b64encode(digest).decode("utf-8")


def signed_request(method, endpoint, extra_params, cfg):
    params = {"secret_id": cfg["secret_id"], "sign_type": cfg["sign_type"]}
    for key, value in extra_params.items():
        if value is None:
            continue
        if isinstance(value, str) and not value.strip():
            continue
        params[key] = value

    if cfg["sign_type"] == "token":
        params["signature"] = get_secret_token(cfg)
    else:
        params["timestamp"] = int(time.time())
        params["signature"] = sign_hmacsha1(method, endpoint, params, cfg["secret_key"])
    return request_json(method, endpoint, params, cfg["timeout"])


def get_proxy_authorization(cfg):
    payload = signed_request("GET", GET_PROXY_AUTHORIZATION_ENDPOINT, {"plaintext": 1}, cfg)
    data = payload.get("data")
    if not isinstance(data, dict):
        raise RuntimeError("Kuaidaili proxy authorization response format error")
    return data


def split_proxy_entry(proxy_text):
    text = str(proxy_text or "").strip()
    if not text:
        raise RuntimeError("empty proxy entry")
    valid_seconds = None
    host_text = text
    if "," in text:
        possible_host, possible_seconds = text.rsplit(",", 1)
        if possible_host.strip():
            host_text = possible_host.strip()
        try:
            seconds = int(possible_seconds.strip())
            if seconds >= 0:
                valid_seconds = seconds
        except ValueError:
            pass
    return host_text, valid_seconds


def build_upstream_proxy(proxy_value, cfg, auth_cache):
    proxy_text = str(proxy_value or "").strip()
    if not proxy_text:
        raise RuntimeError("empty proxy address")
    if "://" not in proxy_text:
        proxy_text = f"http://{proxy_text}"
    parsed = parse.urlsplit(proxy_text)
    host = str(parsed.hostname or "").strip()
    port = parsed.port
    scheme = str(parsed.scheme or "http").strip().lower()
    if scheme not in {"http", "https"}:
        raise RuntimeError(f"unsupported proxy scheme: {scheme}")
    if not host or not port:
        raise RuntimeError(f"cannot parse proxy address: {proxy_value}")

    username = parse.unquote(parsed.username or "")
    password = parse.unquote(parsed.password or "")
    if not username and not password:
        username = cfg["proxy_username"]
        password = cfg["proxy_password"]
    if not username and not password and cfg["include_proxy_auth_in_url"]:
        if not auth_cache:
            auth_cache.update(get_proxy_authorization(cfg))
        username = str(auth_cache.get("username") or "").strip()
        password = str(auth_cache.get("password") or "")
    if username or password:
        if not username or not password:
            raise RuntimeError("proxy username/password incomplete")
    return {"scheme": scheme, "host": host, "port": int(port), "username": username, "password": password}


def format_proxy_url(upstream, include_auth=False):
    auth = ""
    username = str(upstream.get("username") or "")
    password = str(upstream.get("password") or "")
    if include_auth and (username or password):
        auth = f"{parse.quote(username, safe='')}:{parse.quote(password, safe='')}@"
    return f"{upstream['scheme']}://{auth}{upstream['host']}:{int(upstream['port'])}"


def mask_proxy_url(proxy_url):
    parsed = parse.urlsplit(str(proxy_url or ""))
    if not parsed.username and not parsed.password:
        return proxy_url
    host = parsed.hostname or ""
    port = f":{parsed.port}" if parsed.port else ""
    return f"{parsed.scheme}://***:***@{host}{port}"


def fetch_kuaidaili_dps(cfg, num):
    params = {"format": "json", "num": max(1, min(int(num), 50)), "f_et": 1}
    if cfg["area"]:
        params["area"] = cfg["area"]
    if cfg["area_ex"]:
        params["area_ex"] = cfg["area_ex"]
    if cfg["carrier"] > 0:
        params["carrier"] = cfg["carrier"]
    if cfg["dedup"]:
        params["dedup"] = 1

    payload = signed_request("GET", GET_DPS_ENDPOINT, params, cfg)
    data = payload.get("data")
    if not isinstance(data, dict):
        raise RuntimeError("Kuaidaili DPS response format error")
    proxy_list = data.get("proxy_list")
    if not isinstance(proxy_list, list) or not proxy_list:
        raise RuntimeError("Kuaidaili DPS returned no proxies")

    auth_cache = {}
    fetched_at = int(time.time())
    proxies = []
    for raw in proxy_list:
        proxy_host, valid_seconds = split_proxy_entry(raw)
        upstream = build_upstream_proxy(proxy_host, cfg, auth_cache)
        proxy = format_proxy_url(upstream, include_auth=True)
        proxy_display = format_proxy_url(upstream, include_auth=False)
        expires_at = fetched_at + valid_seconds if valid_seconds is not None else None
        proxies.append(
            {
                "proxy": proxy,
                "proxy_masked": mask_proxy_url(proxy),
                "proxy_display": proxy_display,
                "proxy_host": f"{upstream['host']}:{upstream['port']}",
                "raw_proxy": str(raw),
                "valid_seconds": valid_seconds,
                "fetched_at": fetched_at,
                "expires_at": expires_at,
                "upstream_proxy": {
                    "scheme": upstream["scheme"],
                    "host": upstream["host"],
                    "port": upstream["port"],
                    "has_auth": bool(upstream.get("username") or upstream.get("password")),
                },
            }
        )
    return proxies


def build_output_dir(conversation_id, relative_dir):
    day = datetime.now().strftime("%Y%m%d")
    conv = clean_segment(conversation_id, "_manual")
    rel = clean_segment(relative_dir, "")
    base = CHAT_UPLOADS_DIR / day / conv / "tool_outputs" / "proxy_fetch"
    if rel:
        base = base / rel
    base.mkdir(parents=True, exist_ok=True)
    return base


def build_tmp_dir(conversation_id, relative_dir):
    day = datetime.now().strftime("%Y%m%d")
    conv = clean_segment(conversation_id, "_manual")
    rel = clean_segment(relative_dir, "run")
    stamp = datetime.now().strftime("%H%M%S")
    base = TMP_OUTPUT_ROOT / day / conv / rel / f"proxy_fetch_{stamp}"
    base.mkdir(parents=True, exist_ok=True)
    return base


def append_csv(path, rows):
    fieldnames = [
        "fetch_time",
        "provider",
        "proxy",
        "proxy_display",
        "proxy_masked",
        "proxy_host",
        "valid_seconds",
        "expires_at",
    ]
    exists = path.exists() and path.stat().st_size > 0
    with path.open("a", encoding="utf-8-sig", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames)
        if not exists:
            writer.writeheader()
        for row in rows:
            writer.writerow({name: row.get(name, "") for name in fieldnames})


def main():
    parser = argparse.ArgumentParser(description="Fetch proxy addresses for CyberStrikeAI tools.")
    parser.add_argument("--provider", choices=["kuaidaili_dps"], default="kuaidaili_dps")
    parser.add_argument("--config", default="", help="Optional JSON config path. Env KDL_* also supported.")
    parser.add_argument("--num", type=int, default=1, help="Number of proxies to fetch, 1-50.")
    parser.add_argument("--conversation-id", default="")
    parser.add_argument("--relative-dir", default="")
    parser.add_argument("--save-to-file", action="store_true", help="Append proxy records to chat_uploads CSV.")
    parser.add_argument(
        "--include-secret-in-response",
        dest="include_secret_in_response",
        action="store_true",
        default=True,
        help="Return full proxy URL with auth. This is the default because proxy_fetch is intended to feed the AI and follow-up tools.",
    )
    parser.add_argument(
        "--mask-secret-in-response",
        dest="include_secret_in_response",
        action="store_false",
        help=argparse.SUPPRESS,
    )
    args = parser.parse_args()

    tmp_dir = build_tmp_dir(args.conversation_id, args.relative_dir)
    try:
        if args.provider != "kuaidaili_dps":
            raise RuntimeError(f"unsupported provider: {args.provider}")
        cfg = load_config(args.config)
        proxies = fetch_kuaidaili_dps(cfg, args.num)

        fetch_time = datetime.now().isoformat(timespec="seconds")
        for item in proxies:
            item["fetch_time"] = fetch_time
            item["provider"] = args.provider

        persisted_proxies = proxies if args.include_secret_in_response else [proxy_record_without_secret(item) for item in proxies]
        tmp_json = tmp_dir / ("proxy_fetch.secret.json" if args.include_secret_in_response else "proxy_fetch.raw.json")
        write_private_json(tmp_json, {"proxies": persisted_proxies})

        csv_file = ""
        if args.save_to_file:
            output_dir = build_output_dir(args.conversation_id, args.relative_dir)
            csv_path = output_dir / "proxy_fetch.results.csv"
            append_csv(csv_path, proxies)
            csv_file = str(csv_path)

        response_proxies = []
        for item in proxies:
            response_item = dict(item)
            if not args.include_secret_in_response:
                response_item.pop("proxy", None)
            response_proxies.append(response_item)

        result = {
            "status": "success",
            "provider": args.provider,
            "count": len(proxies),
            "proxies": response_proxies,
            "csv_file": csv_file,
            "tmp_raw_json": str(tmp_json),
            "note": "By default, full proxy URLs with credentials are returned and persisted for AI/tool use. Use --mask-secret-in-response only for explicit debugging redaction.",
        }
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except Exception as exc:
        print(json.dumps({"status": "error", "message": str(exc), "tmp_dir": str(tmp_dir)}, ensure_ascii=False, indent=2))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
