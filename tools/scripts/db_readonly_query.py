#!/usr/bin/env python3
import argparse
import base64
import json
import re
import sqlite3
import sys
import time
from pathlib import Path
from urllib.parse import quote


REPO_ROOT = Path(__file__).resolve().parents[2]
DB_ALIASES = {
    "conversations": REPO_ROOT / "data" / "conversations.db",
    "conversation": REPO_ROOT / "data" / "conversations.db",
    "main": REPO_ROOT / "data" / "conversations.db",
    "knowledge": REPO_ROOT / "data" / "knowledge.db",
}
DEFAULT_SCHEMA_QUERY = """
SELECT type, name, tbl_name, sql
FROM sqlite_master
WHERE type IN ('table', 'view', 'index', 'trigger')
ORDER BY type, name
"""
DENY_ACTION_NAMES = {
    "SQLITE_ALTER_TABLE",
    "SQLITE_ANALYZE",
    "SQLITE_ATTACH",
    "SQLITE_CREATE_INDEX",
    "SQLITE_CREATE_TABLE",
    "SQLITE_CREATE_TEMP_INDEX",
    "SQLITE_CREATE_TEMP_TABLE",
    "SQLITE_CREATE_TEMP_TRIGGER",
    "SQLITE_CREATE_TEMP_VIEW",
    "SQLITE_CREATE_TRIGGER",
    "SQLITE_CREATE_VIEW",
    "SQLITE_DELETE",
    "SQLITE_DETACH",
    "SQLITE_DROP_INDEX",
    "SQLITE_DROP_TABLE",
    "SQLITE_DROP_TEMP_INDEX",
    "SQLITE_DROP_TEMP_TABLE",
    "SQLITE_DROP_TEMP_TRIGGER",
    "SQLITE_DROP_TEMP_VIEW",
    "SQLITE_DROP_TRIGGER",
    "SQLITE_DROP_VIEW",
    "SQLITE_INSERT",
    "SQLITE_PRAGMA",
    "SQLITE_REINDEX",
    "SQLITE_SAVEPOINT",
    "SQLITE_TRANSACTION",
    "SQLITE_UPDATE",
}
DENY_ACTIONS = {getattr(sqlite3, name) for name in DENY_ACTION_NAMES if hasattr(sqlite3, name)}
DENY_FUNCTION_NAMES = {"load_extension", "writefile"}


def fail(message, exit_code=2):
    print(json.dumps({"ok": False, "error": message}, ensure_ascii=False))
    return exit_code


def resolve_database(alias):
    key = (alias or "conversations").strip().lower()
    if key not in DB_ALIASES:
        allowed = ", ".join(sorted(k for k in DB_ALIASES if k not in {"conversation", "main"}))
        raise ValueError(f"database must be one of: {allowed}")
    path = DB_ALIASES[key].resolve()
    try:
        path.relative_to(REPO_ROOT.resolve())
    except ValueError as exc:
        raise ValueError("database path is outside project root") from exc
    if not path.exists():
        raise FileNotFoundError(f"database file not found: {path.relative_to(REPO_ROOT)}")
    return key, path


def normalize_query(query):
    text = (query or "").strip()
    if not text:
        return DEFAULT_SCHEMA_QUERY.strip()
    return text


def validate_readonly_sql(query):
    text = query.strip().lstrip("\ufeff")
    without_trailing = text[:-1].rstrip() if text.endswith(";") else text
    if ";" in without_trailing:
        raise ValueError("only one SQL statement is allowed")

    lowered = re.sub(r"\s+", " ", without_trailing).strip().lower()
    if re.match(r"^select\b", lowered) or re.match(r"^with\b", lowered):
        return without_trailing
    if re.match(r"^explain( query plan)? (select|with)\b", lowered):
        return without_trailing
    raise ValueError("only SELECT, WITH SELECT, or EXPLAIN SELECT queries are allowed")


