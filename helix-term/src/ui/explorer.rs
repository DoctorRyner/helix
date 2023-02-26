use super::{Prompt, TreeOp, TreeView, TreeViewItem};
use crate::{
    compositor::{Component, Context, EventResult},
    ctrl, key, shift, ui,
};
use anyhow::{bail, ensure, Result};
use helix_core::Position;
use helix_view::{
    editor::{Action, ExplorerPositionEmbed},
    graphics::{CursorKind, Rect},
    info::Info,
    input::{Event, KeyEvent},
    theme::Modifier,
    Editor,
};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::{borrow::Cow, fs::DirEntry};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, Borders, Widget},
};

macro_rules! get_theme {
    ($theme: expr, $s1: expr, $s2: expr) => {
        $theme.try_get($s1).unwrap_or_else(|| $theme.get($s2))
    };
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug, Clone, Copy)]
enum FileType {
    File,
    Folder,
    Root,
}

#[derive(PartialEq, Eq, Debug, Clone)]
struct FileInfo {
    file_type: FileType,
    path: PathBuf,
}

impl FileInfo {
    fn root(path: PathBuf) -> Self {
        Self {
            file_type: FileType::Root,
            path,
        }
    }

    fn get_text(&self) -> Cow<'static, str> {
        let text = match self.file_type {
            FileType::Root => format!("{}", self.path.display()),
            FileType::File | FileType::Folder => self
                .path
                .file_name()
                .map_or("/".into(), |p| p.to_string_lossy().into_owned()),
        };

        #[cfg(test)]
        let text = text.replace(std::path::MAIN_SEPARATOR, "/");

        text.into()
    }
}

impl PartialOrd for FileInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FileInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        use FileType::*;
        match (self.file_type, other.file_type) {
            (Root, _) => return Ordering::Less,
            (_, Root) => return Ordering::Greater,
            _ => {}
        };

        if let (Some(p1), Some(p2)) = (self.path.parent(), other.path.parent()) {
            if p1 == p2 {
                match (self.file_type, other.file_type) {
                    (Folder, File) => return Ordering::Less,
                    (File, Folder) => return Ordering::Greater,
                    _ => {}
                };
            }
        }
        self.path.cmp(&other.path)
    }
}

impl TreeViewItem for FileInfo {
    type Params = State;

    fn get_children(&self) -> Result<Vec<Self>> {
        match self.file_type {
            FileType::Root | FileType::Folder => {}
            _ => return Ok(vec![]),
        };
        let ret: Vec<_> = std::fs::read_dir(&self.path)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| dir_entry_to_file_info(entry, &self.path))
            .collect();
        Ok(ret)
    }

    fn name(&self) -> String {
        self.get_text().to_string()
    }

    fn is_parent(&self) -> bool {
        matches!(self.file_type, FileType::Folder | FileType::Root)
    }
}

fn dir_entry_to_file_info(entry: DirEntry, path: &Path) -> Option<FileInfo> {
    entry.metadata().ok().map(|meta| {
        let file_type = match meta.is_dir() {
            true => FileType::Folder,
            false => FileType::File,
        };
        FileInfo {
            file_type,
            path: path.join(entry.file_name()),
        }
    })
}

#[derive(Clone, Debug)]
enum PromptAction {
    CreateFolder,
    CreateFile,
    RemoveFolder,
    RemoveFile,
    RenameFile,
}

#[derive(Clone, Debug)]
struct State {
    focus: bool,
    open: bool,
    current_root: PathBuf,
    area_width: u16,
    filter: String,
}

impl State {
    fn new(focus: bool, current_root: PathBuf) -> Self {
        Self {
            focus,
            current_root,
            open: true,
            area_width: 0,
            filter: "".to_string(),
        }
    }
}

pub struct Explorer {
    tree: TreeView<FileInfo>,
    history: Vec<TreeView<FileInfo>>,
    show_help: bool,
    show_preview: bool,
    state: State,
    prompt: Option<(PromptAction, Prompt)>,
    #[allow(clippy::type_complexity)]
    on_next_key: Option<Box<dyn FnMut(&mut Context, &mut Self, &KeyEvent) -> EventResult>>,
    column_width: u16,
}

impl Explorer {
    pub fn new(cx: &mut Context) -> Result<Self> {
        let current_root = std::env::current_dir().unwrap_or_else(|_| "./".into());
        Ok(Self {
            tree: Self::new_tree_view(current_root.clone())?,
            history: vec![],
            show_help: false,
            show_preview: false,
            state: State::new(true, current_root),
            prompt: None,
            on_next_key: None,
            column_width: cx.editor.config().explorer.column_width as u16,
        })
    }

