//! Bounded delivery of complete global render plans, before any host writes.
use super::GlobalRenderPlanV1;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
const PAGE_BYTES: usize = 16 * 1024;
const MAX_PLAN_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalRenderPage {
    pub kind: String,
    pub plan_sha256: String,
    pub offset: usize,
    pub total_bytes: usize,
    pub content: String,
    pub next_offset: Option<usize>,
}

impl GlobalRenderPlanV1 {
    pub fn page(&self, offset: usize, expected: Option<&str>) -> Result<GlobalRenderPage> {
        self.validate()?;
        let text = serde_json::to_string(self)?;
        ensure!(
            text.len() <= MAX_PLAN_BYTES,
            "global render plan exceeds transport bound"
        );
        let sha = format!("{:x}", Sha256::digest(text.as_bytes()));
        if (offset > 0 && expected.is_none()) || expected.is_some_and(|e| e != sha) {
            bail!("error.global_render_plan_stale: restart global render plan from page one");
        }
        ensure!(
            offset < text.len() && text.is_char_boundary(offset),
            "invalid global render offset"
        );
        let mut end = offset.saturating_add(PAGE_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Ok(GlobalRenderPage {
            kind: "bbox.global_render_page.v2".into(),
            plan_sha256: sha,
            offset,
            total_bytes: text.len(),
            content: text[offset..end].into(),
            next_offset: (end < text.len()).then_some(end),
        })
    }
}

#[derive(Default)]
pub struct GlobalRenderAssembler {
    text: String,
    sha: Option<String>,
    total: usize,
    complete: bool,
}
impl GlobalRenderAssembler {
    pub fn push(&mut self, page: GlobalRenderPage) -> Result<Option<GlobalRenderPlanV1>> {
        ensure!(!self.complete, "global render plan already assembled");
        ensure!(
            page.kind == "bbox.global_render_page.v2",
            "unsupported global render page"
        );
        ensure!(
            page.offset == self.text.len()
                && !page.content.is_empty()
                && page.content.len() <= PAGE_BYTES,
            "invalid global render page offset or length"
        );
        ensure!(
            page.total_bytes <= MAX_PLAN_BYTES,
            "global render plan exceeds transport bound"
        );
        if let Some(sha) = &self.sha {
            ensure!(
                sha == &page.plan_sha256 && self.total == page.total_bytes,
                "global render page generation changed"
            );
        } else {
            self.sha = Some(page.plan_sha256.clone());
            self.total = page.total_bytes;
        }
        let end = self.text.len() + page.content.len();
        ensure!(
            end <= self.total,
            "global render page exceeds declared length"
        );
        ensure!(
            page.next_offset == (end < self.total).then_some(end),
            "invalid global render continuation"
        );
        self.text.push_str(&page.content);
        if page.next_offset.is_some() {
            return Ok(None);
        }
        ensure!(
            format!("{:x}", Sha256::digest(self.text.as_bytes())) == page.plan_sha256,
            "global render payload checksum mismatch"
        );
        let plan: GlobalRenderPlanV1 = serde_json::from_str(&self.text)?;
        plan.validate()?;
        self.complete = true;
        Ok(Some(plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paged_plan_round_trips_unicode_and_refuses_stale_pages() {
        let plan = GlobalRenderPlanV1::new(
            std::path::Path::new("/fixture/BLACKBOX.md"),
            "語\n".repeat(40_000),
            vec![],
        );
        let mut assembler = GlobalRenderAssembler::default();
        let mut offset = 0;
        let mut sha = None;
        loop {
            let page = plan.page(offset, sha.as_deref()).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() < 40_000);
            let next = page.next_offset;
            sha = Some(page.plan_sha256.clone());
            if let Some(result) = assembler.push(page).unwrap() {
                assert_eq!(result, plan);
                break;
            }
            offset = next.unwrap();
        }
        assert!(plan.page(1, Some("wrong")).is_err());
        assert!(plan.page(1, None).is_err());
    }
}
