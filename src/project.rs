//! Project tree and project picker navigation helpers.

use crate::*;

impl ProjectTree {
    /// Builds the initial project tree rooted at the selected workspace.
    pub(crate) fn new(root: PathBuf) -> Self {
        let mut expanded = BTreeSet::new();
        expanded.insert(root.clone());
        let src = root.join("src");
        if src.is_dir() {
            expanded.insert(src);
        }

        let mut tree = Self {
            root,
            expanded,
            visible: Vec::new(),
            selected: 0,
            search_query: String::new(),
            search_match: None,
            last_search_input: None,
        };
        tree.refresh(None);
        tree
    }

    /// Rebuilds visible tree entries and preserves selection when possible.
    pub(crate) fn refresh(&mut self, selected_path: Option<PathBuf>) {
        self.visible.clear();
        let root = self.root.clone();
        self.collect_entries(&root, 0);

        if self.visible.is_empty() {
            self.selected = 0;
            return;
        }

        if let Some(path) = selected_path {
            self.select_path(&path);
        } else if self.selected >= self.visible.len() {
            self.selected = self.visible.len() - 1;
        }
    }

    fn collect_entries(&mut self, dir: &Path, depth: usize) {
        for path in sorted_project_entries(dir) {
            let is_dir = path.is_dir();
            self.visible.push(TreeEntry {
                path: path.clone(),
                depth,
                is_dir,
            });

            if is_dir && self.expanded.contains(&path) {
                self.collect_entries(&path, depth + 1);
            }
        }
    }

    /// Moves the active selection by a signed row delta.
    pub(crate) fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            self.selected = 0;
            return;
        }

        let current = self.selected as isize + delta;
        let max = self.visible.len().saturating_sub(1) as isize;
        self.selected = current.clamp(0, max) as usize;
    }

    /// Selects a visible path when it exists in the flattened tree.
    pub(crate) fn select_path(&mut self, path: &Path) {
        if let Some(index) = self.visible.iter().position(|entry| entry.path == path) {
            self.selected = index;
        }
    }

    /// Activates the current selection, toggling directories or opening files.
    pub(crate) fn activate_selected(&mut self) -> Option<TreeAction> {
        let entry = self.visible.get(self.selected)?.clone();
        if entry.is_dir {
            if self.expanded.contains(&entry.path) {
                self.expanded.remove(&entry.path);
            } else {
                self.expanded.insert(entry.path.clone());
            }
            self.refresh(Some(entry.path));
            Some(TreeAction::ToggleDir)
        } else {
            Some(TreeAction::OpenFile(entry.path))
        }
    }

    /// Expands all ancestors needed to reveal a path in the tree.
    pub(crate) fn expand_to(&mut self, path: &Path) {
        for ancestor in path.ancestors() {
            if ancestor.starts_with(&self.root) && ancestor.is_dir() {
                self.expanded.insert(ancestor.to_path_buf());
            }
        }
    }

    /// Returns whether project-tree incremental search is active.
    pub(crate) fn search_active(&self) -> bool {
        !self.search_query.is_empty()
    }

    /// Returns whether incremental search has a resolved match.
    pub(crate) fn has_search_match(&self) -> bool {
        self.search_match.is_some()
    }

    /// Clears the tree search state.
    pub(crate) fn clear_search(&mut self) {
        self.search_query.clear();
        self.search_match = None;
        self.last_search_input = None;
    }

    /// Expires tree search after inactivity.
    pub(crate) fn expire_search(&mut self) {
        if self
            .last_search_input
            .is_some_and(|timestamp| timestamp.elapsed() >= PROJECT_TREE_SEARCH_TIMEOUT)
        {
            self.clear_search();
        }
    }

    /// Appends search text and resolves the new selection.
    pub(crate) fn push_search_text(&mut self, text: &str) -> TreeSearchUpdate {
        self.expire_search();

        let sanitized = text
            .chars()
            .filter(|ch| *ch != '\n' && *ch != '\r')
            .collect::<String>();
        if sanitized.is_empty() {
            return TreeSearchUpdate::Unchanged;
        }

        self.search_query.push_str(&sanitized);
        self.last_search_input = Some(Instant::now());
        self.select_search_match()
    }

    /// Deletes one search character and re-runs the match.
    pub(crate) fn backspace_search(&mut self) -> TreeSearchUpdate {
        if self.search_query.is_empty() {
            return TreeSearchUpdate::Unchanged;
        }

        self.search_query.pop();
        if self.search_query.is_empty() {
            self.clear_search();
            return TreeSearchUpdate::Cleared;
        }

        self.last_search_input = Some(Instant::now());
        self.select_search_match()
    }

    fn select_search_match(&mut self) -> TreeSearchUpdate {
        if self.search_query.is_empty() {
            self.search_match = None;
            return TreeSearchUpdate::Cleared;
        }

        let Some(path) = self.find_match(&self.search_query) else {
            self.search_match = None;
            return TreeSearchUpdate::NoMatch;
        };

        self.expand_to(&path);
        self.refresh(Some(path.clone()));
        self.search_match = Some(path.clone());
        TreeSearchUpdate::Matched(path)
    }

    fn find_match(&self, query: &str) -> Option<PathBuf> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }

        let mut prefix = None;
        let mut contains = None;
        self.find_match_in_dir(&self.root, &needle, &mut prefix, &mut contains);
        prefix.or(contains)
    }

    fn find_match_in_dir(
        &self,
        dir: &Path,
        needle: &str,
        prefix: &mut Option<PathBuf>,
        contains: &mut Option<PathBuf>,
    ) {
        for path in sorted_project_entries(dir) {
            let label = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_lowercase();
            let relative = relative_to_root(&self.root, &path).to_lowercase();

            if prefix.is_none() && (label.starts_with(needle) || relative.starts_with(needle)) {
                *prefix = Some(path.clone());
            }
            if contains.is_none() && (label.contains(needle) || relative.contains(needle)) {
                *contains = Some(path.clone());
            }
            if prefix.is_some() && contains.is_some() {
                return;
            }
            if path.is_dir() {
                self.find_match_in_dir(&path, needle, prefix, contains);
                if prefix.is_some() && contains.is_some() {
                    return;
                }
            }
        }
    }
}

