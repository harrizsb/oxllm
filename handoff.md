# Handoff: oxllm Extension

**One-line summary:** Extend the `oxllm` Rust gateway in place. Add virtual models with weighted routing, daily token accounting (cached / uncached / total), a 3-entry request log, and a read-only dashboard. No new processes. No disk. No CRUD UI.

---

## 0. How to read this document

Read it fully before writing any code.

- Sections 3, 5, and 6 are **binding**. Do not violate or relitigate them.
- Sections 4 and 8 define **what to build** and **how to know it is done**.
- Section 7 is where you use judgment. Prefer the simpler option and write a comment explaining your choice.
- If something is still unclear after reading, stop and ask. Do not guess.

Words used in this document:

- **Provider** — an upstream LLM API. In this project, always OpenAI-compatible.
- **Circuit breaker** — a per-provider switch that opens after repeated failures and closes again after a cooldown. Already exists in oxllm.
- **Virtual model** — a name a client can request, which oxllm resolves to a real provider + model at routing time.
- **SWRR** — smooth weighted round-robin. A standard algorithm that distributes requests across targets in proportion to their weights, without bursty behavior.
- **Ring buffer** — a fixed-size list that drops the oldest entry when a new one is added.

---

## 1. The project

`oxllm` is a small, fast LLM gateway written in Rust.

Repository: https://github.com/planetf1/oxllm

What it already does:

- Routes requests to OpenAI-compatible providers.
- Falls back to the next provider when one fails.
- Tracks circuit breaker state per provider.
- Keeps in-memory stats (request counts, token volumes).
- Exposes them via a local status endpoint.

What it deliberately does **not** do:

- It does not write to disk at runtime.
- It does not persist state across restarts.
- It does not run any background service or sidecar.

These are not accidents. They are the design. Your work must preserve them.

---

## 2. The task

Extend oxllm with four features. Build them in the order listed. Each one should be independently shippable and testable before moving on.

1. Virtual models with weighted routing.
2. Daily token accounting.
3. A 3-entry request log.
4. A read-only dashboard.

Details are in section 4.

---

## 3. Hard constraints

These are not preferences. Violating any of them fails the task.

- **No disk writes at runtime.** The only file oxllm touches is `config.toml`, and only when a human edits it. No SQLite. No log files. No cache files. No temporary files.
- **No new processes.** No wrapper service. No sidecar. No daemon. Everything runs inside the oxllm binary.
- **No heavy dependencies.** A small utility crate is fine. A web framework with a build step, a template engine, or anything requiring Node.js is not. The dashboard is served from a string embedded with `include_str!`.
- **In-memory state only.** Stats, request log, counters — all in RAM. A restart wipes them. This is accepted behavior.
- **OpenAI-compatible providers only.** Do not build adapters for Anthropic, Gemini, or anything else. The provider interface is `base_url` + `api_key` + `models`. Nothing more.
- **Keep it small.** Target roughly 300 lines of new code for features 1 through 3, and up to 100 more for feature 4. If you are approaching 1000 lines, you have gone wrong.

---

## 4. What to build

### 4.1 Virtual models with weighted routing

Add a `[virtual_models]` section to `config.toml`. Each virtual model has a name and a list of targets. Each target is a provider name, a model name, and a weight.

When a client sends a request with `"model": "<virtual_model_name>"`, oxllm picks one target at routing time and forwards the request to that real provider and model.

Use SWRR to pick. Weights are relative, not absolute.

Resolution happens **before** the existing fallback chain. If the picked target's circuit is open, fall through to the next target. If all targets are unavailable, return an error — do not panic.

Health-aware: skip targets whose circuit breaker is open. They rejoin automatically when the circuit closes.

### 4.2 Daily token accounting

Track three numbers per day:

- Cached tokens.
- Uncached tokens.
- Total tokens.

The source of truth is the `usage` field in the provider's response. Specifically:

- Cached tokens come from `prompt_tokens_details.cached_tokens`.
- Uncached tokens are `prompt_tokens` minus the cached value.
- Completion tokens add to the total.

Store each number in an atomic counter. Detect day rollover on each request by comparing the current day to the stored day. On rollover, reset all three counters. No history. Only today.

If a provider omits `prompt_tokens_details`, treat cached tokens as zero. Do not fail the request.

### 4.3 Last-3 request log

Keep a ring buffer of the three most recent requests.

Each entry contains:

- Timestamp.
- Model requested.
- Virtual model name, if any.
- Real provider name.
- Cached tokens.
- Uncached tokens.
- HTTP status code.

New requests push old ones out. The purpose is debugging, not analytics. Do not grow this buffer. Do not add fields beyond the list above without a specific reason.

### 4.4 Read-only dashboard

Serve a single HTML page from oxllm. It shows:

- Today's cached, uncached, and total token counts.
- The three most recent requests.