    #[cfg(test)]
    fn from_path(root: PathBuf, column_width: u16) -> Result<Self> {
        Ok(Self {
            tree: Self::new_tree_view(root.clone())?,
            history: vec![],
            show_help: false,
            show_preview: false,
            state: State::new(true, root),
            prompt: None,
            on_next_key: None,
            column_width,
        })
    }

    fn new_tree_view(root: PathBuf) -> Result<TreeView<FileInfo>> {
        let root = FileInfo::root(root);
        Ok(TreeView::build_tree(root)?.with_enter_fn(Self::toggle_current))
    }

    fn push_history(&mut self, tree_view: TreeView<FileInfo>) {
        self.history.push(tree_view);
        const MAX_HISTORY_SIZE: usize = 20;
        Vec::truncate(&mut self.history, MAX_HISTORY_SIZE)
    }

    fn change_root(&mut self, root: PathBuf) -> Result<()> {
        if self.state.current_root.eq(&root) {
            return Ok(());
        }
        let tree = Self::new_tree_view(root.clone())?;
        let old_tree = std::mem::replace(&mut self.tree, tree);
        self.push_history(old_tree);
        self.state.current_root = root;
        Ok(())
    }

    fn reveal_file(&mut self, path: PathBuf) -> Result<()> {
        let current_root = &self.state.current_root;
        let current_path = &path;
        let current_root = format!(
            "{}{}",
            current_root.as_path().to_string_lossy(),
            std::path::MAIN_SEPARATOR
        );
        let segments = {
            let stripped = match current_path.strip_prefix(current_root.as_str()) {
                Ok(stripped) => Ok(stripped),
                Err(_) => {
                    let parent = path.parent().ok_or_else(|| {
                        anyhow::anyhow!("Failed get parent of '{}'", current_path.to_string_lossy())
                    })?;
                    self.change_root(parent.into())?;
                    current_path
                        .strip_prefix(
                            format!("{}{}", parent.to_string_lossy(), std::path::MAIN_SEPARATOR)
                                .as_str(),
                        )
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "Failed to strip prefix (parent) '{}' from '{}'",
                                parent.to_string_lossy(),
                                current_path.to_string_lossy()
                            )
                        })
                }
            }?;

            stripped
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        };
        self.tree.reveal_item(segments, &self.state.filter)?;
        Ok(())
    }

    pub fn reveal_current_file(&mut self, cx: &mut Context) -> Result<()> {
        self.focus();
        let current_document_path = doc!(cx.editor).path().cloned();
        match current_document_path {
            None => Ok(()),
            Some(current_path) => self.reveal_file(current_path),
        }
    }

    pub fn focus(&mut self) {
        self.state.focus = true;
        self.state.open = true;
    }

    fn unfocus(&mut self) {
        self.state.focus = false;
    }

    fn close(&mut self) {
        self.state.focus = false;
        self.state.open = false;
    }

    pub fn is_focus(&self) -> bool {
        self.state.focus
    }

    fn render_preview(&mut self, area: Rect, surface: &mut Surface, editor: &Editor) {
        if let Ok(current) = self.tree.current() {
            let item = current.item();
            let head_area = render_block(
                area.clip_bottom(area.height.saturating_sub(2)),
                surface,
                Borders::BOTTOM,
            );
            let path_str = format!("{}", item.path.display());
            surface.set_stringn(
                head_area.x,
                head_area.y,
                path_str,
                head_area.width as usize,
                get_theme!(editor.theme, "ui.explorer.dir", "ui.text"),
            );

            let body_area = area.clip_top(2);
            let style = editor.theme.get("ui.text");
            let content = get_preview(&item.path, body_area.height as usize)
                .unwrap_or_else(|err| vec![err.to_string()]);
            content.into_iter().enumerate().for_each(|(row, line)| {
                surface.set_stringn(
                    body_area.x,
                    body_area.y + row as u16,
                    line,
                    body_area.width as usize,
                    style,
                );
            })
        }
    }

    fn new_create_folder_prompt(&mut self) -> Result<()> {
        let folder_path = self.nearest_folder()?;
        self.prompt = Some((
            PromptAction::CreateFolder,
            Prompt::new(
                format!(" New folder: {}/", folder_path.to_string_lossy()).into(),
                None,
                ui::completers::none,
                |_, _, _| {},
            ),
        ));
        Ok(())
    }

    fn new_create_file_prompt(&mut self) -> Result<()> {
        let folder_path = self.nearest_folder()?;
        self.prompt = Some((
            PromptAction::CreateFile,
            Prompt::new(
                format!(" New file: {}/", folder_path.to_string_lossy()).into(),
                None,
                ui::completers::none,
                |_, _, _| {},
            ),
        ));
        Ok(())
    }

    fn nearest_folder(&self) -> Result<PathBuf> {
        let current = self.tree.current()?.item();
        if current.is_parent() {
            Ok(current.path.to_path_buf())
        } else {
            let parent_path = current.path.parent().ok_or_else(|| {
                anyhow::anyhow!(format!(
                    "Unable to get parent path of '{}'",
                    current.path.to_string_lossy()
                ))
            })?;
            Ok(parent_path.to_path_buf())
        }
    }

    fn new_remove_prompt(&mut self) -> Result<()> {
        let item = self.tree.current()?.item();
        match item.file_type {
            FileType::Folder => self.new_remove_folder_prompt(),
            FileType::File => self.new_remove_file_prompt(),
            FileType::Root => bail!("Root is not removable"),
        }
    }

    fn new_rename_prompt(&mut self, cx: &mut Context) -> Result<()> {
        let path = self.tree.current_item()?.path.clone();
        self.prompt = Some((
            PromptAction::RenameFile,
            Prompt::new(
                " Rename to ".into(),
                None,
                ui::completers::none,
                |_, _, _| {},
            )
            .with_line(path.to_string_lossy().to_string(), cx.editor),
        ));
        Ok(())
    }

    fn new_remove_file_prompt(&mut self) -> Result<()> {
        let item = self.tree.current_item()?;
        ensure!(
            item.path.is_file(),
            "The path '{}' is not a file",
            item.path.to_string_lossy()
        );
        self.prompt = Some((
            PromptAction::RemoveFile,
            Prompt::new(
                format!(" Delete file: '{}'? y/n: ", item.path.display()).into(),
                None,
                ui::completers::none,
                |_, _, _| {},
            ),
        ));
        Ok(())
    }

    fn new_remove_folder_prompt(&mut self) -> Result<()> {
        let item = self.tree.current_item()?;
        ensure!(
            item.path.is_dir(),
            "The path '{}' is not a folder",
            item.path.to_string_lossy()
        );

        self.prompt = Some((
            PromptAction::RemoveFolder,
            Prompt::new(
                format!(" Delete folder: '{}'? y/n: ", item.path.display()).into(),
                None,
                ui::completers::none,
                |_, _, _| {},
            ),
        ));
        Ok(())
    }

    fn toggle_current(item: &mut FileInfo, cx: &mut Context, state: &mut State) -> TreeOp {
        (|| -> Result<TreeOp> {
            if item.path == Path::new("") {
                return Ok(TreeOp::Noop);
            }
            let meta = std::fs::metadata(&item.path)?;
            if meta.is_file() {
                cx.editor.open(&item.path, Action::Replace)?;
                state.focus = false;
                return Ok(TreeOp::Noop);
            }

            if item.path.is_dir() {
                return Ok(TreeOp::GetChildsAndInsert);
            }

            Err(anyhow::anyhow!("Unknown file type: {:?}", meta.file_type()))
        })()
        .unwrap_or_else(|err| {
            cx.editor.set_error(format!("{err}"));
            TreeOp::Noop
        })
    }

    fn render_float(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);
        let area = render_block(area, surface, Borders::ALL);

        let mut preview_area = area.clip_left(self.column_width + 1);
        if let Some((_, prompt)) = self.prompt.as_mut() {
            let area = preview_area.clip_bottom(2);
            let promp_area =
                render_block(preview_area.clip_top(area.height), surface, Borders::TOP);
            prompt.render(promp_area, surface, cx);
            preview_area = area;
        }
        if self.show_help {
            self.render_help(preview_area, surface, cx);
        } else {
            self.render_preview(preview_area, surface, cx.editor);
        }

        let list_area = render_block(area.clip_right(preview_area.width), surface, Borders::RIGHT);
        self.render_tree(list_area, surface, cx)
    }

    fn render_tree(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let title_style = cx.editor.theme.get("ui.text");
        let title_style = if self.is_focus() {
            title_style.add_modifier(Modifier::BOLD)
        } else {
            title_style
        };
        surface.set_stringn(
            area.x,
            area.y,
            "Explorer: press ? for help",
            area.width.into(),
            title_style,
        );
        self.tree
            .render(area.clip_top(1), surface, cx, &self.state.filter);
    }

    pub fn render_embed(
        &mut self,
        area: Rect,
        surface: &mut Surface,
        cx: &mut Context,
        position: &ExplorerPositionEmbed,
    ) {
        if !self.state.open {
            return;
        }
        let width = area.width.min(self.column_width + 2);

        self.state.area_width = area.width;

        let side_area = match position {
            ExplorerPositionEmbed::Left => Rect { width, ..area },
            ExplorerPositionEmbed::Right => Rect {
                x: area.width - width,
                width,
                ..area
            },
        }
        .clip_bottom(1);
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(side_area, background);

        let prompt_area = area.clip_top(side_area.height);

        let list_area = match position {
            ExplorerPositionEmbed::Left => {
                render_block(side_area.clip_left(1), surface, Borders::RIGHT).clip_bottom(1)
            }
            ExplorerPositionEmbed::Right => {
                render_block(side_area.clip_right(1), surface, Borders::LEFT).clip_bottom(1)
            }
        };
        self.render_tree(list_area, surface, cx);

        {
            let statusline = if self.is_focus() {
                cx.editor.theme.get("ui.statusline")
            } else {
                cx.editor.theme.get("ui.statusline.inactive")
            };
            let area = side_area.clip_top(list_area.height);
            let area = match position {
                ExplorerPositionEmbed::Left => area.clip_right(1),
                ExplorerPositionEmbed::Right => area.clip_left(1),
            };
            surface.clear_with(area, statusline);
        }

        if self.is_focus() {
            if self.show_help {
                let help_area = match position {
                    ExplorerPositionEmbed::Left => area,
                    ExplorerPositionEmbed::Right => {
                        area.clip_right(list_area.width.saturating_add(2))
                    }
                };
                self.render_help(help_area, surface, cx);
            }
            if self.show_preview {
                const PREVIEW_AREA_MAX_WIDTH: u16 = 90;
                const PREVIEW_AREA_MAX_HEIGHT: u16 = 30;
                let preview_area_width =
                    (area.width.saturating_sub(side_area.width)).min(PREVIEW_AREA_MAX_WIDTH);
                let preview_area_height = area.height.min(PREVIEW_AREA_MAX_HEIGHT);

                let preview_area = match position {
                    ExplorerPositionEmbed::Left => area.clip_left(side_area.width),
                    ExplorerPositionEmbed::Right => (Rect {
                        x: area
                            .width
                            .saturating_sub(side_area.width)
                            .saturating_sub(preview_area_width),
                        ..area
                    })
                    .clip_right(side_area.width),
                }
                .clip_bottom(2);
                if preview_area.width < 30 || preview_area.height < 3 {
                    return;
                }
                let y = self.tree.winline() as u16;
                let y = if (preview_area_height + y) > preview_area.height {
                    preview_area.height.saturating_sub(preview_area_height)
                } else {
                    y
                }
                .saturating_add(1);
                let area = Rect::new(preview_area.x, y, preview_area_width, preview_area_height);
                surface.clear_with(area, background);
                let area = render_block(area, surface, Borders::all());

                self.render_preview(area, surface, cx.editor);
            }
        }

        if let Some((_, prompt)) = self.prompt.as_mut() {
            prompt.render_prompt(prompt_area, surface, cx)
        }
    }

    fn render_help(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        Info::new(
            "Explorer",
            &[
                ("?", "Toggle help"),
                ("a", "Add file"),
                ("A", "Add folder"),
                ("r", "Rename file/folder"),
                ("d", "Delete file"),
                ("B", "Change root to parent folder"),
                ("]", "Change root to current folder"),
                ("[", "Go to previous root"),
                ("+, =", "Increase size"),
                ("-, _", "Decrease size"),
                ("C-t", "Toggle preview (left/right only)"),
                ("q", "Close"),
            ]
            .into_iter()
            .chain(ui::tree::tree_view_help().into_iter())
            .collect::<Vec<_>>(),
        )
        .render(area, surface, cx)
    }

    fn handle_prompt_event(&mut self, event: &KeyEvent, cx: &mut Context) -> EventResult {
        fn handle_prompt_event(
            explorer: &mut Explorer,
            event: &KeyEvent,
            cx: &mut Context,
        ) -> Result<EventResult> {
            let (action, mut prompt) = match explorer.prompt.take() {
                Some((action, p)) => (action, p),
                _ => return Ok(EventResult::Ignored(None)),
            };
            let line = prompt.line();

            let current_item_path = explorer.tree.current_item()?.path.clone();
            match (&action, event) {
                (PromptAction::CreateFolder, key!(Enter)) => explorer.new_folder(line)?,
                (PromptAction::CreateFile, key!(Enter)) => explorer.new_file(line)?,
                (PromptAction::RemoveFolder, key!(Enter)) => {
                    if line == "y" {
                        close_documents(current_item_path, cx)?;
                        explorer.remove_folder()?;
                    }
                }
                (PromptAction::RemoveFile, key!(Enter)) => {
                    if line == "y" {
                        close_documents(current_item_path, cx)?;
                        explorer.remove_file()?;
                    }
                }
                (PromptAction::RenameFile, key!(Enter)) => {
                    close_documents(current_item_path, cx)?;
                    explorer.rename_current(line)?;
                }
                (_, key!(Esc) | ctrl!('c')) => {}
                _ => {
                    prompt.handle_event(&Event::Key(*event), cx);
                    explorer.prompt = Some((action, prompt));
                }
            }
            Ok(EventResult::Consumed(None))
        }
        match handle_prompt_event(self, event, cx) {
            Ok(event_result) => event_result,
            Err(err) => {
                cx.editor.set_error(err.to_string());
                EventResult::Consumed(None)
            }
        }
    }

    fn new_file(&mut self, file_name: &str) -> Result<()> {
        let current_parent = self.nearest_folder()?;
        let path = helix_core::path::get_normalized_path(&current_parent.join(file_name));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut fd = std::fs::OpenOptions::new();
        fd.create_new(true).write(true).open(&path)?;
        self.reveal_file(path)
    }

    fn new_folder(&mut self, file_name: &str) -> Result<()> {
        let current_parent = self.nearest_folder()?;
        let path = helix_core::path::get_normalized_path(&current_parent.join(file_name));
        std::fs::create_dir_all(&path)?;
        self.reveal_file(path)
    }

    fn toggle_help(&mut self) {
        self.show_help = !self.show_help
    }

    fn go_to_previous_root(&mut self) {
        if let Some(tree) = self.history.pop() {
            self.tree = tree
        }
    }

    fn change_root_to_current_folder(&mut self) -> Result<()> {
        self.change_root(self.tree.current_item()?.path.clone())
    }

    fn change_root_parent_folder(&mut self) -> Result<()> {
        if let Some(parent) = self.state.current_root.parent() {
            let path = parent.to_path_buf();
            self.change_root(path)
        } else {
            Ok(())
        }
    }

    pub fn is_opened(&self) -> bool {
        self.state.open
    }

    pub fn column_width(&self) -> u16 {
        self.column_width
    }

    fn increase_size(&mut self) {
        const EDITOR_MIN_WIDTH: u16 = 10;
        self.column_width = std::cmp::min(
            self.state.area_width.saturating_sub(EDITOR_MIN_WIDTH),
            self.column_width.saturating_add(1),
        )
    }

    fn decrease_size(&mut self) {
        self.column_width = self.column_width.saturating_sub(1)
    }

    fn rename_current(&mut self, line: &String) -> Result<()> {
        let item = self.tree.current_item()?;
        let path = PathBuf::from(line);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&item.path, &path)?;
        self.tree.refresh()?;
        self.reveal_file(path)
    }

    fn remove_folder(&mut self) -> Result<()> {
        let item = self.tree.current_item()?;
        std::fs::remove_dir_all(&item.path)?;
        self.tree.refresh()
    }

    fn remove_file(&mut self) -> Result<()> {
        let item = self.tree.current_item()?;
        std::fs::remove_file(&item.path)?;
        self.tree.refresh()
    }

    fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview
    }
}

