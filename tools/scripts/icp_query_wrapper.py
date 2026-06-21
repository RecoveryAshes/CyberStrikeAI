#!/usr/bin/env python3
import argparse
import csv
import json
import os
import re
import shutil
import subprocess
import sys
from datetime import datetime
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
ICP_PROJECT_CANDIDATES = [
    Path("/home/user/tools/icp_querycli"),
    Path("/Users/recovery/opt/tools/自编写小工具/icp_quary111"),
]
CHAT_UPLOADS_DIR = REPO_ROOT / "chat_uploads"
TMP_OUTPUT_ROOT = Path(os.getenv("CYBERSTRIKE_TOOL_TMP_DIR", "/tmp/cyberstrike-ai-tools")) / "icp_query"
ICP_BINARY_CANDIDATES = [
    "icp_querycli",
    "/usr/local/bin/icp_querycli",
    "/home/user/.local/bin/icp_querycli",
    "/Users/recovery/.local/bin/icp_querycli",
]


def str_to_bool(value):
    if isinstance(value, bool):
        return value
    if value is None:
        return False
    return str(value).strip().lower() in {"1", "true", "yes", "y", "on"}


def clean_segment(value, default):
    text = str(value or "").strip()
    if not text:
        return default
    text = re.sub(r"[^\w.\-\u4e00-\u9fff]+", "_", text, flags=re.UNICODE)
    text = text.strip("._-")
    return text[:80] or default


def clean_filename(value):
    name = clean_segment(value, "icp_results")
    if not name.lower().endswith(".csv"):
        name += ".csv"
    return name


def filename_stem(value, default):
    name = clean_filename(value)
    if name.lower().endswith(".csv"):
        name = name[:-4]
    return clean_segment(name, default)


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
    return [str(item).strip() for item in values if str(item).strip() and not str(item).strip().startswith("#")]


def newest_file(directory, pattern):
    try:
        files = list(directory.glob(pattern))
    except OSError:
        return None
    if not files:
        return None
    return max(files, key=lambda p: p.stat().st_mtime)


def newest_file_recursive(directory, pattern):
    try:
        files = list(directory.rglob(pattern))
    except OSError:
        return None
    if not files:
        return None
    return max(files, key=lambda p: p.stat().st_mtime)


def find_icp_binary():
    for candidate in ICP_BINARY_CANDIDATES:
        resolved = shutil.which(candidate) if "/" not in candidate else candidate
        if resolved and Path(resolved).exists():
            return resolved
    return ""


def find_icp_project_dir():
    for candidate in ICP_PROJECT_CANDIDATES:
        if candidate.exists():
            return candidate
    return ICP_PROJECT_CANDIDATES[0]


def copy_if_exists(src, dest_dir):
    if not src or not Path(src).exists():
        return None
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest = dest_dir / Path(src).name
    if Path(src).resolve() != dest.resolve():
        shutil.copy2(src, dest)
    return dest


def path_is_within(path, root):
    try:
        path.resolve().relative_to(root.resolve())
        return True
    except ValueError:
        return False


def resolve_allowed_input_file(raw_path):
    candidate = Path(raw_path).expanduser()
    if not candidate.is_absolute():
        candidate = (REPO_ROOT / candidate).resolve()
    else:
        candidate = candidate.resolve()
    allowed_roots = [
        CHAT_UPLOADS_DIR,
        TMP_OUTPUT_ROOT,
        REPO_ROOT / "tmp",
    ]
    if not any(path_is_within(candidate, root) for root in allowed_roots):
        roots = ", ".join(str(root) for root in allowed_roots)
        raise RuntimeError(f"input_file must be under one of the tool-owned directories: {roots}")
    return candidate


def build_chat_output_dir(conversation_id, relative_dir):
    date_segment = datetime.now().strftime("%Y%m%d")
    conversation_segment = clean_segment(conversation_id, "_manual")
    rel_segments = [clean_segment(part, "") for part in Path(relative_dir).parts if part not in {"", ".", ".."}]
    output_dir = CHAT_UPLOADS_DIR / date_segment / conversation_segment / "tool_outputs" / "icp_query"
    for segment in rel_segments:
        if segment:
            output_dir = output_dir / segment
    output_dir.mkdir(parents=True, exist_ok=True)
    return output_dir


