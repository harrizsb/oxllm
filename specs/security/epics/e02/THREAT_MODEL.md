# Threat Model — e02 Provider-model ping

## Scope
POST /admin/ping: dashboard-triggered real minimal completion pinned to one configured
provider+model, through the shared chat attempt path. Plus the shared-attempt extraction
in create_chat_completions.

## Surface area
- New admin route behind tailnet_only (router-level) + require_same_origin (route-level), same as /config, /validate, /apply.
- New JSON body {provider, model} parsed with the axum Json extractor (default body limit applies).
- Outbound request to a configured provider (fixed payload: max_tokens 1) carrying the provider API key.
- Dashboard JS addition (Ping buttons).

## Vulnerability categories
- SSRF: attacker-controlled URL? No — base_url comes only from config.toml (operator-controlled); provider and model must already exist in config. A tailnet peer cannot introduce new URLs.
- Cost amplification: payload is fixed server-side (max_tokens 1); a caller cannot enlarge it. Each click is one paid request, bounded by the trust boundary.
- Auth bypass: no new auth; tailnet guard + same-origin apply. Cross-origin browser requests get 403 before any upstream call.
- Info disclosure: upstream error strings return to the dashboard caller — acceptable: the caller is the trusted operator who already sees the raw config (keys included).
- Hot-path regression: extraction touches create_chat_completions; mitigated by the existing integration suite plus new ping tests.
- Resource exhaustion: no new locks, no unbounded state; one in-flight request per click; the dashboard disables the button while busy.

## Risk level: Medium
New action that triggers paid upstream calls on a trusted surface; no new trust boundaries crossed.

## Mitigations
1. Route registered with the require_same_origin layer (mirrors /apply).
2. Strict input validation: unknown provider or model not in the provider's list returns 400 and sends nothing upstream.
3. Fixed minimal payload (max_tokens: 1) — no caller-controlled request shaping.
4. Full gate suite before merge (fmt, clippy -D warnings, workspace tests, <15MB release).
