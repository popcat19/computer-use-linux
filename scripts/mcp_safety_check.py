#!/usr/bin/env python3
"""Contract and safety smoke test for the computer-use-linux MCP surface."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import select
import subprocess
import sys
import tempfile
from typing import Any


EXPECTED_TOOLS = {
    "doctor",
    "setup_accessibility",
    "setup_window_targeting",
    "list_apps",
    "get_app_state",
    "list_windows",
    "focused_window",
    "screenshot",
    "zoom",
    "activate_window",
    "move_window",
    "resize_window",
    "click",
    "drag",
    "scroll",
    "press_key",
    "type_text",
    "perform_action",
    "set_value",
    "run_script",
    "act_and_observe",
}
SHELL_TOOL = "run_shell"
COMPLETION_TOOL = "complete_interaction"

INJECTION_PATTERNS = [
    re.compile(pattern, re.IGNORECASE)
    for pattern in [
        r"ignore\s+(all\s+)?previous\s+instructions",
        r"you\s+are\s+now\s+a",
        r"your\s+new\s+(task|role|instructions?)\s+(is|are)",
        r"system\s*:",
        r"<\s*(system|human|assistant|user)\s*>",
        r"do\s+not\s+(tell|inform|mention|reveal)",
        r"(curl|wget|fetch)\s+https?://",
        r"base64\.(b64decode|decodebytes)",
        r"\b(exec|eval)\s*\(",
    ]
]

DANGEROUS_TOOL_NAMES = {
    "exec",
    "eval",
    "shell",
    "run_command",
    SHELL_TOOL,
    "terminal",
    "read_file",
    "write_file",
    "delete_file",
}

FOCUS_SELECTORS = {
    "window_id",
    "pid",
    "app_id",
    "wm_class",
    "title",
    "tty",
    "terminal_pid",
    "terminal_command",
    "terminal_cwd",
}

SEMANTIC_SELECTORS = {
    "element_index",
    "role",
    "name",
    "text",
    "states",
}

OBJECT_REF_SELECTORS = SEMANTIC_SELECTORS | {"element_identifier"}

READ_ONLY_TOOLS = {
    "doctor",
    "list_apps",
    "get_app_state",
    "list_windows",
    "focused_window",
}

DESTRUCTIVE_MUTATING_TOOLS = {
    "click",
    "drag",
    "press_key",
    "type_text",
    "perform_action",
    "set_value",
    "run_script",
    "act_and_observe",
    SHELL_TOOL,
}

NON_DESTRUCTIVE_MUTATING_TOOLS = EXPECTED_TOOLS - READ_ONLY_TOOLS - DESTRUCTIVE_MUTATING_TOOLS

IDEMPOTENT_TOOLS = READ_ONLY_TOOLS | {
    "setup_accessibility",
    "setup_window_targeting",
    "activate_window",
    "move_window",
    "resize_window",
}

OPEN_WORLD_TOOLS = (EXPECTED_TOOLS | {SHELL_TOOL, COMPLETION_TOOL}) - {
    "doctor",
    "setup_accessibility",
    "setup_window_targeting",
}


class McpClient:
    def __init__(self, binary: pathlib.Path, extra_env: dict[str, str] | None = None):
        child_env = os.environ.copy()
        child_env["COMPUTER_USE_LINUX_ENABLE_SHELL"] = "0"
        child_env["COMPUTER_USE_LINUX_NOTIFY_ON_COMPLETE"] = "0"
        if extra_env:
            child_env.update(extra_env)
        self.process = subprocess.Popen(
            [str(binary), "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=child_env,
        )
        self.next_id = 1

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)

    def request(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        message: dict[str, Any] = {
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
        }
        self.next_id += 1
        if params is not None:
            message["params"] = params
        self._write(message)
        return self._read_response(message["id"])

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        message: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self._write(message)

    def _write(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def _read_response(self, request_id: int) -> dict[str, Any]:
        assert self.process.stdout is not None
        ready, _, _ = select.select([self.process.stdout], [], [], 5)
        if not ready:
            stderr = self._stderr_tail()
            raise AssertionError(f"timed out waiting for MCP response {request_id}; stderr={stderr!r}")
        line = self.process.stdout.readline()
        if not line:
            stderr = self._stderr_tail()
            raise AssertionError(f"MCP server closed stdout; stderr={stderr!r}")
        response = json.loads(line)
        if response.get("id") != request_id:
            raise AssertionError(f"expected response id {request_id}, got {response!r}")
        if "error" in response:
            raise AssertionError(f"MCP request {request_id} failed: {response['error']!r}")
        return response

    def _stderr_tail(self) -> str:
        if self.process.stderr is None:
            return ""
        ready, _, _ = select.select([self.process.stderr], [], [], 0)
        if not ready:
            return ""
        return self.process.stderr.read()[-2000:]


def package_version(repo: pathlib.Path) -> str:
    cargo = (repo / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.MULTILINE)
    if not match:
        raise AssertionError("Cargo.toml does not contain a package version")
    return match.group(1)


def assert_no_injection_text(label: str, text: str) -> None:
    for pattern in INJECTION_PATTERNS:
        if pattern.search(text):
            raise AssertionError(f"{label} contains suspicious MCP prompt text matching {pattern.pattern!r}")


def schema_properties(tool: dict[str, Any]) -> set[str]:
    schema = tool.get("inputSchema") or {}
    properties = schema.get("properties") or {}
    if not isinstance(properties, dict):
        raise AssertionError(f"{tool.get('name')} inputSchema.properties is not an object")
    return set(properties)


def assert_tool_annotations(tool: dict[str, Any]) -> None:
    name = tool["name"]
    annotations = tool.get("annotations")
    if not isinstance(annotations, dict):
        raise AssertionError(f"{name} is missing MCP tool annotations")

    expected = {
        "readOnlyHint": name in READ_ONLY_TOOLS,
        "destructiveHint": name in DESTRUCTIVE_MUTATING_TOOLS,
        "idempotentHint": name in IDEMPOTENT_TOOLS,
        "openWorldHint": name in OPEN_WORLD_TOOLS,
    }
    for key, value in expected.items():
        if annotations.get(key) is not value:
            raise AssertionError(
                f"{name} annotation {key}={annotations.get(key)!r}, expected {value!r}"
            )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/debug/computer-use-linux")
    parser.add_argument("--repo", default=".")
    args = parser.parse_args()

    repo = pathlib.Path(args.repo).resolve()
    binary = pathlib.Path(args.binary).resolve()
    if not binary.exists():
        raise AssertionError(f"binary does not exist: {binary}")

    version = package_version(repo)
    annotation_partition = (
        READ_ONLY_TOOLS | NON_DESTRUCTIVE_MUTATING_TOOLS | DESTRUCTIVE_MUTATING_TOOLS
    ) - {SHELL_TOOL}
    if annotation_partition != EXPECTED_TOOLS:
        raise AssertionError(
            "tool annotation classes do not cover the expected MCP tool set: "
            f"missing={EXPECTED_TOOLS - annotation_partition}, extra={annotation_partition - EXPECTED_TOOLS}"
        )

    client = McpClient(binary)
    try:
        initialize = client.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "computer-use-linux-ci", "version": "0"},
            },
        )["result"]
        client.notify("notifications/initialized", {})

        server_info = initialize.get("serverInfo") or {}
        if server_info.get("name") != "computer-use-linux":
            raise AssertionError(f"unexpected server name: {server_info!r}")
        if server_info.get("version") != version:
            raise AssertionError(f"MCP server version {server_info.get('version')!r} != Cargo version {version!r}")

        capabilities = initialize.get("capabilities") or {}
        if set(capabilities) != {"tools"}:
            raise AssertionError(f"unexpected MCP capabilities: {capabilities!r}")

        instructions = initialize.get("instructions") or ""
        assert_no_injection_text("server instructions", instructions)
        for required in [
            "Begin every turn that uses Computer Use by calling get_app_state",
            "Use list_windows/focused_window before targeted keyboard input",
            "Tools with readOnlyHint=false may mutate local desktop or application state",
            "refuse targeted input if focus cannot be verified",
        ]:
            if required not in instructions:
                raise AssertionError(f"server instructions are missing safety guidance: {required!r}")

        tools = client.request("tools/list", {})["result"].get("tools") or []
        names = {tool.get("name") for tool in tools}
        if names != EXPECTED_TOOLS:
            raise AssertionError(f"unexpected tools: missing={EXPECTED_TOOLS - names}, extra={names - EXPECTED_TOOLS}")

        for tool in tools:
            name = tool["name"]
            if not re.fullmatch(r"[a-z][a-z0-9_]*", name):
                raise AssertionError(f"tool name is not provider-safe snake_case: {name!r}")
            if name in DANGEROUS_TOOL_NAMES and name != SHELL_TOOL:
                raise AssertionError(f"unexpected dangerous tool name exposed: {name}")
            description = tool.get("description") or ""
            assert_no_injection_text(f"{name} description", description)
            assert_tool_annotations(tool)
            props = schema_properties(tool)
            if name != SHELL_TOOL and ("env" in props or "shell" in props or "command" in props):
                raise AssertionError(f"{name} exposes a raw process-control parameter: {sorted(props)}")
            if name in {"press_key", "type_text", "activate_window"} and not FOCUS_SELECTORS <= props:
                raise AssertionError(f"{name} is missing focus target selectors: {sorted(FOCUS_SELECTORS - props)}")
            if name == "click" and not SEMANTIC_SELECTORS <= props:
                raise AssertionError(f"{name} is missing semantic element selectors: {sorted(SEMANTIC_SELECTORS - props)}")
            if name in {"perform_action", "set_value"} and not OBJECT_REF_SELECTORS <= props:
                raise AssertionError(f"{name} is missing object/semantic element selectors: {sorted(OBJECT_REF_SELECTORS - props)}")

        zoom_tool = next(tool for tool in tools if tool["name"] == "zoom")
        if schema_properties(zoom_tool) != {"sources"}:
            raise AssertionError("zoom must expose only bounded screenshot sources")
        sources_schema = zoom_tool["inputSchema"]["properties"]["sources"]
        if (sources_schema.get("minItems"), sources_schema.get("maxItems")) != (1, 4):
            raise AssertionError("zoom source bounds are missing from the schema")
        if zoom_tool["inputSchema"].get("additionalProperties") is not False:
            raise AssertionError("zoom must reject unknown top-level arguments")
        definitions = zoom_tool["inputSchema"].get("$defs") or {}
        source_props = set((definitions.get("Source") or {}).get("properties") or {})
        if source_props != {"image", "target", "reference", "raise_window", "regions"}:
            raise AssertionError(f"zoom source schema changed: {source_props}")
        regions_schema = definitions["Source"]["properties"]["regions"]
        if (regions_schema.get("minItems"), regions_schema.get("maxItems")) != (1, 16):
            raise AssertionError("zoom region bounds are missing from the schema")
        factor_schema = definitions["Region"]["properties"]["factor"]
        if (factor_schema.get("minimum"), factor_schema.get("maximum")) != (1, 8):
            raise AssertionError("zoom factor bounds are missing from the schema")
        region_props = set((definitions.get("Region") or {}).get("properties") or {})
        if region_props != {"label", "rect", "element_index", "factor"}:
            raise AssertionError(f"zoom region schema changed: {region_props}")
        for arguments in [
            {"sources": []},
            {"sources": [{"image": {"$image": "foreign:0"}, "regions": [{"label": "old", "element_index": 1}]}]},
            {"sources": [{"reference": {"width": 10, "height": 10, "coordinate_width": 20, "coordinate_height": 20}, "regions": [{"label": "overflow", "rect": {"x": 4294967295, "y": 0, "width": 2, "height": 1}}]}]},
        ]:
            zoom_result = client.request("tools/call", {"name": "zoom", "arguments": arguments})["result"]
            if zoom_result.get("isError") is not True or any(block.get("type") == "image" for block in zoom_result.get("content", [])):
                raise AssertionError(f"invalid zoom must fail before capture: {zoom_result!r}")
        zoom_script = client.request("tools/call", {"name": "run_script", "arguments": {"code": 'tools::invoke("zoom", #{sources: [#{image: #{"$image": "foreign:0"}, regions: [#{label: "foreign", element_index: 1}]}]}); emit("must-not-run");'}})["result"]
        if zoom_script.get("isError") is not True or "must-not-run" in json.dumps(zoom_script):
            raise AssertionError("foreign image handle did not stop zoom script")

        script = client.request("tools/call", {
            "name": "run_script",
            "arguments": {"code": 'for n in 0..3 { emit(#{index: n}); }'},
        })["result"]
        summary = json.loads(script["content"][0]["text"])
        emitted = [json.loads(block["text"]) for block in script["content"][1:]]
        if script.get("isError") or summary != {"ok": True, "calls": 0, "error": None}:
            raise AssertionError(f"script evaluation failed: {script!r}")
        if emitted != [{"index": n} for n in range(3)]:
            raise AssertionError(f"script emit order changed: {emitted!r}")
        for code in [
            'tools::invoke("run_shell", #{command: "id"});',
            'tools::invoke("run_script", #{code: "1"});',
            'tools::invoke("unknown_tool", #{});',
            'tools::invoke("click", #{x: "invalid"});',
            'wait_ms(5001);',
            'loop {}',
        ]:
            result = client.request("tools/call", {
                "name": "run_script", "arguments": {"code": code},
            })["result"]
            if result.get("isError") is not True:
                raise AssertionError(f"script accepted unsafe/invalid code: {code!r}")

        invalid_workflow = client.request("tools/call", {
            "name": "act_and_observe",
            "arguments": {"action": "scroll", "arguments": {}, "settle_ms": 2001},
        })["result"]
        if invalid_workflow.get("isError") is not True:
            raise AssertionError("workflow accepted an unbounded settle delay")
        state = client.request("tools/call", {
            "name": "get_app_state",
            "arguments": {"window_id": 18446744073709551615, "include_screenshot": False},
        })["result"]
        metadata = state.get("structuredContent") or json.loads(state["content"][0]["text"])
        if metadata["accessibility_tree"] or "refusing an unscoped" not in metadata["accessibility_error"]:
            raise AssertionError("unresolved scope returned the desktop tree")
        if "data_url" in json.dumps(metadata):
            raise AssertionError("app state metadata exposed an inline image payload")

        failed_feedback = client.request("tools/call", {
            "name": "run_script",
            "arguments": {"code": 'tools::invoke("act_and_observe", #{action: "click", arguments: #{}, state: #{pid: 4294967295, include_screenshot: false}, settle_ms: 0});'},
        })["result"]
        if failed_feedback.get("isError") is not True or len(failed_feedback["content"]) < 2:
            raise AssertionError("failed script workflow discarded fresh feedback")
        feedback = json.loads(failed_feedback["content"][1]["text"])
        if feedback["feedback"]["action_completed"] is not False or not feedback.get("state"):
            raise AssertionError(f"failed workflow feedback was incomplete: {feedback!r}")
        if "refusing an unscoped" not in feedback["state"]["accessibility_error"]:
            raise AssertionError("failed workflow lost scoped observation refusal")

        request_id = client.next_id
        client.next_id += 1
        client._write({
            "jsonrpc": "2.0", "id": request_id, "method": "tools/call",
            "params": {"name": "run_script", "arguments": {
                "code": 'for n in 0..64 { tools::invoke("doctor", #{}); }',
                "max_calls": 64,
            }},
        })
        client.notify("notifications/cancelled", {"requestId": request_id, "reason": "test"})
        cancelled = client._read_response(request_id)["result"]
        if cancelled.get("isError") is not True or "cancelled" not in cancelled["content"][0]["text"]:
            raise AssertionError(f"MCP cancellation did not stop the script: {cancelled!r}")

        doctor = client.request("tools/call", {"name": "doctor", "arguments": {}})["result"]
        content = doctor.get("content") or []
        if not content or content[0].get("type") != "text":
            raise AssertionError(f"doctor did not return text content: {doctor!r}")
        report = json.loads(content[0].get("text") or "{}")
        for section in ["platform", "accessibility", "windowing", "input", "portals", "readiness"]:
            if section not in report:
                raise AssertionError(f"doctor report missing {section!r}: {report.keys()}")
    finally:
        client.close()

    notification_client = McpClient(binary, {"COMPUTER_USE_LINUX_NOTIFY_ON_COMPLETE": "1"})
    try:
        notification_client.request("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "completion-contract", "version": "0"},
        })
        notification_client.notify("notifications/initialized", {})
        tools = notification_client.request("tools/list", {})["result"]["tools"]
        if {tool["name"] for tool in tools} != EXPECTED_TOOLS | {COMPLETION_TOOL}:
            raise AssertionError("completion opt-in changed the wrong tool set")
        completion = next(tool for tool in tools if tool["name"] == COMPLETION_TOOL)
        assert_tool_annotations(completion)
        if schema_properties(completion):
            raise AssertionError("completion notification must not accept process parameters")
    finally:
        notification_client.close()

    shell_home = tempfile.TemporaryDirectory(prefix="computer-use-linux-shell-home-")
    pathlib.Path(shell_home.name, ".profile").write_text(
        "export COMPUTER_USE_LINUX_PROFILE_SECRET=must-not-be-loaded\n",
        encoding="utf-8",
    )
    shell_client = McpClient(
        binary,
        {
            "COMPUTER_USE_LINUX_ENABLE_SHELL": "1",
            "COMPUTER_USE_LINUX_TEST_SECRET": "must-not-be-inherited",
            "HOME": shell_home.name,
        },
    )
    try:
        shell_client.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "computer-use-linux-shell-ci", "version": "0"},
            },
        )
        shell_client.notify("notifications/initialized", {})
        tools = shell_client.request("tools/list", {})["result"].get("tools") or []
        names = {tool.get("name") for tool in tools}
        expected = EXPECTED_TOOLS | {SHELL_TOOL}
        if names != expected:
            raise AssertionError(
                f"unexpected opt-in tools: missing={expected - names}, extra={names - expected}"
            )
        script = shell_client.request("tools/call", {
            "name": "run_script",
            "arguments": {"code": 'tools::invoke("run_shell", #{command: "id"});'},
        })["result"]
        if script.get("isError") is not True:
            raise AssertionError("scripts gained host shell access through shell opt-in")
        shell_tool = next(tool for tool in tools if tool.get("name") == SHELL_TOOL)
        assert_tool_annotations(shell_tool)
        shell_props = schema_properties(shell_tool)
        required_shell_props = {"command", "cwd", "env", "timeout_seconds"}
        if not required_shell_props <= shell_props:
            raise AssertionError(
                f"{SHELL_TOOL} is missing bounded execution controls: {sorted(required_shell_props - shell_props)}"
            )
        result = shell_client.request(
            "tools/call",
            {
                "name": SHELL_TOOL,
                "arguments": {
                    "command": 'test -z "${COMPUTER_USE_LINUX_TEST_SECRET-}" && test -z "${COMPUTER_USE_LINUX_PROFILE_SECRET-}" && printf %s "$EXPLICIT"',
                    "cwd": str(repo),
                    "env": {"EXPLICIT": "shell-ok"},
                    "timeout_seconds": 5,
                },
            },
        )["result"]
        content = result.get("content") or []
        if not content or content[0].get("type") != "text":
            raise AssertionError(f"{SHELL_TOOL} did not return text content: {result!r}")
        shell_result = json.loads(content[0].get("text") or "{}")
        if shell_result.get("ok") is not True or shell_result.get("stdout") != "shell-ok":
            raise AssertionError(f"{SHELL_TOOL} smoke failed: {shell_result!r}")
        if len(shell_result.get("command_sha256") or "") != 64:
            raise AssertionError(f"{SHELL_TOOL} did not return an audit digest: {shell_result!r}")
    finally:
        shell_client.close()
        shell_home.cleanup()

    print(
        f"MCP safety check passed: {len(EXPECTED_TOOLS)} default tools, "
        f"{len(EXPECTED_TOOLS) + 1} with shell opt-in, version {version}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"mcp_safety_check.py: {exc}", file=sys.stderr)
        raise SystemExit(1)