fn close_documents(current_item_path: PathBuf, cx: &mut Context) -> Result<()> {
    let ids = cx
        .editor
        .documents
        .iter()
        .filter_map(|(id, doc)| {
            if doc
                .path()
                .map(|p| p.starts_with(&current_item_path))
                .unwrap_or(false)
            {
                Some(*id)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for id in ids {
        cx.editor.close_document(id, true)?;
    }
    Ok(())
}

impl Component for Explorer {
    /// Process input events, return true if handled.
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let filter = self.state.filter.clone();
        if self.tree.prompting() {
            return self.tree.handle_event(event, cx, &mut self.state, &filter);
        }
        let key_event = match event {
            Event::Key(event) => event,
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };
        if !self.is_focus() {
            return EventResult::Ignored(None);
        }
        if let Some(mut on_next_key) = self.on_next_key.take() {
            return on_next_key(cx, self, key_event);
        }

        if let EventResult::Consumed(c) = self.handle_prompt_event(key_event, cx) {
            return EventResult::Consumed(c);
        }

        (|| -> Result<()> {
            match key_event {
                key!(Esc) => self.unfocus(),
                key!('q') => self.close(),
                key!('?') => self.toggle_help(),
                key!('a') => self.new_create_file_prompt()?,
                shift!('A') => self.new_create_folder_prompt()?,
                shift!('B') => self.change_root_parent_folder()?,
                key!(']') => self.change_root_to_current_folder()?,
                key!('[') => self.go_to_previous_root(),
                key!('d') => self.new_remove_prompt()?,
                key!('r') => self.new_rename_prompt(cx)?,
                key!('-') | key!('_') => self.decrease_size(),
                key!('+') | key!('=') => self.increase_size(),
                ctrl!('t') => self.toggle_preview(),
                _ => {
                    self.tree
                        .handle_event(&Event::Key(*key_event), cx, &mut self.state, &filter);
                }
            };
            Ok(())
        })()
        .unwrap_or_else(|err| cx.editor.set_error(format!("{err}")));

        EventResult::Consumed(None)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        if area.width < 10 || area.height < 5 {
            cx.editor.set_error("explorer render area is too small");
            return;
        }
        let config = &cx.editor.config().explorer;
        if let Some(position) = config.is_embed() {
            self.render_embed(area, surface, cx, &position);
        } else {
            self.render_float(area, surface, cx);
        }
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let prompt = match self.prompt.as_ref() {
            Some((_, prompt)) => prompt,
            None => return (None, CursorKind::Hidden),
        };
        let config = &editor.config().explorer;
        let (x, y) = if config.is_overlay() {
            let colw = self.column_width as u16;
            if area.width > colw {
                (area.x + colw + 2, area.y + area.height.saturating_sub(2))
            } else {
                return (None, CursorKind::Hidden);
            }
        } else {
            (area.x, area.y + area.height.saturating_sub(1))
        };
        prompt.cursor(Rect::new(x, y, area.width, 1), editor)
    }
}

fn get_preview(p: impl AsRef<Path>, max_line: usize) -> Result<Vec<String>> {
    let p = p.as_ref();
    if p.is_dir() {
        let mut entries = p
            .read_dir()?
            .filter_map(|entry| {
                entry
                    .ok()
                    .and_then(|entry| dir_entry_to_file_info(entry, p))
            })
            .take(max_line)
            .collect::<Vec<_>>();

        entries.sort();

        return Ok(entries
            .into_iter()
            .map(|entry| match entry.file_type {
                FileType::Folder => format!("{}/", entry.name()),
                _ => entry.name(),
            })
            .collect());
    }

    ensure!(p.is_file(), "path: {} is not file or dir", p.display());
    use std::fs::OpenOptions;
    use std::io::BufRead;
    let mut fd = OpenOptions::new();
    fd.read(true);
    let fd = fd.open(p)?;
    Ok(std::io::BufReader::new(fd)
        .lines()
        .take(max_line)
        .filter_map(|line| line.ok())
        .map(|line| line.replace('\t', "    "))
        .collect())
}

fn render_block(area: Rect, surface: &mut Surface, borders: Borders) -> Rect {
    let block = Block::default().borders(borders);
    let inner = block.inner(area);
    block.render(area, surface);
    inner
}

#[cfg(test)]
mod test_explorer {
    use super::Explorer;
    use helix_view::graphics::Rect;
    use pretty_assertions::assert_eq;
    use std::{fs, path::PathBuf};

    fn dummy_file_tree(name: &str) -> PathBuf {
        use build_fs_tree::{dir, file, Build, MergeableFileSystemTree};
        let tree = MergeableFileSystemTree::<&str, &str>::from(dir! {
            "index.html" => file!("")
            "scripts" => dir! {
                "main.js" => file!("")
            }
            "styles" => dir! {
                "style.css" => file!("")
                "public" => dir! {
                    "file" => file!("")
                }
            }
            ".gitignore" => file!("")
        });
        let path: PathBuf = format!("test-explorer{}{}", std::path::MAIN_SEPARATOR, name).into();
        if path.exists() {
            fs::remove_dir_all(path.clone()).unwrap();
        }
        tree.build(&path).unwrap();
        path
    }

    fn render(explorer: &mut Explorer) -> String {
        explorer.tree.render_to_string(Rect::new(0, 0, 50, 10), "")
    }

    fn new_explorer(name: &str) -> (PathBuf, Explorer) {
        let path = dummy_file_tree(name);
        (path.clone(), Explorer::from_path(path, 30).unwrap())
    }

    #[test]
    fn test_reveal_file() {
        let (path, mut explorer) = new_explorer("reveal_file");

        // 0a. Expect the "scripts" folder is not opened
        assert_eq!(
            render(&mut explorer),
            "
(test-explorer/reveal_file)
⏵ scripts
⏵ styles
  .gitignore
  index.html
"
            .trim()
        );

        // 1. Reveal "scripts/main.js"
        explorer.reveal_file(path.join("scripts/main.js")).unwrap();

        // 1a. Expect the "scripts" folder is opened, and "main.js" is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/reveal_file]
⏷ [scripts]
    (main.js)
⏵ styles
  .gitignore
  index.html
"
            .trim()
        );

        // 2. Change root to "scripts"
        explorer.tree.move_up(1);
        explorer.change_root_to_current_folder().unwrap();

        // 2a. Expect the current root is "scripts"
        assert_eq!(
            render(&mut explorer),
            "
(test-explorer/reveal_file/scripts)
  main.js
"
            .trim()
        );

        // 3. Reveal "styles/public/file", which is outside of the current root
        explorer
            .reveal_file(path.join("styles/public/file"))
            .unwrap();

        // 3a. Expect the current root is "public", and "file" is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/reveal_file/styles/public]
  (file)
"
            .trim()
        );
    }

    #[test]
    fn test_rename() {
        let (path, mut explorer) = new_explorer("rename");

        explorer.tree.move_down(3);
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/rename]
⏵ scripts
⏵ styles
  (.gitignore)
  index.html
"
            .trim()
        );

        // 1. Rename the current file to a name that is lexicographically greater than "index.html"
        explorer
            .rename_current(&path.join("who.is").display().to_string())
            .unwrap();

        // 1a. Expect the file is renamed, and is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/rename]
