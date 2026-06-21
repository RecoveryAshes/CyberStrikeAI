#!/usr/bin/env python3
import argparse
import csv
import json
import os
import queue
import re
import shlex
import shutil
import socket
import subprocess
import threading
import time
from collections import defaultdict
from datetime import datetime
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
CHAT_UPLOADS_DIR = REPO_ROOT / "chat_uploads"
TMP_OUTPUT_ROOT = Path(os.getenv("CYBERSTRIKE_TOOL_TMP_DIR", "/tmp/cyberstrike-ai-tools")) / "streaming_port_scan"
MASSCAN_OPEN_RE = re.compile(r"Discovered\s+open\s+port\s+(\d+)\/tcp\s+on\s+(\S+)", re.IGNORECASE)
NMAP_PORT_LINE_RE = re.compile(r"^(\d+)\/tcp\s+(\S+)\s+(\S+)(?:\s+(.*))?$")


def safe_name(value: str, default: str) -> str:
    value = (value or "").strip()
    if not value:
        return default
    return re.sub(r"[^A-Za-z0-9._-]+", "_", value)[:120] or default


def build_output_dir(conversation_id: str, relative_dir: str) -> Path:
    day = datetime.now().strftime("%Y%m%d")
    conv = safe_name(conversation_id, "_manual")
    rel = safe_name(relative_dir, "")
    base = CHAT_UPLOADS_DIR / day / conv / "tool_outputs" / "streaming_port_scan"
    if rel:
        base = base / rel
    base.mkdir(parents=True, exist_ok=True)
    return base


def build_tmp_debug_dir(prefix: str, conversation_id: str, relative_dir: str) -> Path:
    day = datetime.now().strftime("%Y%m%d")
    conv = safe_name(conversation_id, "_manual")
    rel = safe_name(relative_dir, "run")
    stamp = datetime.now().strftime("%H%M%S")
    base = TMP_OUTPUT_ROOT / day / conv / rel / f"{prefix}_{stamp}"
    base.mkdir(parents=True, exist_ok=True)
    return base


def write_line(path: Path, line: str) -> None:
    with path.open("a", encoding="utf-8") as f:
        f.write(line.rstrip("\n") + "\n")


def emit(message: str, events_file: Path) -> None:
    ts = datetime.now().isoformat(timespec="seconds")
    line = f"[{ts}] {message}"
    print(line, flush=True)
    write_line(events_file, line)


def split_extra_args(value: str) -> list[str]:
    if not value:
        return []
    return shlex.split(value)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Confirm open TCP ports with connect or masscan, then enumerate confirmed ports with nmap."
    )
    parser.add_argument("--target", required=True, help="Target IP, hostname, range, or CIDR.")
    parser.add_argument("--ports", default="1-65535", help="Masscan port range/list.")
    parser.add_argument("--rate", type=int, default=800, help="Masscan packets per second.")
    parser.add_argument("--discovery-mode", choices=["auto", "connect", "masscan"], default="auto", help="Discovery backend.")
    parser.add_argument("--small-port-threshold", type=int, default=20, help="Auto mode uses TCP connect when an explicit port list has at most this many ports.")
    parser.add_argument("--connect-timeout-seconds", type=float, default=1.5, help="TCP connect timeout for connect discovery.")
    parser.add_argument("--connect-workers", type=int, default=64, help="Concurrent TCP connect workers.")
    parser.add_argument("--interface", default="", help="Optional masscan network interface.")
    parser.add_argument("--batch-size", type=int, default=10, help="Trigger nmap after this many new ip:port pairs.")
    parser.add_argument("--flush-interval-seconds", type=float, default=8.0, help="Flush pending discoveries to nmap periodically.")
    parser.add_argument("--nmap-workers", type=int, default=1, help="Concurrent nmap worker count.")
    parser.add_argument("--nmap-timeout-seconds", type=float, default=180.0, help="Per-nmap batch timeout in seconds.")
    parser.add_argument("--nmap-timing", default="4", help="Nmap timing value, e.g. 3 or 4.")
    parser.add_argument("--nmap-additional-args", default="-Pn", help="Extra nmap args. Default: -Pn.")
    parser.add_argument("--masscan-additional-args", default="", help="Extra masscan args.")
    parser.add_argument("--masscan-command", default="masscan", help=argparse.SUPPRESS)
    parser.add_argument("--conversation-id", default="", help="Conversation ID for CyberStrikeAI file management.")
    parser.add_argument("--relative-dir", default="", help="Optional output subdirectory.")
    parser.add_argument("--output-name-prefix", default="streaming_port_scan", help="Output filename prefix.")
    parser.add_argument("--save-debug-files", action="store_true", help="Also save logs and JSON under chat_uploads.")
    return parser.parse_args()


