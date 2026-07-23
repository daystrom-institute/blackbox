use bbox_corpus_core::built_from::{BuiltFromStamp, BuiltFromTable};

pub(crate) fn append_built_from_section(mut output: String, table: &BuiltFromTable) -> String {
    if table.is_empty() {
        return output;
    }
    output.push_str("\n\nbuilt_from:\n");
    for (id, stamp) in table.iter() {
        output.push_str("- ");
        output.push_str(id);
        match stamp {
            BuiltFromStamp::Published {
                published_scope,
                published_ref,
                publisher_commit,
            } => {
                output.push_str(" published scope=");
                output.push_str(&format_scope(published_scope));
                output.push_str(" ref=");
                output.push_str(published_ref);
                output.push_str(" commit=");
                output.push_str(publisher_commit);
            }
            BuiltFromStamp::CheckoutOverlay {
                published_scope,
                checkout_id,
                publisher_commit,
                checkout_head,
                merge_base,
                working_fingerprint,
            } => {
                output.push_str(" checkout_overlay scope=");
                output.push_str(&format_scope(published_scope));
                output.push_str(" checkout=");
                output.push_str(checkout_id);
                output.push_str(" publisher_commit=");
                output.push_str(publisher_commit);
                output.push_str(" checkout_head=");
                output.push_str(checkout_head);
                output.push_str(" merge_base=");
                output.push_str(merge_base);
                output.push_str(" working_fingerprint=");
                output.push_str(working_fingerprint);
            }
        }
        output.push('\n');
    }
    output
}

fn format_scope(scope: &bbox_corpus_core::identity::PublishedScope) -> String {
    format!("{}:{}", scope.repo_id(), scope.bbox_root_relpath())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::identity::PublishedScope;

    #[test]
    fn text_table_emits_each_interned_stamp_once() {
        let mut table = BuiltFromTable::default();
        let stamp = BuiltFromStamp::Published {
            published_scope: PublishedScope::try_new("repo", ".").unwrap(),
            published_ref: "refs/heads/main".into(),
            publisher_commit: "abc123".into(),
        };
        let id = table.intern(stamp.clone());
        assert_eq!(table.intern(stamp), id);

        let rendered = append_built_from_section("one row built_from=built_from_0".into(), &table);

        assert_eq!(rendered.matches("- built_from_0 published").count(), 1);
        assert!(rendered.contains("ref=refs/heads/main commit=abc123"));
    }
}
