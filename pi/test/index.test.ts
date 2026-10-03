import type {
	ExtensionAPI,
	ToolDefinition,
	ToolResultEvent,
} from "@earendil-works/pi-coding-agent";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createComputerUseLinuxExtension } from "../extension/index.ts";
import { GENERATED_MCP_TOOLS } from "../extension/generated-tools.ts";

type Handler = (event: any, ctx: any) => unknown;

class FakeMcpClient {
	static instances: FakeMcpClient[] = [];
	static result: {
		content?: unknown[];
		isError?: boolean;
		structuredContent?: unknown;
	} = { content: [{ type: "text", text: "ok" }] };

	readonly calls: Array<{
		name: string;
		args: Record<string, unknown>;
		signal: AbortSignal | undefined;
	}> = [];
	closed = 0;

	constructor(readonly options: Record<string, unknown>) {
		FakeMcpClient.instances.push(this);
	}

	async callTool(
		name: string,
		args: Record<string, unknown>,
		signal?: AbortSignal,
	) {
		this.calls.push({ name, args, signal });
		return FakeMcpClient.result;
	}

	async close() {
		this.closed += 1;
	}
}

function createPi() {
	const tools = new Map<string, ToolDefinition>();
	const handlers = new Map<string, Handler[]>();
	const entries: Array<{
		type: "custom";
		customType: string;
		data: unknown;
	}> = [];
	let activeTools = ["read", "bash"];
	const notify = vi.fn();
	const pi = {
		registerTool(tool: ToolDefinition) {
			tools.set(tool.name, tool);
			if (!activeTools.includes(tool.name)) activeTools.push(tool.name);
		},
		on(event: string, handler: Handler) {
			const entries = handlers.get(event) ?? [];
			entries.push(handler);
			handlers.set(event, entries);
		},
		getActiveTools() {
			return [...activeTools];
		},
		setActiveTools(names: string[]) {
			activeTools = [...names];
		},
		appendEntry(customType: string, data: unknown) {
			entries.push({ type: "custom", customType, data });
		},
		getAllTools() {
			return [...tools.values()].map((tool) => ({
				name: tool.name,
				description: tool.description,
				parameters: tool.parameters,
				sourceInfo: undefined,
			}));
		},
	} as unknown as ExtensionAPI;

	const emit = async (event: string, value: unknown = {}) => {
		let result: unknown;
		for (const handler of handlers.get(event) ?? []) {
			const next = await handler(value, {
				hasUI: true,
					sessionManager: {
						getEntries: () => [...entries],
						getBranch: () => [...entries],
					},
				ui: { notify },
			});
			if (next !== undefined) result = next;
		}
		return result;
	};

	return {
		activeTools: () => [...activeTools],
		emit,
		handlers,
		notify,
		pi,
		tools,
	};
}

