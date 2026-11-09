use crate::tokens::*;
use crate::{Mode, Tokens};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    ChevronRight,
    CircleCheck,
    CircleHelp,
    Circle,
    FolderOpen,
    LoaderCircle,
    LockKeyhole,
    MessageCircleQuestion,
    OctagonX,
    ShieldQuestion,
    Square,
    Sun,
    TriangleAlert,
    WifiOff,
}
impl Icon {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::ChevronRight => include_bytes!("../assets/icons/chevron-right.svg"),
            Self::CircleCheck => include_bytes!("../assets/icons/circle-check.svg"),
            Self::CircleHelp => include_bytes!("../assets/icons/circle-help.svg"),
            Self::Circle => include_bytes!("../assets/icons/circle.svg"),
            Self::FolderOpen => include_bytes!("../assets/icons/folder-open.svg"),
            Self::LoaderCircle => include_bytes!("../assets/icons/loader-circle.svg"),
            Self::LockKeyhole => include_bytes!("../assets/icons/lock-keyhole.svg"),
            Self::MessageCircleQuestion => {
                include_bytes!("../assets/icons/message-circle-question.svg")
            }
            Self::OctagonX => include_bytes!("../assets/icons/octagon-x.svg"),
            Self::ShieldQuestion => include_bytes!("../assets/icons/shield-question.svg"),
            Self::Square => include_bytes!("../assets/icons/square.svg"),
            Self::Sun => include_bytes!("../assets/icons/sun.svg"),
            Self::TriangleAlert => include_bytes!("../assets/icons/triangle-alert.svg"),
            Self::WifiOff => include_bytes!("../assets/icons/wifi-off.svg"),
        }
    }
}
pub fn icon(name: Icon, fg: u32) -> Svg {
    svg()
        .data(name.bytes())
        .size(px(ICON_SIZE))
        .flex_shrink_0()
        .text_color(color(fg))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ControlState {
    #[default]
    Ready,
    Disabled,
    Loading,
    Error(String),
}
impl ControlState {
    pub fn interactive(&self) -> bool {
        matches!(self, Self::Ready | Self::Error(_))
    }
}
/// A keyboard-operable icon control. The label is both accessible name and tooltip.
pub struct IconButton {
    pub id: ElementId,
    pub icon: Icon,
    pub label: SharedString,
    pub state: ControlState,
}
impl IconButton {
    pub fn new(id: impl Into<ElementId>, icon: Icon, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon,
            label: label.into(),
            state: ControlState::Ready,
        }
    }
    pub fn render(
        self,
        mode: Mode,
        on_activate: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let t = Tokens::for_mode(mode);
        let enabled = self.state.interactive();
        let action = std::rc::Rc::new(on_activate);
        let key = action.clone();
        let label = self.label.clone();
        div()
            .id(self.id)
            .role(Role::Button)
            .aria_label(self.label)
            .tab_index(if enabled { 0 } else { -1 })
            .size(px(HIT_AREA))
            .rounded(px(6.))
            .border_2()
            .border_color(gpui_kit::transparent_black())
            .flex()
            .items_center()
            .justify_center()
            .text_color(color(if enabled {
                t.text_secondary
            } else {
                t.text_disabled
            }))
            .when(enabled, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(color(t.bg_hover)))
                    .active(|s| s.bg(color(t.bg_pressed)))
                    .focus_visible(|s| focus_style(s, mode))
            })
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(label.clone()).build(window, cx)
            })
            .child(icon(
                if self.state == ControlState::Loading {
                    Icon::LoaderCircle
                } else {
                    self.icon
                },
                if enabled {
                    t.text_secondary
                } else {
                    t.text_disabled
                },
            ))
            .on_click(move |_, w, cx| {
                if enabled {
                    action(w, cx);
                }
            })
            .on_key_down(move |e, w, cx| {
                if enabled && matches!(e.keystroke.key.as_str(), "enter" | "space") {
                    key(w, cx);
                    cx.stop_propagation();
                }
            })
    }
}
/// Presentation row; host owns stable selection, expanded state and tree navigation.
pub struct TreeRow {
    pub id: ElementId,
    pub label: SharedString,
    pub depth: usize,
    pub selected: bool,
    pub state: ControlState,
    pub badge: Option<crate::StatusBadge>,
}
impl TreeRow {
    pub fn render(
        self,
        mode: Mode,
        on_open: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let t = Tokens::for_mode(mode);
        let enabled = self.state.interactive();
        let callback = std::rc::Rc::new(on_open);
        let key = callback.clone();
        let label = self.label.clone();
        div()
            .id(self.id)
            .role(Role::TreeItem)
            .aria_label(self.label.clone())
            .aria_selected(self.selected)
            .tab_index(if enabled { 0 } else { -1 })
            .h(px(32.))
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .pl(px(8. + self.depth as f32 * TREE_INDENT))
            .pr(px(8.))
            .border_l_2()
            .border_color(color(if self.selected {
                t.accent_fg
            } else {
                t.bg_surface
            }))
            .bg(color(if self.selected {
                t.bg_selected
            } else {
                t.bg_surface
            }))
            .text_size(px(13.))
            .text_color(color(if enabled {
                t.text_primary
            } else {
                t.text_disabled
            }))
            .when(enabled, |d| {
                d.hover(|s| s.bg(color(t.bg_hover)))
                    .focus_visible(|s| focus_style(s, mode))
            })
            .child(icon(Icon::ChevronRight, t.text_secondary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(self.label),
            )
            .when_some(self.badge, |d, b| d.child(b.render_compact(mode)))
            .tooltip(move |w, cx| {
                gpui_kit::component::tooltip::Tooltip::new(label.clone())
                    .mt(px(28.)).build(w, cx)
            })
            .on_click(move |_, w, cx| {
                if enabled {
                    callback(w, cx);
                }
            })
            .on_key_down(move |e, w, cx| {
                if enabled && e.keystroke.key == "enter" {
                    key(w, cx);
                    cx.stop_propagation();
                }
            })
    }
}
/// Small deterministic focus ownership stack. Closing a nested owner returns to its opener.
#[derive(Debug)]
pub struct FocusStack<T> {
    stack: Vec<T>,
}
impl<T> Default for FocusStack<T> {
    fn default() -> Self {
        Self { stack: Vec::new() }
    }
}
impl<T> FocusStack<T> {
    pub fn enter(&mut self, previous: T) {
        self.stack.push(previous);
    }
    pub fn leave(&mut self) -> Option<T> {
        self.stack.pop()
    }
    pub fn depth(&self) -> usize {
        self.stack.len()
    }
}
#[derive(Default)]
pub struct FocusReturn {
    stack: FocusStack<FocusHandle>,
}
impl FocusReturn {
    pub fn enter(&mut self, target: &FocusHandle, window: &mut Window, cx: &mut App) {
        if let Some(previous) = window.focused(cx) {
            self.stack.enter(previous);
        }
        target.focus(window, cx);
    }
    pub fn leave(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(previous) = self.stack.leave() {
            previous.focus(window, cx);
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Dialog,
    CommandPalette,
    List,
    Toast,
    Empty,
}
/// Common shell; content owns its input entities, validation and actions.
/// Modal frames require a focus handle and restore callback; Tab stays within supplied controls.
pub struct Frame {
    pub id: ElementId,
    pub kind: FrameKind,
    pub title: SharedString,
    pub description: SharedString,
    pub state: ControlState,
}
impl Frame {
    pub fn new(
        id: impl Into<ElementId>,
        kind: FrameKind,
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            description: description.into(),
            state: ControlState::Ready,
        }
    }
    pub fn render(
        self,
        mode: Mode,
        children: impl IntoIterator<Item = AnyElement>,
    ) -> Stateful<Div> {
        let t = Tokens::for_mode(mode);
        let enabled = self.state.interactive();
        let elevated = matches!(
            self.kind,
            FrameKind::Dialog | FrameKind::CommandPalette | FrameKind::Toast
        );
        let max_width = match self.kind {
            FrameKind::Dialog => 560.,
            FrameKind::CommandPalette => 640.,
            FrameKind::Toast => 360.,
            FrameKind::Empty => 420.,
            FrameKind::List => 720.,
        };
        let mut d = div()
            .id(self.id)
            .w_full()
            .max_w(px(max_width))
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(if self.kind == FrameKind::Dialog {
                24.
            } else {
                16.
            }))
            .rounded(px(if elevated { 12. } else { 8. }))
            .border_1()
            .border_color(color(t.border_subtle))
            .bg(color(if elevated { t.bg_raised } else { t.bg_surface }))
            .text_color(color(t.text_primary))
            .text_size(px(13.))
            .line_height(px(20.))
            .when(elevated, |d| d.shadow(elevation(2, mode)))
            .child(
                div()
                    .text_size(px(
                        if self.kind == FrameKind::Empty || self.kind == FrameKind::Dialog {
                            20.
                        } else {
                            15.
                        },
                    ))
                    .child(self.title),
            )
            .child(
                div()
                    .text_color(color(t.text_muted))
                    .child(self.description),
            );
        d = match self.state {
            ControlState::Loading => d.child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(icon(Icon::LoaderCircle, t.info_fg))
                    .child("불러오는 중…"),
            ),
            ControlState::Error(message) => d.child(
                div()
                    .flex()
                    .gap(px(8.))
                    .text_color(color(t.danger_fg))
                    .child(icon(Icon::OctagonX, t.danger_fg))
                    .child(message),
            ),
            ControlState::Disabled => d.child("사용할 수 없음"),
            ControlState::Ready => d,
        };
        d.children(children).when(!enabled, |d| {
            d.capture_any_mouse_down(|_, w, cx| {
                w.prevent_default();
                cx.stop_propagation();
            })
            .capture_key_down(|e, w, cx| {
                if e.keystroke.key != "tab" {
                    w.prevent_default();
                    cx.stop_propagation();
                }
            })
        })
    }
}
gpui_kit::actions!(pod_modal, [ModalNext, ModalPrevious, ModalClose]);
pub fn install_key_bindings(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", ModalNext, Some("PodModal")),
        KeyBinding::new("shift-tab", ModalPrevious, Some("PodModal")),
        KeyBinding::new("escape", ModalClose, Some("PodModal")),
    ]);
}
/// Attach to the modal frame after render. Host passes only enabled tab stops.
pub fn modal_keyboard(
    frame: Stateful<Div>,
    owner: &FocusHandle,
    stops: Vec<FocusHandle>,
    close: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let previous = stops.clone();
    frame
        .track_focus(owner)
        .key_context("PodModal")
        .role(Role::Dialog)
        .on_action(move |_: &ModalNext, w, cx| {
            if !stops.is_empty() {
                let i = stops.iter().position(|h| h.is_focused(w));
                stops[next_focus(i, stops.len(), false)].focus(w, cx);
            }
            cx.stop_propagation();
        })
        .on_action(move |_: &ModalPrevious, w, cx| {
            if !previous.is_empty() {
                let i = previous.iter().position(|h| h.is_focused(w));
                previous[next_focus(i, previous.len(), true)].focus(w, cx);
            }
            cx.stop_propagation();
        })
        .on_action(move |_: &ModalClose, w, cx| {
            close(w, cx);
            cx.stop_propagation();
        })
}
pub fn next_focus(current: Option<usize>, len: usize, backwards: bool) -> usize {
    if len == 0 {
        return 0;
    }
    match current {
        None => {
            if backwards {
                len - 1
            } else {
                0
            }
        }
        Some(i) => {
            if backwards {
                (i + len - 1) % len
            } else {
                (i + 1) % len
            }
        }
    }
}
/// Toast timing never resolves the underlying domain event. Error/blocked persist.
pub fn toast_timeout(
    tone: crate::Tone,
    hovered: bool,
    focused: bool,
) -> Option<std::time::Duration> {
    if hovered || focused {
        return None;
    }
    match tone {
        crate::Tone::Success => Some(std::time::Duration::from_secs(5)),
        crate::Tone::Info => Some(std::time::Duration::from_secs(7)),
        _ => None,
    }
}

