//! Scope-bound authority for attended checkout-local blame.
//!
//! This is deliberately narrower than the managed workspace binding. A code
//! producer bearer can authorize one MCP session to execute blame for one of
//! its configured scopes, but it does not acquire project knowledge mutation
//! or any other harness-only capability.

use std::sync::Arc;

use axum::http::{HeaderMap, header};
use bbox_corpus_core::blame_transport::{
    OPERATOR_BLAME_REPO_ID_HEADER, OPERATOR_BLAME_ROOT_RELPATH_HEADER,
    OPERATOR_BLAME_WORKSPACE_ID_HEADER,
};
use bbox_corpus_core::identity::PublishedScope;
use bro_core::WorkspaceId;

use super::{BlackboxServer, SharedState};

#[derive(Clone)]
pub(crate) struct OperatorBlameGrant {
    pub(crate) project_id: String,
    pub(crate) scope: PublishedScope,
    pub(crate) workspace_id: WorkspaceId,
}

impl BlackboxServer {
    pub(crate) fn authoritative_operator_blame_binding(&self) -> Option<Arc<OperatorBlameGrant>> {
        self.session_operator_blame_binding
            .get()
            .and_then(Clone::clone)
    }
}

pub(crate) fn authenticate_operator_blame_binding(
    state: &SharedState,
    headers: &HeaderMap,
) -> Result<Option<OperatorBlameGrant>, &'static str> {
    let repo_id = headers.get(OPERATOR_BLAME_REPO_ID_HEADER);
    let root_relpath = headers.get(OPERATOR_BLAME_ROOT_RELPATH_HEADER);
    let workspace_id = headers.get(OPERATOR_BLAME_WORKSPACE_ID_HEADER);
    if repo_id.is_none() && root_relpath.is_none() && workspace_id.is_none() {
        return Ok(None);
    }
    let (Some(repo_id), Some(root_relpath), Some(workspace_id)) =
        (repo_id, root_relpath, workspace_id)
    else {
        return Err("incomplete operator blame authority");
    };
    let repo_id = repo_id
        .to_str()
        .map_err(|_| "invalid operator blame repo id")?;
    let root_relpath = root_relpath
        .to_str()
        .map_err(|_| "invalid operator blame root relative path")?;
    let workspace_id = workspace_id
        .to_str()
        .map_err(|_| "invalid operator blame workspace id")?;
    let scope = PublishedScope::try_new(repo_id, root_relpath)
        .map_err(|_| "invalid operator blame published scope")?;
    let workspace_id = WorkspaceId::parse(workspace_id.to_string())
        .map_err(|_| "invalid operator blame workspace id")?;

    let candidate = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or("operator blame authorization is missing")?;
    let auth = state.code_sources.producer_auth();
    if !auth.enabled() {
        return Err("operator blame authority is disabled");
    }
    let producer = auth
        .authenticate(candidate)
        .ok_or("invalid operator blame authorization")?;
    let project_id = producer
        .projects
        .get(&scope)
        .cloned()
        .ok_or("operator blame scope is not assigned to this credential")?;

    Ok(Some(OperatorBlameGrant {
        project_id,
        scope,
        workspace_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};
    use bro_rpc::ServiceToken;
    use std::collections::BTreeMap;

    #[test]
    fn operator_blame_headers_require_one_exact_assigned_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let state = SharedState::for_test(tmp.path());
        let token = "a".repeat(64);
        let scope = PublishedScope::try_new("repo", ".").unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test(
                true,
                false,
                vec![(
                    ServiceToken::parse(token.clone()).unwrap(),
                    ProducerGrant {
                        producer_id: "producer".into(),
                        projects: BTreeMap::from([(scope.clone(), "project".into())]),
                    },
                )],
            )));
        let mut headers = HeaderMap::new();
        headers.insert(OPERATOR_BLAME_REPO_ID_HEADER, "repo".parse().unwrap());
        headers.insert(OPERATOR_BLAME_ROOT_RELPATH_HEADER, ".".parse().unwrap());
        headers.insert(
            OPERATOR_BLAME_WORKSPACE_ID_HEADER,
            "0123456789abcdef0123456789abcdef".parse().unwrap(),
        );
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );

        let grant = authenticate_operator_blame_binding(&state, &headers)
            .unwrap()
            .unwrap();
        assert_eq!(grant.project_id, "project");
        assert_eq!(grant.scope, scope);

        headers.insert(OPERATOR_BLAME_REPO_ID_HEADER, "other".parse().unwrap());
        assert_eq!(
            authenticate_operator_blame_binding(&state, &headers).unwrap_err(),
            "operator blame scope is not assigned to this credential"
        );
        headers.remove(OPERATOR_BLAME_WORKSPACE_ID_HEADER);
        assert_eq!(
            authenticate_operator_blame_binding(&state, &headers).unwrap_err(),
            "incomplete operator blame authority"
        );
    }
}
