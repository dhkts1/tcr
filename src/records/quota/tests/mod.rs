use super::*;
use std::collections::HashMap;
use time::Duration;

/// A test header bag with case-insensitive lookup.
struct TestHeaders(HashMap<String, String>);

impl TestHeaders {
    fn new(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| (k.to_lowercase(), v.to_string()))
                .collect(),
        )
    }

    /// Add one header to a bag a shared fixture built, for the test that
    /// differs from that fixture by a single line.
    fn push(&mut self, name: &str, value: &str) {
        self.0.insert(name.to_lowercase(), value.to_string());
    }
}

impl HeaderView for TestHeaders {
    fn get_str(&self, name: &str) -> Option<&str> {
        self.0.get(&name.to_lowercase()).map(String::as_str)
    }
}

mod anthropic;
mod codex;
mod owner;
