use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::{Path, PathBuf};

use helix_view::{
    editor::Action,
    graphics::{Modifier, Rect},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    Editor,
};
use tui::buffer::Buffer as Surface;

use crate::commands;
use crate::compositor::EventResult;
use crate::{ctrl, key};

/// A persistent file tree side panel, owned and driven by `EditorView`.
///
/// The tree is rooted at the directory helix was launched from and lazily
/// loads directory contents using the same ignore rules as the file explorer.
pub struct FileTreePanel {
    root: PathBuf,
    focused: bool,
    expanded: HashSet<PathBuf>,
    /// Lazily loaded directory listings: dir -> sorted (path, is_dir) entries.
    children: HashMap<PathBuf, Vec<(PathBuf, bool)>>,
    /// Visible rows, flattened in display order.
    rows: Vec<TreeRow>,
    selection: usize,
    scroll: usize,
    /// The area the panel was last rendered into, used for mouse hit-testing.
    area: Rect,
    last_revealed: Option<PathBuf>,
    /// Scroll the selection into view on the next render.
    ensure_visible: bool,
    /// Runtime width override (set by resizing); falls back to the config width.
    width_override: Option<u16>,
    /// A mouse drag on the border is resizing the panel.
    resizing: bool,
    /// Multi-key input in progress (`g` prefix or jump labels).
    pending: Option<Pending>,
}

const MIN_WIDTH: u16 = 10;
/// The root directory name shown above the tree rows.
const HEADER_HEIGHT: u16 = 1;

struct TreeRow {
    path: PathBuf,
    is_dir: bool,
    depth: u16,
}

enum Pending {
    /// `g` typed, awaiting `w`/`g`/`e`.
    Goto,
    /// `gw` jump labels over the visible rows: (row index, label chars).
    Jump {
        labels: Vec<(usize, [char; 2])>,
        first: Option<char>,
    },
}

impl FileTreePanel {
    pub fn new(editor: &Editor) -> Self {
        let root = helix_stdx::path::canonicalize(helix_stdx::env::current_working_dir());
        let mut panel = Self {
            expanded: HashSet::from([root.clone()]),
            root,
            focused: false,
            children: HashMap::new(),
            rows: Vec::new(),
            selection: 0,
            scroll: 0,
            area: Rect::default(),
            last_revealed: None,
            ensure_visible: true,
            width_override: None,
            resizing: false,
            pending: None,
        };
        panel.rebuild_rows(editor);
        panel
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        if !focused {
            self.pending = None;
        }
    }

    pub fn last_revealed(&self) -> Option<&Path> {
        self.last_revealed.as_deref()
    }

    pub fn width(&self, default: u16) -> u16 {
        self.width_override.unwrap_or(default).max(MIN_WIDTH)
    }

    fn adjust_width(&mut self, delta: i16) {
        self.width_override = Some(self.area.width.saturating_add_signed(delta).max(MIN_WIDTH));
    }

    fn load_children(&mut self, editor: &Editor, dir: &Path) {
        if !self.children.contains_key(dir) {
            let entries = super::directory_children(dir, editor, false).unwrap_or_default();
            self.children.insert(dir.to_path_buf(), entries);
        }
    }

    fn rebuild_rows(&mut self, editor: &Editor) {
        self.rows.clear();

        let root = self.root.clone();
        self.load_children(editor, &root);
        let mut stack: Vec<(PathBuf, bool, u16)> = Vec::new();
        if let Some(entries) = self.children.get(&root) {
            for (path, is_dir) in entries.iter().rev() {
                stack.push((path.clone(), *is_dir, 0));
            }
        }

        while let Some((path, is_dir, depth)) = stack.pop() {
            if is_dir && self.expanded.contains(&path) {
                self.load_children(editor, &path);
                if let Some(entries) = self.children.get(&path) {
                    for (child, child_is_dir) in entries.iter().rev() {
                        stack.push((child.clone(), *child_is_dir, depth + 1));
                    }
                }
            }
            self.rows.push(TreeRow {
                path,
                is_dir,
                depth,
            });
        }

        self.selection = self.selection.min(self.rows.len().saturating_sub(1));
    }

    /// Expand all ancestors of `path` and select its row, if it is under the root.
    pub fn reveal_path(&mut self, editor: &Editor, path: &Path) {
        // Always record the path, even when it is outside the root or filtered
        // out by ignore rules, so the live-follow check in render doesn't
        // retry every frame.
        self.last_revealed = Some(path.to_path_buf());

        if !path.starts_with(&self.root) {
            return;
        }
        for ancestor in path.ancestors().skip(1) {
            if !ancestor.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(ancestor.to_path_buf());
        }
        self.rebuild_rows(editor);
        if let Some(idx) = self.rows.iter().position(|row| row.path == path) {
            self.selection = idx;
            self.ensure_visible = true;
        }
    }

    fn toggle_dir(&mut self, editor: &Editor, dir: &Path) {
        if self.expanded.remove(dir) {
            // Drop the cached listing so re-expanding re-reads the filesystem.
            self.children.remove(dir);
        } else {
            self.expanded.insert(dir.to_path_buf());
        }
        self.rebuild_rows(editor);
    }

