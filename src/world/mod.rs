//! Persistent structural world model and provider contracts.

#[cfg(test)]
pub(crate) mod fixtures;
mod git;
mod graphify;
mod spans;
mod structural;
mod uses;

pub use git::{GitCommit, GitHistory, GitIngestReport, PreparedGitHistory};
pub use graphify::GraphifyProvider;
pub use spans::{LineRange, LiveSpans, SpanCheck};
pub use structural::{
    FileScan, GraphEdge, GraphNode, GraphSlice, IndexReport, PreparedIndex, ScanResult,
    ScannedEdge, ScannedNode, StructuralIndexStatus, StructuralProvider, TreeSitterProvider,
    WorldModel,
};
pub use uses::{RelatedUse, UseRelation, related_uses};
