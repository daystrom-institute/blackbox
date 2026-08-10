//! Scope-bound authority for attended checkout-local provenance export.
//!
//! The export planner reads corpus state and the CLI writes only into the
//! checkout whose committed scope it proves locally. A producer bearer binds
//! the MCP session to exactly that published scope without restoring raw
//! `?project=` as checkout authority.

use std::sync::Arc;

use axum::http::{HeaderMap, header};
use bbox_corpus_core::identity::PublishedScope;
use bbox_provenance::{
    OPERATOR_PROVENANCE_REPO_ID_HEADER, OPERATOR_PROVENANCE_ROOT_RELPATH_HEADER,
};

use super::{BlackboxServer, SharedState};

#[derive(Debug, Clone)]
pub(crate) struct OperatorProvenanceGrant {
    pub(crate) project_id: String,
    pub(crate) scope: PublishedScope,
}

impl BlackboxServer {
    pub(crate) fn authoritative_operator_provenance_binding(
        &self,
    ) -> Option<Arc<OperatorProvenanceGrant>> {
        self.session_operator_provenance_binding
            .get()
            .and_then(Clone::clone)
    }
}

pub(crate) fn authenticate_operator_provenance_binding(
    state: &SharedState,
    headers: &HeaderMap,
) -> Result<Option<OperatorProvenanceGrant>, &'static str> {
    let repo_id = headers.get(OPERATOR_PROVENANCE_REPO_ID_HEADER);
    let root_relpath = headers.get(OPERATOR_PROVENANCE_ROOT_RELPATH_HEADER);
    if repo_id.is_none() && root_relpath.is_none() {
        return Ok(None);
    }
    let (Some(repo_id), Some(root_relpath)) = (repo_id, root_relpath) else {
        return Err("incomplete operator provenance authority");
    };
    let repo_id = repo_id
        .to_str()
        .map_err(|_| "invalid operator provenance repo id")?;
    let root_relpath = root_relpath
        .to_str()
        .map_err(|_| "invalid operator provenance root relative path")?;
    let scope = PublishedScope::try_new(repo_id, root_relpath)
        .map_err(|_| "invalid operator provenance published scope")?;

    let candidate = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or("operator provenance authorization is missing")?;
    let auth = state.code_sources.producer_auth();
    if !auth.enabled() {
        return Err("operator provenance authority is disabled");
    }
    let producer = auth
        .authenticate(candidate)
        .ok_or("invalid operator provenance authorization")?;
    let project_id = producer
        .projects
        .get(&scope)
        .cloned()
        .ok_or("operator provenance scope is not assigned to this credential")?;

    Ok(Some(OperatorProvenanceGrant { project_id, scope }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};
    use bro_rpc::ServiceToken;
    use std::collections::BTreeMap;

    #[test]
    fn operator_provenance_headers_require_one_exact_assigned_scope() {
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
        headers.insert(OPERATOR_PROVENANCE_REPO_ID_HEADER, "repo".parse().unwrap());
        headers.insert(
            OPERATOR_PROVENANCE_ROOT_RELPATH_HEADER,
            ".".parse().unwrap(),
        );
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );

        let grant = authenticate_operator_provenance_binding(&state, &headers)
            .unwrap()
            .unwrap();
        assert_eq!(grant.project_id, "project");
        assert_eq!(grant.scope, scope);

        headers.insert(OPERATOR_PROVENANCE_REPO_ID_HEADER, "other".parse().unwrap());
        assert_eq!(
            authenticate_operator_provenance_binding(&state, &headers).unwrap_err(),
            "operator provenance scope is not assigned to this credential"
        );
        headers.remove(OPERATOR_PROVENANCE_ROOT_RELPATH_HEADER);
        assert_eq!(
            authenticate_operator_provenance_binding(&state, &headers).unwrap_err(),
            "incomplete operator provenance authority"
        );
    }
}