    fn open_file(cx: &mut commands::Context, path: &Path) {
        if let Err(e) = cx.editor.open(path, Action::Replace) {
            let err = if let Some(err) = e.source() {
                format!("{}", err)
            } else {
                format!("unable to open \"{}\"", path.display())
            };
            cx.editor.set_error(err);
        }
    }

    fn move_selection(&mut self, offset: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.selection = self
            .selection
            .saturating_add_signed(offset)
            .min(self.rows.len() - 1);
        self.ensure_visible = true;
    }

    /// The number of tree rows that fit in the panel, below the header.
    fn view_height(&self) -> usize {
        self.area.height.saturating_sub(HEADER_HEIGHT) as usize
    }

    /// Move the selection to the parent directory's row and collapse it.
    fn collapse_parent(&mut self, editor: &Editor) {
        let on_child = self
            .rows
            .get(self.selection)
            .is_some_and(|row| row.depth > 0);
        if !on_child {
            return;
        }
        self.select_parent();
        let path = self.rows[self.selection].path.clone();
        if self.expanded.contains(&path) {
            self.toggle_dir(editor, &path);
        }
    }

    /// Move the selection to the parent directory's row.
    fn select_parent(&mut self) {
        let Some(row) = self.rows.get(self.selection) else {
            return;
        };
        let Some(parent_depth) = row.depth.checked_sub(1) else {
            return;
        };
        if let Some(idx) = self.rows[..self.selection]
            .iter()
            .rposition(|row| row.depth == parent_depth)
        {
            self.selection = idx;
            self.ensure_visible = true;
        }
    }

    fn activate_selection(&mut self, cx: &mut commands::Context) {
        let Some(row) = self.rows.get(self.selection) else {
            return;
        };
        let path = row.path.clone();
        if row.is_dir {
            self.toggle_dir(cx.editor, &path);
        } else {
            Self::open_file(cx, &path);
            self.focused = false;
        }
    }

    /// Label the visible rows with two-char jump labels, like the editor's `gw`.
    fn start_jump(&mut self, editor: &Editor) {
        let config = editor.config();
        let alphabet = &config.jump_label_alphabet;
        let n = alphabet.len();
        if n == 0 {
            return;
        }
        let visible = self.scroll..(self.scroll + self.view_height()).min(self.rows.len());
        let labels: Vec<_> = visible
            .take(n * n)
            .enumerate()
            .map(|(i, row)| (row, [alphabet[i / n], alphabet[i % n]]))
            .collect();
        if !labels.is_empty() {
            self.pending = Some(Pending::Jump {
                labels,
                first: None,
            });
        }
    }

    /// Handle the second key of a pending multi-key input. The pending state
    /// has already been taken, so falling through cancels the sequence.
    fn handle_pending_key(
        &mut self,
        pending: Pending,
        event: &KeyEvent,
        cx: &mut commands::Context,
    ) {
        match pending {
            Pending::Goto => match *event {
                key!('g') => {
                    self.selection = 0;
                    self.ensure_visible = true;
                }
                key!('e') => {
                    self.selection = self.rows.len().saturating_sub(1);
                    self.ensure_visible = true;
                }
                key!('w') => self.start_jump(cx.editor),
                _ => {}
            },
            Pending::Jump { labels, first } => {
                let Some(ch) = event.char().filter(|_| event.modifiers.is_empty()) else {
                    return;
                };
                match first {
                    None => {
                        let remaining: Vec<_> = labels
                            .into_iter()
                            .filter(|(_, label)| label[0] == ch)
                            .collect();
                        if !remaining.is_empty() {
                            self.pending = Some(Pending::Jump {
                                labels: remaining,
                                first: Some(ch),
                            });
                        }
                    }
                    Some(_) => {
                        if let Some((row, _)) = labels.iter().find(|(_, label)| label[1] == ch) {
                            self.selection = *row;
                            self.ensure_visible = true;
                        }
                    }
                }
            }
        }
    }

    pub fn handle_key_event(&mut self, event: &KeyEvent, cx: &mut commands::Context) {
        if let Some(pending) = self.pending.take() {
            self.handle_pending_key(pending, event, cx);
            return;
        }
        match *event {
            key!(Esc) => self.focused = false,
            key!('j') | key!(Down) => self.move_selection(1),
            key!('k') | key!(Up) => self.move_selection(-1),
            key!(Enter) | key!('l') | key!(Right) => self.activate_selection(cx),
            key!('<') => self.adjust_width(-2),
            key!('>') => self.adjust_width(2),
            key!('h') | key!(Left) => {
                let collapsible = self
                    .rows
                    .get(self.selection)
                    .is_some_and(|row| row.is_dir && self.expanded.contains(&row.path));
                if collapsible {
                    let path = self.rows[self.selection].path.clone();
                    self.toggle_dir(cx.editor, &path);
                } else {
                    self.select_parent();
                }
            }
            key!('H') => self.collapse_parent(cx.editor),
            key!('g') => self.pending = Some(Pending::Goto),
            ctrl!('d') => self.move_selection((self.view_height() / 2).max(1) as isize),
            ctrl!('u') => self.move_selection(-((self.view_height() / 2).max(1) as isize)),
            key!(PageDown) => self.move_selection(self.view_height().max(1) as isize),
            key!(PageUp) => self.move_selection(-(self.view_height().max(1) as isize)),
            // Swallow everything else so keys never leak into the keymap
            // while the panel has focus.
            _ => {}
        }
    }

