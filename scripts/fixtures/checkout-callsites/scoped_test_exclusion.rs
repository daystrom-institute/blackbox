// Fixture for `scripts/acceptance-checkout-callsites.sh --self-test`.
//
// Every acquisition line ends with `// expect: <fn>` when the scan must count
// it under that enclosing fn, or `// expect: excluded` when it sits inside
// test-gated code. The self-test fails unless the scan agrees line for line.
// This file is never compiled.

#[cfg(test)]
use crate::fixtures::TestOnlyHelper;

#[cfg(test)]
static TEST_SWITCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static TEST_TABLE: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();

#[cfg(test)]
const TEST_BRACE: &str = "{ unbalanced";

pub(crate) fn after_gated_use_and_statics(project_id: &str) {
    let lease = acquire_selected_project_access(broker, project_id, kind, intent); // expect: after_gated_use_and_statics
    drop(lease);
}

#[cfg(test)]
fn gated_helper(project_id: &str) {
    let text = "}";
    let raw = r#"} } {"#;
    let brace = '}';
    let lease = acquire_selected_project_access(broker, project_id, kind, intent); // expect: excluded
    drop((text, raw, brace, lease));
}

fn after_gated_fn(project_id: &str) -> anyhow::Result<()> {
    with_selected_project_access(broker, project_id, kind, intent, |lease| Ok(())) // expect: after_gated_fn
}

#[cfg(test)]
#[derive(Debug)]
struct GatedWithAttributes {
    lease: Lease,
}

pub(crate) struct Service;

impl Service {
    #[cfg(test)]
    fn gated_method(&self) {
        let _ = acquire_catalog_project_lease(self, broker, project_id, kind, intent); // expect: excluded
    }

    pub(crate) fn sibling_method(&self) {
        let _ = acquire_catalog_project_lease(self, broker, project_id, kind, intent); // expect: sibling_method
    }
}

fn statement_level_gate(project_id: &str) {
    #[cfg(test)]
    let probe = acquire_project_mutation_lease(broker, project_id); // expect: excluded
    let lease = acquire_project_mutation_lease(broker, project_id); // expect: statement_level_gate
    match lease {
        #[cfg(test)]
        Lease::Probe => acquire_selected_project_access(broker, project_id, kind, intent), // expect: excluded
        Lease::Real => acquire_selected_project_access(broker, project_id, kind, intent), // expect: statement_level_gate
    };
}

#[cfg(test)]
mod out_of_line_tests;

fn after_out_of_line_mod(project_id: &str) {
    let _ = broker.acquire(CheckoutAccessRequest { project_id }); // expect: after_out_of_line_mod
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested() {
        if true {
            let _ = with_discovery(project_id, |scope| { scope }); // expect: excluded
        }
    }

    #[test]
    fn inside_gated_mod() {
        let _ = broker.acquire(CheckoutAccessRequest { project_id }); // expect: excluded
    }
}

fn literal_holds_attribute_text(project_id: &str) {
    let plain = "an escaped \" #[cfg(test)] ";
    let c_plain = c"an escaped \" #[cfg(test)] ";
    let raw = r#"a quote " #[cfg(test)] "#;
    let raw_bytes = br#"a quote " #[cfg(test)] "#;
    // Last literal in the file: a misread here would run to EOF.
    let raw_c = cr#"a quote " #[cfg(test)] "#;
    let _ = acquire_selected_project_access(broker, project_id, kind, intent); // expect: literal_holds_attribute_text
}

#[cfg(test)]
fn gated_after_literals(project_id: &str) {
    let _ = acquire_selected_project_access(broker, project_id, kind, intent); // expect: excluded
}

pub(crate) fn after_gated_mod(checkout: &Checkout) {
    let _ = with_resolved_checkout_access(broker, checkout, kind, intent, |lease| ()); // expect: after_gated_mod
}
