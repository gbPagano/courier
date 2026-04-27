//! W3C trace-context propagation over `Envelope::meta.headers`.

use std::collections::HashMap;

use opentelemetry::Context;
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::propagation::TraceContextPropagator;

pub const TRACEPARENT: &str = "traceparent";
pub const TRACESTATE: &str = "tracestate";

pub fn extract(headers: &HashMap<String, String>) -> Option<Context> {
    let propagator = TraceContextPropagator::new();
    let context = propagator.extract(&HeaderExtractor(headers));
    context.span().span_context().is_valid().then_some(context)
}

pub fn inject(headers: &mut HashMap<String, String>, context: &Context) {
    let propagator = TraceContextPropagator::new();
    propagator.inject_context(context, &mut HeaderInjector(headers));
}

struct HeaderExtractor<'a>(&'a HashMap<String, String>);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

struct HeaderInjector<'a>(&'a mut HashMap<String, String>);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_valid_traceparent() {
        let mut headers = HashMap::new();
        headers.insert(
            TRACEPARENT.to_string(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
        );

        let context = extract(&headers).expect("traceparent should parse");
        assert!(context.span().span_context().is_valid());
    }
}
