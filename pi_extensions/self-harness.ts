/**
 * Self-Harness for Pi
 *
 * Copy this file to ~/.pi/agent/extensions/ and reload Pi.
 *
 * Commands:
 *   /self-harness <validator>  propose one prompt overlay and keep it only if validator passes
 *   /self-harness-show         show the active overlay for this project
 *   /self-harness-clear        remove the active overlay for this project
 */

import { complete, type Message } from "@earendil-works/pi-ai";
import type { ExtensionAPI, ExtensionCommandContext, SessionEntry } from "@earendil-works/pi-coding-agent";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const DIR = ".pi/self-harness";
const ACTIVE = "harness.md";
const CANDIDATE = "candidate.md";
const LOG = "log.jsonl";
const MAX_EVIDENCE_BYTES = 12_000;
const MAX_HARNESS_BYTES = 4_000;

type Candidate = {
	why: string;
	harness: string;
};

function paths(cwd: string) {
	const dir = join(cwd, DIR);
	return {
		dir,
		active: join(dir, ACTIVE),
		candidate: join(dir, CANDIDATE),
		log: join(dir, LOG),
	};
}

function readText(path: string): string | undefined {
	if (!existsSync(path)) return undefined;
	const text = readFileSync(path, "utf8").trim();
	return text || undefined;
}

function truncateTail(text: string, max: number): string {
	if (text.length <= max) return text;
	return text.slice(text.length - max);
}

function entryText(entry: SessionEntry): string {
	if (entry.type === "message") {
		return JSON.stringify(entry.message);
	}
	if (entry.type === "compaction") {
		return `compaction: ${entry.summary}`;
	}
	if (entry.type === "custom_message") {
		return `custom ${entry.customType}: ${JSON.stringify(entry.content)}`;
	}
	return `${entry.type}: ${JSON.stringify(entry)}`;
}

function evidence(ctx: ExtensionCommandContext): string {
	const branch = ctx.sessionManager.getBranch().slice(-30);
	const text = branch
		.map((entry, index) => `#${index} ${entry.timestamp}\n${entryText(entry)}`)
		.join("\n\n");
	return truncateTail(text, MAX_EVIDENCE_BYTES);
}