impl ProjectPicker {
    /// Creates a new project picker rooted at the provided directory.
    pub(crate) fn new(current_dir: PathBuf, selected_path: Option<PathBuf>) -> io::Result<Self> {
        let mut picker = Self {
            current_dir,
            entries: Vec::new(),
            selected: 0,
            search_query: String::new(),
            search_match: None,
            last_search_input: None,
        };
        picker.refresh(selected_path)?;
        Ok(picker)
    }

    /// Reloads picker entries for the current directory.
    pub(crate) fn refresh(&mut self, selected_path: Option<PathBuf>) -> io::Result<()> {
        self.entries = project_picker_entries(&self.current_dir)?;
        self.search_match = None;

        if self.entries.is_empty() {
            self.selected = 0;
            return Ok(());
        }

        if let Some(selected_path) = selected_path {
            if let Some(index) = self
                .entries
                .iter()
                .position(|entry| entry.path == selected_path)
            {
                self.selected = index;
                return Ok(());
            }
        }

        if self.selected >= self.entries.len() {
            self.selected = self.entries.len() - 1;
        }

        Ok(())
    }

    /// Switches the picker into another directory and refreshes its contents.
    pub(crate) fn set_dir(
        &mut self,
        current_dir: PathBuf,
        selected_path: Option<PathBuf>,
    ) -> io::Result<()> {
        self.current_dir = current_dir;
        self.refresh(selected_path)
    }

    /// Moves the active picker selection by a signed row delta.
    pub(crate) fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }

        let current = self.selected as isize + delta;
        let max = self.entries.len().saturating_sub(1) as isize;
        self.selected = current.clamp(0, max) as usize;
    }

    /// Returns the currently selected picker path.
    pub(crate) fn selected_path(&self) -> Option<PathBuf> {
        self.entries
            .get(self.selected)
            .map(|entry| entry.path.clone())
    }

    /// Returns whether picker incremental search is active.
    pub(crate) fn search_active(&self) -> bool {
        !self.search_query.is_empty()
    }

    /// Returns whether picker search has a resolved match.
    pub(crate) fn has_search_match(&self) -> bool {
        self.search_match.is_some()
    }

    /// Clears picker incremental search state.
    pub(crate) fn clear_search(&mut self) {
        self.search_query.clear();
        self.search_match = None;
        self.last_search_input = None;
    }

    /// Expires picker search after inactivity.
    pub(crate) fn expire_search(&mut self) {
        if self
            .last_search_input
            .is_some_and(|timestamp| timestamp.elapsed() >= PROJECT_TREE_SEARCH_TIMEOUT)
        {
            self.clear_search();
        }
    }

    /// Appends search text and resolves the next picker match.
    pub(crate) fn push_search_text(&mut self, text: &str) -> ProjectPickerSearchUpdate {
        self.expire_search();

        let sanitized = text
            .chars()
            .filter(|ch| *ch != '\n' && *ch != '\r')
            .collect::<String>();
        if sanitized.is_empty() {
            return ProjectPickerSearchUpdate::Unchanged;
        }

        self.search_query.push_str(&sanitized);
        self.last_search_input = Some(Instant::now());
        self.select_search_match()
    }

    /// Deletes one picker search character and re-runs the match.
    pub(crate) fn backspace_search(&mut self) -> ProjectPickerSearchUpdate {
        if self.search_query.is_empty() {
            return ProjectPickerSearchUpdate::Unchanged;
        }

        self.search_query.pop();
        if self.search_query.is_empty() {
            self.clear_search();
            return ProjectPickerSearchUpdate::Cleared;
        }

        self.last_search_input = Some(Instant::now());
        self.select_search_match()
    }

    fn select_search_match(&mut self) -> ProjectPickerSearchUpdate {
        if self.search_query.is_empty() {
            self.search_match = None;
            return ProjectPickerSearchUpdate::Cleared;
        }

        let needle = self.search_query.trim().to_lowercase();
        if needle.is_empty() {
            self.clear_search();
            return ProjectPickerSearchUpdate::Cleared;
        }

        let Some(path) = find_project_picker_match(&self.current_dir, &needle) else {
            self.search_match = None;
            return ProjectPickerSearchUpdate::NoMatch;
        };

        if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
            self.selected = index;
        } else {
            let Some(parent) = path.parent() else {
                self.search_match = None;
                return ProjectPickerSearchUpdate::NoMatch;
            };
            self.current_dir = parent.to_path_buf();
            if let Err(_) = self.refresh(Some(path.clone())) {
                self.search_match = None;
                return ProjectPickerSearchUpdate::NoMatch;
            }
        }

        self.search_match = Some(path.clone());
        ProjectPickerSearchUpdate::Matched(path)
    }
}