/// Stable-ID list navigation; callers filter IDs without resetting a surviving selection.
#[derive(Clone, Debug, Default)]
pub struct ListSelection {
    pub ids: Vec<String>,
    pub selected: Option<String>,
}
impl ListSelection {
    pub fn replace(&mut self, ids: Vec<String>) {
        self.ids = ids;
        if !self
            .selected
            .as_ref()
            .is_some_and(|id| self.ids.contains(id))
        {
            self.selected = self.ids.first().cloned();
        }
    }
    pub fn navigate(&mut self, backwards: bool) {
        if self.ids.is_empty() {
            self.selected = None;
            return;
        }
        let i = self
            .selected
            .as_ref()
            .and_then(|id| self.ids.iter().position(|x| x == id));
        self.selected = Some(self.ids[next_focus(i, self.ids.len(), backwards)].clone());
    }
    /// Never turn IME composition confirmation into command execution.
    pub fn activation(&self, composing: bool, state: &ControlState) -> Option<&str> {
        if composing || !state.interactive() {
            None
        } else {
            self.selected.as_deref()
        }
    }
}

/// A 2px focus ring separated from the control by a 2px gap.
pub fn focus_style(style: StyleRefinement, mode: Mode) -> StyleRefinement {
    let t = Tokens::for_mode(mode);
    style.shadow(vec![
        BoxShadow {
            inset: false,
            color: color(t.accent_fg),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(FOCUS_OFFSET + FOCUS_RING),
        },
        BoxShadow {
            inset: false,
            color: color(t.bg_surface),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(FOCUS_OFFSET),
        },
    ])
}

