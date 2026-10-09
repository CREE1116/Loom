//! Registered OpenRouter accounts use the account-filtered catalog, never the
//! public list. Pricing fields are USD/unit; prompt/completion are USD/token.
//! https://openrouter.ai/docs/api/api-reference/models/list-models-filtered-by-user-provider-preferences-privacy-settings-and-guardrails
use super::{Account, Candidate, CostClass, Endpoint};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeSet;

pub(super) fn openrouter(account: &Account, endpoint: &Endpoint) -> Result<Vec<Candidate>> {
    ensure!(
        endpoint
            .api_key
            .as_deref()
            .is_some_and(|key| !key.is_empty()),
        "OpenRouter account catalog requires an API key"
    );
    // No pagination arguments: the documented endpoint returns the complete list.
    // Keep the registered base URL, including regional/custom path and hostname.
    // Reject redirects as well as errors: a redirect to /models must not silently
    // replace the account-filtered list with a public catalog.
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let mut request = agent.get(&super::super::url(&endpoint.base_url, "models/user"));
    for (name, value) in super::super::auth_headers(endpoint) {
        request = request.set(&name, &value);
    }
    let response = request.call().map_err(|error| match error {
        ureq::Error::Status(status, _) => {
            anyhow::anyhow!("OpenRouter account catalog failed: HTTP {status}")
        }
        _ => anyhow::anyhow!("OpenRouter account catalog failed: service unavailable"),
    })?;
    ensure!(
        response.status() == 200,
        "OpenRouter account catalog failed: unexpected status"
    );
    let value = response
        .into_json::<Value>()
        .map_err(|_| anyhow::anyhow!("invalid OpenRouter account model catalog"))?;
    let models = parse(&account.id, &value)?;
    if account.models.is_empty() {
        return Ok(models);
    }
    // Explicit configuration limits choices; it does not bypass account guardrails.
    // Retain its existing order while borrowing all metadata from this account's list.
    let mut seen = BTreeSet::new();
    Ok(account
        .models
        .iter()
        .filter(|id| seen.insert(id.as_str()))
        .filter_map(|id| models.iter().find(|model| model.model == *id).cloned())
        .collect())
}

fn parse(endpoint: &str, value: &Value) -> Result<Vec<Candidate>> {
    let data = value["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid OpenRouter account model catalog"))?;
    let mut seen = BTreeSet::new();
    Ok(data
        .iter()
        .filter_map(|model| {
            let id = model["id"]
                .as_str()
                .filter(|id| !id.trim().is_empty() && !id.chars().any(char::is_control))?;
            if !text_model(model, id) || !seen.insert(id) {
                return None;
            }
            let (price, cost) = pricing(&model["pricing"]);
            Some(Candidate {
                endpoint: endpoint.into(),
                model: id.into(),
                tier: super::super::discover::guess_tier(id),
                cost,
                price,
                tools: model["supported_parameters"]
                    .as_array()
                    .is_some_and(|parameters| {
                        parameters.iter().any(|parameter| parameter == "tools")
                    }),
                context: model["context_length"]
                    .as_u64()
                    .filter(|length| *length > 0),
            })
        })
        .collect())
}

fn text_model(model: &Value, id: &str) -> bool {
    match model["architecture"]["output_modalities"].as_array() {
        Some(outputs) => outputs.iter().any(|modality| modality == "text"),
        // The account endpoint defaults to text output. Preserve old ID filtering
        // only for older/catalog-compatible records lacking explicit modalities.
        None => super::super::is_chat_model(id),
    }
}

