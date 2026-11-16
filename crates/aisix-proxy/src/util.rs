//! Small shared helpers for the proxy layer.

/// Borrow a request object when its only outbound rewrite is `model`.
/// Earlier effort/billing/thinking rewrites are gated by the caller's `Cow`;
/// signatures and provider body overrides still need the owned rewrite path.
pub(crate) fn model_only_fields<'a>(
    body: &'a serde_json::Value,
    pk: &aisix_core::ProviderKey,
    signature_field: Option<&str>,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    let fields = body.as_object()?;
    if pk.request.as_ref().is_some_and(|r| r.rewrites_body()) {
        return None;
    }
    let signatures = signature_field
        .and_then(|field| fields.get(field))
        .is_some_and(aisix_gateway::tool_calls::body_has_thought_signatures);
    (!signatures).then_some(fields)
}

/// The same bytes as cloning an object, replacing an existing `model`,
/// and serializing it, without the intermediate owned object.
pub(crate) struct ModelRenamed<'a> {
    pub fields: &'a serde_json::Map<String, serde_json::Value>,
    pub model: &'a str,
}

pub(crate) struct AnthropicModelRenamed<'a>(ModelRenamed<'a>);

impl<'a> ModelRenamed<'a> {
    pub(crate) fn with_default_cache_ttl(self) -> AnthropicModelRenamed<'a> {
        AnthropicModelRenamed(self)
    }

    fn serialize_fields<S: serde::Serializer>(
        &self,
        serializer: S,
        default_cache_ttl: bool,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for (key, value) in self.fields {
            if key == "model" {
                map.serialize_entry(key, self.model)?;
            } else if default_cache_ttl {
                map.serialize_entry(
                    key,
                    &aisix_provider_anthropic::wire::DefaultCacheTtl::field(key, value),
                )?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl serde::Serialize for ModelRenamed<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.serialize_fields(serializer, false)
    }
}

impl serde::Serialize for AnthropicModelRenamed<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize_fields(serializer, true)
    }
}

/// Wire bytes of a request body a proxy route sends upstream itself,
/// bypassing the provider bridges. Build the outbound `Value` inside a block
/// that ends here, so only these bytes — which reqwest holds until the
/// response head — outlive it; see [`aisix_gateway::json_body`]. A reader
/// that needs the outbound form after the upstream answered uses
/// [`outbound_text`] / [`outbound_value`] instead of a kept copy.
pub(crate) fn outbound_json<T: serde::Serialize + ?Sized>(
    body: &T,
) -> Result<bytes::Bytes, crate::error::ProxyError> {
    aisix_gateway::json_body(body).map_err(|e| {
        crate::error::ProxyError::Bridge(aisix_gateway::BridgeError::Config(format!(
            "serialize request body: {e}"
        )))
    })
}

/// The outbound request as text, for content capture: the wire bytes are
/// exactly what `serde_json::to_string` of the body would produce.
pub(crate) fn outbound_text(outbound: &[u8]) -> String {
    String::from_utf8_lossy(outbound).into_owned()
}

/// The outbound request as a `Value`, rebuilt from its wire bytes for a
/// reader that runs after the upstream has answered.
pub(crate) fn outbound_value(outbound: &[u8]) -> serde_json::Value {
    serde_json::from_slice(outbound).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_response_reference_test_alloc::{AllocationScope, Allocations};
    use std::time::{Duration, Instant};

    fn measured(operation: impl FnOnce() -> bytes::Bytes) -> (bytes::Bytes, Allocations, Duration) {
        let scope = AllocationScope::start();
        let start = Instant::now();
        let output = operation();
        let elapsed = start.elapsed();
        (output, scope.finish(), elapsed)
    }

    #[test]
    fn cache_ttl_borrowed_serialization_never_adds_a_body_sized_allocation() {
        for size in [127 * 1024, 25 * 1024 * 1024] {
            for marked in [false, true] {
                let mut body = serde_json::json!({
                    "model": "alias", "max_tokens": 16,
                    "messages": [{"role": "user", "content": [{"type": "text", "text": "x".repeat(size)}]}],
                    "tools": [{"name": "t", "input_schema": {"properties": {"cache_control": {"type": "ephemeral"}}}}]
                });
                if marked {
                    body["messages"][0]["content"][0]["cache_control"] =
                        serde_json::json!({"type": "ephemeral"});
                }
                let mut normalized = body.clone();
                aisix_provider_anthropic::wire::default_cache_ttl(&mut normalized);
                let expected = serde_json::to_value(ModelRenamed {
                    fields: normalized.as_object().unwrap(),
                    model: "claude-upstream",
                })
                .unwrap();
                for sample in 0..3 {
                    let (output, borrowed, elapsed) = measured(|| {
                        outbound_json(
                            &ModelRenamed {
                                fields: body.as_object().unwrap(),
                                model: "claude-upstream",
                            }
                            .with_default_cache_ttl(),
                        )
                        .unwrap()
                    });
                    let (baseline_output, baseline, baseline_elapsed) = measured(|| {
                        outbound_json(&ModelRenamed {
                            fields: normalized.as_object().unwrap(),
                            model: "claude-upstream",
                        })
                        .unwrap()
                    });
                    let (_, clone_baseline, clone_elapsed) = measured(|| {
                        if marked {
                            let mut owned = body.clone();
                            aisix_provider_anthropic::wire::default_cache_ttl(&mut owned);
                            outbound_json(&ModelRenamed {
                                fields: owned.as_object().unwrap(),
                                model: "claude-upstream",
                            })
                            .unwrap()
                        } else {
                            outbound_json(&ModelRenamed {
                                fields: body.as_object().unwrap(),
                                model: "claude-upstream",
                            })
                            .unwrap()
                        }
                    });
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
                        expected
                    );
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&baseline_output).unwrap(),
                        expected
                    );
                    assert!(borrowed.bytes <= baseline.bytes + 8192,
                        "size={size}, marked={marked}: borrowed={borrowed:?}, baseline={baseline:?}");
                    if marked {
                        assert!(
                            clone_baseline.bytes >= borrowed.bytes + size,
                            "size={size}: clone={clone_baseline:?}, borrowed={borrowed:?}"
                        );
                        assert!(body["messages"][0]["content"][0]["cache_control"]
                            .get("ttl")
                            .is_none());
                    }
                    eprintln!("cache_ttl_perf {{\"payload_bytes\":{size},\"marked\":{marked},\"sample\":{sample},\"borrowed_alloc_bytes\":{},\"borrowed_alloc_calls\":{},\"baseline_alloc_bytes\":{},\"clone_alloc_bytes\":{},\"borrowed_ns\":{},\"baseline_ns\":{},\"clone_ns\":{}}}",
                        borrowed.bytes, borrowed.calls, baseline.bytes, clone_baseline.bytes,
                        elapsed.as_nanos(), baseline_elapsed.as_nanos(), clone_elapsed.as_nanos());
                }
            }
        }
    }
}
