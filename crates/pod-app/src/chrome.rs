//! Host-owned commands, settings and status inspector.
use crate::theme::{Settings, mode};
use crate::workspace::{ToastKind, Workspace};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::input::{Enter, Input, InputEvent, InputState};
use gpui_kit::component::{TitleBar, WindowExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
actions!(pod_chrome, [JumpAlternate, Jump, SettingsAction, NewTask, Notifications, InspectStatus]);
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-k", Jump, None),
        KeyBinding::new("ctrl-shift-k", JumpAlternate, None),
        KeyBinding::new("ctrl-,", SettingsAction, None),
        KeyBinding::new("ctrl-n", NewTask, None),
        KeyBinding::new("ctrl-shift-b", Notifications, None),
        KeyBinding::new("ctrl-shift-s", InspectStatus, None),
    ]);
}
/// A dialog's primary action, shared by its button and Enter in its fields.
pub(crate) type Submit = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// Enter in a dialog's text field runs the dialog's primary action, as its
/// button does (keyboard-only use). Ignored while an IME is composing.
pub(crate) fn submit_on_enter(input: &Entity<InputState>, submit: Submit, window: &mut Window, cx: &mut App) {
    window
        .subscribe(input, cx, move |input, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                let composing = input.update(cx, |i, cx| i.marked_text_range(window, cx).is_some());
                if !composing {
                    submit(window, cx);
                }
            }
        })
        .detach();
}

