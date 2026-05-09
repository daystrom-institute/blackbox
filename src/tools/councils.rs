use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::councils_tools()
}

#[tool_router(router = councils_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_council_list",
        description = "List active and closed councils. Optional `project` filter narrows by project_dir."
    )]
    pub(crate) fn bro_council_list(
        &self,
        Parameters(p): Parameters<CouncilListParams>,
    ) -> CallToolResult {
        let summaries = self.state.councils.list_summaries(p.project.as_deref());
        Self::ok_json(&serde_json::json!({"councils": summaries}))
    }

    #[tool(
        name = "bro_council_open",
        description = "Read full council state: metadata, charter, posts, and current envelope status."
    )]
    pub(crate) fn bro_council_open(
        &self,
        Parameters(p): Parameters<CouncilOpenParams>,
    ) -> CallToolResult {
        let Some(council) = self.state.councils.get(&p.id) else {
            return Self::err_text(&format!("unknown council: {}", p.id));
        };
        let s = council.session.read().clone();
        let posts = council.posts.read().clone();
        let envelopes = council.envelopes.read().clone();
        let summary = council::CouncilSummary {
            id: s.id.clone(),
            team_id: s.team_id.clone(),
            project: s.project.clone(),
            topic: s.topic.clone(),
            status: s.status,
            members: s.member_sessions.keys().cloned().collect(),
            created_at: s.created_at.clone(),
            updated_at: s.updated_at.clone(),
            post_count: posts.len() as u64,
        };
        Self::ok_json(&serde_json::json!({
            "summary": summary,
            "posts": posts,
            "envelopes": envelopes,
            "charter": s.charter,
        }))
    }

    #[tool(
        name = "bro_council_posts",
        description = "Paginated council transcript. `since_seq` returns posts with sequence > since_seq; `limit` caps response (default 100, max 1000)."
    )]
    pub(crate) fn bro_council_posts(
        &self,
        Parameters(p): Parameters<CouncilPostsParams>,
    ) -> CallToolResult {
        let Some(council) = self.state.councils.get(&p.id) else {
            return Self::err_text(&format!("unknown council: {}", p.id));
        };
        let since = p.since_seq.unwrap_or(0);
        let limit = p.limit.unwrap_or(100).min(1000);
        let posts: Vec<council::CouncilPost> = council
            .posts
            .read()
            .iter()
            .filter(|post| post.sequence > since)
            .take(limit)
            .cloned()
            .collect();
        Self::ok_json(&serde_json::json!({
            "council_id": p.id,
            "posts": posts,
        }))
    }
}