def require_binary(name: str) -> str:
    path = shutil.which(name)
    if not path:
        raise RuntimeError(f"required binary not found on PATH: {name}")
    return path


def parse_explicit_ports(ports: str) -> list[int] | None:
    result: list[int] = []
    for part in (ports or "").split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part:
            pieces = part.split("-", 1)
            if len(pieces) != 2 or not pieces[0].isdigit() or not pieces[1].isdigit():
                return None
            start = int(pieces[0])
            end = int(pieces[1])
            if start > end:
                return None
            result.extend(range(start, end + 1))
        elif part.isdigit():
            result.append(int(part))
        else:
            return None
    result = sorted({p for p in result if 1 <= p <= 65535})
    return result if result else None


def looks_like_multi_host_target(target: str) -> bool:
    target = (target or "").strip()
    return "/" in target or "," in target or "-" in target


def choose_discovery_mode(args: argparse.Namespace, explicit_ports: list[int] | None) -> str:
    if args.discovery_mode != "auto":
        return args.discovery_mode
    if explicit_ports is not None and len(explicit_ports) <= max(1, args.small_port_threshold) and not looks_like_multi_host_target(args.target):
        return "connect"
    return "masscan"


def parse_nmap_services(output: str, host: str, started_at: str, finished_at: str) -> list[dict]:
    rows = []
    in_port_table = False
    for raw_line in output.splitlines():
        line = raw_line.rstrip()
        if line.startswith("PORT ") and "STATE" in line and "SERVICE" in line:
            in_port_table = True
            continue
        if not in_port_table:
            continue
        if not line or line.startswith("|") or line.startswith("_"):
            continue
        match = NMAP_PORT_LINE_RE.match(line)
        if not match:
            if rows and not re.match(r"^\d+\/tcp\s+", line):
                in_port_table = False
            continue
        port = int(match.group(1))
        state = match.group(2)
        service = match.group(3)
        version = (match.group(4) or "").strip()
        rows.append(
            {
                "host": host,
                "port": port,
                "protocol": "tcp",
                "masscan_open": True,
                "nmap_state": state,
                "service": service,
                "version": version,
                "nmap_started_at": started_at,
                "nmap_finished_at": finished_at,
            }
        )
    return rows


def write_summary_csv(
    path: Path,
    rows: list[dict],
    all_open_by_host: dict[str, set[int]],
    scan_id: str,
    scan_time: str,
    target: str,
    requested_ports: str,
    discovery_mode: str,
) -> None:
    by_pair: dict[tuple[str, int], dict] = {}
    for row in rows:
        by_pair[(row["host"], int(row["port"]))] = row

    for host, ports in all_open_by_host.items():
        for port in ports:
            by_pair.setdefault(
                (host, int(port)),
                {
                    "host": host,
                    "port": int(port),
                    "protocol": "tcp",
                    "masscan_open": True,
                    "nmap_state": "",
                    "service": "",
                    "version": "",
                    "nmap_started_at": "",
                    "nmap_finished_at": "",
                },
            )

    fieldnames = [
        "scan_id",
        "scan_time",
        "target",
        "requested_ports",
        "discovery_mode",
        "host",
        "port",
        "protocol",
        "masscan_open",
        "nmap_state",
        "service",
        "version",
        "nmap_started_at",
        "nmap_finished_at",
    ]
    file_exists = path.exists() and path.stat().st_size > 0
    with path.open("a", encoding="utf-8-sig", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames)
        if not file_exists:
            writer.writeheader()
        for (_host, _port), row in sorted(by_pair.items(), key=lambda item: (item[0][0], item[0][1])):
            output_row = {name: row.get(name, "") for name in fieldnames}
            output_row.update(
                {
                    "scan_id": scan_id,
                    "scan_time": scan_time,
                    "target": target,
                    "requested_ports": requested_ports,
                    "discovery_mode": discovery_mode,
                }
            )
            writer.writerow(output_row)