#[derive(Clone)]
struct Entry {
    id: String,
    title: String,
    detail: String,
}
const COMMANDS: &[(&str, &str, &str)] = &[
    (
        "shortcuts",
        "키보드 단축키 도움말",
        "단축키 · Keyboard shortcuts",
    ),
    ("new-task", crate::strings::NEW_TASK, "Ctrl+N"),
    ("quick-open", crate::strings::QUICK_OPEN_FILE, crate::strings::QUICK_OPEN_FILE_DETAIL),
    ("files", crate::strings::FILES_PANEL, crate::strings::FILES_PANEL_DETAIL),
    ("save-all", crate::strings::SAVE_ALL_FILES, crate::strings::SAVE_ALL_FILES_DETAIL),
    ("open-repo", crate::strings::OPEN_REPOSITORY, crate::strings::REGISTER_A_LOCAL_GIT_REPOSITORY),
    ("terminal", crate::strings::NEW_TERMINAL, "Ctrl+Shift+T"),
    ("settings", crate::strings::SETTINGS, "Ctrl+,"),
    ("blocked", crate::strings::FIND_NEXT_BLOCKED_REQUEST, crate::strings::RETURN_TO_THE_TERMINAL_THAT_NEEDS_YOU),
    ("inspect", crate::strings::INSPECT_CURRENT_TERMINAL_STATUS, crate::strings::SOURCE_AND_FRESHNESS),
    ("notifications", crate::strings::NOTIFICATIONS, crate::strings::PENDING_REQUESTS_AND_RECENT_EVENTS),
    ("ctrl-k", crate::strings::SEND_CTRL_K_TO_CURRENT_TERMINAL, crate::strings::TERMINAL_SHORTCUT_PASSTHROUGH),
    ("ctrl-p", crate::strings::SEND_CTRL_P_TO_CURRENT_TERMINAL, crate::strings::TERMINAL_SHORTCUT_PASSTHROUGH),
    ("terminal-links", crate::strings::OPEN_LINK_FROM_TERMINAL, crate::strings::OPEN_LINK_FROM_TERMINAL_DETAIL),
    ("install-hooks", crate::strings::INSTALL_AGENT_HOOKS, crate::strings::INSTALL_AGENT_HOOKS_DETAIL),
    ("grid", crate::strings::ARRANGE_PANELS_IN_A_GRID, "Ctrl+Shift+G"),
    ("new-group", crate::strings::NEW_COMPARISON_GROUP, crate::strings::COMPARE_PARALLEL_TASKS),
    ("gates", crate::strings::OPEN_GATES_INBOX, crate::strings::PENDING_APPROVALS),
    ("agents", crate::strings::OPEN_AGENT_TREE, crate::strings::TASKS_GROUPS_AND_PARENT_RELATIONS),
];
impl Workspace {
    fn terminal_task_label<'a>(&'a self, t: &'a crate::vm::TerminalVm) -> &'a str {
        t.task_id
            .as_deref()
            .or_else(|| self.tasks.iter().find(|task| task.terminals.contains(&t.id)).map(|task| task.id.as_str()))
            .unwrap_or(crate::strings::TERMINALS)
    }
    pub(crate) fn title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let context = self
            .focused_terminal
            .as_deref()
            .and_then(|id| self.terminal(id))
            .map(|t| format!("{} / {}", self.terminal_task_label(t), self.label_of(t)))
            .unwrap_or_else(|| crate::strings::YOUR_WORKSPACE.into());
        TitleBar::new().h(px(36.)).child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .flex_1()
                .min_w_0()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Pod"))
                .child(div().flex_1().min_w_0().truncate().child(context))
                .child(div().debug_selector(|| "title-actions".into()).flex().items_center().gap_3().child(
                    Button::new("jump")
                        .label(crate::strings::SEARCH_COMMANDS_TERMINALS_CTRL_K)
                        // The tooltip is also the accessible description: Narrator reads the shortcut.
                        .tooltip(if cx.global::<Settings>().jump_shortcut == "ctrl-shift-k" { "명령·터미널 검색 (Ctrl+Shift+K)" } else { "명령·터미널 검색 (Ctrl+K)" })
                        .compact()
                        .on_click(cx.listener(|w, _, window, cx| w.open_jump(window, cx))),
                )
                .child(Button::new("settings").label(crate::strings::SETTINGS).tooltip("설정 (Ctrl+,)").compact().on_click(cx.listener(|w, _, window, cx| w.open_settings(window, cx))))),
        )
    }
    pub(crate) fn command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = id.strip_prefix("term:") {
            self.open_terminal(id, false, window, cx);
            return;
        }
        if let Some(id) = id.strip_prefix("task:") {
            self.open_task_dialog(id.into(), window, cx);
            return;
        }
        if let Some(n) = id.strip_prefix("link:").and_then(|n| n.parse().ok()) {
            self.open_link_choice(n, window, cx);
            return;
        }
        match id {
            "shortcuts" => {
                let shortcut = if cx.global::<Settings>().jump_shortcut == "ctrl-shift-k" {
                    "Ctrl+Shift+K"
                } else {
                    "Ctrl+K"
                };
                window.open_dialog(cx, move |d, _, _| {
                    // Two columns: one column outgrew a 960 px window (the last rows were cut off).
                    let rows: Vec<String> = std::iter::once(format!("{shortcut} — 명령·터미널 검색")).chain(SHORTCUTS.iter().map(|(keys, label)| format!("{keys} — {label}"))).collect();
                    let half = rows.len().div_ceil(2);
                    let column = |rows: &[String]| div().flex().flex_col().gap_1().flex_1().min_w_0().children(rows.iter().map(|r| div().child(r.clone())));
                    d.title("키보드 단축키").w(px(940.)).child(div().id("shortcut-list").flex().gap_6().text_sm().child(column(&rows[..half])).child(column(&rows[half..])))
                });
            }
            "new-task" => self.open_new_task_dialog(window, cx),
            "quick-open" => self.quick_open_files(true, window, cx),
            "files" => self.quick_open_files(false, window, cx),
            "save-all" => self.save_all_editors(window, cx),
            "open-repo" => self.open_repository(window, cx),
            "terminal" => self.open_new_dialog(window, cx),
            "settings" => self.open_settings(window, cx),
            "grid" => self.arrange_grid(window, cx),
            "new-group" => self.open_new_group_dialog(window, cx),
            "gates" => self.open_gates(window, cx),
            "agents" => self.open_agent_tree(window, cx),
            "inspect" => self.open_inspector(window, cx),
            "notifications" => self.open_notifications(window, cx),
            "blocked" => {
                let id = self.terminals.iter().find(|t| t.status.as_ref().is_some_and(|s| s.state == crate::vm::AgentState::Blocked)).map(|t| t.id.clone());
                if let Some(id) = id {
                    self.open_terminal(&id, false, window, cx);
                } else {
                    self.raise_toast(ToastKind::Info, crate::strings::NO_BLOCKED_REQUESTS, crate::strings::ALL_TERMINALS_ARE_CLEAR_OF_REPORTED_REQUESTS, cx);
                }
            }
            "ctrl-k" => self.send_to_focused_terminal(0x0b),
            "ctrl-p" => self.send_to_focused_terminal(0x10),
            "terminal-links" => self.open_link_picker(window, cx),
            "install-hooks" => self.request_hook_install(window, cx),
            _ => {}
        }
    }
    pub(crate) fn open_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(crate::strings::LOCAL_GIT_REPOSITORY_PATH));
        let busy = std::rc::Rc::new(std::cell::Cell::new(false));
        let error = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let ws = cx.entity().downgrade();
        let focus = input.focus_handle(cx);
        let submit: Submit = {
            let (input, busy, error) = (input.clone(), busy.clone(), error.clone());
            std::rc::Rc::new(move |window: &mut Window, cx: &mut App| {
                if busy.replace(true) {
                    return;
                }
                let path = input.read(cx).value().trim().to_string();
                let busy = busy.clone();
                let error = error.clone();
                let _ = ws.update(cx, |w, cx| {
                    let source = w.source.clone();
                    cx.spawn_in(window, async move |this, cx| {
                        let result = source.add_repo(&path, None).await;
                        busy.set(false);
                        let _ = this.update_in(cx, |w, window, cx| {
                            match result {
                                Ok(Ok(repo)) => {
                                    w.repos.retain(|r| r.name != repo.name);
                                    w.repos.push(repo);
                                    window.close_dialog(cx);
                                    w.touch(cx);
                                }
                                Ok(Err(e)) => *error.borrow_mut() = e.message,
                                Err(_) => *error.borrow_mut() = crate::strings::CONNECTION_CLOSED_RETRY_WHEN_CONNECTED.into(),
                            }
                            window.refresh();
                        });
                    })
                    .detach();
                });
            })
        };
        submit_on_enter(&input, submit.clone(), window, cx);
        window.open_dialog(cx, move |d: Dialog, _, _| {
            let submit = submit.clone();
            d.title(crate::strings::OPEN_REPOSITORY)
                .w(px(560.))
                .child(Input::new(&input))
                .child(div().text_color(rgb(crate::theme::tokens().danger_fg)).child(error.borrow().clone()))
                .child(
                    Button::new("repo-open")
                        .label(if busy.get() { crate::strings::OPENING } else { crate::strings::OPEN })
                        .primary()
                        .on_click(move |_, window, cx| submit(window, cx)),
                )
        });
        focus.focus(window, cx);
    }
    /// A control byte for the terminal the user works in (shortcuts the app keeps for itself).
    pub(crate) fn send_to_focused_terminal(&mut self, byte: u8) {
        if let Some(id) = &self.focused_terminal {
            self.source.input(id, vec![byte]);
        }
    }
    /// A palette over caller-made `(id, title, detail)` entries; Enter runs `command(id)`.
    pub(crate) fn open_picker(&mut self, title: &'static str, entries: Vec<(String, String, String)>, window: &mut Window, cx: &mut Context<Self>) {
        let entries = entries.into_iter().map(|(id, title, detail)| Entry { id, title, detail }).collect();
        let ws = cx.entity().downgrade();
        let palette = cx.new(|cx| Palette::new(entries, ws, Some(crate::strings::FILTER_LINKS), window, cx));
        let input = palette.read(cx).input.clone();
        window.open_dialog(cx, move |d: Dialog, _, _| d.title(title).bg(rgb(crate::theme::tokens().bg_raised)).overflow_hidden().w(px(640.)).child(palette.clone()));
        input.focus_handle(cx).focus(window, cx);
    }
    pub(crate) fn open_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut entries: Vec<Entry> = self
            .terminals
            .iter()
            .map(|t| Entry {
                id: format!("term:{}", t.id),
                title: self.label_of(t),
                detail: format!("{} / {}", self.terminal_task_label(t), crate::orch::terminal_badge(t).label()),
            })
            .collect();
        entries.extend(self.tasks.iter().map(|t| Entry { id: format!("task:{}", t.id), title: t.title.clone(), detail: t.id.clone() }));
        entries.extend(COMMANDS.iter().map(|(id, title, detail)| Entry { id: (*id).into(), title: (*title).into(), detail: (*detail).into() }));
        let recents = &cx.global::<Settings>().recents;
        entries.sort_by_key(|e| {
            recents.iter().position(|id| id == &e.id).unwrap_or(usize::MAX)
        });
        let ws = cx.entity().downgrade();
        let palette = cx.new(|cx| Palette::new(entries, ws, None, window, cx));
        let input = palette.read(cx).input.clone();
        window.open_dialog(cx, move |d: Dialog, _, _| {
            d.title(crate::strings::JUMP_TO_WORK)
                .bg(rgb(crate::theme::tokens().bg_raised))
                .overflow_hidden().w(px(640.)).child(palette.clone())
        });
        input.focus_handle(cx).focus(window, cx);
    }
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_chrome_panel(false, window, cx);
    }
    pub(crate) fn open_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_chrome_panel(true, window, cx);
    }
    fn open_chrome_panel(&mut self, inspector: bool, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::dock::{DockPlacement, PanelId, panel_handle};
        let slot = if inspector { &mut self.inspector_panel } else { &mut self.settings_panel };
        if let Some(panel) = slot.as_ref().and_then(|p| p.upgrade()) {
            self.dock.update(cx, |d, cx| d.select_panel(PanelId::from(panel.entity_id()), window, cx));
            panel.read(cx).focus_handle(cx).focus(window, cx);
            return;
        }
        let workspace = cx.entity().downgrade();
        let panel = cx.new(|cx| {
            let sub = workspace.upgrade().map(|w| cx.observe(&w, |_, _, cx| cx.notify()));
            ChromePanel { workspace, inspector, focus: cx.focus_handle(), _sub: sub }
        });
        *slot = Some(panel.downgrade());
        let focus = panel.read(cx).focus_handle(cx);
        self.dock.update(cx, |d, cx| {
            d.add_panel_view(
                panel_handle(panel),
                if inspector { DockPlacement::Right } else { DockPlacement::Center },
                if inspector { Some(px(304.)) } else { None },
                window,
                cx,
            )
        });
        focus.focus(window, cx);
        self.touch(cx);
    }
    pub(crate) fn open_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events: Vec<_> = self.toasts.iter().rev().map(|t| (t.title.clone(), t.body.clone())).collect();
        let blocked: Vec<_> = self.terminals.iter().filter(|t| t.status.as_ref().is_some_and(|s| s.state == crate::vm::AgentState::Blocked)).cloned().collect();
        let ws = cx.entity().downgrade();
        window.open_dialog(cx, move |d: Dialog, _, _| {
            let mut body = div().id("notification-list").max_h(px(420.)).overflow_y_scroll().flex().flex_col().gap_3();
            for t in &blocked {
                let id = t.id.clone();
                let ws = ws.clone();
                body = body.child(
                    Button::new(SharedString::from(format!("notice-{}", t.id)))
                        .label(format!("{} / {}", t.name.as_deref().unwrap_or(&t.id), crate::orch::terminal_badge(t).label()))
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let _ = ws.update(cx, |w, cx| w.open_terminal(&id, false, window, cx));
                        }),
                );
            }
            for (title, detail) in &events {
                body = body.child(div().child(title.clone()).child(div().text_xs().child(detail.clone())));
            }
            d.title(crate::strings::NOTIFICATIONS).w(px(560.)).child(body)
        });
    }
}
struct Palette {
    /// A picker (links): no recents heading.
    picker: bool,
    scroll: ScrollHandle,
    input: Entity<InputState>,
    entries: Vec<Entry>,
    selection: pod_ui::ListSelection,
    workspace: WeakEntity<Workspace>,
    _sub: Subscription,
}
impl Palette {
    /// `picker`: a caller-made list (its filter placeholder); `None` is the jump palette.
    fn new(entries: Vec<Entry>, workspace: WeakEntity<Workspace>, picker: Option<&'static str>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(picker.unwrap_or(crate::strings::SEARCH_TASKS_TERMINALS_OR_COMMANDS)));
        let sub = cx.subscribe_in(&input, window, |p: &mut Self, _, event, _window, cx| match event {
            InputEvent::Change => {
                p.filter(cx);
                cx.notify();
            }
            InputEvent::PressEnter { .. } => {}
            _ => {}
        });
        let mut selection = pod_ui::ListSelection::default();
        selection.replace(entries.iter().map(|e| e.id.clone()).collect());
        Self { scroll: ScrollHandle::new(), input, entries, selection, workspace, picker: picker.is_some(), _sub: sub }
    }
    fn filter(&mut self, cx: &App) {
        let query = self.input.read(cx).value().to_lowercase();
        let commands = query.starts_with('>');
        let query = query.trim_start_matches('>').trim();
        self.selection.replace(
            self.entries
                .iter()
                .filter(|e| (!commands || !e.id.contains(':')) && (fuzzy_positions(&e.title, query).is_some() || fuzzy_positions(&e.detail, query).is_some()))
                .map(|e| e.id.clone())
                .collect(),
        );
    }
    fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let composing = self.input.update(cx, |input, cx| input.marked_text_range(window, cx).is_some());
        let Some(id) = self.selection.activation(composing, &pod_ui::ControlState::Ready).map(str::to_owned) else {
            return;
        };
        // Picker choices (`link:<n>`) are not worth remembering.
        if !id.starts_with("link:") {
            let mut settings = cx.global::<Settings>().clone();
            settings.recents.retain(|recent| recent != &id);
            settings.recents.insert(0, id.clone());
            settings.recents.truncate(20);
            let _ = settings.save();
            cx.set_global(settings);
        }
        window.close_dialog(cx);
        let _ = self.workspace.update(cx, |w, cx| w.command(&id, window, cx));
    }
}
impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.input.read(cx).value().to_lowercase();
        let commands = query.starts_with('>');
        let query = query.trim_start_matches('>').trim();
        let rows: Vec<_> = self
            .entries
            .iter()
            .filter(|e| (!commands || !e.id.contains(':')) && (fuzzy_positions(&e.title, query).is_some() || fuzzy_positions(&e.detail, query).is_some()))
            .cloned()
            .collect();
        let row_ids: Vec<_> = rows.iter().map(|e| e.id.clone()).collect();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(Input::new(&self.input))
            .child(div().text_xs().child(if query.is_empty() && !self.picker { crate::strings::RECENT } else { crate::strings::RESULTS }))
            .on_action(cx.listener(|p, _: &Enter, window, cx| {
                p.activate(window, cx);
                cx.stop_propagation();
            }))
            .capture_key_down(cx.listener(move |p, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "down" || e.keystroke.key == "up" {
                    p.selection.navigate(e.keystroke.key == "up");
                    if let Some(index) = row_ids.iter().position(|id| Some(id) == p.selection.selected.as_ref()) {
                        p.scroll.scroll_to_item(index);
                    }
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(div().id("jump-results").max_h(px(340.)).overflow_y_scroll().track_scroll(&self.scroll).children(rows.iter().map(|e| {
                let id = e.id.clone();
                let selected = self.selection.selected.as_ref() == Some(&e.id);
                div()
                    .id(SharedString::from(e.id.clone()))
                    .role(Role::ListBoxOption)
                    .aria_label(format!("{} - {}", e.title, e.detail))
                    .aria_selected(selected)
                    .h(px(36.))
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .cursor_pointer()
                    .border_l_2()
                    .border_color(rgb(if selected { crate::theme::tokens().accent_fg } else { crate::theme::tokens().bg_surface }))
                    .bg(rgb(if selected { crate::theme::tokens().bg_selected } else { crate::theme::tokens().bg_surface }))
                    .hover(|s| s.bg(rgb(crate::theme::tokens().bg_hover)))
                    .child(div().flex_1().min_w_0().truncate().child(highlight(&e.title, query)))
                    .child(div().max_w(px(320.)).truncate().text_xs().text_color(rgb(crate::theme::tokens().text_muted)).child(highlight(&e.detail, query)))
                    .on_click(cx.listener(move |p, _, window, cx| {
                        p.selection.selected = Some(id.clone());
                        p.activate(window, cx);
                    }))
            })))
            .when(rows.is_empty(), |d| {
                d.child(crate::strings::NO_MATCHING_ITEMS_CLEAR_THE_SEARCH_TO_SHOW_ALL_COMMANDS).child(
                    Button::new("clear-search").label(crate::strings::CLEAR_SEARCH).on_click(cx.listener(|p, _, window, cx| {
                        p.input.update(cx, |i, cx| i.set_value("", window, cx));
                    })),
                )
            })
            .child(div().text_xs().child(crate::tr!(UP_DOWN_NAVIGATE_ENTER_OPEN_ESC_CLOSE_RESULTS; ; rows.len())))
    }
}

/// Timestamp stays on the VM; freshness is computed only when rendered.
pub(crate) fn freshness(status: Option<&crate::vm::StatusVm>) -> String {
    match status {
        Some(s) if s.since > 0 => {
            crate::tr!(S_AGO; ; crate::orch::now_ms().saturating_sub(s.since) / 1000)
        }
        _ => crate::strings::NOT_REPORTED.into(),
    }
}

pub(crate) struct ChromePanel {
    workspace: WeakEntity<Workspace>,
    inspector: bool,
    _sub: Option<Subscription>,
    focus: FocusHandle,
}
const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+N", "새 작업"),
    ("Ctrl+Shift+T", "새 터미널"),
    ("Ctrl+,", "설정"),
    ("Ctrl+Shift+S", "상태 인스펙터"),
    ("Ctrl+Shift+B", "알림"),
    ("Ctrl+Shift+R", "변경 검토"),
    ("Ctrl+Shift+G", "패널 격자 정렬"),
    ("Ctrl+Shift+N", "새 비교 그룹"),
    ("Ctrl+Shift+I", "승인 요청"),
    ("Ctrl+Shift+E", "에이전트 트리"),
    ("Ctrl+P", "파일 열기 (현재 작업 폴더)"),
    ("Ctrl+S / Ctrl+K S", "편집기: 저장 / 모두 저장"),
    ("Ctrl+W", "편집기: 탭 닫기 (저장 확인)"),
    ("Ctrl+.", "편집기: 코드 작업 (빠른 수정)"),
    ("Ctrl+클릭", "터미널: 파일:줄 · 주소 열기"),
    ("Alt+1 / Alt+2", "편집기: 파일 트리 / 편집기로 초점"),
    ("F2 / Delete", "파일 트리: 이름 바꾸기 / 삭제"),
    ("Ctrl+Shift+O", "검토: 가리킨 줄을 편집기에서 열기"),
    ("Tab / Shift+Tab", "다음 / 이전 컨트롤"),
    ("Esc", "대화 상자 닫고 이전 포커스로"),
    ("↑ / ↓, Enter", "검색 결과 탐색 / 열기"),
    ("F7 / Shift+F7", "검토: 다음 / 이전 변경"),
    ("Alt+↓ / Alt+↑", "검토: 다음 / 이전 변경"),
    ("Alt+PageDown / Alt+PageUp", "검토: 다음 / 이전 파일"),
    ("Ctrl+Shift+C", "검토: 의견 추가"),
    ("Ctrl+Shift+Enter", "검토: 미해결 의견 보내기"),
    ("F5", "검토 새로 고침"),
];
/// Restore without borrowing the workspace while DockArea::load holds its lease.
pub(crate) fn build_chrome_panel(
    ctx: gpui_kit::component::dock::PanelBuildContext,
    _: &mut Window,
    cx: &mut App,
) -> std::sync::Arc<dyn gpui_kit::component::dock::BasePanelView> {
    let inspector = ctx.state().panel_name == "PodInspector";
    let g = cx.global::<crate::workspace::AppGlobals>();
    let (workspace, restore) = (g.workspace.clone(), g.restore.clone());
    let panel = cx.new(|cx| {
        let sub = workspace
            .upgrade()
            .map(|w| cx.observe(&w, |_, _, cx| cx.notify()));
        ChromePanel {
            workspace,
            inspector,
            focus: cx.focus_handle(),
            _sub: sub,
        }
    });
    restore.borrow_mut().built.push(if inspector {
        crate::workspace::RestoredPanel::Inspector(panel.downgrade())
    } else {
        crate::workspace::RestoredPanel::Settings(panel.downgrade())
    });
    gpui_kit::component::dock::panel_handle(panel)
}
impl EventEmitter<gpui_kit::base::dock::PanelEvent> for ChromePanel {}
impl Focusable for ChromePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl gpui_kit::base::dock::Panel for ChromePanel {
    fn panel_name(&self) -> &'static str {
        if self.inspector { "PodInspector" } else { "PodSettings" }
    }
    fn dump(&self, _: &App) -> gpui_kit::base::dock::PanelState {
        gpui_kit::base::dock::PanelState::new(if self.inspector { "PodInspector" } else { "PodSettings" })
    }
    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let ws = self.workspace.clone();
        let inspector = self.inspector;
        cx.defer(move |cx| {
            let _ = ws.update(cx, |w, cx| {
                if inspector {
                    w.inspector_panel = None;
                } else {
                    w.settings_panel = None;
                }
                w.touch(cx);
            });
        });
    }
}
impl gpui_kit::component::dock::Panel for ChromePanel {
    fn tab_name(&self, _: &App) -> Option<SharedString> {
        Some(if self.inspector { crate::strings::STATUS } else { crate::strings::SETTINGS }.into())
    }
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(if self.inspector { crate::strings::STATUS } else { crate::strings::SETTINGS })
    }
}
impl Render for ChromePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ws = self.workspace.clone();
        let content = if self.inspector {
            let terminal = ws.upgrade().and_then(|w| {
                let w = w.read(cx);
                w.focused_terminal.as_deref().and_then(|id| w.terminals.iter().find(|t| t.id == id)).cloned().map(|t| (t, w.conn.is_connected()))
            });
            if let Some((t, connected)) = terminal {
                let mut badge = crate::orch::terminal_badge(&t);
                if !connected {
                    badge.stale_since = Some(crate::strings::CONNECTION_LOST.into());
                }
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(badge.render(mode()))
                    .child(badge.detail())
                    .child(crate::tr!(INSPECTOR_TERMINAL; ; t.id, t.program))
                    .child(crate::tr!(INSPECTOR_UPDATED; ; freshness(t.status.as_ref())))
            } else {
                div().child(crate::strings::INSPECTOR_EMPTY)
            }
        } else {
            let settings = cx.global::<Settings>().clone();
            let mut body = div().flex().flex_col().gap_4();
            for (label, values) in [
                ("Theme", vec!["system", "dark", "light"]),
                ("Font", vec!["Segoe UI", "Malgun Gothic", "Consolas"]),
                ("UI size", vec!["12", "13", "14", "16"]),
                ("Terminal font", vec!["Cascadia Mono", "Consolas", "D2Coding"]),
                ("Terminal size", vec!["12", "14", "16", "18"]),
                ("Density", vec!["compact", "comfortable"]),
                ("Motion", vec!["full", "reduced"]),
                ("고대비 미리보기", vec!["끔", "켬"]),
                ("Jump shortcut", vec!["ctrl-k", "ctrl-shift-k"]),
            ] {
                let mut row =
                    div().flex().flex_wrap().gap_2().items_center().child(div().w(px(110.)).flex_shrink_0().child(crate::strings::setting_label(label)));
                for value in values {
                    let mut next = settings.clone();
                    let selected = match label {
                        "Theme" => {
                            next.theme = value.into();
                            settings.theme == value
                        }
                        "Font" => {
                            next.font = value.into();
                            settings.font == value
                        }
                        "UI size" => {
                            next.font_size = value.parse().unwrap_or(13.);
                            settings.font_size == next.font_size
                        }
                        "Terminal font" => {
                            next.terminal_font = value.into();
                            settings.terminal_font == value
                        }
                        "Terminal size" => {
                            next.terminal_font_size = value.parse().unwrap_or(14.);
                            settings.terminal_font_size == next.terminal_font_size
                        }
                        "Density" => {
                            next.comfortable = value == "comfortable";
                            settings.comfortable == next.comfortable
                        }
                        "Motion" => {
                            next.reduced_motion = value == "reduced";
                            settings.reduced_motion == next.reduced_motion
                        }
                        "고대비 미리보기" => {
                            next.high_contrast = value == "켬";
                            settings.high_contrast == next.high_contrast
                        }
                        _ => {
                            next.jump_shortcut = value.into();
                            settings.jump_shortcut == value
                        }
                    };
                    let ws = ws.clone();
                    row = row.child(
                        Button::new(SharedString::from(format!("{label}-{value}")))
                            .label(crate::strings::setting_label(value))
                            .accessibility_label(format!("{}: {}", crate::strings::setting_label(label), crate::strings::setting_label(value)))
                            .when(selected, |b| b.primary())
                            .on_click(move |_, window, cx| {
                                let result = next.save();
                                if result.is_ok() {
                                    next.apply(cx);
                                }
                                let _ = ws.update(cx, |w, cx| {
                                    if result.is_ok() {
                                        for p in w.live_panels() {
                                            let v = p.read(cx).view.clone();
                                            v.update(cx, |v, cx| {
                                                v.set_palette(crate::theme::terminal_palette(), cx);
                                                v.set_font(next.terminal_font.clone().into(), next.terminal_font_size, cx);
                                            });
                                        }
                                    }
                                    w.refresh_editor_theme(cx);
                                    w.touch(cx);
                                    if let Err(e) = &result {
                                        w.raise_toast(ToastKind::Error, crate::strings::SETTINGS_COULD_NOT_BE_SAVED, e.to_string(), cx);
                                    }
                                });
                                window.refresh();
                            }),
                    );
                }
                body = body.child(row);
            }

            body.child(div().text_xs().child(crate::strings::CTRL_K))
        };
        div()
            .id("chrome-panel")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .bg(rgb(crate::theme::tokens().bg_surface))
            .text_color(rgb(crate::theme::tokens().text_primary))
            .child(content)
    }
}

