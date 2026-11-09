//! `EditorWorkspace`: the embeddable editing view for one worktree root.
//!
//! Layout: file tree | (tabs / banner / code editor / problems+references) /
//! status bar, plus overlays (quick open, go to line, rename + preview).
//! Hosts (pod-app) create one per task worktree and listen to
//! [`EditorEvent`]; see docs/13-editor-and-lsp.md §API.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::base::input::Diagnostic as EditorDiagnostic;
use gpui_kit::component::input::{Editor, EditorState, GoToDefinition, Input, InputEditsEvent, InputEvent, InputState, Rope, RopeExt as _, ShowCompletions, ShowDocumentHandler};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use serde::{Deserialize, Serialize};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lsp_types as lt;
use pod_ui::{Icon, Mode, Tokens, icon, tokens::color as tc};

use super::quick_open::{QuickOpen, QuickOpenEvent};
use super::strings as txt;
use crate::git::{self, GitMark};
use crate::index::{self, IndexSnapshot, Recents};
use crate::lsp::providers::LspBridge;
use crate::lsp::store::apply_edits;
use crate::lsp::{FileEdit, Language, Location, LspStore, LspStoreEvent, ServerState, language_for_path};
use crate::text::{self, LineEnding};
use crate::tree::{FileTree, TreeAction};
use crate::uri::{path_key, simplify, uri_to_path};
use crate::watch::{Batch, RootWatcher};

const CTX: &str = "PodEditor";
const TREE_CTX: &str = "PodEditorTree";
const TREE_ROW_H: f32 = 28.;
const TREE_W: f32 = 256.;
const PANEL_H: f32 = 200.;

actions!(
    pod_editor,
    [QuickOpenFile, Save, SaveAll, CloseTab, NextTab, PrevTab, GotoLine, FindReferences, RenameSymbol, NextProblem, TogglePanel, FocusTree, FocusEditor]
);
actions!(pod_editor_tree, [TreeUp, TreeDown, TreeLeft, TreeRight, TreeOpen, TreeHome, TreeEnd, TreeNewFile, TreeRename, TreeDelete]);

/// Registers key bindings. Call once after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    let c = Some(CTX);
    let tc_ = Some(TREE_CTX);
    cx.bind_keys([
        KeyBinding::new("ctrl-p", QuickOpenFile, c),
        KeyBinding::new("ctrl-s", Save, c),
        KeyBinding::new("ctrl-k s", SaveAll, c),
        KeyBinding::new("ctrl-w", CloseTab, c),
        KeyBinding::new("ctrl-tab", NextTab, c),
        KeyBinding::new("ctrl-shift-tab", PrevTab, c),
        KeyBinding::new("ctrl-g", GotoLine, c),
        KeyBinding::new("shift-f12", FindReferences, c),
        KeyBinding::new("f2", RenameSymbol, c),
        KeyBinding::new("f8", NextProblem, c),
        KeyBinding::new("ctrl-j", TogglePanel, c),
        KeyBinding::new("alt-1", FocusTree, c),
        KeyBinding::new("alt-2", FocusEditor, c),
        // gpui-component defines these editor actions without default keys.
        KeyBinding::new("f12", GoToDefinition, Some("Input")),
        KeyBinding::new("ctrl-space", ShowCompletions, Some("Input")),
        KeyBinding::new("up", TreeUp, tc_),
        KeyBinding::new("down", TreeDown, tc_),
        KeyBinding::new("left", TreeLeft, tc_),
        KeyBinding::new("right", TreeRight, tc_),
        KeyBinding::new("enter", TreeOpen, tc_),
        KeyBinding::new("space", TreeOpen, tc_),
        KeyBinding::new("home", TreeHome, tc_),
        KeyBinding::new("end", TreeEnd, tc_),
        KeyBinding::new("f2", TreeRename, tc_),
        KeyBinding::new("delete", TreeDelete, tc_),
        KeyBinding::new("ctrl-n", TreeNewFile, tc_),
    ]);
}

/// Events for the host (pod-app).
#[derive(Clone, Debug, PartialEq)]
pub enum EditorEvent {
    FileOpened(PathBuf),
    ActiveFileChanged(Option<PathBuf>),
    FileSaved(PathBuf),
    DirtyChanged { path: PathBuf, dirty: bool },
    /// The file changed on disk (e.g. an agent wrote it). `reloaded` is
    /// false when the buffer had unsaved edits and a prompt is shown.
    ExternalChange { path: PathBuf, reloaded: bool },
    FileClosed(PathBuf),
}

/// One open tab for layout persistence (path relative to the root, `/`
/// separators; 1-based line/column).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabSession {
    pub path: String,
    #[serde(default = "one")]
    pub line: u32,
    #[serde(default = "one")]
    pub col: u32,
}

fn one() -> u32 {
    1
}

/// Open tabs and the active one, restored with the host layout.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorSession {
    #[serde(default)]
    pub tabs: Vec<TabSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DiskState {
    Same,
    /// Disk differs and the buffer is dirty: prompt.
    Changed(String),
    Deleted,
}

struct Buffer {
    path: PathBuf,
    rel: String,
    state: Entity<EditorState>,
    language: Option<Language>,
    ending: LineEnding,
    bom: bool,
    /// LF text as last loaded/saved.
    saved: String,
    dirty: bool,
    large: bool,
    disk: DiskState,
    /// Registered with the language server (done on first activation, so
    /// restored background tabs never start a server).
    lsp_open: bool,
    _subs: Vec<Subscription>,
    _sync: Task<()>,
}

