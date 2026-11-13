// Super Simple - AI analytics runner.
// Started by the `supersimple` program; talks to Claude through the Claude Agent SDK.
// Prints one JSON object per line. The last one has type "supersimple_final".
//
//   node runner.mjs test    -> tiny round trip that proves the SDK, login and skill work
//   node runner.mjs retro   -> runs the weekly-retro skill
//
// Limits come from the SUPERSIMPLE_AI env var (JSON): timeout_secs, max_turns,
// max_budget_usd, model, days. The parent program also enforces the time limit
// by killing this process, so a stuck run can never keep spending.

import { query } from "@anthropic-ai/claude-agent-sdk";

const mode = process.argv[2] ?? "test";
const cfg = JSON.parse(process.env.SUPERSIMPLE_AI ?? "{}");
const days = cfg.days ?? 7;
const SCRIPTS = ".claude/skills/weekly-retro/scripts";

const emit = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");

// Soft time limit: stop a little before the parent's hard kill so we can report why.
const abort = new AbortController();
let timedOut = false;
const softMs = Math.max(5, (cfg.timeout_secs ?? 300) - 10) * 1000;
const timer = setTimeout(() => {
  timedOut = true;
  abort.abort();
}, softMs);

const common = {
  cwd: process.cwd(),
  settingSources: ["project"], // only this folder's settings: loads the skill, nothing else
  skills: ["weekly-retro"],
  // Ignore the account's connectors (Notion, calendars, ...). Their tool lists
  // would otherwise be attached to every request: about 100k extra tokens per run.
  strictMcpConfig: true,
  mcpServers: {},
  persistSession: false, // do not leave these runs in the Claude Code transcript folder
  permissionMode: "dontAsk", // never prompt; anything not allowed below is denied
  maxTurns: cfg.max_turns ?? 15,
  maxBudgetUsd: cfg.max_budget_usd ?? 1.0,
  abortController: abort,
  stderr: (data) => emit({ type: "supersimple_stderr", data: String(data) }),
};
if (cfg.model) common.model = cfg.model;

let prompt;
let options;
if (mode === "test") {
  prompt = "Reply with exactly this text and nothing else: SUPERSIMPLE_OK";
  options = { ...common, tools: ["Skill"], maxTurns: 2, maxBudgetUsd: Math.min(common.maxBudgetUsd, 0.25) };
} else {
  prompt =
    `Use the weekly-retro skill to write the retrospective for the last ${days} days. ` +
    `Pass --days ${days} to both of its scripts. Output only the report the skill specifies.`;
  options = {
    ...common,
    tools: ["Skill", "Bash", "Read"],
    // The only shell commands that may run are the skill's two scripts.
    allowedTools: [
      "Skill",
      "Read",
      `Bash(python3 ${SCRIPTS}/completed_work.py *)`,
      `Bash(python3 ${SCRIPTS}/transcripts.py *)`,
    ],
  };
}

function describe(block) {
  const input = block.input ?? {};
  if (block.name === "Bash") return `running ${String(input.command ?? "").slice(0, 90)}`;
  if (block.name === "Skill") return `loading skill ${input.skill ?? input.command ?? ""}`;
  if (block.name === "Read") return `reading ${input.file_path ?? ""}`;
  return `using ${block.name}`;
}

let result = null;
let skills = [];
let thrown = null;
try {
  for await (const message of query({ prompt, options })) {
    emit(message);
    if (message.type === "system" && message.subtype === "init") {
      skills = message.skills ?? [];
      emit({ type: "supersimple_progress", text: `connected (${message.model})` });
    } else if (message.type === "assistant") {
      for (const block of message.message?.content ?? []) {
        if (block.type === "tool_use") emit({ type: "supersimple_progress", text: describe(block) });
      }
    } else if (message.type === "result") {
      result = message;
    }
  }
} catch (err) {
  thrown = err;
}
clearTimeout(timer);

const final = {
  type: "supersimple_final",
  mode,
  ok: false,
  text: "",
  error: null,
  skills,
  cost_usd: result?.total_cost_usd ?? null,
  turns: result?.num_turns ?? null,
  denials: (result?.permission_denials ?? []).length,
};
if (timedOut) {
  final.error = `time limit reached (${Math.round(softMs / 1000)}s)`;
} else if (result && result.subtype === "success" && !result.is_error) {
  final.ok = true;
  final.text = String(result.result ?? "").trim();
} else if (result) {
  const why = {
    error_max_turns: "turn limit reached",
    error_max_budget_usd: "spending limit reached",
  }[result.subtype];
  const kind = result.subtype === "success" ? null : result.subtype;
  final.error = why ?? [kind, ...(result.errors ?? []), result.result].filter(Boolean).join(": ");
} else {
  final.error = thrown ? String(thrown?.message ?? thrown) : "no result received";
}

process.stdout.write(JSON.stringify(final) + "\n", () => process.exit(final.ok ? 0 : 1));
