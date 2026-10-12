//! Shared types and constants, mirroring DigUp's Swift engine (index schema v5).

/// Embedding dimension of EmbeddingGemma 2.
pub const DIM: usize = 768;
/// `PRAGMA user_version` of the index files we read and write.
pub const SCHEMA_VERSION: i64 = 5;

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($var:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($var),+ }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$var => $s),+ }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(Self::$var),)+ _ => None }
            }
        }
    };
}

str_enum!(
    /// `files.kind`
    FileKind {
        Screenshot => "screenshot",
        Image => "image",
        Pdf => "pdf",
        Doc => "doc",
        Audio => "audio",
        Video => "video",
        Code => "code",
    }
);

str_enum!(
    /// `segments.kind`
    SegmentKind {
        Name => "name",
        Image => "image",
        Ocr => "ocr",
        Page => "page",
        PageImage => "page_image",
        Chunk => "chunk",
        Audio => "audio",
        Frame => "frame",
        Lines => "lines",
        Cells => "cells",
    }
);

str_enum!(
    /// `segments.modality`: what kind of input produced the vector.
    Modality {
        None => "none",
        Text => "text",
        Image => "image",
        Audio => "audio",
    }
);

str_enum!(
    /// `files.state`
    FileState {
        Pending => "pending",
        Done => "done",
        Partial => "partial",
        Skipped => "skipped",
        Failed => "failed",
    }
);

/// Prompt used when embedding a search query (Model.swift:88-100).
pub fn query_prompt(text: &str, code: bool) -> String {
    if code {
        format!("task: code retrieval | query: {text}")
    } else {
        format!("task: search result | query: {text}")
    }
}

/// Prompt used when embedding a text document chunk.
pub fn doc_prompt(title: Option<&str>, text: &str) -> String {
    format!("title: {} | text: {}", title.unwrap_or("none"), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip() {
        for k in ["screenshot", "image", "pdf", "doc", "audio", "video", "code"] {
            assert_eq!(FileKind::parse(k).unwrap().as_str(), k);
        }
        for k in ["name", "image", "ocr", "page", "page_image", "chunk", "audio", "frame", "lines", "cells"] {
            assert_eq!(SegmentKind::parse(k).unwrap().as_str(), k);
        }
        assert!(Modality::parse("bogus").is_none());
    }

    #[test]
    fn prompts_match_swift() {
        assert_eq!(query_prompt("dog", false), "task: search result | query: dog");
        assert_eq!(query_prompt("retry", true), "task: code retrieval | query: retry");
        assert_eq!(doc_prompt(None, "hi"), "title: none | text: hi");
        assert_eq!(doc_prompt(Some("a.rs"), "hi"), "title: a.rs | text: hi");
    }
}
