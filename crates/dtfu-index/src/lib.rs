//! Indexing pipeline: sync (crawl vs store), prepare (decode/extract on all cores), embed + commit (one thread).
//! Port of Swift `Indexer`; spec: docs/porting/spec-pipeline.md sections 2.2-2.6 and 3.1.

pub mod commit;
pub mod options;
pub mod prepare;
pub mod run;
pub mod service;
pub mod sync;
#[cfg(test)]
pub mod testing;
pub mod types;

pub use options::IndexOptions;
pub use types::{Embed, Job, Leave, Ocr, NoOcr, SystemOcr, Payload, PdfPage, Prepared, VideoFrame};