⏵ scripts
⏵ styles
  index.html
  (who.is)
"
            .trim()
        );

        assert!(path.join("who.is").exists());

        // 2. Rename the current file into an existing folder
        explorer
            .rename_current(&path.join("styles/lol").display().to_string())
            .unwrap();

        // 2a. Expect the file is moved to the folder, and is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/rename]
⏵ scripts
⏷ [styles]
  ⏵ public
    (lol)
    style.css
  index.html
"
            .trim()
        );

        assert!(path.join("styles/lol").exists());

        // 3. Rename the current file into a non-existent folder
        explorer
            .rename_current(&path.join("new_folder/sponge/bob").display().to_string())
            .unwrap();

        // 3a. Expect the non-existent folder to be created,
        //     and the file is moved into it,
        //     and the renamed file is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/rename]
⏷ [new_folder]
  ⏷ [sponge]
      (bob)
⏵ scripts
⏷ styles
  ⏵ public
    style.css
  index.html
"
            .trim()
        );

        assert!(path.join("new_folder/sponge/bob").exists());

        // 4. Change current root to "new_folder/sponge"
        explorer.tree.move_up(1);
        explorer.change_root_to_current_folder().unwrap();

        // 4a. Expect the current root to be "sponge"
        assert_eq!(
            render(&mut explorer),
            "
(test-explorer/rename/new_folder/sponge)
  bob
"
            .trim()
        );

        // 5. Move cursor to "bob", and move it outside of the current root
        explorer.tree.move_down(1);
        explorer
            .rename_current(&path.join("scripts/bob").display().to_string())
            .unwrap();

        // 5a. Expect the current root to be "scripts"
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/rename/scripts]
  (bob)
  main.js
