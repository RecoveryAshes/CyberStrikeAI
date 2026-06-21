import argparse
from pathlib import Path

import nucleiPlus_wrapper as wrapper


def make_args(scan_mode: str) -> argparse.Namespace:
    return argparse.Namespace(
        scan_mode=scan_mode,
        severity="",
        exclude_tags="",
        poc_name="",
        template_dir="",
        workflow_yaml="",
        finger_yaml="",
        dir_yaml="",
        web_threads=0,
        web_timeout=0,
        nmap_threads=0,
        nmap_timeout=0,
        disable_interactsh=False,
        audit_log=False,
    )


def test_build_dddd_command_uses_active_layer_for_precheck():
    cmd = wrapper.build_dddd_command(
        "/opt/dddd",
        make_args("precheck"),
        Path("targets.txt"),
        Path("out.txt"),
        Path("out.html"),
        Path("audit.log"),
    )

    assert cmd[:2] == ["/opt/dddd", "-active"]
    assert cmd.count("-active") == 1
    assert "-t" in cmd
    assert "-npoc" in cmd
    assert "-nd" in cmd
    assert "-ngp" in cmd
    assert "-nb" in cmd
    assert "-nhb" in cmd


def test_build_dddd_command_uses_active_layer_once_for_vuln_scan():
    cmd = wrapper.build_dddd_command(
        "/opt/dddd",
        make_args("vuln_scan"),
        Path("targets.txt"),
        Path("out.txt"),
        Path("out.html"),
        Path("audit.log"),
    )

    assert cmd[:2] == ["/opt/dddd", "-active"]
    assert cmd.count("-active") == 1
    assert "-npoc" not in cmd
    assert "-nd" not in cmd
    assert "-ngp" in cmd
    assert "-nb" in cmd
    assert "-nhb" in cmd
