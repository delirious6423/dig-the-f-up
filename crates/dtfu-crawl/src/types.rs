/// Model.swift:4 `FileKind`. Declaration order is the first-pass priority (Model.swift:10-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileKind {
    Screenshot = 0,
    Image = 1,
    Pdf = 2,
    Doc = 3,
    Audio = 4,
    Video = 5,
    /// Code index only.
    Code = 6,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Screenshot => "screenshot",
            FileKind::Image => "image",
            FileKind::Pdf => "pdf",
            FileKind::Doc => "doc",
            FileKind::Audio => "audio",
            FileKind::Video => "video",
            FileKind::Code => "code",
        }
    }
}
