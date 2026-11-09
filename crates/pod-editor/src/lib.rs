//! pod-editor: P3 editing for Pod (docs/13-editor-and-lsp.md).
//!
//! GUI-free core: [`tree`] (lazy, .gitignore-aware file tree), [`git`]
//! (status markers), [`watch`] (debounced watcher), [`index`] + [`fuzzy`]
//! (quick open), [`text`] (line endings, size guards, column units),
//! [`uri`] (Windows-safe file URIs) and [`lsp`] (client, registry, store).
//!
//! GPUI: [`EditorWorkspace`] is the embeddable view for one worktree root:
//! file tree + tabs + code editor + quick open + problems/references panel.
#![recursion_limit = "512"]

pub mod fixture;
pub mod fuzzy;
pub mod git;
pub mod index;
pub mod lsp;
pub mod text;
pub mod tree;
pub mod uri;
pub mod watch;

mod ui;

pub use ui::workspace::{EditorEvent, EditorSession, EditorWorkspace, TabSession, init, parse_line_col};
/// Actions (key context `PodEditor`) a host may dispatch or rebind.
pub use ui::workspace::{CloseTab, FindReferences, FocusEditor, FocusTree, GotoLine, NextProblem, NextTab, PrevTab, QuickOpenFile, RenameSymbol, Save, SaveAll, TogglePanel};