def build_tmp_run_dir(conversation_id, relative_dir, query_id):
    date_segment = datetime.now().strftime("%Y%m%d")
    conversation_segment = clean_segment(conversation_id, "_manual")
    rel_segment = clean_segment(relative_dir, "run")
    output_dir = TMP_OUTPUT_ROOT / date_segment / conversation_segment / rel_segment / query_id
    output_dir.mkdir(parents=True, exist_ok=True)
    return output_dir


def append_csv_with_union(master_csv, source_csv, metadata):
    if not source_csv.exists():
        return 0

    with source_csv.open("r", encoding="utf-8-sig", newline="") as f:
        reader = csv.DictReader(f)
        source_fieldnames = list(reader.fieldnames or [])
        new_rows = [dict(row) for row in reader]

    if not source_fieldnames or not new_rows:
        return 0

    metadata_fieldnames = [
        "query_id",
        "query_time",
        "query_mode",
        "query_type",
        "icp_path",
        "investment",
        "conprop",
        "query_item",
        "query_source",
        "parent_company",
        "subsidiary_conprop",
        "query_status",
        "failure_count",
    ]
    for row in new_rows:
        row.update(metadata)

    existing_rows = []
    existing_fieldnames = []
    if master_csv.exists() and master_csv.stat().st_size > 0:
        with master_csv.open("r", encoding="utf-8-sig", newline="") as f:
            reader = csv.DictReader(f)
            existing_fieldnames = list(reader.fieldnames or [])
            existing_rows = [dict(row) for row in reader]

    fieldnames = []
    for name in metadata_fieldnames + existing_fieldnames + source_fieldnames:
        if name and name not in fieldnames:
            fieldnames.append(name)

    with master_csv.open("w", encoding="utf-8-sig", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames)
        writer.writeheader()
        for row in existing_rows + new_rows:
            writer.writerow({name: row.get(name, "") for name in fieldnames})

    return len(new_rows)


def append_rows_with_union(master_csv, rows, preferred_fieldnames):
    if not rows:
        return 0

    existing_rows = []
    existing_fieldnames = []
    if master_csv.exists() and master_csv.stat().st_size > 0:
        with master_csv.open("r", encoding="utf-8-sig", newline="") as f:
            reader = csv.DictReader(f)
            existing_fieldnames = list(reader.fieldnames or [])
            existing_rows = [dict(row) for row in reader]

    fieldnames = []
    for name in preferred_fieldnames + existing_fieldnames:
        if name and name not in fieldnames:
            fieldnames.append(name)
    for row in rows:
        for name in row.keys():
            if name and name not in fieldnames:
                fieldnames.append(name)

    with master_csv.open("w", encoding="utf-8-sig", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames)
        writer.writeheader()
        for row in existing_rows + rows:
            writer.writerow({name: row.get(name, "") for name in fieldnames})

    return len(rows)


def csv_has_failed_query(source_csv):
    if not source_csv.exists():
        return True
    with source_csv.open("r", encoding="utf-8-sig", newline="") as f:
        reader = csv.DictReader(f)
        rows = [dict(row) for row in reader]
    if not rows:
        return True
    failure_markers = {"失败", "failed", "error", "查询失败"}
    for row in rows:
        status = str(row.get("查询状态") or row.get("query_status") or "").strip().lower()
        subject = str(row.get("主体名称") or row.get("备案号/许可证号") or "").strip().lower()
        if status in failure_markers or subject in failure_markers:
            return True
    return False