fn amount(value: &Value) -> Option<f64> {
    let number = match value {
        Value::String(raw) => {
            let raw = raw.trim();
            let parsed = raw.parse::<f64>().ok()?;
            // A positive decimal underflow must never become a fabricated zero.
            if parsed == 0.0
                && raw
                    .split(['e', 'E'])
                    .next()?
                    .bytes()
                    .any(|c| matches!(c, b'1'..=b'9'))
            {
                return None;
            }
            parsed
        }
        Value::Number(number) => number.as_f64()?,
        _ => return None,
    };
    (number.is_finite() && !number.is_sign_negative()).then_some(number)
}
fn million(value: &Value) -> Option<f64> {
    let price = amount(value)? * 1_000_000.0;
    price.is_finite().then_some(price)
}
fn pricing(value: &Value) -> (Option<(f64, f64)>, CostClass) {
    let Some(fields) = value.as_object() else {
        return (None, CostClass::Unknown);
    };
    let Some(input) = million(&value["prompt"]) else {
        return (None, CostClass::Unknown);
    };
    let Some(output) = million(&value["completion"]) else {
        return (None, CostClass::Unknown);
    };
    let price = Some((input, output));
    // Every advertised charge participates, including future pricing fields.
    // Missing/invalid extra amounts cannot establish that a model is free.
    let mut nonzero = input > 0.0 || output > 0.0;
    for extra in fields.values() {
        let Some(extra) = amount(extra) else {
            return (price, CostClass::Unknown);
        };
        nonzero |= extra > 0.0;
    }
    (
        price,
        if nonzero {
            CostClass::Paid
        } else {
            CostClass::Free
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_preserves_real_prices_context_and_advertised_tools() {
        let models = parse("personal", &json!({"data":[
            {"id":"vendor/paid","pricing":{"prompt":"0.000003","completion":"0.000015"},"supported_parameters":["tools","temperature"],"context_length":128000,"architecture":{"output_modalities":["text"]}},
            {"id":"vendor/chat","pricing":{"prompt":0,"completion":0},"supported_parameters":["temperature"],"context_length":8192,"architecture":{"output_modalities":["text"]}},
            {"id":"vendor/image","pricing":{"prompt":"0","completion":"0"},"architecture":{"output_modalities":["image"]}},
            {"id":"vendor/unknown"}
        ]})).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].endpoint, "personal");
        assert_eq!(models[0].price, Some((3.0, 15.0)));
        assert_eq!(models[0].cost, CostClass::Paid);
        assert!(models[0].tools);
        assert_eq!(models[0].context, Some(128000));
        assert_eq!(models[1].cost, CostClass::Free);
        assert!(!models[1].tools);
        assert_eq!(models[2].price, None);
        assert_eq!(models[2].cost, CostClass::Unknown);
    }
    #[test]
    fn missing_invalid_negative_nonfinite_and_overflow_prices_are_unknown() {
        for prompt in [
            Value::Null,
            json!(""),
            json!("NaN"),
            json!("inf"),
            json!("-0.1"),
            json!(-1),
            json!("1e308"),
            json!("1e-999"),
            json!(false),
        ] {
            assert_eq!(
                pricing(&json!({"prompt":prompt,"completion":"0"})),
                (None, CostClass::Unknown)
            );
        }
        assert_eq!(pricing(&json!({"prompt":"0"})), (None, CostClass::Unknown));
        assert_eq!(
            pricing(&json!({"completion":"0"})),
            (None, CostClass::Unknown)
        );
        assert_eq!(pricing(&Value::Null), (None, CostClass::Unknown));
    }
    #[test]
    fn free_requires_all_advertised_charges_zero() {
        for name in [
            "request",
            "image",
            "web_search",
            "internal_reasoning",
            "input_cache_read",
            "audio",
            "new_charge",
        ] {
            let mut value = json!({"prompt":"0","completion":"0"});
            value[name] = json!("0.005");
            assert_eq!(pricing(&value), (Some((0.0, 0.0)), CostClass::Paid));
            value[name] = json!("0");
            assert_eq!(pricing(&value), (Some((0.0, 0.0)), CostClass::Free));
            value[name] = json!("unknown");
            assert_eq!(pricing(&value), (Some((0.0, 0.0)), CostClass::Unknown));
        }
        assert_eq!(
            pricing(&json!({"prompt":"0","completion":"0.000001"})),
            (Some((0.0, 1.0)), CostClass::Paid)
        );
        assert_eq!(
            pricing(&json!({"prompt":"0.000001","completion":"0"})),
            (Some((1.0, 0.0)), CostClass::Paid)
        );
    }
    #[test]
    fn malformed_catalog_is_an_error_and_empty_valid_catalog_has_no_fallback() {
        assert!(parse("work", &json!({"error":{"message":"denied"}})).is_err());
        assert!(parse("work", &json!({"data":{}})).is_err());
        assert!(parse("work", &json!({"data":[]})).unwrap().is_empty());
    }
}
