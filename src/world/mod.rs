//! Persistent structural world model and provider contracts.

mod git;
mod graphify;
mod structural;

pub use git::{GitCommit, GitHistory, GitIngestReport, PreparedGitHistory};
pub use graphify::GraphifyProvider;
pub use structural::{
    FileScan, GraphEdge, GraphNode, GraphSlice, IndexReport, PreparedIndex, ScanResult,
    ScannedEdge, ScannedNode, StructuralIndexStatus, StructuralProvider, TreeSitterProvider,
    WorldModel,
};
