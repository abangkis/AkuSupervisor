//! Deterministic description of the MCP surface visible to agents.
//!
//! Binary identity is deliberately excluded. Core-only implementation changes
//! must not force a Codex MCP host restart when the advertised contract remains
//! identical.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Manual semantic revision for agent-visible behavior that is not represented
/// by the advertised initialize, tool, capability, or service-schema documents.
pub const MCP_CONTRACT_REVISION: u32 = 1;

/// Returns the complete deterministic MCP contract document.
#[must_use]
pub fn document() -> Value {
    json!({
        "schemaVersion": 1,
        "contractRevision": MCP_CONTRACT_REVISION,
        "readOnly": super::mcp::contract_surface(),
        "registration": super::registration_mcp::contract_surface()
    })
}

/// Computes the stable SHA-256 of the canonical contract document.
///
/// # Errors
///
/// Returns a serialization error if the JSON contract cannot be encoded.
pub fn fingerprint() -> Result<String, serde_json::Error> {
    let encoded = serde_json::to_vec(&document())?;
    let digest = Sha256::digest(encoded);
    Ok(format!("sha256:{digest:x}"))
}

/// Returns the machine-readable contract report used by release tooling.
///
/// # Errors
///
/// Returns a serialization error if the contract cannot be fingerprinted.
pub fn report() -> Result<Value, serde_json::Error> {
    Ok(json!({
        "schemaVersion": 1,
        "fingerprint": fingerprint()?,
        "contract": document()
    }))
}

#[cfg(test)]
mod tests {
    use super::{MCP_CONTRACT_REVISION, document, fingerprint, report};

    #[test]
    fn contract_fingerprint_is_deterministic_and_excludes_binary_version() {
        assert_eq!(
            fingerprint().expect("first fingerprint"),
            fingerprint().expect("second fingerprint")
        );
        let contract = document();
        assert_eq!(contract["contractRevision"], MCP_CONTRACT_REVISION);
        assert!(contract["readOnly"]["tools"].is_array());
        assert!(contract["registration"]["serviceSchema"].is_object());
        assert!(
            !serde_json::to_string(&contract)
                .expect("contract should serialize")
                .contains(crate::VERSION)
        );
    }

    #[test]
    fn report_binds_the_document_to_its_fingerprint() {
        let report = report().expect("contract report");
        assert_eq!(report["fingerprint"], fingerprint().expect("fingerprint"));
        assert_eq!(report["contract"], document());
    }
}
