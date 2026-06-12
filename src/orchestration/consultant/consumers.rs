//! Code-owned registry of consultant consumers.
//!
//! Descriptors are constants compiled into the daemon (recursion-guard
//! invariant: a consumer selects code-owned vocabulary and handlers, it can
//! never define them in data), so the registry is a static lookup, not a
//! store. The reference into `orchestration::badgey` is a same-crate module
//! edge; it inverts the consultant←badgey direction only here, at the
//! catalog leaf, so the runtime modules stay consumer-agnostic.

use super::descriptor::ConsumerDescriptor;

pub fn lookup(name: &str) -> Option<&'static ConsumerDescriptor> {
    match name {
        "badgey" => Some(&crate::orchestration::badgey::vocabulary::BADGEY),
        _ => None,
    }
}

pub fn names() -> &'static [&'static str] {
    &["badgey"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_resolves_every_listed_consumer() {
        for name in names() {
            let descriptor = lookup(name).expect("listed consumer must resolve");
            assert_eq!(descriptor.name, *name);
        }
        assert!(lookup("nonexistent").is_none());
    }
}