function stripFence(text: string): string {
	const trimmed = text.trim();
	if (!trimmed.startsWith("```")) return trimmed;
	const withoutOpening = trimmed.slice(3);
	const body = withoutOpening.includes("\n") ? withoutOpening.slice(withoutOpening.indexOf("\n") + 1) : withoutOpening;
	return body.replace(/```$/u, "").trim();
}

function parseCandidate(text: string): Candidate {
	const whySplit = text.split("WHY:");
	if (whySplit.length < 2) throw new Error("missing WHY marker");
	const harnessSplit = whySplit.slice(1).join("WHY:").split("HARNESS:");
	if (harnessSplit.length < 2) throw new Error("missing HARNESS marker");

	const why = harnessSplit[0].trim();
	const harness = stripFence(harnessSplit.slice(1).join("HARNESS:").split("\nEND")[0]);
	if (!harness) throw new Error("empty harness");
	if (harness.length > MAX_HARNESS_BYTES) {
		throw new Error(`harness is ${harness.length} bytes; max is ${MAX_HARNESS_BYTES}`);
	}
	return { why, harness };
}

async function propose(ctx: ExtensionCommandContext, current: string, validationCommand: string): Promise<Candidate> {
	if (!ctx.model) throw new Error("no model selected");

	const auth = await ctx.modelRegistry.getApiKeyAndHeaders(ctx.model);
	if (!auth.ok || !auth.apiKey) {
		throw new Error(auth.ok ? `no API key for ${ctx.model.provider}` : auth.error);
	}

	const user: Message = {
		role: "user",
		content: [
			{
				type: "text",
				text: `Use the Self-Harness loop from arXiv:2606.09498.

Weakness Mining: cluster recurring failures in the evidence.
Harness Proposal: produce one minimal prompt-overlay edit tied to those failures.
Proposal Validation: Pi will temporarily install the overlay and run:
${validationCommand}

Current overlay:
${current || "none"}

Evidence:
${evidence(ctx)}

Return exactly:
WHY: one sentence naming the recurring weakness and why this edit targets it
HARNESS:
<plain instructions for Pi, max ${MAX_HARNESS_BYTES} bytes>
END`,
			},
		],
		timestamp: Date.now(),
	};

	const response = await complete(
		ctx.model,
		{
			systemPrompt:
				"You propose tiny Pi prompt overlays. Do not use tools. Return only WHY/HARNESS/END.",
			messages: [user],
		},
		{ apiKey: auth.apiKey, headers: auth.headers },
	);

	if (response.stopReason === "aborted") throw new Error("proposal aborted");
	const text = response.content
		.filter((part): part is { type: "text"; text: string } => part.type === "text")
		.map((part) => part.text)
		.join("\n");
	return parseCandidate(text);
}

function log(cwd: string, accepted: boolean, validationCommand: string, candidate: Candidate, validation: unknown) {
	const p = paths(cwd);
	mkdirSync(p.dir, { recursive: true });
	const line = JSON.stringify({
		ts: new Date().toISOString(),
		accepted,
		validationCommand,
		why: candidate.why,
		harness: candidate.harness,
		validation,
	});
	const old = existsSync(p.log) ? readFileSync(p.log, "utf8") : "";
	writeFileSync(p.log, `${old}${line}\n`);
}

export default function selfHarness(pi: ExtensionAPI) {
	pi.on("before_agent_start", async (event) => {
		const active = readText(paths(event.systemPromptOptions.cwd).active);
		if (!active) return;
		return {
			systemPrompt: `${event.systemPrompt}

## Active Self-Harness Overlay

${active}`,
		};
	});

	pi.registerCommand("self-harness", {
		description: "Improve the project prompt overlay after a validator passes",
		handler: async (args, ctx) => {
			const validationCommand = args.trim();
			if (!validationCommand) {
				ctx.ui.notify("Usage: /self-harness <validator command>", "error");
				return;
			}
			if (!ctx.model) {
				ctx.ui.notify("No model selected", "error");
				return;
			}

			const p = paths(ctx.cwd);
			const previous = readText(p.active);
			let candidate: Candidate;
			try {
				candidate = await propose(ctx, previous || "", validationCommand);
			} catch (error) {
				ctx.ui.notify(`self-harness proposal failed: ${String(error)}`, "error");
				return;
			}

			mkdirSync(p.dir, { recursive: true });
			writeFileSync(p.candidate, candidate.harness);
			writeFileSync(p.active, candidate.harness);

			const result = await pi.exec("sh", ["-lc", validationCommand], { cwd: ctx.cwd, timeout: 600_000 });
			const output = truncateTail(`${result.stdout}${result.stderr}`, 8_000);
			if (result.code === 0) {
				log(ctx.cwd, true, validationCommand, candidate, { code: result.code, output });
				ctx.ui.notify(`self-harness accepted: ${candidate.why}`, "info");
				return;
			}

			if (previous) {
				writeFileSync(p.active, previous);
			} else if (existsSync(p.active)) {
				rmSync(p.active);
			}
			log(ctx.cwd, false, validationCommand, candidate, { code: result.code, output });
			ctx.ui.notify(`self-harness rejected: validator exit ${result.code}`, "warning");
		},
	});

	pi.registerCommand("self-harness-show", {
		description: "Show active self-harness overlay",
		handler: async (_args, ctx) => {
			const active = readText(paths(ctx.cwd).active);
			ctx.ui.notify(active || "No active self-harness overlay", active ? "info" : "warning");
		},
	});

	pi.registerCommand("self-harness-clear", {
		description: "Clear active self-harness overlay",
		handler: async (_args, ctx) => {
			const p = paths(ctx.cwd);
			if (existsSync(p.active)) rmSync(p.active);
			ctx.ui.notify("self-harness overlay cleared", "info");
		},
	});
}