"
            .trim()
        );
    }

    #[test]
    fn test_new_folder() {
        let (path, mut explorer) = new_explorer("new_folder");

        // 1. Add a new folder at the root
        explorer.new_folder("yoyo").unwrap();

        // 1a. Expect the new folder is added, and is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/new_folder]
⏵ scripts
⏵ styles
⏷ (yoyo)
  .gitignore
  index.html
"
            .trim()
        );

        assert!(fs::read_dir(path.join("yoyo")).is_ok());

        // 2. Move up to "styles"
        explorer.tree.move_up(1);

        // 3. Add a new folder
        explorer.new_folder("sus.sass").unwrap();

        // 3a. Expect the new folder is added under "styles", although "styles" is not opened
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/new_folder]
⏵ scripts
⏷ [styles]
  ⏵ public
  ⏷ (sus.sass)
    style.css
⏷ yoyo
  .gitignore
  index.html
"
            .trim()
        );

        assert!(fs::read_dir(path.join("styles/sus.sass")).is_ok());

        // 4. Add a new folder with non-existent parents
        explorer.new_folder("a/b/c").unwrap();

        // 4a. Expect the non-existent parents are created,
        //     and the new folder is created,
        //     and is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/new_folder]
⏷ [styles]
  ⏷ [sus.sass]
    ⏷ [a]
      ⏷ [b]
        ⏷ (c)
    style.css
