// Purpose: Plan bounded native image attachments and repair metadata associations before rendering Pi results.

import { randomUUID } from "node:crypto";
import type { AgentToolResult } from "@earendil-works/pi-coding-agent";
type Content = AgentToolResult<Record<string, unknown>>["content"][number];
const MAX_TEXT = 200_000;
const MAX_BYTES = 2 * 1024 * 1024;
const MAX_IMAGES = 16;
const NOTICE = "[Result truncated by the Pi extension. Request a smaller/bounded result.]";
const record = (value: unknown): value is Record<string, any> => typeof value === "object" && value !== null && !Array.isArray(value);
const encodedBytes = (data: string) => Math.max(0, Math.floor(data.length * 3 / 4) - (data.endsWith("==") ? 2 : data.endsWith("=") ? 1 : 0));
type Image = { key: string; data: string; mimeType: string; original?: number; handle?: string; label?: string; source?: number; ordinal?: number; omitted?: boolean; referenced?: boolean; bounds?: Record<string,unknown> };
type Text = { text?: string; value?: unknown };

export function convertMcpResult(result: { content?: unknown[]; structuredContent?: unknown }): Content[] {
	const request = `cul-result-${randomUUID()}`;
	const images: Image[] = [];
	const texts: Text[] = [];
	const native = new Map<number, Image>();
	const handles = new Map<string, Image>();
	let truncated = false;
	let visited = 0;
	const visit = (value: unknown, depth: number, fn: (value: Record<string, any>) => void): void => {
		if (++visited > 100_000 || depth > 128) throw new Error("traversal limit");
		if (Array.isArray(value)) { for (const child of value) visit(child, depth + 1, fn); }
		else if (record(value)) { fn(value); for (const child of Object.values(value)) visit(child, depth + 1, fn); }
	};
	const bounds = (owner: Record<string,any>): Record<string,unknown> | undefined => {
		const keys = ["crop_rect","source_dimensions","reference","width","height","coordinate_width","coordinate_height","factor","bit_depth","transform","window_scope","retained_source","detail_note"];
		const subset = Object.fromEntries(keys.filter(key => owner[key] !== undefined).map(key => [key,owner[key]]));
		return Object.keys(subset).length && JSON.stringify(subset).length <= 4096 ? subset : undefined;
	};
	const addLegacy = (owner: Record<string, any>, data: string, mimeType: string) => {
		const image: Image = { key: `${request}:legacy:${images.length}`, data, mimeType, label: typeof owner.label === "string" ? owner.label.slice(0,128) : undefined, source: typeof owner.source_index === "number" ? owner.source_index : undefined, referenced: true, bounds: bounds(owner) };
		images.push(image);
		owner.image = { attachment_key: image.key };
	};
	const addText = (text: string) => {
		let value: unknown;
		try { value = JSON.parse(text); } catch {
			if (text.startsWith("data:image/")) value = { data_url: text };
			else { texts.push({ text }); return; }
		}
		if (typeof value === "string" && value.startsWith("data:image/")) value = { data_url: value };
		const before = images.length;
		try {
			visit(value, 0, (owner) => {
				if (owner.type === "image" && typeof owner.data === "string") {
					const data = owner.data; delete owner.data;
					if (typeof owner.mimeType === "string") addLegacy(owner, data, owner.mimeType);
					else truncated = true;
					owner.transport = "native";
				}
				if (typeof owner.data_url === "string") {
					const match = /^data:(image\/[a-zA-Z0-9.+-]+);base64,([a-zA-Z0-9+/=\r\n]*)$/.exec(owner.data_url);
					delete owner.data_url;
					if (match) addLegacy(owner, match[2]!, match[1]!);
					else truncated = true;
				}
			});
			visited = 0;
			visit(value, 0, (owner) => {
				if (!Array.isArray(owner.content)) return;
				const local = new Map<number, string>();
				for (const [index, block] of owner.content.entries()) if (record(block) && record(block.image) && typeof block.image.attachment_key === "string") local.set(index, block.image.attachment_key);
				if (!local.size) return;
				const repair = (nested: unknown, depth: number): void => {
					if (++visited > 100_000 || depth > 128) throw new Error("traversal limit");
					if (Array.isArray(nested)) for (const child of nested) repair(child, depth + 1);
					else if (record(nested)) {
						if (typeof nested.content_index === "number" && local.has(nested.content_index)) { nested.attachment_key = local.get(nested.content_index); delete nested.content_index; }
						for (const child of Object.values(nested)) repair(child, depth + 1);
					}
				};
				repair(owner, 0);
			});
			if (/data:image\/[^;]+;base64,|"data"\s*:\s*"[A-Za-z0-9+/=]{128}/.test(JSON.stringify(value))) throw new Error("unextracted encoded image");
			texts.push({ value });
		} catch {
			images.splice(before);
			truncated = true;
			texts.push({ text: "[JSON result omitted: image extraction traversal limit exceeded.]" });
		}
	};
	let pendingHandle: string | undefined;
	for (const [index, raw] of (result.content ?? []).entries()) {
		if (!record(raw)) { addText(JSON.stringify(raw)); continue; }
		if (raw.type === "text" && typeof raw.text === "string") {
			addText(raw.text);
			try { const caption = JSON.parse(raw.text); pendingHandle = typeof caption.image_handle === "string" ? caption.image_handle.slice(0,128) : undefined; } catch { pendingHandle = undefined; }
		} else if (raw.type === "image" && typeof raw.data === "string" && typeof raw.mimeType === "string") {
			const image: Image = { key: `${request}:mcp:${index}`, data: raw.data, mimeType: raw.mimeType, original: index, handle: pendingHandle };
			images.push(image); native.set(index, image); if (pendingHandle) handles.set(pendingHandle, image); pendingHandle = undefined;
		} else if (raw.type === "resource" && record(raw.resource)) {
			if (typeof raw.resource.text === "string") addText(raw.resource.text);
			else if (typeof raw.resource.blob === "string" && typeof raw.resource.mimeType === "string" && raw.resource.mimeType.startsWith("image/")) {
				const image: Image = { key: `${request}:mcp:${index}`, data: raw.resource.blob, mimeType: raw.resource.mimeType, original: index }; images.push(image); native.set(index, image);
			} else { truncated = true; texts.push({ text: "[Binary resource omitted.]" }); }
		} else if (raw.type === "image") truncated = true;
		else addText(JSON.stringify(raw));
	}
	if (texts.length === 0 && result.structuredContent !== undefined) addText(JSON.stringify(result.structuredContent));
	const byKey = new Map(images.map((image) => [image.key, image]));
	const lookup = (ref: unknown): Image | undefined => record(ref) ? typeof ref.attachment_key === "string" ? byKey.get(ref.attachment_key) : typeof ref.content_index === "number" ? native.get(ref.content_index) : typeof ref.$image === "string" ? handles.get(ref.$image) : undefined : undefined;
	try {
		visited = 0;
		for (const text of texts) visit(text.value, 0, (owner) => {
			const image = lookup(owner.image) ?? lookup(owner);
			if (image) { image.referenced = true; image.bounds ??= bounds(owner); image.label ??= typeof owner.label === "string" ? owner.label.slice(0,128) : undefined; image.source ??= typeof owner.source_index === "number" ? owner.source_index : undefined; }
		});
	} catch { truncated = true; texts.splice(0, texts.length, { text: "[JSON result omitted: image extraction traversal limit exceeded.]" }); }
	let bytes = 0, count = 0;
	for (const image of images) {
		const size = encodedBytes(image.data);
		if (!image.mimeType.startsWith("image/") || count >= MAX_IMAGES || bytes + size > MAX_BYTES) { image.omitted = true; truncated = true; }
		else { image.ordinal = count++; bytes += size; }
	}
	const reference = (image: Image) => image.omitted ? { image_omitted: true, attachment_id: image.key, label: image.label ?? null, source_index: image.source ?? null, reason: "native image count/byte limit" } : { transport: "native", attachment_id: image.key, native_image_index: image.ordinal, original_mcp_content_index: image.original ?? null, original_image_handle: image.handle ?? null };
	try {
		visited = 0;
		for (const text of texts) visit(text.value, 0, (owner) => {
			const image = lookup(owner.image);
			if (image) {
				if (image.omitted) { delete owner.image; owner.image_omitted = reference(image); owner.ok = false; }
				else owner.image = reference(image);
			} else if (record(owner.image) && (typeof owner.image.content_index === "number" || typeof owner.image.$image === "string")) {
				delete owner.image; owner.image_omitted = { reason: "image reference has no native attachment in this result", label: owner.label ?? null, source_index: owner.source_index ?? null }; owner.ok = false;
			} else {
				const direct = lookup(owner);
				if (direct) { for (const key of Object.keys(owner)) delete owner[key]; Object.assign(owner, reference(direct)); }
				else if (Object.keys(owner).length === 1 && (typeof owner.$image === "string" || typeof owner.content_index === "number")) {
					const handle = owner.$image, index = owner.content_index;
					for (const key of Object.keys(owner)) delete owner[key];
					Object.assign(owner, { image_omitted: true, original_image_handle: handle ?? null, original_mcp_content_index: index ?? null, reason: "native attachment unavailable in this result" });
				}
			}
		});
	} catch { texts.splice(0, texts.length, { text: "[JSON result omitted: image extraction traversal limit exceeded.]" }); truncated = true; }
	const content: Content[] = [];
	const kept = images.filter((image) => !image.omitted);
	const omitted = images.filter((image) => image.omitted);
	const captions = new Map(kept.filter(image => image.referenced || image.label !== undefined).map(image => [image.key, JSON.stringify({ attachment_id: image.key, label: image.label ?? null, source_index: image.source ?? null, native_image_index: image.ordinal, original_mcp_content_index: image.original ?? null, original_image_handle: image.handle ?? null, ...image.bounds })]));
	const omissionNotice = omitted.length ? JSON.stringify({ images_omitted: omitted.slice(0,16).map((image) => ({ attachment_id: image.key, label: image.label ?? null, source_index: image.source ?? null })), additional_omitted_labels: Math.max(0,omitted.length-16), message: `Native images omitted: ${omitted.length} (16 images / 2 MiB total limit; no encoded text fallback).` }) : "";
	let remaining = MAX_TEXT - NOTICE.length - omissionNotice.length - [...captions.values()].reduce((sum,text) => sum+text.length,0);
	const append = (text: string) => { if (text.length > remaining) truncated = true; if (remaining > 0) content.push({ type: "text", text: text.slice(0, remaining) }); remaining = Math.max(0, remaining - text.length); };
	for (const text of texts) append(text.text ?? JSON.stringify(text.value));
	for (const image of kept) {
		const caption = captions.get(image.key);
		if (caption) content.push({type:"text",text:caption});
		content.push({ type: "image", data: image.data, mimeType: image.mimeType });
	}
	if (omissionNotice) content.push({type:"text",text:omissionNotice});
	if (truncated) content.push({ type: "text", text: NOTICE });
	if (!content.length) content.push({ type: "text", text: "(empty result)" });
	return content;
}
