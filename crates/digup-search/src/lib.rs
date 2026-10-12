//! Hybrid search (port of Searcher.swift and SearchText.swift).

pub mod code_lookup;
pub mod searcher;
pub use digup_text as text;

pub use searcher::{
    fts_query, is_lookup, kind_hints, strong_count, terms, SearchHit, SearchOptions, Searcher,
};
