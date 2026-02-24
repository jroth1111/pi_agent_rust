//! JSON schema generation and validation for Pi settings.
//!
//! Uses `schemars` to generate JSON Schema from the Config struct,
//! providing both schema export and validation capabilities.

use schemars::schema_for;
use serde_json::Value;

use super::Config;

/// Generate the full JSON schema for the Config struct.
///
/// This schema can be used for:
/// - IDE autocomplete/validation in settings.json
/// - Pre-flight validation of config files
/// - Documentation generation
pub fn config_schema() -> Value {
    serde_json::to_value(schema_for!(Config))
        .unwrap_or_else(|_| serde_json::json!({ "type": "object" }))
}

/// Generate a minimal schema scaffold for quick validation.
///
/// This is a lightweight alternative to the full schema,
/// useful for embedded contexts or quick checks.
pub fn minimal_config_schema() -> Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://pi.agent/schemas/settings.min.json",
        "title": "Pi Agent Settings (Minimal)",
        "type": "object",
        "properties": {
            "defaultProvider": { "type": "string" },
            "defaultModel": { "type": "string" },
            "shellPath": { "type": "string" },
            "shellCommandPrefix": { "type": "string" },
            "extensions": { "type": "array", "items": { "type": "string" } },
            "skills": { "type": "array", "items": { "type": "string" } },
            "extensionPolicy": { "type": "object" }
        },
        "additionalProperties": true
    })
}

/// Validation error with location and message.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// JSON pointer to the invalid field (e.g., "/extensionPolicy/profile")
    pub path: String,
    /// Human-readable error message
    pub message: String,
    /// Line number in the source file (if available)
    pub line: Option<usize>,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(line) = self.line {
            write!(f, "Error at {} (line {line}): {}", self.path, self.message)
        } else {
            write!(f, "Error at {}: {}", self.path, self.message)
        }
    }
}

/// Validate a JSON value against the Config schema.
///
/// Returns a list of validation errors, or an empty vector if valid.
pub fn validate_config(value: &Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Basic type check
    if !value.is_object() {
        errors.push(ValidationError {
            path: "/".to_string(),
            message: "Config must be a JSON object".to_string(),
            line: None,
        });
        return errors;
    }

    let obj = value.as_object().unwrap();

    // Validate known fields with specific rules
    validate_provider(obj, &mut errors);
    validate_extension_policy(obj, &mut errors);
    validate_repair_policy(obj, &mut errors);
    validate_thinking_budgets(obj, &mut errors);
    validate_numeric_fields(obj, &mut errors);

    errors
}

fn validate_provider(obj: &serde_json::Map<String, Value>, errors: &mut Vec<ValidationError>) {
    if let Some(provider) = obj.get("defaultProvider").and_then(|v| v.as_str()) {
        let valid_providers = [
            "anthropic",
            "openai",
            "gemini",
            "azure",
            "openrouter",
            "bedrock",
            "vertex",
            "cerebras",
            "deepseek",
            "mistral",
        ];
        if !valid_providers.contains(&provider) {
            errors.push(ValidationError {
                path: "/defaultProvider".to_string(),
                message: format!(
                    "Provider '{provider}' is not recognized. Valid providers: {}",
                    valid_providers.join(", ")
                ),
                line: None,
            });
        }
    }
}

fn validate_extension_policy(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(policy) = obj.get("extensionPolicy").and_then(|v| v.as_object()) {
        if let Some(profile) = policy.get("profile").and_then(|v| v.as_str()) {
            let valid_profiles = ["safe", "balanced", "permissive", "standard"];
            if !valid_profiles.contains(&profile) {
                errors.push(ValidationError {
                    path: "/extensionPolicy/profile".to_string(),
                    message: format!(
                        "Profile '{profile}' is not valid. Must be one of: {}",
                        valid_profiles.join(", ")
                    ),
                    line: None,
                });
            }
        }
    }
}

