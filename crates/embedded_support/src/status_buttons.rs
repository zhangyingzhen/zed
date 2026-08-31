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

/// 定位工程里的 main 函数并在其所在行添加断点（已存在则不动）。
/// 失败静默跳过（找不到 main 或无法打开 buffer 都不阻断调试启动）。
async fn ensure_main_breakpoint(
    workspace: &gpui::WeakEntity<Workspace>,
    cx: &mut gpui::AsyncApp,
) {
    let open_task = workspace.update(cx, |workspace, cx| {
        let Some(worktree) = workspace.visible_worktrees(cx).next() else {
            return None;
        };
        let root = worktree.read(cx).abs_path().to_path_buf();
        let main_rel = scaffold::find_main_source(&root)?;
        let worktree_id = worktree.read(cx).id();
        let rel_path = util::rel_path::RelPath::from_unix_str(&main_rel).ok()?;
        let project = workspace.project().clone();
        let open = project.update(cx, |project, cx| {
            project.open_buffer(
                project::ProjectPath {
                    worktree_id,
                    path: std::sync::Arc::from(rel_path),
                },
                cx,
            )
        });
        Some(open)
    });
    let Ok(Some(open_task)) = open_task else {
        return;
    };
    let Ok(buffer) = open_task.await else {
        return;
    };
    let _ = workspace.update(cx, |workspace, cx| {
        use project::debugger::breakpoint_store::{
            Breakpoint, BreakpointEditAction, BreakpointStore, BreakpointWithPosition,
        };
        let buffer_snapshot = buffer.read(cx).snapshot();
        let text = buffer_snapshot.text();
        let Some(row) = text
            .lines()
            .position(|line| line.contains("int main(") || line.contains("void main("))
        else {
            return;
        };
        let row = row as u32;
        let breakpoint_store = workspace
            .project()
            .read(cx)
            .dap_store()
            .read(cx)
            .breakpoint_store()
            .clone();
        let Some(abs_path) = BreakpointStore::abs_path_from_buffer(&buffer, cx) else {
            return;
        };
        if breakpoint_store
            .read(cx)
            .breakpoint_at_row(&abs_path, row, cx)
            .is_some()
        {
            return;
        }
        let anchor = buffer_snapshot.anchor_after(language::PointUtf16::new(row, 0));
        breakpoint_store.update(cx, |store, cx| {
            store.toggle_breakpoint(
                buffer.clone(),
                BreakpointWithPosition {
                    position: anchor,
                    bp: Breakpoint::new_standard(),
                },
                BreakpointEditAction::Toggle,
                cx,
            );
        });
        log::info!("embedded_support: added main breakpoint at row {}", row + 1);
    });
}

pub struct EmbeddedButtons {
    workspace: WeakEntity<Workspace>,
    visible: bool,
    build_task: Option<String>,
    flash_task: Option<String>,
    _worktree_subscription: Option<Subscription>,
}

impl EmbeddedButtons {
    /// 由 zed.rs 的 initialize_workspace 在诊断指示器注册之后调用，
    /// 使按钮出现在 Project Diagnostics 图标的右侧。
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        log::info!(
            "embedded_support: refresh root={} visible={}",
            root.display(),
            visible
        );
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
        // 调试适配器由 yz61-embedded 扩展提供；扩展未加载时给出明确提示
        //（DebugPanel::start_session 对缺失适配器是静默返回的）。
        if dap::DapRegistry::global(cx).adapter(EMBEDDED_ADAPTER).is_none() {
            log::error!("embedded_support: adapter '{}' not registered", EMBEDDED_ADAPTER);
            workspace.update(cx, |workspace, cx| {
                workspace.show_error(
                    anyhow::anyhow!(
                        "未找到 yz61-embedded 调试适配器：请打开扩展面板，\
                         对 yz61-embedded 执行 Reinstall Dev Extension 后重试"
                    ),
                    cx,
                );
            });
            return;
        }
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
            log::info!(
                "embedded_support: contexts worktree={:?} active_ctx={} other_ctxs={}",
                contexts.worktree(),
                contexts.active_worktree_context.is_some(),
                contexts.other_worktree_contexts.len()
            );
            let listing = inventory
                .update(cx, |inventory, cx| {
                    inventory.list_debug_scenarios(&contexts, vec![], vec![], false, cx)
                })
                .await;
            let (scenarios, _) = listing;
            log::info!(
                "embedded_support: {} debug scenario(s) found",
                scenarios.len()
            );
            let chosen = scenarios
                .iter()
                .find(|(scenario, _)| scenario.adapter == EMBEDDED_ADAPTER)
                .or_else(|| scenarios.first())
                .cloned();
            let Some((scenario, context)) = chosen else {
                // 一个场景都没查到（例如 debug.json 尚未加载进 Inventory）：
                // 回退到官方面板，避免按钮无反应。
                log::warn!("embedded_support: no debug scenarios, falling back to debugger::Start");
                workspace
                    .update_in(cx, |_workspace, window, cx| {
                        window.dispatch_action(debugger_ui::Start.boxed_clone(), cx);
                    })
                    .ok();
                return;
            };

            // 启动前确保 main 处有断点：launch 后 Zed 会在 configurationDone 前
            // 把断点发给 probe-rs，固件跑到 main 即停（等效“停在入口”）。
            ensure_main_breakpoint(&workspace.downgrade(), cx).await;

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
