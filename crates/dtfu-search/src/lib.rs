//! Hybrid search (port of Searcher.swift and SearchText.swift).

pub mod code_lookup;
pub mod filters;
pub mod router;
pub mod searcher;
pub use dtfu_text as text;

pub use searcher::{
    fts_query, is_lookup, kind_hints, strong_count, terms, RefreshStats, SearchHit, SearchOptions, Searcher,
};