    /// Returns `None` when the event is outside the panel area so the caller
    /// can fall through to the regular editor mouse handling.
    pub fn handle_mouse_event(
        &mut self,
        event: &MouseEvent,
        cx: &mut commands::Context,
    ) -> Option<EventResult> {
        let area = self.area;

        // While dragging the border, capture all mouse events regardless of
        // bounds so the drag can move outside the panel.
        if self.resizing {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let width = (event.column.saturating_sub(area.x) + 1).max(MIN_WIDTH);
                    self.width_override = Some(width);
                }
                MouseEventKind::Up(_) => self.resizing = false,
                _ => {}
            }
            return Some(EventResult::Consumed(None));
        }

        let inside = area.width > 0
            && (area.x..area.x + area.width).contains(&event.column)
            && (area.y..area.y + area.height).contains(&event.row);
        if !inside {
            return None;
        }

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pending = None;
                // grabbing the border column starts a resize drag
                if event.column == area.x + area.width - 1 {
                    self.resizing = true;
                    return Some(EventResult::Consumed(None));
                }
                // clicks on the header row select nothing
                if event.row >= area.y + HEADER_HEIGHT {
                    let idx = (event.row - area.y - HEADER_HEIGHT) as usize + self.scroll;
                    if idx < self.rows.len() {
                        self.selection = idx;
                        self.activate_selection(cx);
                    }
                }
                Some(EventResult::Consumed(None))
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let lines = cx.editor.config().scroll_lines.unsigned_abs();
                self.scroll = if event.kind == MouseEventKind::ScrollUp {
                    self.scroll.saturating_sub(lines)
                } else {
                    (self.scroll + lines).min(self.rows.len().saturating_sub(1))
                };
                Some(EventResult::Consumed(None))
            }
            _ => Some(EventResult::Ignored(None)),
        }
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, editor: &Editor) {
        self.area = area;
        if area.width == 0 || area.height == 0 {
            return;
        }

        let theme = &editor.theme;
        let border_style = theme.get("ui.window");
        let directory_style = theme.get("ui.text.directory");
        let text_style = theme.get("ui.text");
        let selected_style = if self.focused {
            theme.get("ui.menu.selected")
        } else {
            theme.get("ui.selection")
        };

        let border_x = area.x + area.width - 1;
        for y in area.y..area.y + area.height {
            surface.set_string(border_x, y, "│", border_style);
        }

        let inner = area.clip_right(1);

        // Header: the name of the directory helix was opened in.
        let root_name = self
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string());
        surface.set_stringn(
            inner.x,
            inner.y,
            &format!("{}/", root_name),
            inner.width as usize,
            directory_style.add_modifier(Modifier::BOLD),
        );

        let rows_area = inner.clip_top(HEADER_HEIGHT);
        let height = rows_area.height as usize;

        if self.ensure_visible && height > 0 {
            if self.selection < self.scroll {
                self.scroll = self.selection;
            } else if self.selection >= self.scroll + height {
                self.scroll = self.selection + 1 - height;
            }
            self.ensure_visible = false;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));

        let jump_labels = match &self.pending {
            Some(Pending::Jump { labels, first }) => Some((labels, *first)),
            _ => None,
        };
        let jump_label_style = theme.get("ui.virtual.jump-label");

        for (i, row) in self.rows.iter().enumerate().skip(self.scroll).take(height) {
            let y = rows_area.y + (i - self.scroll) as u16;
            let name = row
                .path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            let (text, style) = if row.is_dir {
                let arrow = if self.expanded.contains(&row.path) {
                    "▾"
                } else {
                    "▸"
                };
                (format!("{} {}/", arrow, name), directory_style)
            } else {
                (name.into_owned(), text_style)
            };

            let x = inner.x.saturating_add(row.depth * 2);
            if x < inner.x + inner.width {
                let max_width = (inner.x + inner.width - x) as usize;
                surface.set_stringn(x, y, &text, max_width, style);
            }
            if i == self.selection {
                surface.set_style(Rect::new(inner.x, y, inner.width, 1), selected_style);
            }
            if let Some((labels, first)) = jump_labels {
                if let Some((_, label)) = labels.iter().find(|(row_idx, _)| *row_idx == i) {
                    let label_text: String = match first {
                        None => label.iter().collect(),
                        Some(_) => label[1].to_string(),
                    };
                    if x < inner.x + inner.width {
                        let max_width = (inner.x + inner.width - x) as usize;
                        surface.set_stringn(x, y, &label_text, max_width, jump_label_style);
                    }
                }
            }
        }
    }
}