def nmap_worker(
    worker_id: int,
    jobs: "queue.Queue[tuple[str, list[int]] | None]",
    nmap_bin: str,
    args: argparse.Namespace,
    nmap_raw_file: Path,
    events_file: Path,
    nmap_runs: list[dict],
    nmap_service_rows: list[dict],
    lock: threading.Lock,
) -> None:
    while True:
        item = jobs.get()
        if item is None:
            jobs.task_done()
            return
        host, ports = item
        ports_str = ",".join(str(p) for p in sorted(set(ports)))
        cmd = [
            nmap_bin,
            "-sT",
            "-sV",
            "-sC",
            f"-T{args.nmap_timing}",
            "-p",
            ports_str,
            host,
        ]
        cmd.extend(split_extra_args(args.nmap_additional_args))
        started = datetime.now().isoformat(timespec="seconds")
        emit(f"nmap worker {worker_id} start: {host} ports={ports_str}", events_file)
        timed_out = False
        timeout_seconds = max(1.0, float(getattr(args, "nmap_timeout_seconds", 180.0) or 180.0))
        try:
            proc = subprocess.run(
                cmd,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=timeout_seconds,
            )
            stdout = proc.stdout
            return_code = proc.returncode
        except subprocess.TimeoutExpired as exc:
            timed_out = True
            stdout = (exc.stdout or "")
            return_code = None
            if isinstance(stdout, bytes):
                stdout = stdout.decode("utf-8", errors="replace")
            stdout = (stdout or "") + f"\n[nmap timeout] exceeded {timeout_seconds:.1f}s\n"
        finished = datetime.now().isoformat(timespec="seconds")
        service_rows = parse_nmap_services(stdout, host, started, finished)
        block = (
            f"\n===== nmap start {started} host={host} ports={ports_str} exit={return_code} timeout={timed_out} =====\n"
            f"$ {shlex.join(cmd)}\n"
            f"{stdout}"
            f"\n===== nmap end {finished} host={host} ports={ports_str} =====\n"
        )
        with lock:
            with nmap_raw_file.open("a", encoding="utf-8") as f:
                f.write(block)
            nmap_runs.append(
                {
                    "host": host,
                    "ports": sorted(set(ports)),
                    "command": cmd,
                    "exit_code": return_code,
                    "timed_out": timed_out,
                    "timeout_seconds": timeout_seconds,
                    "started_at": started,
                    "finished_at": finished,
                    "service_row_count": len(service_rows),
                }
            )
            nmap_service_rows.extend(service_rows)
        emit(f"nmap worker {worker_id} done: {host} ports={ports_str} exit={return_code} timeout={timed_out}", events_file)
        jobs.task_done()


