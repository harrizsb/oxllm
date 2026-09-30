# Story e02s01: Ping — per-provider, per-model real request via the shared chat path

**1. Story ID:** e02s01 · **2. Title:** Ping: per-provider, per-model real request via the shared chat path · **3. Type:** feat · **4. Risk:** P1 (hot-path refactor + new admin route) · **5. Context (domain):** the dashboard is the trusted tailnet terminal (Amendment A1); the operator needs to confirm a configured provider+model actually answers before trusting the config. Config validation only proves the file parses.

**6. Zoom-out (mandate):**
- *create_chat_completions* — Purpose: route a chat request through the candidate loop. Callers: POST /v1/chat/completions. Contracts: model rewrite, upstream POST, circuit feedback, request log, daily tokens, telemetry, 502 on total failure.
- *build_router* — Purpose: assemble the router with the tailnet guard and same-origin layers. Callers: run_serve, tests. Contracts: /config, /validate, /apply already sit behind both layers.
- *dashboard.html* — Purpose: read-only status + config editor, embedded via include_str!. Callers: GET /dashboard. Contracts: vanilla JS, no build step, no new dependencies.

**7. Requirements (deltas):**
#### ADDED: `POST /admin/ping`
**Before:** No way to test a specific provider+model from the dashboard; config validation proves only that the file parses.
**After:** Accepts bounded JSON `{provider, model}`. Rejects unknown provider or a model not in that provider's configured models list with 400 and a diagnostic. Builds one minimal completion payload (`max_tokens: 1`, single "ping" user message) and sends it through the shared chat-completions attempt path pinned to that provider+model. Returns JSON `{ok, status, latency_ms, error?}`. Sits behind the tailnet guard and the same-origin layer (like /config, /validate, /apply). Full parity: the attempt increments provider counters, feeds circuit-breaker feedback, records a request-log entry, adds daily tokens, and emits telemetry — identical to a normal request.

#### MODIFIED: `create_chat_completions`
**Before:** The per-provider attempt (build request, send, feedback, counters, log, tokens, telemetry) is inline in the route loop.
**After:** The attempt is extracted into one shared helper `attempt_chat_completion`, called once per loop iteration by the main route and once by the ping. No behavior change; no parallel request logic.

#### ADDED: dashboard Ping controls
**Before:** Provider and model rows are display-only.
**After:** Each provider row and each model row gains a Ping button. Clicking it POSTs to /admin/ping and renders the result inline (success + latency, or the upstream error) with a busy-state disable. Vanilla JS; still include_str!-embedded.

**8. Success criteria:** (a) clicking Ping on a provider/model row sends one real minimal completion to that pair and shows the result inline; (b) a successful ping increments provider request/success counters, records a request-log entry, and adds daily tokens — identical to a normal request; (c) a failed ping feeds circuit-breaker failure feedback exactly like a normal failed request; (d) unknown provider/model returns 400; cross-origin browser request returns 403; (e) fmt/clippy/tests green, binary <15MB.

**9. Implementation steps:** see e02s01-tasks.yaml tasks 1–6 (risk P1; allure severity critical, categories ["Security Review","unit","integration"]).

**10. Verification script (manual):** run `cargo run -- serve`; open the dashboard; click Ping on a provider row and on a model row; confirm inline success with latency; stop the mock upstream (or use a bad key) and confirm the error renders; confirm /status shows the ping in the request log and the counters moved.

**11. Test plan:** integration tests with `spawn_mock_upstream`: ping success (counters, request log, daily tokens, telemetry parity); ping failure (circuit-breaker feedback); unknown provider 400; unknown model 400; cross-origin Origin 403; same-origin absent permitted. Unit tests for the 400 validation paths.

**12. Security considerations:** the endpoint sits behind the tailnet guard (defense in depth behind binding) and the same-origin layer (prevents a third-party page in a tailnet-connected browser from triggering pings); no auth is added (tailnet is the trust boundary); the ping payload is fixed server-side (max_tokens 1) — the client cannot amplify it; no unwrap/expect in the handler path.

**13. Observability:** the ping appears in /status exactly like a normal request (provider counters, request log, daily tokens); the handler logs the outcome at info level with provider, model, status, and latency.

**14. Out of scope:** CLI ping, ping-all, browser-to-upstream direct calls, changes to routing/virtual-model resolution, ping history or persistence.

**15. Risks:** the extraction touches the hot path — mitigated by tests-first (write the ping integration test against the helper before refactoring) and the existing integration suite covering chat success, streaming, failover, and 502.

**16. Dependencies:** none (first story). **17. Acceptance criteria (Gherkin):**
```gherkin
Scenario: Ping a working provider+model
  Given the dashboard is open and a provider has a configured model
  When the operator clicks Ping on that model row
  Then one real minimal completion is sent to that provider+model
  And the row shows success with latency
  And /status shows the ping in the request log with counters incremented

Scenario: Ping a failing provider+model
  Given the upstream returns an error
  When the operator clicks Ping
  Then the row shows the upstream error
  And the circuit breaker records the failure exactly like a normal request

Scenario: Ping an unknown provider or model
  When the operator pings a provider or model not in the config
  Then the endpoint returns 400 with a diagnostic and sends nothing upstream

Scenario: Cross-origin browser ping is refused
  Given a third-party page in a tailnet-connected browser
  When it POSTs to /admin/ping with a cross-origin Origin
  Then the request is refused with 403 before any upstream call
```
**18. Definition of done:** all tasks passing; verify evidence in specs/verifications/; ping behavior covered by tests. **19. Open questions:** none. **20. References:** specs/planning-context.yaml; specs/IMPACT_LATEST.md; handoff.md Amendment A1 rev 2; CLAUDE.md.
