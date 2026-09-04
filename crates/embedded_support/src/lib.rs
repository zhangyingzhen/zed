//! 嵌入式（STM32/Cortex-M）开发支持：为嵌入式工程在状态栏提供
//! 编译 / 烧录 / 调试快捷按钮，并在打开工程时自动生成 `.zed/` 任务
//! 与调试配置（基于 CMakePresets.json 发现多芯片目标）。

mod scaffold;
pub mod status_buttons;

pub use status_buttons::EmbeddedButtons;

use gpui::App;

pub fn init(cx: &mut App) {
    scaffold::init(cx);
}