/// Semantic action control: hover adds a border and press adds an inset, not color alone.
/// Status indicators must use StatusBadge instead.
pub fn semantic_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: crate::Tone,
    mode: Mode,
    state: ControlState,
    activate: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let t = Tokens::for_mode(mode);
    let (fg, bg) = tone.colors(t);
    let label = label.into();
    let enabled = state.interactive();
    let activate = std::rc::Rc::new(activate);
    let key = activate.clone();
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .tab_index(if enabled { 0 } else { -1 })
        .h(px(HEIGHTS[1]))
        .px(px(SPACE[3]))
        .rounded(px(RADII[2]))
        .flex()
        .items_center()
        .border_1()
        .border_color(color(bg))
        .bg(color(bg))
        .text_color(color(if enabled { fg } else { t.text_disabled }))
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|s| s.border_color(color(fg)))
                .active(|s| {
                    s.shadow(vec![BoxShadow {
                        inset: true,
                        color: color(fg),
                        offset: point(px(0.), px(1.)),
                        blur_radius: px(0.),
                        spread_radius: px(1.),
                    }])
                })
                .focus_visible(|s| focus_style(s, mode))
        })
        .child(if state == ControlState::Loading {
            SharedString::from("Loading...")
        } else {
            label
        })
        .on_click(move |_, w, cx| {
            if enabled {
                activate(w, cx);
            }
        })
        .on_key_down(move |e, w, cx| {
            if enabled && matches!(e.keystroke.key.as_str(), "enter" | "space") {
                key(w, cx);
                cx.stop_propagation();
            }
        })
}
