use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::{Path, PathBuf};

use helix_view::{
    editor::Action,
    graphics::Rect,
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    Editor,
};
use tui::buffer::Buffer as Surface;

use crate::commands;
use crate::compositor::EventResult;
use crate::key;

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
}

struct TreeRow {
    path: PathBuf,
    is_dir: bool,
    depth: u16,
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
        };
        panel.rebuild_rows(editor);
        panel
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn last_revealed(&self) -> Option<&Path> {
        self.last_revealed.as_deref()
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
            self.rows.push(TreeRow { path, is_dir, depth });
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

    pub fn handle_key_event(&mut self, event: &KeyEvent, cx: &mut commands::Context) {
        match *event {
            key!(Esc) => self.focused = false,
            key!('j') | key!(Down) => self.move_selection(1),
            key!('k') | key!(Up) => self.move_selection(-1),
            key!(Enter) | key!('l') | key!(Right) => self.activate_selection(cx),
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
        let inside = area.width > 0
            && (area.x..area.x + area.width).contains(&event.column)
            && (area.y..area.y + area.height).contains(&event.row);
        if !inside {
            return None;
        }

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let idx = (event.row - area.y) as usize + self.scroll;
                if idx < self.rows.len() {
                    self.selection = idx;
                    self.activate_selection(cx);
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
        let height = inner.height as usize;

        if self.ensure_visible && height > 0 {
            if self.selection < self.scroll {
                self.scroll = self.selection;
            } else if self.selection >= self.scroll + height {
                self.scroll = self.selection + 1 - height;
            }
            self.ensure_visible = false;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));

        for (i, row) in self.rows.iter().enumerate().skip(self.scroll).take(height) {
            let y = inner.y + (i - self.scroll) as u16;
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
        }
    }
}
