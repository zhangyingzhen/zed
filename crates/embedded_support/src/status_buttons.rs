//! 状态栏的嵌入式快捷按钮：编译 / 烧录 / 调试。
//! 仅当当前工作区被识别为嵌入式工程时渲染（见 scaffold::is_embedded_project）。

use gpui::{
    div, Action, App, ClickEvent, Context, IntoElement, ParentElement as _, Render, Styled as _,
    Subscription, WeakEntity, Window,
};
use icons::IconName;
use project::worktree_store::WorktreeStoreEvent;
use ui::{h_flex, prelude::*, IconButton, IconSize, Tooltip};
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace};

use crate::scaffold;

const EMBEDDED_ADAPTER: &str = "yz61-embedded";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx: &mut Context<Workspace>| {
        let Some(window) = window else { return };
        let buttons = cx.new(|cx| EmbeddedButtons::new(workspace, window, cx));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(buttons, window, cx);
        });
    })
    .detach();
}

pub struct EmbeddedButtons {
    workspace: WeakEntity<Workspace>,
    visible: bool,
    build_task: Option<String>,
    flash_task: Option<String>,
    _worktree_subscription: Option<Subscription>,
}

impl EmbeddedButtons {
    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            workspace: workspace.weak_handle(),
            visible: false,
            build_task: None,
            flash_task: None,
            _worktree_subscription: None,
        };
        let project = workspace.project().clone();
        let worktree_store = project.read(cx).worktree_store().clone();
        this._worktree_subscription =
            Some(cx.subscribe(&worktree_store, |this, _, event, cx| match event {
                WorktreeStoreEvent::WorktreeAdded(_) | WorktreeStoreEvent::WorktreeUpdatedEntries(..) => {
                    this.refresh(cx)
                }
                _ => {}
            }));
        // 初始扫描完成后做首次检测。
        cx.spawn_in(window, async move |this, cx| {
            let _ = project
                .update(cx, |project, cx| project.wait_for_initial_scan(cx))
                .await;
            this.update(cx, |this, cx| this.refresh(cx)).ok();
        })
        .detach();
        this
    }

    /// 重新检测工程特征并缓存可用任务标签；仅在状态变化时刷新 UI。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(worktree) = workspace.read(cx).visible_worktrees(cx).next() else {
            return;
        };
        let root = worktree.read(cx).abs_path().to_path_buf();

        let visible = scaffold::is_embedded_project(&root);
        if visible {
            let labels = scaffold::read_tasks_labels(&root);
            self.build_task = labels
                .iter()
                .find(|label| label.ends_with(": build") || label.as_str() == "stm32: build")
                .cloned();
            self.flash_task = labels
                .iter()
                .find(|label| label.contains("flash") || label.contains("download"))
                .cloned();
        }
        if self.visible != visible {
            self.visible = visible;
        }
        cx.notify();
    }

    fn dispatch_task(
        &mut self,
        label: Option<String>,
        _ev: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let action = match label {
            Some(task_name) => tasks_ui::Spawn::ByName {
                task_name,
                reveal_target: None,
            },
            None => tasks_ui::Spawn::ViaModal { reveal_target: None },
        };
        window.dispatch_action(action.boxed_clone(), cx);
    }

    fn on_debug(&mut self, _ev: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(provider) = workspace.read(cx).debugger_provider() else {
            window.dispatch_action(debugger_ui::Start.boxed_clone(), cx);
            return;
        };

        cx.spawn_in(window, async move |_, cx| {
            let Ok((task_contexts_fut, inventory)) = workspace
                .update_in(cx, |workspace, window, cx| {
                    let task_contexts = tasks_ui::task_contexts(workspace, window, cx);
                    let inventory = workspace
                        .project()
                        .read(cx)
                        .task_store()
                        .read(cx)
                        .task_inventory()
                        .cloned();
                    (task_contexts, inventory)
                })
            else {
                return;
            };
            let Some(inventory) = inventory else {
                return;
            };
            let contexts = task_contexts_fut.await;
            let listing = inventory
                .update(cx, |inventory, cx| {
                    inventory.list_debug_scenarios(&contexts, vec![], vec![], false, cx)
                })
                .await;
            let (scenarios, _) = listing;
            let chosen = scenarios
                .iter()
                .find(|(scenario, _)| scenario.adapter == EMBEDDED_ADAPTER)
                .or_else(|| scenarios.first())
                .cloned();
            let Some((scenario, context)) = chosen else {
                return;
            };
            workspace
                .update_in(cx, |_workspace, window, cx| {
                    provider.start_session(
                        scenario,
                        context.task_context,
                        None,
                        context.worktree_id,
                        window,
                        cx,
                    );
                })
                .ok();
        })
        .detach();
    }
}

impl StatusItemView for EmbeddedButtons {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    // 按钮自带工程特征判定（非嵌入式工程不渲染），此处不再提供额外隐藏设置。
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

impl Render for EmbeddedButtons {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div().hidden().into_any_element();
        }
        let build_task = self.build_task.clone();
        let flash_task = self.flash_task.clone();

        h_flex()
            .gap_1()
            .child(
                IconButton::new("embedded-build", IconName::ToolHammer)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        this.dispatch_task(build_task.clone(), ev, window, cx)
                    }))
                    .tooltip(Tooltip::text("嵌入式编译（点击执行构建任务）")),
            )
            .child(
                IconButton::new("embedded-flash", IconName::BoltFilled)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        this.dispatch_task(flash_task.clone(), ev, window, cx)
                    }))
                    .tooltip(Tooltip::text("嵌入式烧录（下载固件到目标板）")),
            )
            .child(
                IconButton::new("embedded-debug", IconName::Debug)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(Self::on_debug))
                    .tooltip(Tooltip::text("嵌入式调试（编译 → 烧录 → 挂起）")),
            )
            .into_any_element()
    }
}
