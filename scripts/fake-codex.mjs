#!/usr/bin/env node
// A stand-in for `codex app-server`, for testing the guide without Codex:
// it speaks just enough of the protocol, and answers by script.
//
//   PROJJ_CODEX=scripts/fake-codex.mjs projj serve
//
// A message is echoed back as "You said: …", in pieces, as Codex writes.
// One that starts with `/call ` is followed by a JSON list of
// `[tool, arguments]`, which are called in turn, and the reply is what they
// answered. `/wait` keeps the turn going until it is stopped. Everything
// received is written to FAKE_CODEX_LOG, if set, one message a line.

import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";

const log = process.env.FAKE_CODEX_LOG;
let next = 0;
const waiting = new Map();
const stopped = new Map();
const send = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
const notify = (method, params) => send({ method, params });
const ask = (method, params) =>
  new Promise((resolve) => {
    const id = `fake-${++next}`;
    waiting.set(id, resolve);
    send({ id, method, params });
  });
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function turn(threadId, turnId, text, context) {
  notify("turn/started", { threadId, turn: { id: turnId, status: "inProgress" } });
  let reply;
  if (text.startsWith("/call ")) {
    const results = [];
    for (const [tool, args] of JSON.parse(text.slice(6))) {
      const callId = `call-${++next}`;
      const item = { type: "dynamicToolCall", id: callId, namespace: "projj", tool, arguments: args, status: "inProgress" };
      notify("item/started", { threadId, turnId, item });
      const answer = await ask("item/tool/call", { threadId, turnId, callId, namespace: "projj", tool, arguments: args });
      const done = { ...item, status: "completed", success: answer.result?.success, contentItems: answer.result?.contentItems };
      notify("item/completed", { threadId, turnId, item: done });
      results.push(`${tool}: ${answer.result?.success ? "" : "failed: "}${answer.result?.contentItems?.[0]?.text}`);
    }
    reply = results.join("\n\n");
  } else if (text === "/wait") {
    while (!stopped.get(turnId)) await sleep(50);
    notify("turn/completed", { threadId, turn: { id: turnId, status: "interrupted", error: null } });
    return;
  } else {
    let view = "";
    try {
      view = JSON.parse(context).view;
    } catch {}
    reply = `You said: ${text} (looking at ${view})`;
  }
  const id = `msg-${++next}`;
  notify("item/started", { threadId, turnId, item: { type: "agentMessage", id, text: "", phase: "final_answer" } });
  const third = Math.ceil(reply.length / 3);
  for (let at = 0; at < reply.length; at += third) {
    notify("item/agentMessage/delta", { threadId, turnId, itemId: id, delta: reply.slice(at, at + third) });
    await sleep(20);
  }
  notify("item/completed", { threadId, turnId, item: { type: "agentMessage", id, text: reply, phase: "final_answer" } });
  notify("turn/completed", { threadId, turn: { id: turnId, status: "completed", error: null } });
}

const ANSWERS = {
  initialize: () => ({ userAgent: "fake-codex", codexHome: "/nowhere", platformFamily: "unix", platformOs: "fake" }),
  "account/read": () => ({ account: { type: "apiKey" }, requiresOpenaiAuth: true }),
  "config/read": () => ({ config: { mcp_servers: { someone_elses: { command: "true" } } } }),
  "thread/start": () => ({ thread: { id: `thread-${++next}` } }),
  "thread/unsubscribe": () => ({}),
  "turn/start": ({ threadId, input, additionalContext }) => {
    const turnId = `turn-${++next}`;
    setTimeout(() => turn(threadId, turnId, input[0]?.text || "", additionalContext?.["projj-view"]?.value));
    return { turn: { id: turnId, status: "inProgress", items: [] } };
  },
  "turn/interrupt": ({ turnId }) => {
    stopped.set(turnId, true);
    return {};
  },
};

for await (const line of createInterface({ input: process.stdin })) {
  if (log) appendFileSync(log, `${line}\n`);
  const message = JSON.parse(line);
  if (message.method == null) {
    waiting.get(message.id)?.(message);
    waiting.delete(message.id);
  } else if (message.id != null) {
    const answer = ANSWERS[message.method];
    if (answer) send({ id: message.id, result: answer(message.params || {}) });
    else send({ id: message.id, error: { code: -32601, message: `fake-codex does not do ${message.method}` } });
  }
}
