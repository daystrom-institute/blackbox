use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, next_hops, schema,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct WhiteboardProvider;

impl InspectableEntityProvider for WhiteboardProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Whiteboard
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Whiteboard { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Whiteboard { board_id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("board_id".into(), board_id.clone());
        if let Some(stores) = ctx.stores() {
            let board = stores
                .whiteboards
                .get(board_id)
                .ok_or_else(|| anyhow::anyhow!("whiteboard entity {board_id} not found"))?;
            let board = board.read();
            properties.insert("topic".into(), board.topic.clone());
            properties.insert("project".into(), board.project.clone());
            properties.insert("phase".into(), board.phase.as_str().into());
            properties.insert("status".into(), "historical".into());
            properties.insert("body".into(), public_history(&board)?.to_string());
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["board_id", "topic", "project", "phase", "status", "body"],
            &["BOARD_FROM_ARC", "BOARD_REGISTERED_AGENT"],
            &["project", "phase"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("BOARD_FROM_ARC", false),
            expected("BOARD_REGISTERED_AGENT", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(
            full_neighborhood,
            &["BOARD_FROM_ARC", "BOARD_REGISTERED_AGENT"],
        )
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Whiteboard { board_id } = r else {
            return None;
        };
        if let Some(stores) = ctx.stores() {
            if let Some(board) = stores.whiteboards.get(board_id) {
                return Some(truncate_label(&board.read().topic));
            }
        }
        Some(truncate_label(board_id))
    }
}

// Corpus inspection has no authenticated board role. Expose only evidence
// visible to every registered participant at the stored phase, never invent
// facilitator authority or advance a blind board because its runtime retired.
fn public_history(board: &bbox_whiteboards::whiteboards::Board) -> Result<serde_json::Value> {
    use bbox_whiteboards::whiteboards::Phase;
    let posts = if board.phase == Phase::Blind {
        vec![]
    } else {
        board.posts.clone()
    };
    let annotations = if matches!(
        board.phase,
        Phase::Validate | Phase::Resolve | Phase::Archived
    ) {
        board.annotations.clone()
    } else {
        vec![]
    };
    let votes = if matches!(board.phase, Phase::Resolve | Phase::Archived) {
        board.votes.clone()
    } else {
        vec![]
    };
    Ok(serde_json::json!({"posts":posts,"annotations":annotations,"votes":votes}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_board_reads_preserve_phase_visibility() {
        let mut board: bbox_whiteboards::whiteboards::Board = serde_json::from_value(serde_json::json!({
            "id":"archive", "topic":"Review", "project":"fixture", "created_at":"2026-01-01T00:00:00Z",
            "phase":"blind", "phase_history":[], "agents":{},
            "posts":[{"id":"p1","agent":"alice","type":"claim","title":"Evidence","body":"private until shared","posted_at":"2026-01-01T00:00:00Z"}],
            "annotations":[{"id":"a1","post_id":"p1","agent":"bob","type":"challenge","body":"review","posted_at":"2026-01-01T00:00:00Z"}],
            "votes":[{"post_id":"p1","agent":"bob","vote":"accept","at":"2026-01-01T00:00:00Z"}]
        })).unwrap();
        use bbox_whiteboards::whiteboards::Phase;
        for phase in [
            Phase::Blind,
            Phase::Read,
            Phase::Validate,
            Phase::Debate,
            Phase::Resolve,
            Phase::Archived,
        ] {
            board.phase = phase;
            let value = public_history(&board).unwrap();
            assert_eq!(
                value["posts"].as_array().unwrap().len(),
                usize::from(phase != Phase::Blind)
            );
            assert_eq!(
                value["annotations"].as_array().unwrap().len(),
                usize::from(matches!(
                    phase,
                    Phase::Validate | Phase::Resolve | Phase::Archived
                ))
            );
            assert_eq!(
                value["votes"].as_array().unwrap().len(),
                usize::from(matches!(phase, Phase::Resolve | Phase::Archived))
            );
        }
    }
}
