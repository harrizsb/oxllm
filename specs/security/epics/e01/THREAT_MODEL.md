# e01 threat model: Tailscale gateway and dashboard config editor

## Scope and assets

- Raw `config.toml` and provider credentials, including literal secrets and `${VAR}` placeholders.
- Active proxy state, provider endpoints, credentials, models, and routing.
- Config file, one-generation `.bak`, temporary files, and filesystem metadata.
- Dashboard browser context and tailnet-connected clients.

## Security boundary

Tailscale reachability plus loopback is the network boundary. It is not per-device authentication. The bind address, host firewall, and tailnet ACLs remain the security boundary. No application-layer authentication is added by e01.

## Threats and mitigations

1. **Tailnet client reads or changes config.** Any client in the permitted source range can call editor endpoints directly. Mitigation: keep the router-level guard, restrictive file permissions, and tailnet ACLs; do not treat CGNAT source identity as device identity. Residual risk: a compromised tailnet device can read secrets and change routing.
2. **Cross-origin browser access.** A third-party page in a tailnet-connected browser could otherwise read raw config or submit editor requests. Mitigation: editor endpoints reject a present Origin unless exactly one valid value matches the request Host authority; reject null, malformed, and duplicate values. CORS remains permissive on pre-existing `/v1` routes. Origin checks are not authentication. Residual risk: direct clients without Origin remain allowed by design.
3. **Secret exposure through diagnostics or logs.** Mitigation: return raw config only to permitted same-origin callers; do not log response bodies or expanded secrets; label parser spans approximate when expansion changes offsets.
4. **Unsafe provider URLs.** Existing `Url::parse` validation is retained. Local/private endpoints are intentionally supported, so no blanket private-address ban is added. Unsupported schemes and malformed URLs fail before state construction. Residual risk: DNS and redirect behavior can change the eventual destination; this is outside e01 scope.
5. **Partial or interleaved Apply.** Mitigation: one shared lock serializes HTTP reload, SIGHUP, and Apply; candidate state is built before any write; staged temp files are synced and renamed in the same directory; publish failure restores old bytes. Residual risk: rollback failure is reported distinctly and requires operator intervention.
6. **Browser DOM injection.** Mitigation: dashboard editor content is assigned with `textContent`; no HTML interpolation of config or diagnostics.

## Verification

- e01s01: source-IP decision matrix, route coverage, dual bind, and graceful shutdown tests.
- e01s02: strict parsing, raw-byte fidelity, Origin matrix, zero-write validation tests.
- e01s03: serialization, staged failure injection, backup exactness, rollback, and metrics continuity tests.
- Final: security review, strict Clippy, formatting, workspace tests, and release size gate.
