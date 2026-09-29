# Story e01s01: Tailnet reachability — dual bind + tailnet/loopback guard on all routes

**1. Story ID:** e01s01 · **2. Title:** Tailnet reachability: dual bind + tailnet/loopback guard on all routes · **3. Type:** feat · **4. Risk:** P0 (security boundary + ≥5 BCP) · **5. Context (domain):** oxllm becomes reachable exactly on the VPS Tailscale address and loopback; tailnet membership is the single trust boundary; the guard is defense-in-depth behind binding.

**6. Zoom-out (mandate):**
- *run_serve* — Purpose: bind socket(s), assemble router, install signal handlers. Callers: CLI dispatch (serve). Contracts: graceful shutdown, PID file, SIGHUP reload, CORS/request-id layers, connect-info service.
- *localhost_only* — Purpose: route-level source-IP access control. Callers: 7 route layers (status, dashboard, health, reload, admin×3). Contracts: 403 JSON error shape, x-request-id preserved on 403, existing loopback pass-through.
- *ServerConfig* — Purpose: listener config. Callers: run_serve, tests. Contracts: serde defaults (timeout 5, bind_family "ipv4"), existing config.toml parses.

**7. Requirements (deltas; before states verified in code this session):**
#### MODIFIED: `localhost_only` middleware → `tailnet_only`
**Before:** Per-route layer on 7 admin/ops routes only; permits loopback v4/v6/IPv4-mapped; `/v1/*` routes are reachable by any source that completes TCP.
**After:** Single router-level layer applied to ALL routes including `/v1/*`; permits loopback (v4/v6/IPv4-mapped via to_canonical) and IPv4 CGNAT 100.64.0.0/10; 403 JSON error otherwise; 403 responses retain x-request-id.
#### MODIFIED: listener modes
**Before:** One listener chosen by bind_family: ipv4→host:port, ipv6→[::]:port (host ignored), dual→socket2 dual-stack [::]:port (host ignored).
**After:** bind_family accepts only "ipv4" (values "ipv6"/"dual" fail validation with a migration diagnostic naming the replacement: set host); host is parsed as a literal non-unspecified IPv4 address (no DNS, no wildcard, no IPv6 listener); run_serve binds host:port plus 127.0.0.1:port, skipping the duplicate when host is loopback; both sockets served by one Router via tokio::select! over two graceful axum::serve futures (single shared shutdown trigger); socket2 dependency and dual-stack code removed; wildcard/unspecified host (0.0.0.0) and all IPv6 host literals are rejected at validation (it silently defeats address-only exposure and causes specific-after-wildcard EADDRINUSE).

**8. Success criteria:** (a) loopback client reaches every route unauthenticated; (b) unit matrix proves guard decisions incl. IPv4-mapped CGNAT; (c) ipv6/dual configs fail fast with migration message; (d) fmt/clippy/tests green, binary <15MB.

**9. Implementation steps:** see e01s01-tasks.yaml tasks 1–6 (risk P0; allure severity critical, categories ["Security Review","unit","integration"]).

**10. Verification script (manual):** run `cargo run -- serve` with host=127.0.0.1; `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:PORT/health` → 200; same for /status, /dashboard, /v1/models; `curl -s -X POST http://127.0.0.1:PORT/reload` → 200/400 JSON. On the VPS later: repeat via `http://<tailscale-ip>:PORT/status` from a tailnet client → 200.

**11. Test plan:** pure-fn decision matrix (loopback v4/v6/mapped, 100.64.0.0/10 bounds 100.64.0.1 & 100.127.255.254, rejections 8.8.8.8/192.168.1.10/100.0.0.1/100.128.0.0/2001:db8::1); bind-mode validation tests (ipv6 rejected, dual rejected, wildcard rejected, host non-IP rejected, IPv6 literal rejected); integration test on 127.0.0.1:0 asserting 200s and 403-shape on a forge-socket path via the pure fn.

**12. Security considerations:** 100.64.0.0/10 is shared CGNAT space — source range proves routing, not Tailscale identity; documented acceptance: binding + host firewall are primary; IPv6 tailnet ULA (fd7a:115c:a1e0::/48) is intentionally NOT permitted (user decision: IPv4-only listener); no auth is added (out of scope).

**13. Observability:** retain warn! log on 403 with peer IP; add info! lines naming both bound addresses.

**14. Out of scope:** CSRF/Origin hardening (e01s02 task); tailnet IPv6 ULA support; host firewall/VPS changes (deploy phase).

**15. Risks:** two serve futures need one shared shutdown signal — use tokio::select! with a single shutdown future polled by reference (shutdown_signal takes no args today; refactor to a shared future/oneshot); removing socket2 touches two Cargo.tomls — workspace and crate entries must both go; existing CORS tests assert `*` on /v1 — unchanged.

**16. Dependencies:** none (first story). **17. Acceptance criteria (Gherkin):**
```gherkin
Scenario: Tailnet peer reaches all routes
  Given the server is bound to a Tailscale address plus 127.0.0.1
  When a client connects from a 100.64.0.0/10 source
  Then /health, /status, /dashboard, /reload, /admin/*, and /v1/* respond as if local

Scenario: Public source is refused everywhere
  Given the server is running
  When a request arrives from a non-loopback non-CGNAT source
  Then every route including /v1/* returns 403 with the JSON error body and x-request-id header

Scenario: Legacy wildcard mode fails fast
  Given a config with bind_family = "dual"
  When the user runs oxllm serve
  Then startup fails with a diagnostic that tells the user to remove bind_family and set host
```
**18. Definition of done:** all tasks passing; verify evidence in specs/verifications/; guard+bind behavior covered by tests. **19. Open questions:** Unknown: [whether SIGHUP reload of a changed host takes effect — it does not; listener is startup-bound — documented, accepted]. **20. References:** handoff.md Amendment A1 rev2; specs/IMPACT_LATEST.md; SCOPE S1/S2; user decisions this session (IPv4-only, host authoritative).