⏷ yoyo
  .gitignore
  index.html
"
            .trim()
        );

        assert!(fs::read_dir(path.join("styles/sus.sass/a/b/c")).is_ok());

        // 5. Move to "style.css"
        explorer.tree.move_down(1);

        // 6. Add a new folder here
        explorer.new_folder("foobar").unwrap();

        // 6a. Expect the folder is added under "styles",
        //     because the folder of the current item, "style.css" is "styles/"
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/new_folder]
⏵ scripts
⏷ [styles]
  ⏷ (foobar)
  ⏵ public
  ⏷ sus.sass
    ⏷ a
      ⏷ b
        ⏷ c
    style.css
"
            .trim()
        );

        assert!(fs::read_dir(path.join("styles/foobar")).is_ok());
    }

    #[test]
    fn test_new_file() {
        let (path, mut explorer) = new_explorer("new_file");
        // 1. Add a new file at the root
        explorer.new_file("yoyo").unwrap();

        // 1a. Expect the new file is added, and is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/new_file]
⏵ scripts
⏵ styles
  .gitignore
  index.html
  (yoyo)
"
            .trim()
        );

        assert!(fs::read_to_string(path.join("yoyo")).is_ok());

        // 2. Move up to "styles"
        explorer.tree.move_up(3);

        // 3. Add a new file
        explorer.new_file("sus.sass").unwrap();

        // 3a. Expect the new file is added under "styles", although "styles" is not opened
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/new_file]
⏵ scripts
⏷ [styles]
  ⏵ public
    style.css
    (sus.sass)
  .gitignore
  index.html
  yoyo