describe("native Pi extension", () => {
	let agentDir: string;

	beforeEach(() => {
		agentDir = mkdtempSync(join(tmpdir(), "computer-use-linux-pi-test-"));
		vi.stubEnv("PI_CODING_AGENT_DIR", agentDir);
		FakeMcpClient.instances = [];
		FakeMcpClient.result = { content: [{ type: "text", text: "ok" }] };
	});

	afterEach(() => {
		vi.unstubAllEnvs();
		if (agentDir && existsSync(agentDir)) {
			rmSync(agentDir, { recursive: true, force: true });
		}
	});

	function load() {
		const harness = createPi();
		createComputerUseLinuxExtension({
			findBinary: () => ({
				binaryPath: "/tmp/computer-use-linux",
				env: { PATH: "/usr/bin" },
			}),
			loadClientModule: () => ({
				ComputerUseMcpClient: FakeMcpClient as never,
			}),
		})(harness.pi);
		return harness;
	}

	it("registers exact native schemas but starts with only the loader active", async () => {
		const harness = load();

		expect(harness.tools.size).toBe(GENERATED_MCP_TOOLS.length + 1);
		expect(FakeMcpClient.instances).toHaveLength(0);

		await harness.emit("session_start");

		expect(harness.activeTools()).toEqual([
			"read",
			"bash",
			"computer_use_linux_tools",
		]);
		expect(FakeMcpClient.instances).toHaveLength(0);

		for (const tool of GENERATED_MCP_TOOLS) {
			const registered = harness.tools.get(`computer_use_linux_${tool.name}`);
			expect(registered?.description).toContain(tool.description);
			expect(registered?.executionMode).toBe("sequential");
			expect(registered?.parameters).toMatchObject(tool.inputSchema);
		}
		expect(
			harness.tools.get("computer_use_linux_click")?.description,
		).toContain("obtain user approval");
	});

	it("activates exact tools additively without starting the desktop process", async () => {
		const harness = load();
		await harness.emit("session_start");
		const loader = harness.tools.get("computer_use_linux_tools")!;

		const result = await loader.execute(
			"loader",
			{ tools: ["doctor", "get_app_state"] },
			undefined,
			undefined,
			{} as never,
		);

		expect(harness.activeTools()).toEqual([
			"read",
			"bash",
			"computer_use_linux_tools",
			"computer_use_linux_doctor",
			"computer_use_linux_get_app_state",
		]);
		expect(result.details).toMatchObject({
			matches: ["doctor", "get_app_state"],
		});
		expect(FakeMcpClient.instances).toHaveLength(0);
	});

	it("enables and forwards a whole script without activating individual tools", async () => {
		const harness = load();
		await harness.emit("session_start");
		await harness.tools.get("computer_use_linux_tools")!.execute(
			"loader", { tools: ["run_script"] }, undefined, undefined, {} as never,
		);
		expect(harness.activeTools()).toContain("computer_use_linux_run_script");
		expect(harness.activeTools()).not.toContain("computer_use_linux_list_windows");
		const args = {
			code: 'let windows = tools::invoke("list_windows", #{}); emit(windows);',
			max_calls: 8,
			timeout_secs: 30,
		};
		const signal = new AbortController().signal;
		await harness.tools.get("computer_use_linux_run_script")!.execute(
			"script", args, signal, undefined, {} as never,
		);
		expect(FakeMcpClient.instances[0]?.calls).toEqual([
			{ name: "run_script", args, signal },
		]);
	});

	it("restores enabled tools from persisted branch state", async () => {
		const harness = load();
		await harness.emit("session_start", { reason: "startup" });
		const loader = harness.tools.get("computer_use_linux_tools")!;
		await loader.execute(
			"loader",
			{ tools: ["doctor"] },
			undefined,
			undefined,
			{} as never,
		);

		await harness.emit("session_start", { reason: "reload" });

		expect(harness.activeTools()).toContain("computer_use_linux_doctor");
	});

	it("finds tools by capability query", async () => {
		const harness = load();
		await harness.emit("session_start");
		const loader = harness.tools.get("computer_use_linux_tools")!;

		const result = await loader.execute(
			"loader",
			{ query: "screen capture" },
			undefined,
			undefined,
			{} as never,
		);

		expect(result.details).toMatchObject({
			matches: expect.arrayContaining(["screenshot"]),
		});
		expect(harness.activeTools()).toContain(
			"computer_use_linux_screenshot",
		);
	});

	it("reuses one client and preserves text and image results", async () => {
		const harness = load();
		await harness.emit("session_start");
		FakeMcpClient.result = {
			content: [
				{ type: "text", text: "done" },
				{ type: "image", data: "aGVsbG8=", mimeType: "image/png" },
			],
		};
		const doctor = harness.tools.get("computer_use_linux_doctor")!;

		const first = await doctor.execute(
			"one",
			{},
			undefined,
			undefined,
			{} as never,
		);
		const second = await doctor.execute(
			"two",
			{},
			undefined,
			undefined,
			{} as never,
		);

		expect(FakeMcpClient.instances).toHaveLength(1);
		expect(FakeMcpClient.instances[0]?.calls).toHaveLength(2);
		expect(first.content).toEqual([
			{ type: "text", text: "done" },
			{ type: "image", data: "aGVsbG8=", mimeType: "image/png" },
		]);
		expect(second.content).toEqual(first.content);
	});

	it("extracts legacy JSON screenshots before applying text limits", async () => {
		const harness = load();
		await harness.emit("session_start");
		const data = "A".repeat(400_000);
		FakeMcpClient.result = { content: [{ type: "text", text: JSON.stringify({
			screenshot: { data_url: `data:image/png;base64,${data}`, width: 480 },
			accessibility_tree: [{ name: "Navigation" }],
		}) }] };
		const result = await harness.tools.get("computer_use_linux_get_app_state")!.execute(
			"state", {}, undefined, undefined, {} as never,
		);
		const text = result.content.filter((block) => block.type === "text").map((block) => block.text).join("\n");
		expect(text.length).toBeLessThan(1000);
		expect(text).not.toContain("base64");
		expect(text).toContain("Navigation");
		expect(result.content.find((block) => block.type === "image")).toMatchObject({ data, mimeType: "image/png" });
	});

	it("converts bare and JSON-string data URLs to native images", async () => {
		const harness = load();
		await harness.emit("session_start");
		const url = "data:image/png;base64,aGVsbG8=";
		for (const text of [url, JSON.stringify(url)]) {
			FakeMcpClient.result = { content: [{ type: "text", text }] };
			const result = await harness.tools.get("computer_use_linux_get_app_state")!.execute(
				"state", {}, undefined, undefined, {} as never,
			);
			expect(result.content.filter((block) => block.type === "image")).toHaveLength(1);
			expect(result.content.filter((block) => block.type === "text").map((block) => block.text).join("\n")).not.toContain("base64");
		}
	});

	it("never falls back to base64 text when legacy images exceed the image cap", async () => {
		const harness = load();
		await harness.emit("session_start");
		FakeMcpClient.result = { structuredContent: {
			state: { screenshot: { data_url: `data:image/png;base64,${"A".repeat(3_000_000)}`, width: 480 } },
		} };
		const result = await harness.tools.get("computer_use_linux_run_script")!.execute(
			"script", { code: "emit(1);" }, undefined, undefined, {} as never,
		);
		expect(result.content.filter((block) => block.type === "image")).toHaveLength(0);
		const text = result.content.filter((block) => block.type === "text").map((block) => block.text).join("\n");
		expect(text.length).toBeLessThan(1000);
		expect(text).not.toContain("base64");
		expect(text).toContain("Result truncated");
	});

	it("bounds deep JSON extraction without throwing or dumping image payloads", async () => {
		const harness = load();
		await harness.emit("session_start");
		FakeMcpClient.result = { content: [{ type: "text", text:
			'{"nested":'.repeat(5000) + '{"data_url":"data:image/png;base64,aGVsbG8="}' + "}".repeat(5000),
		}] };
		const result = await harness.tools.get("computer_use_linux_get_app_state")!.execute(
			"state", {}, undefined, undefined, {} as never,
		);
		const text = result.content.filter((block) => block.type === "text").map((block) => block.text).join("\n");
		expect(text).toContain("traversal limit");
		expect(text).not.toContain("base64");
	});

	it("extracts images nested in legacy script JSON envelopes", async () => {
		const harness = load();
		await harness.emit("session_start");
		FakeMcpClient.result = { content: [{ type: "text", text: JSON.stringify({
			capture: { content: [{ type: "image", mimeType: "image/png", data: "aGVsbG8=" }] },
		}) }] };
		const result = await harness.tools.get("computer_use_linux_run_script")!.execute(
			"script", { code: "emit(1);" }, undefined, undefined, {} as never,
		);
		expect(result.content.filter((block) => block.type === "image")).toHaveLength(1);
		expect(result.content.filter((block) => block.type === "text").map((block) => block.text).join("\n")).not.toContain("aGVsbG8=");
	});

	it("marks MCP tool-level failures as Pi tool errors", async () => {
		const harness = load();
		await harness.emit("session_start");
		FakeMcpClient.result = {
			content: [{ type: "text", text: "failed" }],
			isError: true,
		};
		const doctor = harness.tools.get("computer_use_linux_doctor")!;
		const result = await doctor.execute(
			"one",
			{},
			undefined,
			undefined,
			{} as never,
		);

		const override = await harness.emit("tool_result", {
			type: "tool_result",
			toolName: "computer_use_linux_doctor",
			toolCallId: "one",
			input: {},
			content: result.content,
			details: result.details,
			isError: false,
		} satisfies Partial<ToolResultEvent>);

		expect(override).toEqual({ isError: true });
	});

	it("bounds aggregate text results and always reports omitted blocks", async () => {
		const harness = load();
		await harness.emit("session_start");
		FakeMcpClient.result = {
			content: [
				{ type: "text", text: "x".repeat(200_000) },
				{ type: "text", text: "omitted tail" },
			],
		};
		const doctor = harness.tools.get("computer_use_linux_doctor")!;
		const result = await doctor.execute(
			"one",
			{},
			undefined,
			undefined,
			{} as never,
		);
		const texts = result.content
			.filter((block) => block.type === "text")
			.map((block) => block.text);
		expect(texts.reduce((total, text) => total + text.length, 0)).toBeLessThanOrEqual(
			200_000,
		);
		expect(texts.at(-1)).toContain("Result truncated by the Pi extension");
		expect(texts.join("")).not.toContain("omitted tail");
	});

	it("bounds aggregate image count and reports omitted images", async () => {
		const harness = load();
		await harness.emit("session_start");
		FakeMcpClient.result = {
			content: Array.from({ length: 5 }, () => ({
				type: "image",
				data: "aGVsbG8=",
				mimeType: "image/png",
			})),
		};
		const screenshot = harness.tools.get("computer_use_linux_screenshot")!;
		const result = await screenshot.execute(
			"one",
			{},
			undefined,
			undefined,
			{} as never,
		);

		expect(result.content.filter((block) => block.type === "image")).toHaveLength(4);
		expect(
			result.content
				.filter((block) => block.type === "text")
				.map((block) => block.text)
				.join("\n"),
		).toContain("Result truncated by the Pi extension");
	});

	it("closes the session client exactly once", async () => {
		const harness = load();
		await harness.emit("session_start");
		const doctor = harness.tools.get("computer_use_linux_doctor")!;
		await doctor.execute(
			"one",
			{},
			undefined,
			undefined,
			{} as never,
		);

		await harness.emit("session_shutdown");
		await harness.emit("session_shutdown");

		expect(FakeMcpClient.instances[0]?.closed).toBe(1);
	});

	it("fails clearly when the binary is unavailable", async () => {
		const harness = createPi();
		createComputerUseLinuxExtension({
			findBinary: () => null,
			loadClientModule: () => ({
				ComputerUseMcpClient: FakeMcpClient as never,
			}),
		})(harness.pi);
		await harness.emit("session_start");

		const doctor = harness.tools.get("computer_use_linux_doctor")!;
		await expect(
			doctor.execute(
				"one",
				{},
				undefined,
				undefined,
				{} as never,
			),
		).rejects.toThrow("binary was not found");
		expect(harness.notify).toHaveBeenCalledWith(
			expect.stringContaining("binary not found"),
			"warning",
		);
	});
});
