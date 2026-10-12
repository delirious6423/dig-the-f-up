//! Port of DigUpKit `Crawler.swift`, `Rules.swift` and `FolderSelection.swift`
//! (spec: docs/porting/spec-pipeline.md sections 1-4). Metadata-only parallel crawl.

pub mod crawler;
pub mod folder_selection;
pub mod rules;
pub mod types;
pub mod volume;

pub use crawler::{crawl, Candidate, CrawlOptions, CrawlReport};
pub use folder_selection::{FolderSelection, Mark};
pub use rules::{classify, classify_code, Classification};
pub use types::FileKind;
