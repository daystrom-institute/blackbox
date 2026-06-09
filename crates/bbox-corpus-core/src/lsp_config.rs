use serde::{Deserialize, Serialize};

/// LSP configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspConfig {
    pub idle_timeout_secs: u64,
    pub request_timeout_secs: u64,
    pub jdtls_init_timeout_secs: u64,
    pub jdtls_ready_timeout_secs: u64,
    pub rust_analyzer_init_timeout_secs: u64,
    pub roslyn_init_timeout_secs: u64,
    pub jdtls_bin: Option<String>,
    pub rust_analyzer_bin: Option<String>,
    pub roslyn_lsp_bin: Option<String>,
}