fn validate_repair_policy(obj: &serde_json::Map<String, Value>, errors: &mut Vec<ValidationError>) {
    if let Some(policy) = obj.get("repairPolicy").and_then(|v| v.as_object()) {
        if let Some(mode) = policy.get("mode").and_then(|v| v.as_str()) {
            let valid_modes = ["off", "suggest", "auto-safe", "auto-strict"];
            if !valid_modes.contains(&mode) {
                errors.push(ValidationError {
                    path: "/repairPolicy/mode".to_string(),
                    message: format!(
                        "Mode '{mode}' is not valid. Must be one of: {}",
                        valid_modes.join(", ")
                    ),
                    line: None,
                });
            }
        }
    }
}

fn validate_thinking_budgets(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(budgets) = obj.get("thinkingBudgets").and_then(|v| v.as_object()) {
        for (level, value) in budgets {
            if let Some(num) = value.as_u64() {
                if num > 100_000 {
                    errors.push(ValidationError {
                        path: format!("/thinkingBudgets/{level}"),
                        message: format!(
                            "Thinking budget {level} is unusually high ({num}). Typical values are 1024-32768."
                        ),
                        line: None,
                    });
                }
            } else if !value.is_null() {
                errors.push(ValidationError {
                    path: format!("/thinkingBudgets/{level}"),
                    message: "Thinking budget must be a positive integer".to_string(),
                    line: None,
                });
            }
        }
    }
}

fn validate_numeric_fields(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<ValidationError>,
) {
    // Check numeric fields have valid ranges
    let numeric_fields = [
        ("editorPaddingX", 0, 100),
        ("autocompleteMaxVisible", 1, 50),
        ("sessionPickerInput", 1, 1000),
    ];

    for (field, min, max) in numeric_fields {
        if let Some(value) = obj.get(field).and_then(serde_json::Value::as_u64) {
            if value < min || value > max {
                errors.push(ValidationError {
                    path: format!("/{field}"),
                    message: format!("{field} must be between {min} and {max}, got {value}"),
                    line: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_config_schema_generates() {
        let schema = config_schema();
        assert!(schema.is_object());
        assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
    }

    #[test]
    fn test_minimal_schema_has_required_fields() {
        let schema = minimal_config_schema();
        assert!(schema["properties"]["defaultProvider"].is_object());
        assert!(schema["properties"]["extensions"].is_object());
        assert!(schema["properties"]["extensionPolicy"].is_object());
    }

    #[test]
    fn test_validate_valid_config() {
        let config = json!({
            "defaultProvider": "anthropic",
            "defaultModel": "claude-sonnet-4-6",
            "extensionPolicy": {
                "profile": "balanced"
            }
        });
        let errors = validate_config(&config);
        assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");
    }

    #[test]
    fn test_validate_invalid_provider() {
        let config = json!({
            "defaultProvider": "invalid-provider"
        });
        let errors = validate_config(&config);
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("not recognized"));
    }

    #[test]
    fn test_validate_invalid_policy_profile() {
        let config = json!({
            "extensionPolicy": {
                "profile": "ultra-secure"
            }
        });
        let errors = validate_config(&config);
        assert!(!errors.is_empty());
        assert!(errors[0].path == "/extensionPolicy/profile");
    }

    #[test]
    fn test_validate_numeric_out_of_range() {
        let config = json!({
            "autocompleteMaxVisible": 500
        });
        let errors = validate_config(&config);
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("must be between"));
    }

    #[test]
    fn test_validate_thinking_budget_too_high() {
        let config = json!({
            "thinkingBudgets": {
                "high": 500_000
            }
        });
        let errors = validate_config(&config);
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("unusually high"));
    }

    #[test]
    fn test_validation_error_display() {
        let error = ValidationError {
            path: "/test/field".to_string(),
            message: "Invalid value".to_string(),
            line: Some(15),
        };
        let display = format!("{error}");
        assert!(display.contains("line 15"));
        assert!(display.contains("/test/field"));
    }
}
