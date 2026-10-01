use crate::error::{OxllmError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub otel_endpoint: String,
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout_secs: u64,
    #[serde(default = "default_bind_family")]
    pub bind_family: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Additional static headers for upstream requests. Gateway-owned headers cannot be overridden.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Extra JSON request properties. `model` and `stream` are reserved for the gateway/client.
    #[serde(default)]
    pub extra_body: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualModelTarget {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_virtual_model_weight")]
    pub weight: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub virtual_models: HashMap<String, Vec<VirtualModelTarget>>,
}

impl Config {
    /// Loads a TOML configuration file and expands environment variables of style `${VAR_NAME}`.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path)
            .map_err(|e| OxllmError::ConfigLoad(format!("Failed to read config file: {}", e)))?;

        let expanded = expand_env_vars(&content)?;
        let config: Config = toml::from_str(&expanded)?;
        Ok(config)
    }

    /// Validates the configuration syntax and cross-references virtual models with defined providers.
    pub fn validate(&self) -> Result<()> {
        match self.server.bind_family.as_str() {
            "ipv4" => {},
            legacy => {
                return Err(OxllmError::ConfigLoad(format!(
                    "Unsupported server.bind_family '{legacy}'; remove bind_family and set server.host to an IPv4 address"
                )));
            },
        }
        let host: std::net::IpAddr = self.server.host.parse().map_err(|_| {
            OxllmError::ConfigLoad(format!(
                "Invalid server.host '{}': set it to a literal IPv4 address",
                self.server.host
            ))
        })?;
        match host {
            std::net::IpAddr::V4(ipv4) if !ipv4.is_unspecified() => {},
            std::net::IpAddr::V4(_) => {
                return Err(OxllmError::ConfigLoad(
                    "Invalid server.host '0.0.0.0': wildcard binding is not permitted; set a specific IPv4 address".into(),
                ));
            },
            std::net::IpAddr::V6(_) => {
                return Err(OxllmError::ConfigLoad(format!(
                    "Invalid server.host '{}': IPv6 listeners are not supported; set a literal IPv4 address",
                    self.server.host
                )));
            },
        }
        for provider in self.providers.iter().filter(|provider| provider.enabled) {
            validate_provider_custom_options(provider)?;
            let url = reqwest::Url::parse(&provider.base_url).map_err(|error| {
                OxllmError::ConfigLoad(format!(
                    "Invalid base URL for enabled provider '{}': {}",
                    provider.name, error
                ))
            })?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err(OxllmError::ConfigLoad(format!(
                    "Invalid base URL for enabled provider '{}': expected an http or https URL with a host",
                    provider.name
                )));
            }
        }

        let provider_map: HashMap<&str, &ProviderConfig> = self
            .providers
            .iter()
            .map(|p| (p.name.as_str(), p))
            .collect();

        // 1. Validate that at least one provider is configured
        if self.providers.is_empty() {
            return Err(OxllmError::ConfigLoad(
                "At least one provider must be defined".into(),
            ));
        }

        // 2. Validate that virtual models target existing, enabled providers
        for (vm_name, targets) in &self.virtual_models {
            if targets.is_empty() {
                return Err(OxllmError::ConfigLoad(format!(
                    "Virtual model '{}' has no targets configured",
                    vm_name
                )));
            }

            for target in targets {
                if target.weight == 0 {
                    return Err(OxllmError::ConfigLoad(format!(
                        "Virtual model '{}' target for provider '{}' must have a positive weight",
                        vm_name, target.provider
                    )));
                }

                match provider_map.get(target.provider.as_str()) {
                    Some(provider) => {
                        if !provider.enabled {
                            // Warn or skip: we allow referencing disabled providers,
                            // but the virtual model resolution loop will bypass them.
                        }
                    },
                    None => {
                        return Err(OxllmError::ConfigLoad(format!(
                            "Virtual model '{}' targets undefined provider '{}'",
                            vm_name, target.provider
                        )));
                    },
                }
            }
        }

        Ok(())
    }
}

fn default_virtual_model_weight() -> u32 {
    1
}

fn default_upstream_timeout() -> u64 {
    5
}

fn default_bind_family() -> String {
    "ipv4".to_string()
}