def read_expanded_subsidiaries(subsidiaries_csv):
    if not subsidiaries_csv.exists():
        return []
    with subsidiaries_csv.open("r", encoding="utf-8-sig", newline="") as f:
        reader = csv.DictReader(f)
        rows = []
        for row in reader:
            name = str(row.get("子公司名称") or "").strip()
            if not name:
                continue
            rows.append(
                {
                    "item": name,
                    "parent_company": str(row.get("母公司名称") or "").strip(),
                    "conprop": str(row.get("控股比例") or "").strip(),
                    "source": "subsidiary",
                }
            )
        return rows


def dedupe_query_entries(entries):
    deduped = []
    seen = set()
    for entry in entries:
        key = (entry.get("item") or "").strip()
        if not key or key in seen:
            continue
        seen.add(key)
        deduped.append(entry)
    return deduped


def process_timeout_seconds(args):
    if args.process_timeout is not None and args.process_timeout > 0:
        return args.process_timeout
    if args.timeout is not None and args.timeout > 0:
        return max(args.timeout + 20, args.timeout * 3)
    return 180


def build_cli_command(icp_bin, config_path, input_file, output_file, args):
    cmd = [icp_bin, "-config", str(config_path), "-input", str(input_file), "-output", str(output_file), "-query-mode", args.query_mode]
    if args.query_type:
        cmd.extend(["-query-type", args.query_type])
    if args.icp_path:
        cmd.extend(["-icp-path", args.icp_path])
    if args.investment:
        cmd.append("-investment")
    if args.conprop is not None:
        cmd.extend(["-conprop", str(args.conprop)])
    if args.timeout is not None:
        cmd.extend(["-timeout", str(args.timeout)])
    cmd.extend(["-max-retries", str(args.max_retries if args.max_retries is not None else args.item_max_failures)])
    if args.page_size is not None:
        cmd.extend(["-page-size", str(args.page_size)])
    delay_per_key = str(args.delay_per_key).strip()
    if delay_per_key:
        cmd.extend(["-delay-per-key", delay_per_key])
    if args.smart_mode != "":
        cmd.extend(["-smart-mode", "true" if str_to_bool(args.smart_mode) else "false"])
    if args.debug:
        cmd.extend(["-debug", "true"])
    professional_member_rps = str(args.professional_member_rps).strip()
    if professional_member_rps:
        cmd.extend(["-professional-member-rps", professional_member_rps])
    if args.professional_member_mode:
        cmd.extend(["-professional-member-mode", args.professional_member_mode])
    if args.pipeline:
        cmd.append("-pipeline")
    if args.workers is not None:
        cmd.extend(["-workers", str(args.workers)])
    return cmd


def build_subsidiaries_command(icp_bin, config_path, input_file, output_file, args):
    cmd = [
        icp_bin,
        "-config",
        str(config_path),
        "-input",
        str(input_file),
        "-output",
        str(output_file),
        "-query-mode",
        "subject",
        "-subsidiaries-only",
    ]
    if args.conprop is not None:
        cmd.extend(["-conprop", str(args.conprop)])
    if args.timeout is not None:
        cmd.extend(["-timeout", str(args.timeout)])
    if args.max_retries is not None:
        cmd.extend(["-max-retries", str(args.max_retries)])
    return cmd