def run_connect_discovery(
    args: argparse.Namespace,
    ports: list[int],
    events_file: Path,
    connect_raw_file: Path,
    enqueue_open,
) -> int:
    emit(
        f"connect discovery start: target={args.target} ports={','.join(map(str, ports))} timeout={args.connect_timeout_seconds}",
        events_file,
    )
    write_line(connect_raw_file, f"target={args.target} ports={','.join(map(str, ports))}")
    port_queue: "queue.Queue[int | None]" = queue.Queue()
    open_count = 0
    lock = threading.Lock()

    def worker() -> None:
        nonlocal open_count
        while True:
            port = port_queue.get()
            if port is None:
                port_queue.task_done()
                return
            status = "closed"
            err_text = ""
            try:
                with socket.create_connection((args.target, int(port)), timeout=args.connect_timeout_seconds):
                    status = "open"
            except OSError as exc:
                err_text = str(exc)
            write_line(connect_raw_file, f"{args.target},{port},tcp,{status},{err_text}")
            if status == "open":
                with lock:
                    open_count += 1
                emit(f"connect open: {args.target}:{port}/tcp", events_file)
                enqueue_open(args.target, int(port))
            port_queue.task_done()

    workers = []
    worker_count = max(1, min(args.connect_workers, len(ports)))
    for _ in range(worker_count):
        t = threading.Thread(target=worker, daemon=True)
        t.start()
        workers.append(t)

    for port in ports:
        port_queue.put(port)
    for _ in workers:
        port_queue.put(None)
    port_queue.join()
    for t in workers:
        t.join(timeout=1)

    emit(f"connect discovery done open={open_count}", events_file)
    return 0