/// Helper function to perform Unix shell-style `${VAR_NAME}` environment variable expansions.
#[allow(clippy::while_let_on_iterator)]
pub fn expand_env_vars(raw_content: &str) -> Result<String> {
    let mut expanded = String::new();
    let mut chars = raw_content.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch == '$' {
            if let Some(&(_, '{')) = chars.peek() {
                chars.next(); // Consume '{'

                let mut var_name = String::new();
                let mut found_close = false;

                while let Some((_, var_ch)) = chars.next() {
                    if var_ch == '}' {
                        found_close = true;
                        break;
                    }
                    var_name.push(var_ch);
                }

                if !found_close {
                    return Err(OxllmError::ConfigLoad(format!(
                        "Unclosed environment variable placeholder starting at index {}",
                        idx
                    )));
                }

                // Strictly resolve environment variable
                let val = std::env::var(&var_name)
                    .map_err(|_| OxllmError::EnvVarMissing(var_name.clone()))?;

                expanded.push_str(&val);
                continue;
            }
        }
        expanded.push(ch);
    }

    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_user_agent_is_optional_and_parsed() {
        let config: Config = toml::from_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 8080
            otel_endpoint = ""

            [[providers]]
            name = "provider-a"
            enabled = true
            base_url = "https://example.com"
            api_key = "key"
            models = ["model-a"]
            user_agent = "pi/0.87.1 (linux; node/v24.20.0; x64)"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.providers[0].user_agent.as_deref(),
            Some("pi/0.87.1 (linux; node/v24.20.0; x64)")
        );
    }

    #[test]
    fn provider_user_agent_defaults_to_none() {
        let config: Config = toml::from_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 8080
            otel_endpoint = ""

            [[providers]]
            name = "provider-a"
            enabled = true
            base_url = "https://example.com"
            api_key = "key"
            models = ["model-a"]
            "#,
        )
        .unwrap();

        assert_eq!(config.providers[0].user_agent, None);
    }

    #[test]
    fn virtual_model_target_defaults_weight_to_one() {
        let target: VirtualModelTarget = toml::from_str(
            r#"provider = "provider-a"
model = "model-a""#,
        )
        .unwrap();

        assert_eq!(target.weight, 1);
    }

    #[test]
    fn virtual_model_target_reads_configured_weight() {
        let target: VirtualModelTarget = toml::from_str(
            r#"provider = "provider-a"
model = "model-a"
weight = 3"#,
        )
        .unwrap();

        assert_eq!(target.weight, 3);
    }

    #[test]
    fn virtual_model_target_rejects_zero_weight() {
        let config: Config = toml::from_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 8080
            otel_endpoint = "http://127.0.0.1:4318"

            [[providers]]
            name = "provider-a"
            enabled = true
            base_url = "https://example.com"
            api_key = "key"
            models = ["model-a"]

            [virtual_models]
            dual = [
                { provider = "provider-a", model = "model-a", weight = 0 },
            ]
            "#,
        )
        .unwrap();

        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("positive weight"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_without_virtual_models_section_parses() {
        let config: Config = toml::from_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 8080
            otel_endpoint = "http://127.0.0.1:4318"

            [[providers]]
            name = "provider-a"
            enabled = true
            base_url = "https://example.com"
            api_key = "key"
            models = ["model-a"]
            "#,
        )
        .unwrap();

        assert!(config.virtual_models.is_empty());
    }

    #[test]
    fn test_expand_env_vars_success() {
        std::env::set_var("TEST_HOST", "127.0.0.1");
        std::env::set_var("TEST_KEY", "groq-key-123");

        let input = r#"
            host = "${TEST_HOST}"
            api_key = "${TEST_KEY}"
            other = "literal $10 string"
        "#;

        let result = expand_env_vars(input).unwrap();
        assert!(result.contains("host = \"127.0.0.1\""));
        assert!(result.contains("api_key = \"groq-key-123\""));
        assert!(result.contains("other = \"literal $10 string\""));
    }

    #[test]
    fn test_expand_env_vars_missing() {
        std::env::remove_var("MISSING_VAR_XYZ");
        let input = r#"api_key = "${MISSING_VAR_XYZ}""#;
        let result = expand_env_vars(input);
        assert!(
            matches!(result, Err(OxllmError::EnvVarMissing(ref name)) if name == "MISSING_VAR_XYZ")
        );
    }

    fn valid_config_with_host(host: &str, bind_family: &str) -> Config {
        toml::from_str(&format!(
            r#"
            [server]
            host = "{host}"
            port = 8080
            otel_endpoint = "http://127.0.0.1:4318"
            bind_family = "{bind_family}"

            [[providers]]
            name = "provider-a"
            enabled = true
            base_url = "https://example.com"
            api_key = "key"
            models = ["model-a"]
            "#
        ))
        .unwrap()
    }

    #[test]
    fn validate_accepts_ipv4_host_and_default_bind_family() {
        let config = valid_config_with_host("100.115.92.30", "ipv4");
        config.validate().unwrap();
    }

    #[test]
    fn validate_rejects_wildcard_host() {
        let config = valid_config_with_host("0.0.0.0", "ipv4");
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("wildcard"), "unexpected: {err}");
    }

    #[test]
    fn validate_rejects_ipv6_host_literal() {
        let config = valid_config_with_host("2001:db8::1", "ipv4");
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("IPv4"), "unexpected: {err}");
    }

    #[test]
    fn validate_rejects_non_ip_host() {
        for host in ["example.com", "localhost", ""] {
            let config = valid_config_with_host(host, "ipv4");
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string().contains("IPv4 address"),
                "host {host}: unexpected: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_ipv6_bind_family_with_migration_hint() {
        let config = valid_config_with_host("127.0.0.1", "ipv6");
        let err = config.validate().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("bind_family"), "unexpected: {message}");
        assert!(message.contains("host"), "unexpected: {message}");
    }

    #[test]
    fn validate_rejects_dual_bind_family_with_migration_hint() {
        let config = valid_config_with_host("127.0.0.1", "dual");
        let err = config.validate().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("bind_family"), "unexpected: {message}");
        assert!(message.contains("host"), "unexpected: {message}");
    }

    #[test]
    fn test_expand_env_vars_unclosed() {
        let input = r#"api_key = "${UNCLOSED"#;
        let result = expand_env_vars(input);
        assert!(matches!(result, Err(OxllmError::ConfigLoad(_))));
    }

    #[test]
    fn unknown_fields_are_rejected_with_field_name_at_every_config_level() {
        let header = r#"[server]
host = "127.0.0.1"
port = 8080
otel_endpoint = "http://127.0.0.1:4318""#;
        let provider = r#"[[providers]]
name = "p"
enabled = true
base_url = "https://example.com"
api_key = "key"
models = ["m"]"#;
        let cases = [
            (format!("{header}\nserver_typo = true\n\n{provider}"), "server_typo"),
            (format!("{header}\n\n{provider}\nprovider_typo = true"), "provider_typo"),
            (
                format!(
                    "{header}\n\n{provider}\n\n[virtual_models]\nvm = [{{ provider = \"p\", model = \"m\", target_typo = true }}]"
                ),
                "target_typo",
            ),
            (format!("{header}\n\n{provider}\n\nroot_typo = true"), "root_typo"),
        ];
        for (raw, field) in cases {
            let error = toml::from_str::<Config>(&raw).expect_err("unknown field must fail");
            assert!(error.to_string().contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn validate_rejects_malformed_enabled_provider_url() {
        let config = valid_config_with_host("127.0.0.1", "ipv4");
        // Replacing after parse isolates validation behavior from TOML syntax.
        let mut config = config;
        config.providers[0].base_url = "file:///etc/passwd".to_string();
        let error = config.validate().expect_err("non-http scheme must fail");
        assert!(error.to_string().contains("http or https"), "{error}");
    }
}

/// Reserved request headers. These are set by the gateway; config cannot override them.
const RESERVED_PROVIDER_HEADERS: [&str; 5] = [
    "authorization",
    "content-type",
    "user-agent",
    "host",
    "content-length",
];

/// Body keys the gateway owns. Config-level `model` and `stream` would subvert routing
/// and streaming semantics, so they are rejected outright.
const RESERVED_EXTRA_BODY_KEYS: [&str; 2] = ["model", "stream"];

fn validate_provider_custom_options(provider: &ProviderConfig) -> Result<()> {
    for (name, value) in &provider.headers {
        let lower = name.to_ascii_lowercase();
        if RESERVED_PROVIDER_HEADERS.contains(&lower.as_str()) {
            return Err(OxllmError::ConfigLoad(format!(
                "Provider '{}' header '{name}' is reserved; the gateway owns it",
                provider.name
            )));
        }
        if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
            return Err(OxllmError::ConfigLoad(format!(
                "Provider '{}' has invalid HTTP header name '{name}'",
                provider.name
            )));
        }
        if http::HeaderValue::from_str(value).is_err() {
            return Err(OxllmError::ConfigLoad(format!(
                "Provider '{}' has invalid value for header '{name}': must be visible ASCII without control characters",
                provider.name
            )));
        }
    }
    for (key, value) in &provider.extra_body {
        if RESERVED_EXTRA_BODY_KEYS.contains(&key.as_str()) {
            return Err(OxllmError::ConfigLoad(format!(
                "Provider '{}' extra_body key '{key}' is reserved for the gateway/client",
                provider.name
            )));
        }
        if value.is_datetime() {
            return Err(OxllmError::ConfigLoad(format!(
                "Provider '{}' extra_body key '{key}' has a TOML datetime value, which has no JSON representation",
                provider.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod provider_custom_tests {
    use super::*;

    fn provider_with_custom() -> &'static str {
        r#"
        [server]
        host = "127.0.0.1"
        port = 8080
        otel_endpoint = ""

        [[providers]]
        name = "provider-a"
        enabled = true
        base_url = "https://example.com"
        api_key = "key"
        models = ["model-a"]
        user_agent = "pi/0.99.1"
        headers = { X-TokenTable-Modalities = "text", x-custom-a = "value a" }
        extra_body = { modalities = ["text"], reasoning = { effort = "low" } }
        "#
    }

    fn minimal_with_body(raw: &str) -> Config {
        let source = format!(
            r#"
            [server]
            host = "127.0.0.1"
            port = 8080
            otel_endpoint = ""

            [[providers]]
            name = "provider-a"
            enabled = true
            base_url = "https://example.com"
            api_key = "key"
            models = ["model-a"]
            {}"#,
            raw
        );
        toml::from_str(&source).expect("config must parse")
    }

    #[test]
    fn provider_custom_headers_and_extra_body_parse() {
        let config: Config = toml::from_str(provider_with_custom()).unwrap();
        config.validate().expect("valid config must validate");

        assert_eq!(
            config.providers[0]
                .headers
                .get("X-TokenTable-Modalities")
                .map(String::as_str),
            Some("text")
        );
        assert_eq!(
            config.providers[0]
                .headers
                .get("x-custom-a")
                .map(String::as_str),
            Some("value a")
        );

        let modalities = config.providers[0]
            .extra_body
            .get("modalities")
            .expect("modalities key present");
        assert_eq!(
            modalities.as_array(),
            Some(&vec![toml::Value::String("text".to_string())])
        );
        assert!(config.providers[0].extra_body.contains_key("reasoning"));
    }

    #[test]
    fn provider_custom_defaults_empty() {
        let config: Config = toml::from_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 8080
            otel_endpoint = ""

            [[providers]]
            name = "provider-a"
            enabled = true
            base_url = "https://example.com"
            api_key = "key"
            models = ["model-a"]
            "#,
        )
        .unwrap();
        config.validate().unwrap();

        assert!(config.providers[0].headers.is_empty());
        assert!(config.providers[0].extra_body.is_empty());
    }

    #[test]
    fn provider_headers_reject_invalid_name() {
        let config = minimal_with_body(r#"headers = { "Bad Header" = "v" }"#);
        let err = config.validate().unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("provider-a"),
            "diagnostic names provider: {msg}"
        );
        assert!(msg.contains("Bad Header"), "diagnostic names key: {msg}");
    }

    #[test]
    fn provider_headers_reject_invalid_value() {
        let config = minimal_with_body("headers = { X-Test = \"bad\\u000Avalue\" }");
        let err = config.validate().unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("X-Test"), "diagnostic names key: {msg}");
    }

    #[test]
    fn provider_headers_reject_reserved_override() {
        for reserved in [
            "Authorization",
            "content-type",
            "USER-AGENT",
            "Host",
            "content-length",
        ] {
            let config = minimal_with_body(&format!("headers = {{ {reserved} = \"v\" }}"));
            let err = config.validate().unwrap_err();
            let msg = format!("{err:?}");
            assert!(msg.contains("reserved"), "({reserved}) msg: {msg}");
        }
    }

    #[test]
    fn provider_extra_body_rejects_model_and_stream() {
        for protected in ["model", "stream"] {
            let config = minimal_with_body(&format!(r#"extra_body = {{ {protected} = "x" }}"#));
            let err = config.validate().unwrap_err();
            let msg = format!("{err:?}");
            assert!(
                msg.contains(protected),
                "({protected}) diagnostic names key: {msg}"
            );
        }
    }

    #[test]
    fn provider_extra_body_rejects_toml_datetime() {
        let config = minimal_with_body("extra_body = { when = 2026-10-01T00:00:00Z }");
        let err = config.validate().unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("when"), "diagnostic names key: {msg}");
    }
}
