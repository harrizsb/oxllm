# Story e01s02: Raw config view + Validate dry-run on the dashboard

**1. Story ID:** e01s02 · **2. Title:** Raw config view + Validate dry-run on the dashboard · **3. Type:** feat · **4. Risk:** P0 (≥5 BCP, config parsing security) · **5. Context:** The dashboard becomes a trusted terminal: the operator sees the config exactly as written and can dry-run changes with the same validation an Apply will run.

**6. Zoom-out:** *config.rs structs* — Purpose: deserialize + validate TOML. Callers: main.rs (serve/validate/reload/SIGHUP), state.rs (VirtualModelTarget), config.rs tests. Contracts: serde defaults, OxllmError error surface, existing config.toml parses. *routes.rs/dashboard.html* — Purpose: dashboard handlers/UI. Callers: router registration in main.rs. Contracts: response shapes stable, include_str! embedding, vanilla JS. Adding deny_unknown_fields changes the contract deliberately (typo'd fields become errors).

**7. Requirements (deltas):**
#### MODIFIED: config parsing strictness
**Before:** Unknown fields/tables anywhere in [server], [providers], [virtual_models] are silently ignored; base_url is only parsed later in build_app_state; host/bind_family unvalidated.
**After:** deny_unknown_fields on all config structs (field name in the error); Config::validate rejects bind_family values other than "ipv4", host that is not a literal non-unspecified IP, and enabled providers with malformed base_url.
#### ADDED: GET /config
Returns the exact raw bytes of the resolved config file (values visible, ${VAR} placeholders preserved, no expansion), text/plain, cache-control: no-store, behind the router-level tailnet guard; if an Origin header is present, require it to match the request Host origin (Origin-absent CLI clients remain allowed), preventing arbitrary internet pages opened on a tailnet-connected browser from reading secrets.
#### ADDED: POST /validate
Accepts application/json {"config": "<raw TOML>"}; runs the exact Apply pipeline preconditions: expand_env_vars → strict TOML parse → Config::validate → build_app_state dry-run; returns 200 {"valid":true} or 400 {"valid":false,"errors":[{"message", optional "line","col"}]}; performs zero disk writes; if Origin is present it must match the request Host origin (Origin-absent direct clients allowed).
#### MODIFIED: dashboard
**Before:** read-only status views only.
**After:** adds a config-editor section: full-file textarea populated from same-origin GET /config, Validate button, diagnostics list rendered via textContent (no HTML interpolation of editor content).

**8. Success criteria:** validation matrix (syntax, unknown field, missing env var, dangling provider ref, zero weight, malformed enabled base_url) each returns precise diagnostics with config file untouched; valid config returns valid:true; existing endpoints unchanged.

**9. Implementation steps:** e01s02-tasks.yaml tasks 1–7. **10. Verification script (manual):** open dashboard via tailnet/loopback; editor textarea shows config.toml verbatim incl. ${VAR}; introduce `port_typo = 1`; press Validate; diagnostics list names the unknown field; verify config.toml on disk unchanged (`git diff` clean).

**11. Test plan:** see tasks 2/4/5; spans from toml::de errors on the expanded text are labeled approximate when placeholders change offsets. **12. Security:** editor content is never interpolated into HTML (textContent only); secrets visible by explicit user decision; GET /config + /validate sit behind the tailnet guard (route-level Origin check blocks cross-origin browser reads/writes for editor endpoints despite current global CORS Any; Origin is not treated as authentication). **13. Observability:** warn! on failed validation attempts with request id. **14. Out of scope:** Apply/write path; CSRF Origin enforcement (e01s03); masking. **15. Risks:** deny_unknown_fields is a breaking change for configs carrying stale keys — mitigated by precise field-name diagnostics and by bind_family remaining known (ipv4-only). **16. Dependencies:** e01s01 (guard, host validation). **17. Acceptance criteria (Gherkin):**
```gherkin
Scenario: Operator validates a config with a typo
  Given the dashboard editor shows the current raw config
  When the operator adds "port_typo = 1" and clicks Validate
  Then diagnostics list an unknown-field error naming port_typo
  And config.toml on disk is byte-identical

Scenario: Validate matches what Apply would do
  Given a config whose enabled provider has a malformed base_url
  When the operator clicks Validate
  Then diagnostics report the URL error before any write could occur
```
**18. Definition of done:** tasks passing; evidence in specs/verifications/. **19. Open questions:** none. **20. References:** SCOPE S3/S4/S6; IMPACT_LATEST; handoff.md A1 rev2.