def main() -> int:
    args = parse_args()
    output_dir = build_output_dir(args.conversation_id, args.relative_dir)
    prefix = safe_name(args.output_name_prefix, "streaming_port_scan")
    scan_id = f"{prefix}_{datetime.now().strftime('%Y%m%d_%H%M%S')}"
    if args.save_debug_files:
        debug_dir = output_dir
    else:
        debug_dir = build_tmp_debug_dir(scan_id, args.conversation_id, args.relative_dir)
    events_file = debug_dir / f"{prefix}.events.log"
    masscan_raw_file = debug_dir / f"{prefix}.masscan.log"
    connect_raw_file = debug_dir / f"{prefix}.connect.log"
    nmap_raw_file = debug_dir / f"{prefix}.nmap.log"
    findings_file = debug_dir / f"{prefix}.findings.json"
    summary_csv_file = output_dir / "streaming_port_scan.summary.csv"

    explicit_ports = parse_explicit_ports(args.ports)
    discovery_mode = choose_discovery_mode(args, explicit_ports)
    if discovery_mode == "connect" and explicit_ports is None:
        discovery_mode = "masscan"

    try:
        nmap_bin = require_binary("nmap")
        masscan_bin = require_binary(args.masscan_command) if discovery_mode == "masscan" else ""
    except RuntimeError as exc:
        print(json.dumps({"error": str(exc), "output_dir": str(output_dir), "discovery_mode": discovery_mode}, ensure_ascii=False), flush=True)
        return 2

    jobs: "queue.Queue[tuple[str, list[int]] | None]" = queue.Queue()
    nmap_runs: list[dict] = []
    nmap_service_rows: list[dict] = []
    lock = threading.Lock()
    workers = []
    for idx in range(max(1, args.nmap_workers)):
        t = threading.Thread(
            target=nmap_worker,
            args=(idx + 1, jobs, nmap_bin, args, nmap_raw_file, events_file, nmap_runs, nmap_service_rows, lock),
            daemon=True,
        )
        t.start()
        workers.append(t)

    seen_pairs: set[tuple[str, int]] = set()
    pending_by_host: dict[str, set[int]] = defaultdict(set)
    all_open_by_host: dict[str, set[int]] = defaultdict(set)
    pending_count = 0
    last_flush = time.monotonic()
    pending_lock = threading.Lock()
    stop_flush = threading.Event()

    def flush_pending(reason: str) -> None:
        nonlocal pending_count, last_flush
        with pending_lock:
            if pending_count <= 0:
                return
            batch_hosts = len(pending_by_host)
            batch_pairs = pending_count
            emit(f"flush to nmap reason={reason} hosts={batch_hosts} pairs={batch_pairs}", events_file)
            for host, ports_set in list(pending_by_host.items()):
                if ports_set:
                    jobs.put((host, sorted(ports_set)))
            pending_by_host.clear()
            pending_count = 0
            last_flush = time.monotonic()

    def enqueue_open(host: str, port: int) -> None:
        nonlocal pending_count
        should_flush = False
        with pending_lock:
            pair = (host, port)
            if pair in seen_pairs:
                return
            seen_pairs.add(pair)
            pending_by_host[host].add(port)
            all_open_by_host[host].add(port)
            pending_count += 1
            should_flush = pending_count >= max(1, args.batch_size)
        if should_flush:
            flush_pending("batch_size")

    def flush_timer() -> None:
        interval = max(1.0, float(args.flush_interval_seconds or 8.0))
        while not stop_flush.wait(interval):
            flush_pending("flush_interval")

    started_at = datetime.now().isoformat(timespec="seconds")
    masscan_exit = 0
    timer_thread = threading.Thread(target=flush_timer, daemon=True)
    timer_thread.start()
    if discovery_mode == "connect":
        masscan_exit = run_connect_discovery(args, explicit_ports or [], events_file, connect_raw_file, enqueue_open)

    if discovery_mode == "masscan":
        cmd = [masscan_bin, args.target, "-p", args.ports, "--rate", str(args.rate)]
        if args.interface:
            cmd.extend(["-e", args.interface])
        cmd.extend(split_extra_args(args.masscan_additional_args))

        emit(f"masscan start: target={args.target} ports={args.ports} rate={args.rate}", events_file)
        write_line(masscan_raw_file, f"$ {shlex.join(cmd)}")

        proc = subprocess.Popen(
            cmd,
            cwd=str(REPO_ROOT),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            bufsize=1,
        )

        assert proc.stdout is not None
        for line in proc.stdout:
            write_line(masscan_raw_file, line)
            match = MASSCAN_OPEN_RE.search(line)
            if not match:
                continue
            port = int(match.group(1))
            host = match.group(2).strip()
            emit(f"masscan open: {host}:{port}/tcp", events_file)
            enqueue_open(host, port)

        masscan_exit = proc.wait()
        emit(f"masscan done exit={masscan_exit}; final nmap flush", events_file)

    finished_at = datetime.now().isoformat(timespec="seconds")
    emit(f"discovery done mode={discovery_mode}; final nmap flush", events_file)
    stop_flush.set()
    timer_thread.join(timeout=1)
    flush_pending("masscan_done")

    for _ in workers:
        jobs.put(None)
    jobs.join()
    for t in workers:
        t.join(timeout=1)

    write_summary_csv(summary_csv_file, nmap_service_rows, all_open_by_host, scan_id, started_at, args.target, args.ports, discovery_mode)

    findings = {
        "target": args.target,
        "scan_id": scan_id,
        "ports": args.ports,
        "rate": args.rate,
        "discovery_mode": discovery_mode,
        "small_port_threshold": args.small_port_threshold,
        "started_at": started_at,
        "finished_at": finished_at,
        "masscan_exit_code": masscan_exit,
        "open_ports_by_host": {host: sorted(ports) for host, ports in sorted(all_open_by_host.items())},
        "open_pair_count": len(seen_pairs),
        "nmap_run_count": len(nmap_runs),
        "nmap_runs": nmap_runs,
        "services": sorted(nmap_service_rows, key=lambda row: (row["host"], int(row["port"]))),
        "files": {
            "summary_csv": str(summary_csv_file),
            "debug_dir": str(debug_dir),
        },
        "file_management_note": "Generated files are under chat_uploads and should be visible in CyberStrikeAI File Management.",
    }
    if args.save_debug_files:
        findings["files"].update(
            {
                "events_log": str(events_file),
                "connect_log": str(connect_raw_file),
                "masscan_log": str(masscan_raw_file),
                "nmap_log": str(nmap_raw_file),
                "findings_json": str(findings_file),
            }
        )
        findings_file.write_text(json.dumps(findings, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(findings, ensure_ascii=False, indent=2), flush=True)
    return 0 if masscan_exit == 0 else masscan_exit


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        raise SystemExit(130)