def parse_params(raw):
    if raw is None or str(raw).strip() == "":
        return []
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ValueError(f"params_json must be valid JSON: {exc}") from exc
    if isinstance(parsed, (list, dict)):
        return parsed
    raise ValueError("params_json must be a JSON array or object")


def safe_rel(path):
    try:
        return str(path.relative_to(REPO_ROOT))
    except ValueError:
        return str(path)


def encode_value(value, max_cell_chars):
    if isinstance(value, bytes):
        return {
            "type": "bytes",
            "length": len(value),
            "base64": base64.b64encode(value[:max_cell_chars]).decode("ascii"),
            "truncated": len(value) > max_cell_chars,
        }
    if isinstance(value, str):
        if len(value) > max_cell_chars:
            omitted = len(value) - max_cell_chars
            return value[:max_cell_chars] + f"...[truncated {omitted} chars]"
        return value
    if value is None or isinstance(value, (bool, int, float)):
        return value
    return str(value)


def query_database(db_path, query, params, max_rows, max_cell_chars, timeout_seconds):
    uri = "file:" + quote(str(db_path), safe="/:") + "?mode=ro"
    started = time.monotonic()
    conn = sqlite3.connect(uri, uri=True, timeout=min(max(timeout_seconds, 1), 30))
    try:
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA query_only=ON")

        def authorizer(action, arg1, arg2, dbname, source):
            if action in DENY_ACTIONS:
                return sqlite3.SQLITE_DENY
            if action == getattr(sqlite3, "SQLITE_FUNCTION", -1):
                function_name = (arg1 or arg2 or "").lower()
                if function_name in DENY_FUNCTION_NAMES:
                    return sqlite3.SQLITE_DENY
            return sqlite3.SQLITE_OK

        def progress():
            if time.monotonic() - started > timeout_seconds:
                return 1
            return 0

        conn.set_authorizer(authorizer)
        conn.set_progress_handler(progress, 1000)
        cur = conn.execute(query, params)
        if cur.description is None:
            raise ValueError("query did not return rows")
        columns = [col[0] or f"col_{idx}" for idx, col in enumerate(cur.description)]
        fetched = cur.fetchmany(max_rows + 1)
        rows = []
        for row in fetched[:max_rows]:
            rows.append({columns[idx]: encode_value(row[idx], max_cell_chars) for idx in range(len(columns))})
        return {
            "columns": columns,
            "rows": rows,
            "row_count": len(rows),
            "truncated": len(fetched) > max_rows,
            "max_rows": max_rows,
            "elapsed_ms": int((time.monotonic() - started) * 1000),
        }
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser(description="Read-only query tool for CyberStrikeAI SQLite databases.")
    parser.add_argument("--database", default="conversations", help="Database alias: conversations or knowledge.")
    parser.add_argument("--query", default="", help="Read-only SQL query. Empty query returns sqlite schema.")
    parser.add_argument("--params-json", default="", help="Optional JSON array/object for SQLite parameters.")
    parser.add_argument("--max-rows", type=int, default=50, help="Maximum returned rows, 1-200.")
    parser.add_argument("--max-cell-chars", type=int, default=4000, help="Maximum returned characters per cell, 100-20000.")
    parser.add_argument("--timeout-seconds", type=int, default=5, help="Query timeout seconds, 1-30.")
    args = parser.parse_args()

    try:
        alias, db_path = resolve_database(args.database)
        query = validate_readonly_sql(normalize_query(args.query))
        params = parse_params(args.params_json)
        max_rows = min(max(args.max_rows, 1), 200)
        max_cell_chars = min(max(args.max_cell_chars, 100), 20000)
        timeout_seconds = min(max(args.timeout_seconds, 1), 30)
        result = query_database(db_path, query, params, max_rows, max_cell_chars, timeout_seconds)
        result.update({
            "ok": True,
            "database": alias,
            "database_path": safe_rel(db_path),
            "query": query,
            "readonly": True,
        })
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0
    except Exception as exc:
        return fail(str(exc))


if __name__ == "__main__":
    sys.exit(main())