"
            .trim()
        );

        assert!(fs::read_to_string(path.join("styles/sus.sass")).is_ok());

        // 4. Add a new file with non-existent parents
        explorer.new_file("a/b/c").unwrap();

        // 4a. Expect the non-existent parents are created,
        //     and the new file is created,
        //     and is focused
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/new_file]
⏵ scripts
⏷ [styles]
  ⏷ [a]
    ⏷ [b]
        (c)
  ⏵ public
    style.css
    sus.sass
  .gitignore
"
            .trim()
        );

        assert!(fs::read_to_string(path.join("styles/a/b/c")).is_ok());

        // 5. Move to "style.css"
        explorer.tree.move_down(2);

        // 6. Add a new file here
        explorer.new_file("foobar").unwrap();

        // 6a. Expect the file is added under "styles",
        //     because the folder of the current item, "style.css" is "styles/"
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/new_file]
⏷ [styles]
    ⏷ b
        c
  ⏵ public
    (foobar)
    style.css
    sus.sass
  .gitignore
  index.html
"
            .trim()
        );

        assert!(fs::read_to_string(path.join("styles/foobar")).is_ok());
    }

    #[test]
    fn test_remove_file() {
        let (path, mut explorer) = new_explorer("remove_file");

        // 1. Move to ".gitignore"
        explorer.reveal_file(path.join(".gitignore")).unwrap();

        // 1a. Expect the cursor is at ".gitignore"
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/remove_file]
⏵ scripts
⏵ styles
  (.gitignore)
  index.html