Requirements:

- Embedded in the binary with `include_str!`.
- No build step. No framework. Plain HTML. Vanilla JavaScript only if truly needed.
- No login. No auth. Assume localhost or a trusted network.
- Do not add forms, buttons, or write endpoints.

---

## 5. What not to build

These will be tempting. Do not build them.

- **No CRUD UI for virtual models or providers.** Edit `config.toml` by hand. Restart or send SIGHUP to reload.
- **No persistence.** No SQLite. No files. No external store.
- **No weekly or monthly rollups.** Daily only. The reset is destructive and that is fine.
- **No request log beyond 3 entries.** Not 10. Not 100. Three.
- **No token counting logic.** Always read `usage` from the provider. Never compute tokens yourself.
- **No auth, rate limiting, or multi-tenancy.**
- **No per-account key rotation as a feature inside oxllm.** See section 6.3 for the accepted answer.
- **No streaming complexity beyond what is required.** If streaming makes `usage` extraction hard, handle non-streaming first and leave a clear TODO.

---

## 6. Decisions already made

Do not relitigate these. They were decided after weighing alternatives.

**6.1 Extend oxllm in place. Do not wrap it.**

A wrapper service was considered and rejected. It breaks the zero-disk design and adds a process boundary. Everything goes inside oxllm.

**6.2 Dashboard is read-only. No CRUD UI.**

A CRUD UI was considered and rejected. It either edits memory, which is useless after restart, or edits `config.toml`, which adds form-rendering code and a disk write path. If hand-editing the config becomes painful later, revisit. Not now.

**6.3 Multiple keys for one service: multiple provider entries.**

If a user has three GLM keys, they define three provider entries — `glm-a`, `glm-b`, `glm-c` — each with the same `base_url` and a different `api_key`. A virtual model spreads traffic across them. Each entry has its own circuit breaker. This gives account rotation with zero new code.

Do not add key arrays to the provider struct. Do not build key-rotation logic. The config workaround is the answer.

**6.4 Weighted routing, not strict sequential.**

Use SWRR. If a user wants "try A, then B, then C," they express it with weights, or they rely on the fallback chain when a circuit opens. Do not build a separate sequential mode.

---

## 7. Decisions left to you

Use judgment here. Prefer the simpler option. Document your choice in a code comment.

- Where to place the new code. New module or extend existing ones? Match the repo's current structure.
- How to expose the dashboard. New route, or extend the existing status endpoint? Match existing routing patterns.
- Config schema details. Field names, list versus map for targets, whether `model` is optional on a target. Match existing config style.
- Ring buffer implementation. A `Mutex<VecDeque>` is enough. Do not reach for lock-free unless the existing code already does.
- SWRR state storage. Per virtual model. Atomic if easy. Mutex if not. Do not over-engineer.

---

## 8. Definition of done

All boxes must be checked.

- `config.toml` accepts a `[virtual_models]` section without error.
- A request to a virtual model name is routed to a real provider.
- Weights are respected across many requests — statistically, not exactly.
- If a target's circuit is open, traffic goes to another target.
- Daily cached, uncached, and total token counts increment correctly from `usage`.
- Counts reset at the day boundary.
- The last three requests are visible and correct.
- The dashboard renders and shows the above.
- Existing tests still pass.
- New behavior has tests.
- No new disk writes at runtime. Verify with `strace` or an equivalent tool.
- Binary size increase is reasonable. No accidental dependency bloat.

---

## 9. Traps

Things that will bite you.

- **Streaming.** SSE responses put `usage` in the final chunk. You must parse the stream to extract it. If this is hard, handle non-streaming first and leave a clear TODO. Do not ship a broken streaming path silently.
- **Provider `usage` naming.** OpenAI-compatible is not always identical across providers. `prompt_tokens_details.cached_tokens` is standard, but some providers omit it. Treat missing as zero.
- **Day rollover race.** Two requests at exactly midnight could both reset the counters. Use compare-and-swap on the day value. Do not put a mutex around all requests.
- **Virtual model name collisions.** A virtual model named `gpt-4o` would shadow a real model. Decide the precedence, document it, and log a startup warning if a collision exists. Recommended: virtual models win, with a warning.
- **Circuit breaker scope.** The existing circuit is per provider. Keep it that way. Do not make it per virtual model.
- **Empty target list.** A virtual model with no targets must fail cleanly. Do not panic.

---

## 10. Working style

- Read the existing code before writing new code. Match its style.
- Ship one feature at a time. Do not bundle.
- When two options are close, pick the simpler one and write a comment explaining why.
- If you are unsure whether something is in scope, it is not. Ask first.
- Prefer the smallest diff that satisfies the requirement.
- Do not refactor existing code unless the new feature requires it.
- If you find a real bug in existing code, note it. Do not silently fix it in the same change.
