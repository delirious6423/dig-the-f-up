//! Port of FolderSelection.swift (pure logic, ordering and uniqueness-by-first-occurrence preserved).

/// FolderSelection.swift:13 `Mark`: a folder's checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    On,
    Mixed,
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderSelection {
    pub chosen: Vec<String>,
    pub skipped: Vec<String>,
    /// Folders already searched (when Settings adds more): whatever is clicked, they stay.
    pub kept: Vec<String>,
}

fn unique(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths.into_iter().filter(|p| seen.insert(p.clone())).collect()
}

impl FolderSelection {
    pub fn new(chosen: Vec<String>, skipped: Vec<String>, kept: Vec<String>) -> Self {
        Self { chosen: unique(chosen), skipped: unique(skipped), kept: unique(kept) }
    }

    fn kept_and_chosen(&self) -> Vec<String> {
        self.kept.iter().chain(self.chosen.iter()).cloned().collect()
    }

    /// FolderSelection.swift:30 Whether `path` (a folder or a file) is searched.
    pub fn is_searched(&self, path: &str) -> bool {
        Self::is_searched_in(path, &self.kept_and_chosen(), &self.skipped)
    }

    /// FolderSelection.swift:32-36
    pub fn is_searched_in(path: &str, folders: &[String], skipped: &[String]) -> bool {
        folders.iter().any(|folder| {
            (path == folder || Self::is_inside(path, folder))
                && !skipped
                    .iter()
                    .any(|skip| Self::is_inside(skip, folder) && (path == skip || Self::is_inside(path, skip)))
        })
    }

    /// FolderSelection.swift:40 The closest chosen (or kept) folder that searches `path`.
    /// Ties on length: Swift `max(by:)` replaces only on a strictly greater element, so the first wins.
    pub fn searched_as(&self, path: &str) -> Option<String> {
        let mut best: Option<String> = None;
        for folder in self.kept_and_chosen() {
            let ok = (path == folder || Self::is_inside(path, &folder))
                && Self::is_searched_in(path, std::slice::from_ref(&folder), &self.skipped);
            if ok && best.as_ref().map_or(true, |b| char_count(b) < char_count(&folder)) {
                best = Some(folder);
            }
        }
        best
    }

    /// FolderSelection.swift:47-53 What the indexer gets.
    pub fn folders(&self) -> Vec<String> {
        let all = unique(self.kept_and_chosen());
        all.iter()
            .filter(|folder| {
                !all.iter().any(|other| {
                    other != *folder
                        && Self::is_inside(folder, other)
                        && Self::is_searched_in(folder, std::slice::from_ref(other), &self.skipped)
                })
            })
            .cloned()
            .collect()
    }

    /// FolderSelection.swift:58 The skips that leave something out (inside a folder that's searched).
    pub fn effective_skips(&self) -> Vec<String> {
        let folders = self.folders();
        self.skipped.iter().filter(|s| folders.iter().any(|f| Self::is_inside(s, f))).cloned().collect()
    }

    /// FolderSelection.swift:65-70
    pub fn mark(&self, folder: &str) -> Mark {
        if self.is_searched(folder) {
            return if self.skipped.iter().any(|s| Self::parent(s) == folder && !self.is_searched(s)) {
                Mark::Mixed
            } else {
                Mark::On
            };
        }
        if self.kept_and_chosen().iter().any(|c| Self::parent(c) == folder) {
            Mark::Mixed
        } else {
            Mark::Off
        }
    }

    /// FolderSelection.swift:73-77 Checks or unchecks a folder and its subfolders.
    pub fn set_folder(&mut self, folder: &str, on: bool) {
        self.skipped.retain(|s| Self::parent(s) != folder);
        self.chosen.retain(|s| Self::parent(s) != folder);
        self.set_subfolder(folder, on);
    }

    /// FolderSelection.swift:80-90 Checks or unchecks one folder, leaving the choices inside it as they are.
    pub fn set_subfolder(&mut self, folder: &str, on: bool) {
        if on {
            self.skipped.retain(|s| s != folder);
            if !self.is_searched(folder) {
                self.chosen.push(folder.to_string());
            }
        } else {
            self.chosen.retain(|s| s != folder);
            if self.is_searched(folder) {
                self.skipped.push(folder.to_string());
            }
        }
    }

    /// Stops skipping `folder` (without choosing it).
    pub fn unskip(&mut self, folder: &str) {
        self.skipped.retain(|s| s != folder);
    }

    /// FolderSelection.swift:95 Whole-name compare: `/a/bc` is not inside `/a/b`.
    pub fn is_inside(path: &str, folder: &str) -> bool {
        if folder.ends_with('/') {
            path.starts_with(folder)
        } else {
            path.len() > folder.len() && path.starts_with(folder) && path.as_bytes()[folder.len()] == b'/'
        }
    }

    /// NSString.deletingLastPathComponent.
    pub fn parent(path: &str) -> String {
        let trimmed = if path.len() > 1 { path.trim_end_matches('/') } else { path };
        match trimmed.rfind('/') {
            None => String::new(),
            Some(0) => "/".to_string(),
            Some(i) => trimmed[..i].to_string(),
        }
    }

    /// FolderSelection.swift:104 realpath, or the path itself when it can't be resolved.
    pub fn real_path(path: &str) -> String {
        std::fs::canonicalize(path).ok().and_then(|p| p.to_str().map(String::from)).unwrap_or_else(|| path.to_string())
    }
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}