enum Overlay {
    None,
    QuickOpen(Entity<QuickOpen>),
    GotoLine(Entity<InputState>),
    Rename { input: Entity<InputState>, path: PathBuf, pos: lt::Position },
    RenamePreview { new_name: String, edits: Vec<FileEdit> },
    /// Closing a tab with unsaved edits.
    ConfirmClose(PathBuf),
    NewEntry { parent: PathBuf, folder: bool, input: Entity<InputState>, error: Option<String> },
    RenameEntry { path: PathBuf, input: Entity<InputState>, error: Option<String> },
    ConfirmDelete(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    Problems,
    References,
}

pub struct EditorWorkspace {
    root: PathBuf,
    mode: Mode,
    focus: FocusHandle,
    tree_focus: FocusHandle,
    tree: FileTree,
    tree_scroll: UniformListScrollHandle,
    buffers: Vec<Buffer>,
    active: Option<usize>,
    lsp: Entity<LspStore>,
    index: IndexSnapshot,
    indexing: bool,
    index_cancel: Arc<AtomicBool>,
    recents: Recents,
    overlay: Overlay,
    panel_open: bool,
    panel_tab: PanelTab,
    references: Vec<Location>,
    references_title: String,
    message: Option<(String, bool)>,
    branch: Option<String>,
    git_changes: usize,
    /// Reload clean buffers silently when the file changes on disk.
    pub auto_reload_clean: bool,
    /// While true (session restore), activating a tab does not start its
    /// language server; the first focus inside the view clears it.
    defer_lsp: bool,
    _watcher: Option<RootWatcher>,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
    _index_task: Task<()>,
    _git_task: Task<()>,
    _msg_task: Task<()>,
}

impl EventEmitter<EditorEvent> for EditorWorkspace {}

impl Focusable for EditorWorkspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn rel_of(root: &Path, path: &Path) -> String {
    let r = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
    if cfg!(windows) { r.replace('\\', "/") } else { r }
}

impl EditorWorkspace {
    pub fn new(root: PathBuf, mode: Mode, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let root = simplify(&root.canonicalize().unwrap_or(root));
        let lsp = cx.new(|_| LspStore::new(root.clone()));
        let sub = cx.subscribe_in(&lsp, window, |this: &mut Self, _, ev: &LspStoreEvent, window, cx| match ev {
            LspStoreEvent::DiagnosticsChanged(p) => this.apply_diagnostics(p, window, cx),
            LspStoreEvent::ServersChanged => cx.notify(),
        });
        let mut ws = EditorWorkspace {
            tree: FileTree::new(root.clone()),
            root: root.clone(),
            mode,
            focus: cx.focus_handle(),
            // Tracked handles ignore `.tab_index()`; the tree must be a tab stop itself.
            tree_focus: cx.focus_handle().tab_stop(true),
            tree_scroll: UniformListScrollHandle::new(),
            buffers: vec![],
            active: None,
            lsp,
            index: IndexSnapshot::default(),
            indexing: false,
            index_cancel: Arc::new(AtomicBool::new(false)),
            recents: Recents::default(),
            overlay: Overlay::None,
            panel_open: false,
            panel_tab: PanelTab::Problems,
            references: vec![],
            references_title: String::new(),
            message: None,
            branch: None,
            git_changes: 0,
            auto_reload_clean: true,
            defer_lsp: false,
            _watcher: None,
            _tasks: vec![],
            _subs: vec![sub],
            _index_task: Task::ready(()),
            _git_task: Task::ready(()),
            _msg_task: Task::ready(()),
        };
        let focus_in = cx.on_focus_in(&ws.focus, window, |this: &mut Self, _, cx| {
            if std::mem::take(&mut this.defer_lsp)
                && let Some(i) = this.active
            {
                this.ensure_lsp(i, cx);
            }
        });
        ws._subs.push(focus_in);
        ws.rebuild_index(cx);
        ws.refresh_git(cx);
        match RootWatcher::start(&root) {
            Ok((w, mut rx)) => {
                ws._watcher = Some(w);
                ws._tasks.push(cx.spawn_in(window, async move |this, cx| {
                    while let Some(batch) = rx.next().await {
                        if this.update_in(cx, |s, window, cx| s.on_fs_batch(batch, window, cx)).is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(e) => ws.message = Some((format!("파일 감시를 시작하지 못했습니다: {e}"), true)),
        }
        ws
    }

    // ---- public API --------------------------------------------------------

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn lsp(&self) -> &Entity<LspStore> {
        &self.lsp
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    /// Switch light/dark. The host applies `pod_ui::apply_theme` itself.
    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        cx.notify();
    }
    pub fn active_path(&self) -> Option<&Path> {
        self.active.and_then(|i| self.buffers.get(i)).map(|b| b.path.as_path())
    }
    pub fn open_paths(&self) -> Vec<PathBuf> {
        self.buffers.iter().map(|b| b.path.clone()).collect()
    }
    pub fn dirty_paths(&self) -> Vec<PathBuf> {
        self.buffers.iter().filter(|b| b.dirty).map(|b| b.path.clone()).collect()
    }
    /// The active tab's editor state (for host-driven scripting/tests).
    pub fn active_editor(&self) -> Option<Entity<EditorState>> {
        self.active.and_then(|i| self.buffers.get(i)).map(|b| b.state.clone())
    }
    pub fn tree(&self) -> &FileTree {
        &self.tree
    }
    pub fn index_len(&self) -> (usize, bool) {
        (self.index.files.len(), self.indexing)
    }

    /// Opens (or activates) a file. `path` may be relative to the root.
    pub fn open_file(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        self.open_at(path, None, window, cx)
    }

    /// Opens a file and moves the cursor to 1-based `line`/`col`.
    pub fn goto(&mut self, path: &Path, line: u32, col: u32, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        self.open_at(path, Some(lt::Position::new(line.saturating_sub(1), col.saturating_sub(1))), window, cx)
    }

    pub fn open_quick_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let qo = cx.new(|cx| QuickOpen::new(self.index.files.clone(), self.recents.items().to_vec(), self.indexing, self.index.truncated, self.mode, window, cx));
        let sub = cx.subscribe_in(&qo, window, |this: &mut Self, _, ev: &QuickOpenEvent, window, cx| match ev {
            QuickOpenEvent::Open(rel) => {
                this.close_overlay(window, cx);
                let p = this.root.join(rel);
                if let Err(e) = this.open_file(&p, window, cx) {
                    this.flash(e, true, cx);
                }
            }
            QuickOpenEvent::Dismiss => this.close_overlay(window, cx),
        });
        self._subs.push(sub);
        qo.update(cx, |q, cx| q.focus(window, cx));
        self.overlay = Overlay::QuickOpen(qo);
        cx.notify();
    }

    /// Sets the quick-open query (scripting; opens it if needed).
    pub fn quick_open_query(&mut self, q: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.overlay, Overlay::QuickOpen(_)) {
            self.open_quick_open(window, cx);
        }
        if let Overlay::QuickOpen(qo) = &self.overlay {
            qo.update(cx, |q2, cx| q2.set_query(q, window, cx));
        }
    }

    pub fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.active {
            self.save_buffer(i, window, cx);
        }
    }

    pub fn save_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for i in 0..self.buffers.len() {
            if self.buffers[i].dirty {
                self.save_buffer(i, window, cx);
            }
        }
    }

    /// Stops language servers (call before dropping the view / on quit).
    pub fn shutdown(&mut self, cx: &mut Context<Self>) -> Task<()> {
        self.index_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        self.lsp.update(cx, |s, cx| s.shutdown_all(cx))
    }

    pub fn show_panel(&mut self, references: bool, cx: &mut Context<Self>) {
        self.panel_open = true;
        self.panel_tab = if references { PanelTab::References } else { PanelTab::Problems };
        cx.notify();
    }

    /// Open tabs with cursor positions, for the host's layout file.
    pub fn session(&self, cx: &App) -> EditorSession {
        let tabs = self
            .buffers
            .iter()
            .map(|b| {
                let p = b.state.read(cx).cursor_position();
                TabSession { path: b.rel.clone(), line: p.line + 1, col: p.character + 1 }
            })
            .collect();
        EditorSession { tabs, active: self.active }
    }

    /// Reopens saved tabs (missing files are skipped) without starting a
    /// language server; the server starts when the view first gets focus.
    pub fn restore_session(&mut self, session: &EditorSession, window: &mut Window, cx: &mut Context<Self>) {
        self.defer_lsp = true;
        let mut active = None;
        for (i, t) in session.tabs.iter().enumerate() {
            let path = self.root.join(&t.path);
            if self.find_buffer(&path).is_some() {
                continue;
            }
            let Ok(loaded) = text::load(&path) else { continue };
            let b = self.make_buffer(&path, loaded, window, cx);
            let pos = lt::Position::new(t.line.saturating_sub(1), t.col.saturating_sub(1));
            b.state.update(cx, |s, cx| s.set_cursor_position(pos, window, cx));
            self.buffers.push(b);
            cx.emit(EditorEvent::FileOpened(path));
            if session.active == Some(i) {
                active = Some(self.buffers.len() - 1);
            }
        }
        if let Some(ix) = active.or((!self.buffers.is_empty()).then_some(0)) {
            self.activate(ix, window, cx);
            if let Some(p) = self.active_path().map(Path::to_path_buf) {
                self.tree.reveal(&p);
            }
        }
        cx.notify();
    }

    /// Whether the view has an open modal overlay (quick open, prompts).
    pub fn has_overlay(&self) -> bool {
        !matches!(self.overlay, Overlay::None)
    }

    // ---- files ---------------------------------------------------------------

    fn find_buffer(&self, path: &Path) -> Option<usize> {
        let k = path_key(path);
        self.buffers.iter().position(|b| path_key(&b.path) == k)
    }

    pub(crate) fn open_at(&mut self, path: &Path, pos: Option<lt::Position>, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        let path = if path.is_absolute() { simplify(path) } else { self.root.join(path) };
        let ix = match self.find_buffer(&path) {
            Some(i) => i,
            None => {
                let loaded = text::load(&path).map_err(|e| e.to_string())?;
                let b = self.make_buffer(&path, loaded, window, cx);
                self.buffers.push(b);
                cx.emit(EditorEvent::FileOpened(path.clone()));
                self.buffers.len() - 1
            }
        };
        self.activate(ix, window, cx);
        let state = self.buffers[ix].state.clone();
        if let Some(p) = pos {
            state.update(cx, |s, cx| s.set_cursor_position(p, window, cx));
        } else {
            state.update(cx, |s, cx| s.focus(window, cx));
        }
        let rel = self.buffers[ix].rel.clone();
        self.recents.touch(&rel);
        self.tree.reveal(&path);
        if let Some(i) = self.tree.selected_index() {
            self.tree_scroll.scroll_to_item(i, ScrollStrategy::Nearest);
        }
        cx.notify();
        Ok(())
    }

    fn make_buffer(&mut self, path: &Path, loaded: text::Loaded, window: &mut Window, cx: &mut Context<Self>) -> Buffer {
        let language = language_for_path(path);
        let large = loaded.large;
        let initial = loaded.text.clone();
        let state = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).line_number(true).searchable(true).soft_wrap(false);
            if let (Some(l), false) = (language, large) {
                s = s.language(l.highlight);
            }
            s.default_value(initial)
        });
        if large {
            state.update(cx, |s, cx| s.set_readonly(true, cx));
        }
        if !large && language.is_some() {
            let apply: crate::lsp::providers::ApplyEdits = {
                let ws = cx.entity().downgrade();
                Rc::new(move |edits, window: &mut Window, cx: &mut App| {
                    let _ = ws.update(cx, |ws, cx| {
                        let n = ws.apply_file_edits(edits, window, cx);
                        ws.flash(format!("코드 작업을 적용했습니다: 파일 {n}개 (저장 전)"), false, cx);
                    });
                })
            };
            let bridge = LspBridge::with_apply(self.lsp.clone(), path.to_path_buf(), Some(apply));
            let ws = cx.entity().downgrade();
            let show: ShowDocumentHandler = Rc::new(move |params: &lt::ShowDocumentParams, window: &mut Window, cx: &mut App| {
                let Some(target) = uri_to_path(&params.uri) else { return false };
                let pos = params.selection.map(|r| r.start);
                let ws = ws.clone();
                // The editor is mid-update here: defer so a same-file jump can update it.
                window.defer(cx, move |window, cx| {
                    let _ = ws.update(cx, |ws, cx| {
                        if let Err(e) = ws.open_at(&target, pos, window, cx) {
                            ws.flash(e, true, cx);
                        }
                    });
                });
                true
            });
            state.update(cx, |s, cx| {
                let l = s.lsp_mut();
                l.completion_provider = Some(bridge.clone());
                l.hover_provider = Some(bridge.clone());
                l.code_action_providers = vec![bridge.clone()];
                l.definition_provider = Some(bridge);
                l.show_document = Some(show);
                cx.notify();
            });
        }
        let p2 = path.to_path_buf();
        let change = cx.subscribe_in(&state, window, move |this: &mut Self, _, ev: &InputEvent, window, cx| {
            if let InputEvent::Change = ev {
                this.on_buffer_changed(&p2, window, cx);
            }
        });
        // Incremental sync: each edit goes to the store as a ranged change
        // (the debounced full-text sync below stays as a repair path).
        let p3 = path.to_path_buf();
        let lsp = self.lsp.clone();
        let edits = cx.subscribe(&state, move |_, _, ev: &InputEditsEvent, cx| {
            for (e, after) in ev.edits() {
                let new_text = after.slice(e.start_byte..e.new_end_byte).to_string();
                lsp.update(cx, |s, _| s.edit_document(&p3, e.start_byte, e.old_end_byte, &new_text));
            }
        });
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut b = Buffer {
            path: path.to_path_buf(),
            rel: rel_of(&self.root, path),
            state,
            language,
            ending: loaded.ending,
            bom: loaded.had_bom,
            saved: loaded.text,
            dirty: false,
            large,
            disk: DiskState::Same,
            lsp_open: false,
            _subs: vec![change, edits, observe],
            _sync: Task::ready(()),
        };
        // Diagnostics that arrived before the tab existed.
        let diags = self.lsp.read(cx).diagnostics_for(path).to_vec();
        if !diags.is_empty() {
            set_editor_diagnostics(&b.state, diags, cx);
        }
        b.dirty = false;
        b
    }

    fn on_buffer_changed(&mut self, path: &Path, _: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.find_buffer(path) else { return };
        let text = self.buffers[i].state.read(cx).value().to_string();
        let dirty = text != self.buffers[i].saved;
        if dirty != self.buffers[i].dirty {
            self.buffers[i].dirty = dirty;
            cx.emit(EditorEvent::DirtyChanged { path: path.to_path_buf(), dirty });
        }
        if self.buffers[i].language.is_some() && !self.buffers[i].large {
            // Debounced full-text sync; providers flush synchronously anyway.
            let lsp = self.lsp.clone();
            let p = path.to_path_buf();
            self.buffers[i]._sync = cx.spawn(async move |_, cx| {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                lsp.update(cx, |s, _| s.change_document(&p, text));
            });
        }
        cx.notify();
    }

    fn save_buffer(&mut self, i: usize, _: &mut Window, cx: &mut Context<Self>) {
        let b = &self.buffers[i];
        let text = b.state.read(cx).value().to_string();
        let bytes = text::encode(&text, b.ending, b.bom);
        let path = b.path.clone();
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                let b = &mut self.buffers[i];
                b.saved = text.clone();
                b.disk = DiskState::Same;
                if b.dirty {
                    b.dirty = false;
                    cx.emit(EditorEvent::DirtyChanged { path: path.clone(), dirty: false });
                }
                self.lsp.update(cx, |s, _| {
                    s.change_document(&path, text);
                    s.save_document(&path);
                });
                cx.emit(EditorEvent::FileSaved(path.clone()));
                let rel = self.buffers[i].rel.clone();
                self.flash(format!("저장했습니다: {rel}"), false, cx);
            }
            Err(e) => self.flash(format!("저장하지 못했습니다: {e}"), true, cx),
        }
        cx.notify();
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.active != Some(ix) {
            self.active = Some(ix);
            cx.emit(EditorEvent::ActiveFileChanged(Some(self.buffers[ix].path.clone())));
        }
        if !self.defer_lsp {
            self.ensure_lsp(ix, cx);
        }
        let _ = window;
        cx.notify();
    }

    /// Registers buffer `ix` with its language server (starting it on first
    /// use), or marks the server used so the host's LRU cap keeps it.
    fn ensure_lsp(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(b) = self.buffers.get_mut(ix) else { return };
        if b.language.is_none() || b.large {
            return;
        }
        let path = b.path.clone();
        if b.lsp_open {
            self.lsp.update(cx, |s, cx| s.touch(&path, cx));
            return;
        }
        b.lsp_open = true;
        let text_now = b.state.read(cx).value().to_string();
        self.lsp.update(cx, |s, cx| s.open_document(&path, text_now, cx));
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.buffers.len() {
            return;
        }
        if self.buffers[ix].dirty {
            // Unsaved: ask (save / discard / cancel).
            self.overlay = Overlay::ConfirmClose(self.buffers[ix].path.clone());
            self.focus.focus(window, cx);
            cx.notify();
            return;
        }
        self.remove_tab(ix, window, cx);
    }

    /// Closes the tab of `path` (if open). With `force`, unsaved edits are
    /// discarded; otherwise a dirty tab asks first.
    pub fn close_path(&mut self, path: &Path, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.find_buffer(path) else { return };
        if force {
            self.remove_tab(ix, window, cx);
        } else {
            self.close_tab(ix, window, cx);
        }
    }

    fn resolve_close(&mut self, save: Option<bool>, window: &mut Window, cx: &mut Context<Self>) {
        let Overlay::ConfirmClose(path) = std::mem::replace(&mut self.overlay, Overlay::None) else { return };
        let Some(ix) = self.find_buffer(&path) else { return };
        match save {
            Some(true) => {
                self.save_buffer(ix, window, cx);
                if !self.buffers[ix].dirty {
                    self.remove_tab(ix, window, cx);
                }
            }
            Some(false) => self.remove_tab(ix, window, cx),
            None => self.close_overlay(window, cx),
        }
        cx.notify();
    }

    fn remove_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffers[ix].dirty {
            let p = self.buffers[ix].path.clone();
            cx.emit(EditorEvent::DirtyChanged { path: p, dirty: false });
        }
        let b = self.buffers.remove(ix);
        self.lsp.update(cx, |s, _| s.close_document(&b.path));
        cx.emit(EditorEvent::FileClosed(b.path.clone()));
        self.active = if self.buffers.is_empty() {
            None
        } else {
            Some(match self.active {
                Some(a) if a > ix => a - 1,
                Some(a) if a == ix => ix.min(self.buffers.len() - 1),
                Some(a) => a,
                None => 0,
            })
        };
        if let Some(a) = self.active {
            let st = self.buffers[a].state.clone();
            st.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.emit(EditorEvent::ActiveFileChanged(self.active_path().map(Path::to_path_buf)));
        cx.notify();
    }

    fn flash(&mut self, msg: impl Into<String>, error: bool, cx: &mut Context<Self>) {
        self.message = Some((msg.into(), error));
        self._msg_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(if error { 8 } else { 4 })).await;
            let _ = this.update(cx, |s, cx| {
                s.message = None;
                cx.notify();
            });
        });
        cx.notify();
    }

    // ---- background: index, git, watcher ------------------------------------

    fn rebuild_index(&mut self, cx: &mut Context<Self>) {
        self.index_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.index_cancel = cancel.clone();
        self.indexing = true;
        let root = self.root.clone();
        let job = cx.background_spawn(async move { index::build(&root, &cancel) });
        self._index_task = cx.spawn(async move |this, cx| {
            let snap = job.await;
            let _ = this.update(cx, |s, cx| {
                s.index = snap;
                s.indexing = false;
                if let Overlay::QuickOpen(qo) = &s.overlay {
                    let (files, tr) = (s.index.files.clone(), s.index.truncated);
                    qo.update(cx, |q, cx| q.set_files(files, false, tr, cx));
                }
                cx.notify();
            });
        });
    }

    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let job = cx.background_spawn(async move { git::read_status(&root) });
        self._git_task = cx.spawn(async move |this, cx| {
            let st = job.await;
            let _ = this.update(cx, |s, cx| {
                s.branch = st.branch.clone();
                s.git_changes = st.changed_count();
                s.tree.set_git(st);
                cx.notify();
            });
        });
    }

    fn on_fs_batch(&mut self, batch: Batch, window: &mut Window, cx: &mut Context<Self>) {
        if !batch.paths.is_empty() {
            if batch.paths.iter().any(|p| p == &self.root) {
                self.tree.refresh_all();
            } else {
                self.tree.refresh_paths(&batch.paths);
            }
            // Re-index on creations/deletions only.
            let files = self.index.files.clone();
            let structural = batch.paths.iter().any(|p| {
                let rel = rel_of(&self.root, p);
                let known = files.iter().any(|f| f == &rel);
                (p.is_file() && !known) || (!p.exists() && (known || files.iter().any(|f| f.starts_with(&format!("{rel}/")))))
            });
            if structural {
                self.rebuild_index(cx);
            }
            for p in &batch.paths {
                if let Some(i) = self.find_buffer(p) {
                    self.check_disk(i, window, cx);
                }
            }
        }
        self.refresh_git(cx);
        cx.notify();
    }

    /// Compares an open buffer with the file on disk after a change event.
    fn check_disk(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.buffers[i].path.clone();
        if !path.exists() {
            self.buffers[i].disk = DiskState::Deleted;
            cx.emit(EditorEvent::ExternalChange { path, reloaded: false });
            return;
        }
        let Ok(loaded) = text::load(&path) else { return };
        let b = &mut self.buffers[i];
        if loaded.text == b.saved {
            b.disk = DiskState::Same; // our own save, or a touch
            return;
        }
        if !b.dirty && self.auto_reload_clean {
            self.reload_buffer(i, loaded, window, cx);
            let rel = self.buffers[i].rel.clone();
            self.flash(format!("디스크에서 바뀐 내용을 다시 불러왔습니다: {rel}"), false, cx);
            cx.emit(EditorEvent::ExternalChange { path, reloaded: true });
        } else {
            b.disk = DiskState::Changed(loaded.text);
            cx.emit(EditorEvent::ExternalChange { path, reloaded: false });
        }
    }

    fn reload_buffer(&mut self, i: usize, loaded: text::Loaded, window: &mut Window, cx: &mut Context<Self>) {
        let b = &mut self.buffers[i];
        b.ending = loaded.ending;
        b.bom = loaded.had_bom;
        b.saved = loaded.text.clone();
        b.disk = DiskState::Same;
        let was_dirty = b.dirty;
        b.dirty = false;
        let state = b.state.clone();
        let path = b.path.clone();
        let text_v = loaded.text.clone();
        state.update(cx, |s, cx| {
            let pos = s.cursor_position();
            s.set_value(text_v, window, cx);
            s.set_cursor_position(pos, window, cx);
        });
        self.lsp.update(cx, |s, _| s.change_document(&path, loaded.text));
        if was_dirty {
            cx.emit(EditorEvent::DirtyChanged { path, dirty: false });
        }
        cx.notify();
    }

    fn resolve_disk_prompt(&mut self, reload: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.active else { return };
        match std::mem::replace(&mut self.buffers[i].disk, DiskState::Same) {
            DiskState::Changed(_) if reload => {
                if let Ok(l) = text::load(&self.buffers[i].path) {
                    self.reload_buffer(i, l, window, cx);
                }
            }
            DiskState::Changed(disk_text) => {
                // Keep mine: the disk version becomes the comparison base so
                // the buffer stays dirty and saving overwrites it.
                self.buffers[i].saved = disk_text;
            }
            DiskState::Deleted if reload => {
                self.buffers[i].dirty = false;
                self.close_tab(i, window, cx);
            }
            DiskState::Deleted => {
                self.buffers[i].saved = String::new();
                self.buffers[i].dirty = true;
            }
            DiskState::Same => {}
        }
        cx.notify();
    }

    // ---- diagnostics ---------------------------------------------------------

    fn apply_diagnostics(&mut self, path: &Path, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.find_buffer(path) {
            let diags = self.lsp.read(cx).diagnostics_for(path).to_vec();
            set_editor_diagnostics(&self.buffers[i].state, diags, cx);
        }
        cx.notify();
    }

    fn next_problem(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut all: Vec<(PathBuf, lt::Position)> = vec![];
        for f in self.lsp.read(cx).diagnostics() {
            for d in &f.items {
                all.push((f.path.clone(), d.range.start));
            }
        }
        if all.is_empty() {
            self.flash("문제가 없습니다", false, cx);
            return;
        }
        all.sort_by(|a, b| a.0.cmp(&b.0).then((a.1.line, a.1.character).cmp(&(b.1.line, b.1.character))));
        let cur = self.active.map(|i| (self.buffers[i].path.clone(), self.buffers[i].state.read(cx).cursor_position()));
        let next = cur
            .and_then(|(p, c)| all.iter().find(|(fp, pos)| (path_key(fp) == path_key(&p) && (pos.line, pos.character) > (c.line, c.character)) || fp > &p).cloned())
            .unwrap_or_else(|| all[0].clone());
        let _ = self.open_at(&next.0, Some(next.1), window, cx);
    }

    // ---- LSP actions -----------------------------------------------------------

    fn cursor_target(&self, cx: &App) -> Option<(PathBuf, lt::Position, Entity<EditorState>)> {
        let b = self.buffers.get(self.active?)?;
        let st = b.state.read(cx);
        Some((b.path.clone(), st.text().offset_to_position(st.cursor()), b.state.clone()))
    }

    fn flush_active(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.active {
            let text = self.buffers[i].state.read(cx).value().to_string();
            let p = self.buffers[i].path.clone();
            self.lsp.update(cx, |s, _| s.change_document(&p, text));
        }
    }

    pub fn find_references(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, pos, _)) = self.cursor_target(cx) else { return };
        self.flush_active(cx);
        let task = self.lsp.update(cx, |s, cx| s.references(&path, pos, cx));
        let word = self.word_at_cursor(cx).unwrap_or_default();
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update(cx, |s, cx| match res {
                Ok(v) => {
                    s.references_title = format!("`{word}` 참조 {}개", v.len());
                    s.references = v;
                    s.show_panel(true, cx);
                }
                Err(e) => s.flash(format!("참조를 찾지 못했습니다: {e}"), true, cx),
            });
        }));
    }

    fn word_at_cursor(&self, cx: &App) -> Option<String> {
        let b = self.buffers.get(self.active?)?;
        let st = b.state.read(cx);
        let text: &Rope = st.text();
        let r = text.word_range(st.cursor())?;
        Some(text.slice(r).to_string())
    }

    pub fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, pos, _)) = self.cursor_target(cx) else { return };
        let Some(word) = self.word_at_cursor(cx) else {
            self.flash("커서 위치에 이름이 없습니다", true, cx);
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("새 이름").default_value(word));
        let sub = cx.subscribe_in(&input, window, |this: &mut Self, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.request_rename(window, cx);
            }
        });
        self._subs.push(sub);
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        self.overlay = Overlay::Rename { input, path, pos };
        cx.notify();
    }

    fn request_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Overlay::Rename { input, path, pos } = &self.overlay else { return };
        let new_name = input.read(cx).value().trim().to_string();
        if new_name.is_empty() {
            return;
        }
        let (path, pos) = (path.clone(), *pos);
        self.flush_active(cx);
        let task = self.lsp.update(cx, |s, cx| s.rename(&path, pos, new_name.clone(), cx));
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |s, window, cx| match res {
                Ok(edits) if edits.iter().all(|f| f.edits.is_empty()) => {
                    s.close_overlay(window, cx);
                    s.flash("바꿀 곳이 없습니다", false, cx);
                }
                Ok(edits) => {
                    s.overlay = Overlay::RenamePreview { new_name, edits };
                    s.focus.focus(window, cx);
                    cx.notify();
                }
                Err(e) => {
                    s.close_overlay(window, cx);
                    s.flash(format!("이름을 바꾸지 못했습니다: {e}"), true, cx);
                }
            });
        }));
    }

    /// Applies the previewed rename to buffers (opening files as needed).
    /// Files end up dirty; the user reviews and saves (Ctrl+K S saves all).
    pub fn apply_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Overlay::RenamePreview { edits, .. } = std::mem::replace(&mut self.overlay, Overlay::None) else { return };
        let changed = self.apply_file_edits(edits, window, cx);
        self.flash(format!("파일 {changed}개에서 이름을 바꿨습니다 (저장 전)"), false, cx);
        cx.notify();
    }

    /// Applies LSP file edits (rename, code actions) to buffers, opening
    /// files as needed; returns how many files changed. Files end up dirty.
    pub fn apply_file_edits(&mut self, edits: Vec<FileEdit>, window: &mut Window, cx: &mut Context<Self>) -> usize {
        let mut changed = 0;
        let keep_active = self.active;
        for fe in edits {
            if self.find_buffer(&fe.path).is_none() && self.open_at(&fe.path, None, window, cx).is_err() {
                continue;
            }
            let Some(i) = self.find_buffer(&fe.path) else { continue };
            let st = self.buffers[i].state.clone();
            let new_text = apply_edits(&st.read(cx).value(), &fe.edits);
            st.update(cx, |s, cx| {
                let pos = s.cursor_position();
                s.set_value(new_text, window, cx);
                s.set_cursor_position(pos, window, cx);
            });
            self.on_buffer_changed(&fe.path, window, cx);
            changed += 1;
        }
        if let Some(a) = keep_active
            && a < self.buffers.len() {
                self.activate(a, window, cx);
            }
        cx.notify();
        changed
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = Overlay::None;
        if let Some(st) = self.active_editor() {
            st.update(cx, |s, cx| s.focus(window, cx));
        } else {
            self.tree_focus.focus(window, cx);
        }
        cx.notify();
    }

    // ---- action handlers -------------------------------------------------------

    fn on_quick_open(&mut self, _: &QuickOpenFile, window: &mut Window, cx: &mut Context<Self>) {
        self.open_quick_open(window, cx);
    }
    fn on_save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        self.save_active(window, cx);
    }
    fn on_save_all(&mut self, _: &SaveAll, window: &mut Window, cx: &mut Context<Self>) {
        self.save_all(window, cx);
    }
    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.active {
            self.close_tab(i, window, cx);
        }
    }
    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, window, cx);
    }
    fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, window, cx);
    }
    fn cycle_tab(&mut self, d: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.buffers.len() as isize;
        if n == 0 {
            return;
        }
        let i = ((self.active.unwrap_or(0) as isize + d).rem_euclid(n)) as usize;
        self.activate(i, window, cx);
        let st = self.buffers[i].state.clone();
        st.update(cx, |s, cx| s.focus(window, cx));
    }
    fn on_goto_line(&mut self, _: &GotoLine, window: &mut Window, cx: &mut Context<Self>) {
        self.open_goto_line(window, cx);
    }
    pub fn open_goto_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.is_none() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("줄 번호 또는 줄:열 (예: 42:5)"));
        let sub = cx.subscribe_in(&input, window, |this: &mut Self, inp, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                let v = inp.read(cx).value().to_string();
                this.close_overlay(window, cx);
                if let Some(pos) = parse_line_col(&v)
                    && let Some(st) = this.active_editor() {
                        st.update(cx, |s, cx| s.set_cursor_position(pos, window, cx));
                    }
            }
        });
        self._subs.push(sub);
        input.update(cx, |s, cx| s.focus(window, cx));
        self.overlay = Overlay::GotoLine(input);
        cx.notify();
    }
    fn on_find_references(&mut self, _: &FindReferences, window: &mut Window, cx: &mut Context<Self>) {
        self.find_references(window, cx);
    }
    fn on_rename(&mut self, _: &RenameSymbol, window: &mut Window, cx: &mut Context<Self>) {
        self.start_rename(window, cx);
    }
    fn on_next_problem(&mut self, _: &NextProblem, window: &mut Window, cx: &mut Context<Self>) {
        self.next_problem(window, cx);
    }
    fn on_toggle_panel(&mut self, _: &TogglePanel, _: &mut Window, cx: &mut Context<Self>) {
        self.panel_open = !self.panel_open;
        cx.notify();
    }
    fn on_focus_tree(&mut self, _: &FocusTree, window: &mut Window, cx: &mut Context<Self>) {
        self.tree_focus.focus(window, cx);
        if self.tree.selected().is_none() {
            self.tree.move_home();
        }
        cx.notify();
    }
    fn on_focus_editor(&mut self, _: &FocusEditor, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(st) = self.active_editor() {
            st.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    fn tree_moved(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.tree.selected_index() {
            self.tree_scroll.scroll_to_item(i, ScrollStrategy::Nearest);
        }
        cx.notify();
    }
    fn on_tree_up(&mut self, _: &TreeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.tree.move_up();
        self.tree_moved(cx);
    }
    fn on_tree_down(&mut self, _: &TreeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.tree.move_down();
        self.tree_moved(cx);
    }
    fn on_tree_left(&mut self, _: &TreeLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.tree.move_left();
        self.tree_moved(cx);
    }
    fn on_tree_right(&mut self, _: &TreeRight, _: &mut Window, cx: &mut Context<Self>) {
        self.tree.move_right();
        self.tree_moved(cx);
    }
    fn on_tree_home(&mut self, _: &TreeHome, _: &mut Window, cx: &mut Context<Self>) {
        self.tree.move_home();
        self.tree_moved(cx);
    }
    fn on_tree_end(&mut self, _: &TreeEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.tree.move_end();
        self.tree_moved(cx);
    }
    fn on_tree_open(&mut self, _: &TreeOpen, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(i) = self.tree.selected_index() {
            self.tree_activate(i, false, window, cx);
        }
    }
    fn tree_target(&self) -> Option<(PathBuf, bool)> {
        let i = self.tree.selected_index()?;
        let r = self.tree.rows().get(i)?;
        Some((r.path.clone(), r.is_dir))
    }
    fn on_tree_new_file(&mut self, _: &TreeNewFile, window: &mut Window, cx: &mut Context<Self>) {
        let parent = match self.tree_target() {
            Some((p, true)) => p,
            Some((p, false)) => p.parent().map(Path::to_path_buf).unwrap_or_else(|| self.root.clone()),
            None => self.root.clone(),
        };
        self.start_new_entry(parent, false, window, cx);
    }
    fn on_tree_rename(&mut self, _: &TreeRename, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((p, _)) = self.tree_target() {
            self.start_rename_entry(p, window, cx);
        }
    }
    fn on_tree_delete(&mut self, _: &TreeDelete, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((p, _)) = self.tree_target() {
            self.overlay = Overlay::ConfirmDelete(p);
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    /// Tree: "new file/folder" prompt under `parent`.
    pub fn start_new_entry(&mut self, parent: PathBuf, folder: bool, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(txt::NAME_PLACEHOLDER));
        let sub = cx.subscribe_in(&input, window, |this: &mut Self, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.commit_new_entry(window, cx);
            }
        });
        self._subs.push(sub);
        input.update(cx, |s, cx| s.focus(window, cx));
        self.overlay = Overlay::NewEntry { parent, folder, input, error: None };
        cx.notify();
    }

    fn commit_new_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Overlay::NewEntry { parent, folder, input, .. } = &self.overlay else { return };
        let (parent, folder) = (parent.clone(), *folder);
        let name = input.read(cx).value().trim().replace('\\', "/");
        let target = match valid_child(&self.root, &parent, &name) {
            Ok(t) => t,
            Err(e) => return self.set_overlay_error(e, cx),
        };
        if target.exists() {
            return self.set_overlay_error(txt::ALREADY_EXISTS.into(), cx);
        }
        let res = if folder {
            std::fs::create_dir_all(&target)
        } else {
            target.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::write(&target, ""))
        };
        match res {
            Ok(()) => {
                self.close_overlay(window, cx);
                self.tree.refresh_paths(&[parent]);
                let rel = rel_of(&self.root, &target);
                self.flash(txt::created(&rel), false, cx);
                if folder {
                    self.tree.reveal(&target);
                } else if let Err(e) = self.open_file(&target, window, cx) {
                    self.flash(e, true, cx);
                }
                self.rebuild_index(cx);
            }
            Err(e) => self.set_overlay_error(txt::fs_error("만들지", &e), cx),
        }
    }

    pub fn start_rename_entry(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
        let sub = cx.subscribe_in(&input, window, |this: &mut Self, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.commit_rename_entry(window, cx);
            }
        });
        self._subs.push(sub);
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        self.overlay = Overlay::RenameEntry { path, input, error: None };
        cx.notify();
    }

    fn commit_rename_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Overlay::RenameEntry { path, input, .. } = &self.overlay else { return };
        let path = path.clone();
        let name = input.read(cx).value().trim().to_string();
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.root.clone());
        if name.contains(['/', '\\']) {
            return self.set_overlay_error(txt::INVALID_NAME.into(), cx);
        }
        let target = match valid_child(&self.root, &parent, &name) {
            Ok(t) => t,
            Err(e) => return self.set_overlay_error(e, cx),
        };
        if target == path {
            return self.close_overlay(window, cx);
        }
        // Case-only renames are fine on Windows; anything else must be new.
        if target.exists() && path_key(&target) != path_key(&path) {
            return self.set_overlay_error(txt::ALREADY_EXISTS.into(), cx);
        }
        match std::fs::rename(&path, &target) {
            Ok(()) => {
                self.close_overlay(window, cx);
                self.retarget_buffers(&path, &target, cx);
                self.tree.refresh_paths(&[parent]);
                self.tree.reveal(&target);
                let (a, b) = (rel_of(&self.root, &path), rel_of(&self.root, &target));
                self.flash(txt::renamed(&a, &b), false, cx);
                self.rebuild_index(cx);
            }
            Err(e) => self.set_overlay_error(txt::fs_error("이름을 바꾸지", &e), cx),
        }
    }

    /// Open buffers at/under `from` follow a rename to `to`.
    fn retarget_buffers(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        let fk = path_key(from);
        for i in 0..self.buffers.len() {
            let old = self.buffers[i].path.clone();
            let k = path_key(&old);
            let new = if k == fk {
                to.to_path_buf()
            } else if let Ok(rest) = old.strip_prefix(from) {
                to.join(rest)
            } else {
                continue;
            };
            let text_now = self.buffers[i].state.read(cx).value().to_string();
            let reopen = self.buffers[i].lsp_open;
            self.lsp.update(cx, |s, cx| {
                s.close_document(&old);
                if reopen {
                    s.open_document(&new, text_now, cx);
                }
            });
            let rel = rel_of(&self.root, &new);
            let b = &mut self.buffers[i];
            b.path = new.clone();
            b.rel = rel;
            cx.emit(EditorEvent::FileClosed(old));
            cx.emit(EditorEvent::FileOpened(new));
        }
        cx.emit(EditorEvent::ActiveFileChanged(self.active_path().map(Path::to_path_buf)));
    }

    fn commit_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Overlay::ConfirmDelete(path) = std::mem::replace(&mut self.overlay, Overlay::None) else { return };
        let dir = path.is_dir();
        let res = if dir { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        match res {
            Ok(()) => {
                // Open tabs of deleted files close (they are gone; unsaved
                // edits there were confirmed away with the delete).
                let doomed: Vec<PathBuf> = self.buffers.iter().filter(|b| path_key(&b.path) == path_key(&path) || b.path.starts_with(&path)).map(|b| b.path.clone()).collect();
                for p in doomed {
                    if let Some(ix) = self.find_buffer(&p) {
                        self.remove_tab(ix, window, cx);
                    }
                }
                if let Some(parent) = path.parent() {
                    self.tree.refresh_paths(&[parent.to_path_buf()]);
                }
                self.flash(txt::deleted(&rel_of(&self.root, &path)), false, cx);
                self.rebuild_index(cx);
                self.close_overlay(window, cx);
            }
            Err(e) => self.flash(txt::fs_error("삭제", &e), true, cx),
        }
        cx.notify();
    }

    fn set_overlay_error(&mut self, e: String, cx: &mut Context<Self>) {
        match &mut self.overlay {
            Overlay::NewEntry { error, .. } | Overlay::RenameEntry { error, .. } => *error = Some(e),
            _ => {}
        }
        cx.notify();
    }

    fn tree_activate(&mut self, ix: usize, focus_editor: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let TreeAction::Open(p) = self.tree.activate(ix) {
            match self.open_at(&p, None, window, cx) {
                Ok(()) if !focus_editor => self.tree_focus.focus(window, cx),
                Ok(()) => {}
                Err(e) => self.flash(e, true, cx),
            }
        }
        cx.notify();
    }

    // ---- rendering ------------------------------------------------------------

    fn render_tree(&mut self, t: Tokens, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.tree_focus.contains_focused(window, cx);
        let mode = self.mode;
        let root_name = self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let count = self.tree.rows().len();
        div()
            .id("pod-editor-tree")
            .track_focus(&self.tree_focus)
            .key_context(TREE_CTX)
            .on_action(cx.listener(Self::on_tree_up))
            .on_action(cx.listener(Self::on_tree_down))
            .on_action(cx.listener(Self::on_tree_left))
            .on_action(cx.listener(Self::on_tree_right))
            .on_action(cx.listener(Self::on_tree_open))
            .on_action(cx.listener(Self::on_tree_home))
            .on_action(cx.listener(Self::on_tree_end))
            .on_action(cx.listener(Self::on_tree_new_file))
            .on_action(cx.listener(Self::on_tree_rename))
            .on_action(cx.listener(Self::on_tree_delete))
            .tab_index(0)
            .role(Role::Tree)
            .aria_label(txt::TREE_LABEL)
            .focus_visible(move |st| pod_ui::focus_style(st, mode))
            .w(px(TREE_W))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(tc(t.bg_chrome))
            .border_r_1()
            .border_color(tc(t.border_subtle))
            .child(
                div()
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(tc(t.text_primary)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(root_name))
                    .child(div().text_size(px(12.)).text_color(tc(t.text_muted)).child(if self.git_changes > 0 { format!("변경 {}", self.git_changes) } else { String::new() })),
            )
            .child(
                uniform_list(
                    "pod-editor-tree-rows",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        let t = Tokens::for_mode(this.mode);
                        let sel = this.tree.selected_index();
                        let active_key = this.active_path().map(path_key);
                        range
                            .map(|ix| {
                                let r = &this.tree.rows()[ix];
                                let is_sel = sel == Some(ix);
                                let is_active = active_key.as_deref() == Some(path_key(&r.path).as_str());
                                let mark_color = r.mark.map(|m| mark_color(m, t));
                                let (row_path, row_dir) = (r.path.clone(), r.is_dir);
                                let ws = cx.entity().downgrade();
                                let row_label = match r.mark {
                                    Some(m) if !r.is_dir => format!("{} ({})", r.name, m.letter()),
                                    _ => r.name.clone(),
                                };
                                let name_color = mark_color.unwrap_or(t.text_primary);
                                let chevron = if r.is_dir {
                                    let mut s = icon(Icon::ChevronRight, t.text_secondary).size(px(14.));
                                    if r.expanded {
                                        s = s.with_transformation(Transformation::rotate(radians(std::f32::consts::FRAC_PI_2)));
                                    }
                                    div().w(px(16.)).flex_none().flex().items_center().justify_center().child(s).into_any_element()
                                } else {
                                    div().w(px(16.)).flex_none().into_any_element()
                                };
                                div()
                                    .id(("tree-row", ix))
                                    .role(Role::TreeItem)
                                    .aria_label(row_label)
                                    .aria_selected(is_sel)
                                    .w_full()
                                    .h(px(TREE_ROW_H))
                                    .pl(px(8. + r.depth as f32 * pod_ui::tokens::TREE_INDENT))
                                    .pr(px(8.))
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .cursor_pointer()
                                    .text_size(px(13.))
                                    .when(is_sel, |d| d.bg(tc(t.bg_selected)))
                                    .when(is_sel && focused, |d| d.border_1().border_color(tc(t.accent_fg)))
                                    .when(!is_sel, |d| d.hover(|d| d.bg(tc(t.bg_hover))))
                                    .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                                        this.tree_focus.focus(window, cx);
                                        this.tree_activate(ix, e.click_count() > 1, window, cx);
                                    }))
                                    .context_menu(move |menu, _, _| {
                                        let dir = if row_dir { row_path.clone() } else { row_path.parent().map(Path::to_path_buf).unwrap_or_else(|| row_path.clone()) };
                                        let (w1, w2, w3, w4) = (ws.clone(), ws.clone(), ws.clone(), ws.clone());
                                        let (d1, d2, p3, p4) = (dir.clone(), dir, row_path.clone(), row_path.clone());
                                        menu.item(PopupMenuItem::new(txt::NEW_FILE).on_click(move |_, window, cx| {
                                            let _ = w1.update(cx, |w, cx| w.start_new_entry(d1.clone(), false, window, cx));
                                        }))
                                        .item(PopupMenuItem::new(txt::NEW_FOLDER).on_click(move |_, window, cx| {
                                            let _ = w2.update(cx, |w, cx| w.start_new_entry(d2.clone(), true, window, cx));
                                        }))
                                        .separator()
                                        .item(PopupMenuItem::new(txt::RENAME).on_click(move |_, window, cx| {
                                            let _ = w3.update(cx, |w, cx| w.start_rename_entry(p3.clone(), window, cx));
                                        }))
                                        .item(PopupMenuItem::new(txt::DELETE).on_click(move |_, window, cx| {
                                            let _ = w4.update(cx, |w, cx| {
                                                w.overlay = Overlay::ConfirmDelete(p4.clone());
                                                w.focus.focus(window, cx);
                                                cx.notify();
                                            });
                                        }))
                                    })
                                    .child(chevron)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_color(tc(name_color))
                                            .when(is_active, |d| d.font_weight(FontWeight::SEMIBOLD))
                                            .child(r.name.clone()),
                                    )
                                    .when_some(r.mark, |d, m| d.child(div().flex_none().w(px(14.)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(tc(mark_color.unwrap_or(t.text_muted))).child(if r.is_dir { "•" } else { m.letter() })))
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.tree_scroll)
                .flex_1(),
            )
            .into_any_element()
    }

    fn render_tabs(&mut self, t: Tokens, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("pod-editor-tabs")
            .role(Role::TabList)
            .aria_label(txt::TABS_LABEL)
            .h(px(32.))
            .flex_none()
            .flex()
            .items_end()
            .bg(tc(t.bg_chrome))
            .border_b_1()
            .border_color(tc(t.border_subtle))
            .overflow_x_scroll()
            .children(self.buffers.iter().enumerate().map(|(ix, b)| {
                let active = self.active == Some(ix);
                let name = b.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let has_err = self.lsp.read(cx).diagnostics_for(&b.path).iter().any(|d| d.severity == Some(lt::DiagnosticSeverity::ERROR));
                div()
                    .id(("tab", ix))
                    .role(Role::Tab)
                    .aria_label(txt::tab_label(&name, b.dirty))
                    .aria_selected(active)
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .flex_none()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .border_r_1()
                    .border_color(tc(t.border_subtle))
                    .when(active, |d| d.bg(tc(t.bg_surface)).text_color(tc(t.text_primary)).border_b_2().border_color(tc(t.accent_fg)))
                    .when(!active, |d| d.text_color(tc(t.text_secondary)).hover(|d| d.bg(tc(t.bg_hover))))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, window, cx| {
                            this.close_tab(ix, window, cx);
                        }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate(ix, window, cx);
                        let st = this.buffers[ix].state.clone();
                        st.update(cx, |s, cx| s.focus(window, cx));
                    }))
                    .child(div().when(has_err, |d| d.text_color(tc(t.danger_fg))).child(name.clone()))
                    .child(
                        div()
                            .id(("tab-close", ix))
                            .role(Role::Button)
                            .aria_label(format!("{} {name}", txt::CLOSE_TAB))
                            .w(px(16.))
                            .h(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .text_size(px(12.))
                            .text_color(tc(t.text_muted))
                            .hover(|d| d.bg(tc(t.bg_pressed)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(ix, window, cx);
                            }))
                            .child(if b.dirty { "●" } else { "×" }),
                    )
            }))
            .into_any_element()
    }

    fn render_banner(&mut self, t: Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mode = self.mode;
        let b = self.buffers.get(self.active?)?;
        let bar = |bg: u32, fg: u32| div().flex_none().min_h(px(32.)).px(px(12.)).py(px(4.)).flex().flex_wrap().items_center().gap(px(8.)).bg(tc(bg)).text_size(px(13.)).text_color(tc(fg));
        let button = |id: &'static str, label: &'static str| {
            div().id(id).role(Role::Button).aria_label(label).tab_index(0).focus_visible(move |st| pod_ui::focus_style(st, mode)).flex_none().px(px(8.)).h(px(24.)).flex().items_center().rounded(px(6.)).border_1().border_color(tc(t.border_control)).text_color(tc(t.text_primary)).bg(tc(t.bg_surface)).cursor_pointer().hover(|d| d.bg(tc(t.bg_hover))).child(label)
        };
        match &b.disk {
            DiskState::Changed(_) => {
                return Some(
                    bar(t.warning_bg, t.warning_fg)
                        .child(icon(Icon::TriangleAlert, t.warning_fg))
                        .child(div().flex_1().min_w(px(220.)).child("디스크의 파일이 바뀌었습니다 (에이전트 또는 다른 프로그램). 저장하지 않은 변경이 있습니다."))
                        .child(button("disk-reload", "디스크 내용 불러오기").on_click(cx.listener(|this, _, w, cx| this.resolve_disk_prompt(true, w, cx))))
                        .child(button("disk-keep", "내 변경 유지").on_click(cx.listener(|this, _, w, cx| this.resolve_disk_prompt(false, w, cx))))
                        .into_any_element(),
                );
            }
            DiskState::Deleted => {
                return Some(
                    bar(t.danger_bg, t.danger_fg)
                        .child(icon(Icon::OctagonX, t.danger_fg))
                        .child(div().flex_1().min_w(px(220.)).child("디스크에서 파일이 삭제되었습니다."))
                        .child(button("del-close", "탭 닫기").on_click(cx.listener(|this, _, w, cx| this.resolve_disk_prompt(true, w, cx))))
                        .child(button("del-keep", "내 변경 유지").on_click(cx.listener(|this, _, w, cx| this.resolve_disk_prompt(false, w, cx))))
                        .into_any_element(),
                );
            }
            DiskState::Same => {}
        }
        if b.large {
            return Some(bar(t.info_bg, t.info_fg).child(icon(Icon::CircleHelp, t.info_fg)).child("큰 파일입니다: 읽기 전용으로 열었고 구문 강조와 언어 서버를 껐습니다.").into_any_element());
        }
        let path = b.path.clone();
        let status = self.lsp.read(cx).status_for(&path)?;
        match &status.state {
            ServerState::NotInstalled(reason, hint) => Some(
                bar(t.info_bg, t.info_fg)
                    .child(icon(Icon::CircleHelp, t.info_fg))
                    .child(div().flex_1().min_w(px(220.)).child(format!("언어 서버 {}가 설치되어 있지 않습니다 ({reason}). 설치: {hint}", status.id)))
                    .into_any_element(),
            ),
            ServerState::Failed(reason) => Some(
                bar(t.danger_bg, t.danger_fg)
                    .child(icon(Icon::OctagonX, t.danger_fg))
                    .child(div().flex_1().min_w(px(220.)).child(format!("{}가 반복해서 종료되어 멈췄습니다: {}", status.id, reason.lines().next().unwrap_or(""))))
                    .child(button("lsp-restart", "다시 시작").on_click(cx.listener(move |this, _, _, cx| {
                        let p = path.clone();
                        this.lsp.update(cx, |s, cx| s.restart(&p, cx));
                    })))
                    .into_any_element(),
            ),
            _ => None,
        }
    }

    fn render_panel(&mut self, t: Tokens, cx: &mut Context<Self>) -> AnyElement {
        let store = self.lsp.read(cx);
        let mut problems: Vec<(PathBuf, lt::Diagnostic)> = store.diagnostics().flat_map(|f| f.items.iter().map(move |d| (f.path.clone(), d.clone()))).collect();
        problems.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.severity.map(|s| format!("{s:?}")).cmp(&b.1.severity.map(|s| format!("{s:?}")))).then(a.1.range.start.line.cmp(&b.1.range.start.line)));
        let (errs, warns) = store.diagnostic_counts();
        let tab = |id: &'static str, label: String, on: bool| {
            div()
                .id(id)
                .h(px(28.))
                .px(px(12.))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(12.))
                .when(on, |d| d.text_color(tc(t.text_primary)).border_b_2().border_color(tc(t.accent_fg)))
                .when(!on, |d| d.text_color(tc(t.text_secondary)))
                .child(label)
        };
        let root = self.root.clone();
        let rows: Vec<AnyElement> = match self.panel_tab {
            PanelTab::Problems => problems
                .iter()
                .enumerate()
                .map(|(i, (p, d))| {
                    let (ic, col) = match d.severity {
                        Some(lt::DiagnosticSeverity::ERROR) => (Icon::OctagonX, t.danger_fg),
                        Some(lt::DiagnosticSeverity::WARNING) => (Icon::TriangleAlert, t.warning_fg),
                        _ => (Icon::CircleHelp, t.info_fg),
                    };
                    let (p2, pos) = (p.clone(), d.range.start);
                    panel_row(("prob", i), t)
                        .on_click(cx.listener(move |this, _, w, cx| {
                            let _ = this.open_at(&p2, Some(pos), w, cx);
                        }))
                        .child(icon(ic, col).size(px(14.)))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(tc(t.text_primary)).child(d.message.lines().next().unwrap_or("").to_string()))
                        .child(div().flex_none().text_color(tc(t.text_muted)).child(format!("{}:{}", rel_of(&root, p), d.range.start.line + 1)))
                        .into_any_element()
                })
                .collect(),
            PanelTab::References => self
                .references
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let (p2, pos) = (l.path.clone(), l.range.start);
                    let line = std::fs::read_to_string(&l.path).ok().and_then(|s| s.lines().nth(l.range.start.line as usize).map(|x| x.trim().to_string())).unwrap_or_default();
                    panel_row(("ref", i), t)
                        .on_click(cx.listener(move |this, _, w, cx| {
                            let _ = this.open_at(&p2, Some(pos), w, cx);
                        }))
                        .child(div().flex_none().text_color(tc(t.text_muted)).child(format!("{}:{}", rel_of(&root, &l.path), l.range.start.line + 1)))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().font_family("Cascadia Mono").text_color(tc(t.text_primary)).child(line))
                        .into_any_element()
                })
                .collect(),
        };
        let empty = rows.is_empty();
        let empty_text = match self.panel_tab {
            PanelTab::Problems => "문제가 없습니다",
            PanelTab::References => "Shift+F12로 참조를 찾으세요",
        };
        div()
            .h(px(PANEL_H))
            .flex_none()
            .flex()
            .flex_col()
            .bg(tc(t.bg_surface))
            .border_t_1()
            .border_color(tc(t.border_subtle))
            .child(
                div()
                    .flex()
                    .items_center()
                    .bg(tc(t.bg_chrome))
                    .child(tab("panel-problems", format!("문제 · 오류 {errs} · 경고 {warns}"), self.panel_tab == PanelTab::Problems).on_click(cx.listener(|this, _, _, cx| {
                        this.panel_tab = PanelTab::Problems;
                        cx.notify();
                    })))
                    .child(
                        tab("panel-refs", if self.references_title.is_empty() { "참조".to_string() } else { self.references_title.clone() }, self.panel_tab == PanelTab::References).on_click(cx.listener(|this, _, _, cx| {
                            this.panel_tab = PanelTab::References;
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .id("panel-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(empty, |d| d.child(div().p(px(12.)).text_size(px(13.)).text_color(tc(t.text_muted)).child(empty_text)))
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_status(&mut self, t: Tokens, cx: &mut Context<Self>) -> AnyElement {
        let mut left: Vec<String> = vec![];
        if let Some(b) = &self.branch {
            left.push(format!("⎇ {b}"));
        }
        let (e, w) = self.lsp.read(cx).diagnostic_counts();
        left.push(format!("오류 {e} · 경고 {w}"));
        if self.indexing {
            left.push("파일 색인 중…".into());
        }
        let mut right: Vec<String> = vec![];
        let mut lsp_label: Option<(String, u32)> = None;
        if let Some(b) = self.active.and_then(|i| self.buffers.get(i)) {
            let pos = b.state.read(cx).cursor_position();
            right.push(format!("줄 {}, 열 {}", pos.line + 1, pos.character + 1));
            right.push(b.ending.label().into());
            right.push(if b.bom { "UTF-8 BOM".into() } else { "UTF-8".into() });
            right.push(b.language.map(|l| l.label).unwrap_or("일반 텍스트").into());
            if let Some(s) = self.lsp.read(cx).status_for(&b.path) {
                let col = match s.state {
                    ServerState::Running => t.success_fg,
                    ServerState::NotInstalled(..) | ServerState::Checking | ServerState::Starting => t.text_muted,
                    ServerState::Restarting { .. } => t.warning_fg,
                    _ => t.danger_fg,
                };
                let mut label = s.state.label(&s.id);
                if let Some(p) = s.progress {
                    let p: String = if p.chars().count() > 60 { p.chars().take(59).chain(std::iter::once('…')).collect() } else { p };
                    label = format!("{} · {p}", s.id);
                }
                lsp_label = Some((label, col));
            }
        }
        div()
            .h(px(24.))
            .flex_none()
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(16.))
            .bg(tc(t.bg_chrome))
            .border_t_1()
            .border_color(tc(t.border_subtle))
            .text_size(px(12.))
            .text_color(tc(t.text_secondary))
            .children(left.into_iter().map(|s| div().flex_none().child(s)))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().when_some(self.message.clone(), |d, (m, err)| d.text_color(tc(if err { t.danger_fg } else { t.text_primary })).child(m)))
            .children(right.into_iter().map(|s| div().flex_none().child(s)))
            .when_some(lsp_label, |d, (l, c)| d.child(div().flex_none().flex().items_center().gap(px(4.)).child(div().w(px(8.)).h(px(8.)).rounded_full().bg(tc(c))).child(l)))
            .into_any_element()
    }

    fn render_overlay(&mut self, t: Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mode = self.mode;
        let frame = |title: &str| {
            div()
                .w(px(480.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(12.))
                .bg(tc(t.bg_raised))
                .border_1()
                .border_color(tc(t.border_subtle))
                .rounded(px(12.))
                .shadow(pod_ui::tokens::elevation(2, mode))
                .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(tc(t.text_primary)).child(title.to_string()))
        };
        let esc = cx.listener(|this, e: &KeyDownEvent, w, cx| {
            if e.keystroke.key == "escape" {
                cx.stop_propagation();
                this.close_overlay(w, cx);
            }
        });
        let button = |id: &'static str, label: &'static str, primary: bool, danger: bool| {
            div()
                .id(id)
                .role(Role::Button)
                .aria_label(label)
                .tab_index(0)
                .focus_visible(move |st| pod_ui::focus_style(st, mode))
                .px(px(12.))
                .h(px(28.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .cursor_pointer()
                .when(primary, |d| d.bg(tc(if danger { t.danger_fg } else { t.action_primary_bg })).text_color(tc(t.action_primary_fg)))
                .when(!primary, |d| d.border_1().border_color(tc(t.border_control)).text_color(tc(t.text_primary)).hover(|d| d.bg(tc(t.bg_hover))))
                .child(label)
        };
        let err_line = |e: &Option<String>| e.clone().map(|e| div().text_size(px(12.)).text_color(tc(t.danger_fg)).child(e));
        let el = match &self.overlay {
            Overlay::None => return None,
            Overlay::ConfirmClose(path) => {
                let rel = rel_of(&self.root, path);
                let keys = cx.listener(|this, e: &KeyDownEvent, w, cx| match e.keystroke.key.as_str() {
                    "escape" => {
                        cx.stop_propagation();
                        this.resolve_close(None, w, cx);
                    }
                    "enter" => {
                        cx.stop_propagation();
                        this.resolve_close(Some(true), w, cx);
                    }
                    _ => {}
                });
                frame(txt::UNSAVED_TITLE)
                    .id("confirm-close")
                    .role(Role::AlertDialog)
                    .aria_label(txt::UNSAVED_TITLE)
                    .capture_key_down(keys)
                    .child(div().text_size(px(13.)).text_color(tc(t.text_secondary)).child(txt::unsaved_body(&rel)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(button("close-cancel", txt::CANCEL, false, false).on_click(cx.listener(|this, _, w, cx| this.resolve_close(None, w, cx))))
                            .child(button("close-discard", txt::CLOSE_WITHOUT_SAVING, false, false).on_click(cx.listener(|this, _, w, cx| this.resolve_close(Some(false), w, cx))))
                            .child(button("close-save", txt::SAVE_AND_CLOSE, true, false).on_click(cx.listener(|this, _, w, cx| this.resolve_close(Some(true), w, cx)))),
                    )
                    .into_any_element()
            }
            Overlay::NewEntry { parent, folder, input, error } => {
                let rel = rel_of(&self.root, parent);
                let title = format!("{} · {}", if *folder { txt::NEW_FOLDER_TITLE } else { txt::NEW_FILE_TITLE }, if rel.is_empty() { "/".to_string() } else { format!("{rel}/") });
                frame(&title).capture_key_down(esc).child(Input::new(input)).children(err_line(error)).child(hint(t, txt::ENTER_CREATE)).into_any_element()
            }
            Overlay::RenameEntry { path, input, error } => {
                let title = format!("{} · {}", txt::RENAME_TITLE, rel_of(&self.root, path));
                frame(&title).capture_key_down(esc).child(Input::new(input)).children(err_line(error)).child(hint(t, txt::ENTER_RENAME)).into_any_element()
            }
            Overlay::ConfirmDelete(path) => {
                let rel = rel_of(&self.root, path);
                let keys = cx.listener(|this, e: &KeyDownEvent, w, cx| match e.keystroke.key.as_str() {
                    "escape" => {
                        cx.stop_propagation();
                        this.close_overlay(w, cx);
                    }
                    "enter" => {
                        cx.stop_propagation();
                        this.commit_delete(w, cx);
                    }
                    _ => {}
                });
                frame(txt::DELETE_TITLE)
                    .id("confirm-delete")
                    .role(Role::AlertDialog)
                    .aria_label(txt::DELETE_TITLE)
                    .capture_key_down(keys)
                    .child(div().text_size(px(13.)).text_color(tc(t.text_secondary)).child(txt::delete_body(&rel, path.is_dir())))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(button("delete-cancel", txt::CANCEL, false, false).on_click(cx.listener(|this, _, w, cx| this.close_overlay(w, cx))))
                            .child(button("delete-ok", txt::DELETE_CONFIRM, true, true).on_click(cx.listener(|this, _, w, cx| this.commit_delete(w, cx)))),
                    )
                    .into_any_element()
            }
            Overlay::QuickOpen(qo) => qo.clone().into_any_element(),
            Overlay::GotoLine(inp) => frame("줄로 이동 (Ctrl+G)").capture_key_down(esc).child(Input::new(inp)).child(hint(t, "Enter: 이동 · Esc: 닫기")).into_any_element(),
            Overlay::Rename { input, .. } => frame("이름 바꾸기 (F2)").capture_key_down(esc).child(Input::new(input)).child(hint(t, "Enter: 미리 보기 · Esc: 취소")).into_any_element(),
            Overlay::RenamePreview { new_name, edits } => {
                let total: usize = edits.iter().map(|f| f.edits.len()).sum();
                let root = self.root.clone();
                let mut list = div().id("rename-preview").max_h(px(320.)).overflow_y_scroll().flex().flex_col().gap(px(2.));
                for f in edits {
                    let text = self.buffers.iter().find(|b| path_key(&b.path) == path_key(&f.path)).map(|b| b.state.read(cx).value().to_string()).or_else(|| text::load(&f.path).ok().map(|l| l.text)).unwrap_or_default();
                    list = list.child(div().pt(px(4.)).text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(tc(t.text_secondary)).child(format!("{} · {}곳", rel_of(&root, &f.path), f.edits.len())));
                    for e in f.edits.iter().take(20) {
                        let line = text::line_of(&text, e.range.start.line).to_string();
                        let after = if e.range.start.line == e.range.end.line { apply_edits(&line, &[lt::TextEdit { range: lt::Range::new(lt::Position::new(0, e.range.start.character), lt::Position::new(0, e.range.end.character)), new_text: e.new_text.clone() }]) } else { e.new_text.clone() };
                        list = list.child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .text_size(px(12.))
                                .font_family("Cascadia Mono")
                                .child(div().w(px(40.)).flex_none().text_color(tc(t.text_muted)).child(format!("{}", e.range.start.line + 1)))
                                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(tc(t.text_primary)).child(format!("{} → {}", line.trim(), after.trim()))),
                        );
                    }
                }
                let enter = cx.listener(|this, e: &KeyDownEvent, w, cx| match e.keystroke.key.as_str() {
                    "enter" => {
                        cx.stop_propagation();
                        this.apply_rename(w, cx);
                    }
                    "escape" => {
                        cx.stop_propagation();
                        this.close_overlay(w, cx);
                    }
                    _ => {}
                });
                frame(&format!("이름 바꾸기 미리 보기: `{new_name}` · 파일 {}개 · {total}곳", edits.len()))
                    .w(px(640.))
                    .capture_key_down(enter)
                    .child(list)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(div().id("rename-cancel").px(px(12.)).h(px(28.)).flex().items_center().rounded(px(6.)).border_1().border_color(tc(t.border_control)).text_color(tc(t.text_primary)).cursor_pointer().child("취소 (Esc)").on_click(cx.listener(|this, _, w, cx| this.close_overlay(w, cx))))
                            .child(div().id("rename-apply").px(px(12.)).h(px(28.)).flex().items_center().rounded(px(6.)).bg(tc(t.action_primary_bg)).text_color(tc(t.action_primary_fg)).cursor_pointer().child("적용 (Enter)").on_click(cx.listener(|this, _, w, cx| this.apply_rename(w, cx)))),
                    )
                    .into_any_element()
            }
        };
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .justify_center()
                .pt(px(48.))
                .child(div().child(el))
                .into_any_element(),
        )
    }
}

/// `parent/name` for a tree file operation, refusing empty names, `..`,
/// absolute paths and anything outside the worktree root.
pub(crate) fn valid_child(root: &Path, parent: &Path, name: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let name = name.trim().trim_matches('/');
    if name.is_empty() || name.contains(':') {
        return Err(txt::INVALID_NAME.into());
    }
    let rel = Path::new(name);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(txt::INVALID_NAME.into());
    }
    let target = parent.join(rel);
    if !target.starts_with(root) || target.components().any(|c| c.as_os_str() == ".git") {
        return Err(txt::INVALID_NAME.into());
    }
    Ok(target)
}

