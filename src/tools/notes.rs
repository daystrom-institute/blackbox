use crate::notes::{NoteListParams, NoteParams, NoteResolveParams};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::notes_tools()
}

#[tool_router(router = notes_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_note",
        description = "Record a structured side-channel note while working."
    )]
    pub(crate) fn bbox_note(&self, Parameters(p): Parameters<NoteParams>) -> CallToolResult {
        Self::run("bbox_note", || self.state.notes.write().create(&p))
    }

    #[tool(
        name = "bbox_notes",
        description = "List / filter notes by exact id, kind, project, session, thread, resolution."
    )]
    pub(crate) fn bbox_notes(&self, Parameters(p): Parameters<NoteListParams>) -> CallToolResult {
        Self::run("bbox_notes", || self.state.notes.read().list(&p))
    }

    #[tool(
        name = "bbox_note_resolve",
        description = "Mark a note acknowledged or addressed."
    )]
    pub(crate) fn bbox_note_resolve(
        &self,
        Parameters(p): Parameters<NoteResolveParams>,
    ) -> CallToolResult {
        Self::run("bbox_note_resolve", || self.state.notes.write().resolve(&p))
    }
}