/// Unicode subsequence matching, returning byte ranges for GPUI shaping.
fn fuzzy_positions(text: &str, query: &str) -> Option<Vec<std::ops::Range<usize>>> {
    let mut chars = text.char_indices();
    let mut ranges = Vec::new();
    for needle in query.chars().filter(|c| !c.is_whitespace()) {
        let (index, ch) = chars.find(|(_, c)| c.to_lowercase().to_string() == needle.to_lowercase().to_string())?;
        ranges.push(index..index + ch.len_utf8());
    }
    Some(ranges)
}
fn highlight(text: &str, query: &str) -> StyledText {
    let ranges = fuzzy_positions(text, query).unwrap_or_default();
    StyledText::new(text.to_owned()).with_highlights(ranges.into_iter().map(|range| {
        (
            range,
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                underline: Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false }),
                ..Default::default()
            },
        )
    }))
}
/// Static loading structure respects reduced motion and never masquerades as an empty result.
pub(crate) fn loading_placeholder(label: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .text_sm()
        .text_color(rgb(crate::theme::tokens().text_muted))
        .child(label)
        .children([160., 240., 200.].map(|width| div().w(px(width)).max_w_full().h(px(12.)).rounded_sm().bg(rgb(crate::theme::tokens().bg_hover))))
}

#[cfg(test)]
mod palette_tests {
    use super::fuzzy_positions;
    #[test]
    fn fuzzy_unicode_ranges_and_order() {
        assert_eq!(fuzzy_positions(crate::strings::FUZZY_SAMPLE, crate::strings::FUZZY_QUERY), Some(vec![0..3, 4..7]));
        assert!(fuzzy_positions("Terminals", "tml").is_some());
        assert!(fuzzy_positions("abc", "ca").is_none());
        assert_eq!(fuzzy_positions("abc", ""), Some(vec![]));
    }
    #[test]
    fn documented_shortcuts_match_in_app_help() {
        let design = include_str!("../../../docs/10-ui-design.md");
        for (keys, label) in super::SHORTCUTS {
            assert!(
                design.contains(&format!("| {keys} | {label} |")),
                "missing shortcut {keys}"
            );
        }
    }
}