fn hint(t: Tokens, s: &'static str) -> impl IntoElement {
    div().text_size(px(12.)).text_color(tc(t.text_muted)).child(s)
}

fn panel_row(id: (&'static str, usize), t: Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(24.))
        .px(px(12.))
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .cursor_pointer()
        .hover(|d| d.bg(tc(t.bg_hover)))
}

fn mark_color(m: GitMark, t: Tokens) -> u32 {
    match m {
        GitMark::Modified => t.warning_fg,
        GitMark::Added | GitMark::Untracked => t.success_fg,
        GitMark::Deleted | GitMark::Conflicted => t.danger_fg,
        GitMark::Renamed => t.info_fg,
    }
}

fn set_editor_diagnostics(state: &Entity<EditorState>, diags: Vec<lt::Diagnostic>, cx: &mut App) {
    state.update(cx, |s, cx| {
        if let Some(set) = s.diagnostics_mut() {
            set.clear();
            set.extend(diags.into_iter().map(EditorDiagnostic::from));
        }
        cx.notify();
    });
}

/// "42" -> line 42, "42:5" / "42,5" -> line 42 col 5 (1-based input).
pub fn parse_line_col(s: &str) -> Option<lt::Position> {
    let s = s.trim().trim_start_matches(':');
    let mut it = s.split([':', ',']);
    let line: u32 = it.next()?.trim().parse().ok()?;
    let col: u32 = it.next().and_then(|c| c.trim().parse().ok()).unwrap_or(1);
    (line >= 1).then(|| lt::Position::new(line - 1, col.saturating_sub(1)))
}

impl Render for EditorWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Tokens::for_mode(self.mode);
        let tree = self.render_tree(t, window, cx);
        let tabs = self.render_tabs(t, cx);
        let banner = self.render_banner(t, cx);
        let panel = self.panel_open.then(|| self.render_panel(t, cx));
        let status = self.render_status(t, cx);
        let overlay = self.render_overlay(t, cx);
        let editor: AnyElement = match self.active.and_then(|i| self.buffers.get(i)) {
            Some(b) => div().flex_1().min_h_0().bg(tc(t.bg_surface)).child(Editor::new(&b.state).aria_label(txt::editor_label(&b.rel)).bordered(false).h_full()).into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .bg(tc(t.bg_canvas))
                .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(tc(t.text_primary)).child("열린 파일이 없습니다"))
                .child(div().text_size(px(13.)).text_color(tc(t.text_muted)).child("왼쪽 트리에서 파일을 고르거나 Ctrl+P로 파일 이름을 검색하세요"))
                .into_any_element(),
        };
        div()
            .id("pod-editor")
            .key_context(CTX)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_quick_open))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_save_all))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_goto_line))
            .on_action(cx.listener(Self::on_find_references))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(Self::on_next_problem))
            .on_action(cx.listener(Self::on_toggle_panel))
            .on_action(cx.listener(Self::on_focus_tree))
            .on_action(cx.listener(Self::on_focus_editor))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(tc(t.bg_canvas))
            .text_color(tc(t.text_primary))
            .font_family(cx.theme().font_family.clone())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(tree)
                    .child(div().flex_1().min_w_0().flex().flex_col().child(tabs).children(banner).child(editor).children(panel)),
            )
            .child(status)
            .children(overlay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[core::prelude::v1::test]
    fn line_col_parsing() {
        assert_eq!(parse_line_col("42"), Some(lt::Position::new(41, 0)));
        assert_eq!(parse_line_col("42:5"), Some(lt::Position::new(41, 4)));
        assert_eq!(parse_line_col(":7,2"), Some(lt::Position::new(6, 1)));
        assert_eq!(parse_line_col("0"), None);
        assert_eq!(parse_line_col("x"), None);
    }

    #[core::prelude::v1::test]
    fn tree_names_stay_inside_the_root() {
        let root = Path::new("C:/w/repo");
        let src = root.join("src");
        assert_eq!(valid_child(root, &src, "a.rs").unwrap(), src.join("a.rs"));
        assert_eq!(valid_child(root, &src, "mod/b.rs").unwrap(), src.join("mod/b.rs"));
        assert_eq!(valid_child(root, root, "한글 노트.md").unwrap(), root.join("한글 노트.md"));
        for bad in ["", "  ", "../x", "a/../../x", "C:/x", ".git/config", "a/.git"] {
            assert!(valid_child(root, &src, bad).is_err(), "{bad}");
        }
    }

    #[core::prelude::v1::test]
    fn session_json_is_stable() {
        let s = EditorSession { tabs: vec![TabSession { path: "src/main.rs".into(), line: 3, col: 5 }], active: Some(0) };
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j, serde_json::json!({"tabs":[{"path":"src/main.rs","line":3,"col":5}],"active":0}));
        let back: EditorSession = serde_json::from_value(serde_json::json!({"tabs":[{"path":"a"}]})).unwrap();
        assert_eq!(back.tabs[0].line, 1);
        assert_eq!(back.active, None);
    }
}