def main():
    parser = argparse.ArgumentParser(description="Run icp_quary111 and save outputs under CyberStrikeAI chat_uploads.")
    parser.add_argument("--config", default="")
    parser.add_argument("--input-file", default="")
    parser.add_argument("--items", default="")
    parser.add_argument("--output-name", default="icp_results.csv")
    parser.add_argument("--conversation-id", default="_manual")
    parser.add_argument("--relative-dir", default="")
    parser.add_argument("--query-mode", choices=["subject", "domain"], default="subject")
    parser.add_argument("--query-type", default="web")
    parser.add_argument("--icp-path", choices=["default", "professional_member"], default="professional_member")
    parser.add_argument("--investment", action="store_true")
    parser.add_argument("--conprop", type=int, default=None)
    parser.add_argument("--timeout", type=int, default=None)
    parser.add_argument("--max-retries", type=int, default=None)
    parser.add_argument("--page-size", type=int, default=None)
    parser.add_argument("--delay-per-key", default="")
    parser.add_argument("--smart-mode", default="")
    parser.add_argument("--debug", action="store_true")
    parser.add_argument("--professional-member-rps", default="")
    parser.add_argument("--professional-member-mode", choices=["pipeline", "serial"], default="")
    parser.add_argument("--pipeline", action="store_true")
    parser.add_argument("--workers", type=int, default=None)
    parser.add_argument("--save-debug-files", action="store_true")
    parser.add_argument("--item-max-failures", type=int, default=10)
    parser.add_argument("--total-max-failures", type=int, default=200)
    parser.add_argument("--process-timeout", type=int, default=None)
    args = parser.parse_args()

    icp_project_dir = find_icp_project_dir()
    if not icp_project_dir.exists():
        print(json.dumps({"status": "error", "message": f"ICP project not found: {icp_project_dir}"}, ensure_ascii=False, indent=2))
        return 1

    icp_bin = find_icp_binary()
    if not icp_bin:
        print(json.dumps({"status": "error", "message": "icp_querycli binary not found. Build/install it to /usr/local/bin/icp_querycli or add it to PATH."}, ensure_ascii=False, indent=2))
        return 1

    config_arg = str(args.config or "").strip()
    if config_arg:
        config_path = Path(config_arg).expanduser()
        if not config_path.is_absolute():
            config_path = icp_project_dir / config_path
    else:
        config_path = icp_project_dir / "config.json"
    if not config_path.exists():
        print(json.dumps({"status": "error", "message": f"ICP config not found: {config_path}"}, ensure_ascii=False, indent=2))
        return 1

    query_time = datetime.now().isoformat(timespec="seconds")
    query_id = f"{filename_stem(args.output_name, 'icp_query')}_{datetime.now().strftime('%Y%m%d_%H%M%S')}"
    output_dir = build_chat_output_dir(args.conversation_id, args.relative_dir)
    tmp_run_dir = build_tmp_run_dir(args.conversation_id, args.relative_dir, query_id)

    items = parse_items(args.items)
    if args.input_file:
        try:
            candidate = resolve_allowed_input_file(args.input_file)
        except RuntimeError as exc:
            print(json.dumps({"status": "error", "message": str(exc)}, ensure_ascii=False, indent=2))
            return 1
        if not candidate.exists():
            print(json.dumps({"status": "error", "message": f"Input file not found: {candidate}"}, ensure_ascii=False, indent=2))
            return 1
        items = parse_items(candidate.read_text(encoding="utf-8"))
    elif items:
        input_file = tmp_run_dir / "input.txt"
        input_file.write_text("\n".join(items) + "\n", encoding="utf-8")
    else:
        print(json.dumps({"status": "error", "message": "Provide either --items or --input-file."}, ensure_ascii=False, indent=2))
        return 1

    if args.query_mode == "domain" and args.query_type != "web":
        print(json.dumps({"status": "error", "message": "query_mode=domain only supports query_type=web."}, ensure_ascii=False, indent=2))
        return 1

    query_entries = [{"item": item, "parent_company": "", "conprop": "", "source": "parent"} for item in items]
    expansion_summary = {"enabled": bool(args.investment), "subsidiary_count": 0, "csv_file": "", "error": ""}
    if args.investment and args.query_mode == "subject":
        expansion_dir = tmp_run_dir / "_subsidiary_expansion"
        expansion_dir.mkdir(parents=True, exist_ok=True)
        expansion_input = expansion_dir / "parents.txt"
        expansion_input.write_text("\n".join(items) + "\n", encoding="utf-8")
        expansion_csv = expansion_dir / "subsidiaries.csv"
        expansion_cmd = build_subsidiaries_command(icp_bin, config_path, expansion_input, expansion_csv, args)
        try:
            expansion_proc = subprocess.run(
                expansion_cmd,
                cwd=str(expansion_dir),
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=process_timeout_seconds(args),
            )
            last_stdout = expansion_proc.stdout or ""
            last_stderr = expansion_proc.stderr or ""
            if expansion_proc.returncode == 0:
                subsidiaries = read_expanded_subsidiaries(expansion_csv)
                query_entries = dedupe_query_entries(query_entries + subsidiaries)
                expansion_summary.update(
                    {
                        "subsidiary_count": len(subsidiaries),
                        "csv_file": str(expansion_csv) if expansion_csv.exists() else "",
                    }
                )
            else:
                expansion_summary["error"] = (last_stderr or last_stdout or "subsidiary expansion failed")[-2000:]
        except subprocess.TimeoutExpired as exc:
            expansion_summary["error"] = f"subsidiary expansion timeout: {process_timeout_seconds(args)}s"
            last_stdout = exc.stdout or ""
            last_stderr = exc.stderr or expansion_summary["error"]
    elif args.investment and args.query_mode != "subject":
        expansion_summary["error"] = "investment expansion only supports subject query_mode; skipped"

    run_output_file = tmp_run_dir / "result.csv"
    master_csv = output_dir / "icp_query.results.csv"
    item_max_failures = max(1, args.item_max_failures)
    total_max_failures = max(1, args.total_max_failures)
    proc_timeout = process_timeout_seconds(args)
    metadata_base = {
        "query_id": query_id,
        "query_time": query_time,
        "query_mode": args.query_mode,
        "query_type": args.query_type,
        "icp_path": args.icp_path,
        "investment": str(bool(args.investment)),
        "conprop": "" if args.conprop is None else str(args.conprop),
    }

    commands = []
    item_results = []
    appended_rows = 0
    total_failures = 0
    stopped_by_total_failure_limit = False
    last_stdout = ""
    last_stderr = ""

    for item_index, entry in enumerate(query_entries, start=1):
        item = entry["item"]
        item_failures = 0
        item_succeeded = False
        item_slug = clean_segment(item, f"item_{item_index}")

        while item_failures < item_max_failures:
            if total_failures >= total_max_failures:
                stopped_by_total_failure_limit = True
                break

            attempt_number = item_failures + 1
            attempt_dir = tmp_run_dir / f"{item_index:04d}_{item_slug}" / f"attempt_{attempt_number:02d}"
            attempt_dir.mkdir(parents=True, exist_ok=True)
            attempt_input = attempt_dir / "input.txt"
            attempt_input.write_text(item + "\n", encoding="utf-8")
            attempt_output = attempt_dir / "result.csv"
            original_investment = args.investment
            args.investment = False
            cmd = build_cli_command(icp_bin, config_path, attempt_input, attempt_output, args)
            args.investment = original_investment
            commands.append(cmd)

            timed_out = False
            try:
                completed = subprocess.run(
                    cmd,
                    cwd=str(attempt_dir),
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=proc_timeout,
                )
                returncode = completed.returncode
                stdout = completed.stdout or ""
                stderr = completed.stderr or ""
            except subprocess.TimeoutExpired as exc:
                timed_out = True
                returncode = 124
                stdout = exc.stdout or ""
                stderr = exc.stderr or f"Process exceeded wrapper timeout: {proc_timeout}s"

            if isinstance(stdout, bytes):
                stdout = stdout.decode("utf-8", errors="replace")
            if isinstance(stderr, bytes):
                stderr = stderr.decode("utf-8", errors="replace")
            last_stdout = stdout
            last_stderr = stderr

            attempt_failed_in_csv = csv_has_failed_query(attempt_output)
            if returncode == 0 and attempt_output.exists() and not attempt_failed_in_csv:
                rows = append_csv_with_union(
                    master_csv,
                    attempt_output,
                    {
                        **metadata_base,
                        "query_item": item,
                        "query_source": entry.get("source", ""),
                        "parent_company": entry.get("parent_company", ""),
                        "subsidiary_conprop": entry.get("conprop", ""),
                        "query_status": "success",
                        "failure_count": str(item_failures),
                    },
                )
                appended_rows += rows
                item_results.append({"item": item, "status": "success", "attempts": attempt_number, "appended_rows": rows})
                item_succeeded = True
                break

            item_failures += 1
            total_failures += 1
            if attempt_failed_in_csv:
                last_stderr = f"ICP CLI returned failed rows in CSV for item={item}"
            if timed_out:
                last_stderr = (last_stderr + "\n" if last_stderr else "") + f"wrapper_timeout={proc_timeout}s"

        if stopped_by_total_failure_limit:
            break

        if not item_succeeded:
            failure_row = {
                **metadata_base,
                "query_item": item,
                "query_source": entry.get("source", ""),
                "parent_company": entry.get("parent_company", ""),
                "subsidiary_conprop": entry.get("conprop", ""),
                "query_status": "failed",
                "failure_count": str(item_failures),
                "failure_reason": (last_stderr or last_stdout or "ICP query failed")[-2000:],
            }
            appended_rows += append_rows_with_union(
                master_csv,
                [failure_row],
                [
                    "query_id",
                    "query_time",
                    "query_mode",
                    "query_type",
                    "icp_path",
                    "investment",
                    "conprop",
                    "query_item",
                    "query_source",
                    "parent_company",
                    "subsidiary_conprop",
                    "query_status",
                    "failure_count",
                    "failure_reason",
                ],
            )
            item_results.append({"item": item, "status": "failed", "attempts": item_failures, "appended_rows": 1})

    latest_raw_csv = newest_file_recursive(tmp_run_dir, "result.csv")
    run_log = newest_file_recursive(tmp_run_dir, "run_*.log")
    checkpoint = newest_file_recursive(tmp_run_dir, "*_checkpoint.json")
    status = "stopped" if stopped_by_total_failure_limit else "success"
    if item_results and all(item.get("status") == "failed" for item in item_results):
        status = "error"

    debug_files = {
        "tmp_run_dir": str(tmp_run_dir),
        "tmp_csv_file": str(latest_raw_csv) if latest_raw_csv else "",
        "tmp_checkpoint_file": str(checkpoint) if checkpoint and checkpoint.exists() else "",
        "tmp_log_file": str(run_log) if run_log else "",
    }

    if args.save_debug_files:
        debug_dest = output_dir / "debug" / query_id
        debug_dest.mkdir(parents=True, exist_ok=True)
        copied_csv = copy_if_exists(latest_raw_csv, debug_dest)
        copied_checkpoint = copy_if_exists(checkpoint, debug_dest)
        copied_debug_log = copy_if_exists(run_log, debug_dest)
        debug_files.update(
            {
                "debug_csv_file": str(copied_csv) if copied_csv else "",
                "debug_checkpoint_file": str(copied_checkpoint) if copied_checkpoint else "",
                "debug_log_file": str(copied_debug_log) if copied_debug_log else "",
            }
        )

    result = {
        "status": status,
        "exit_code": 2 if status == "error" else 0,
        "commands": commands[-20:],
        "output_dir": str(output_dir),
        "query_id": query_id,
        "input_items": len(items),
        "expanded_items": len(query_entries),
        "subsidiary_expansion": expansion_summary,
        "csv_file": str(master_csv) if master_csv.exists() else "",
        "appended_rows": appended_rows,
        "item_results": item_results,
        "item_max_failures": item_max_failures,
        "total_failures": total_failures,
        "total_max_failures": total_max_failures,
        "process_timeout_seconds": proc_timeout,
        "stopped_by_total_failure_limit": stopped_by_total_failure_limit,
        "debug_files": debug_files,
        "file_management_note": "Only the appended result CSV is saved under chat_uploads by default. Logs, checkpoint, and per-run raw CSV are under CYBERSTRIKE_TOOL_TMP_DIR/icp_query or /tmp/cyberstrike-ai-tools/icp_query unless save_debug_files=true.",
        "stdout": last_stdout[-12000:],
        "stderr": last_stderr[-12000:],
    }
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 2 if status == "error" else 0


if __name__ == "__main__":
    sys.exit(main())