"
            .trim()
        );

        assert!(fs::read_to_string(path.join(".gitignore")).is_ok());

        // 2. Remove the current file
        explorer.remove_file().unwrap();

        // 3. Expect ".gitignore" is deleted, and the cursor moved down
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/remove_file]
⏵ scripts
⏵ styles
  (index.html)
"
            .trim()
        );

        assert!(fs::read_to_string(path.join(".gitignore")).is_err());

        // 3a. Expect "index.html" exists
        assert!(fs::read_to_string(path.join("index.html")).is_ok());

        // 4. Remove the current file
        explorer.remove_file().unwrap();

        // 4a. Expect "index.html" is deleted, at the cursor moved up
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/remove_file]
⏵ scripts
⏵ (styles)
"
            .trim()
        );

        assert!(fs::read_to_string(path.join("index.html")).is_err());
    }

    #[test]
    fn test_remove_folder() {
        let (path, mut explorer) = new_explorer("remove_folder");

        // 1. Move to "styles/"
        explorer.reveal_file(path.join("styles")).unwrap();

        // 1a. Expect the cursor is at "styles"
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/remove_folder]
⏵ scripts
⏷ (styles)
  ⏵ public
    style.css
  .gitignore
  index.html
"
            .trim()
        );

        assert!(fs::read_dir(path.join("styles")).is_ok());

        // 2. Remove the current folder
        explorer.remove_folder().unwrap();

        // 3. Expect "styles" is deleted, and the cursor moved down
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/remove_folder]
⏵ scripts
  (.gitignore)
  index.html
"
            .trim()
        );

        assert!(fs::read_dir(path.join("styles")).is_err());
    }

    #[test]
    fn test_change_root() {
        let (path, mut explorer) = new_explorer("change_root");

        // 1. Move cursor to "styles"
        explorer.reveal_file(path.join("styles")).unwrap();

        // 2. Change root to current folder, and move cursor down
        explorer.change_root_to_current_folder().unwrap();
        explorer.tree.move_down(1);

        // 2a. Expect the current root to be "styles", and the cursor is at "public"
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/change_root/styles]
⏵ (public)
  style.css
"
            .trim()
        );

        // 3. Change root to the parent of current folder
        explorer.change_root_parent_folder().unwrap();

        // 3a. Expect the current root to be "change_root"
        assert_eq!(
            render(&mut explorer),
            "
(test-explorer/change_root)
⏵ scripts
⏵ styles
  .gitignore
  index.html
"
            .trim()
        );

        // 4. Go back to previous root
        explorer.go_to_previous_root();

        // 4a. Expect the root te become "styles", and the cursor position is not forgotten
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/change_root/styles]
⏵ (public)
  style.css
"
            .trim()
        );

        // 5. Go back to previous root again
        explorer.go_to_previous_root();

        // 5a. Expect the current root to be "change_root" again,
        //     but this time the "styles" folder is opened,
        //     because it was opened before any change of root
        assert_eq!(
            render(&mut explorer),
            "
[test-explorer/change_root]
⏵ scripts
⏷ (styles)
  ⏵ public
    style.css
  .gitignore
  index.html
"
            .trim()
        );
    }
}
