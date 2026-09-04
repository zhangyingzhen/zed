//! 打开工程后的自动配置：识别嵌入式工程，生成 `.zed/tasks.json`、
//! `.zed/debug.json` 与 `.zed/embedded.json`（只补缺失文件，从不覆盖）。

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use gpui::{App, Context};
use serde_json::{Value, json};
use workspace::Workspace;

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, cx: &mut Context<Workspace>| {
            let project = workspace.project().clone();
            cx.spawn(async move |workspace, cx| {
                let _ = project
                    .update(cx, |project, cx| project.wait_for_initial_scan(cx))
                    .await;
                let outcome = workspace.update(cx, |workspace, cx| generate(workspace, cx));
                match outcome {
                    Ok(Ok(Some(message))) => log::info!("embedded_support: {message}"),
                    Ok(Err(error)) => {
                        log::warn!("embedded_support: 生成配置失败: {error:#}")
                    }
                    _ => {}
                }
            })
            .detach();
        },
    )
    .detach();
}

/// 工程是否为嵌入式（STM32/Cortex-M 交叉编译）工程。
pub fn is_embedded_project(root: &Path) -> bool {
    if root.join(".zed/embedded.json").is_file() || root.join(".zed/debug.json").is_file() {
        return true;
    }
    if root.join("CMakePresets.json").is_file() {
        if root.join("MCU.cmake").is_file() {
            return true;
        }
        if let Ok(list) = std::fs::read_to_string(root.join("CMakeLists.txt")) {
            if list.contains("arm-none-eabi") {
                return true;
            }
        }
    }
    false
}

/// 读取 `.zed/tasks.json` 中的任务标签（用于状态栏按钮）。
pub fn read_tasks_labels(root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join(".zed/tasks.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let tasks = value.as_array().cloned().unwrap_or_else(|| {
        value
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    });
    tasks
        .iter()
        .filter_map(|task| {
            task.get("label")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

pub fn generate(workspace: &mut Workspace, cx: &mut App) -> Result<Option<String>> {
    let Some(worktree) = workspace.visible_worktrees(cx).next() else {
        bail!("工作区没有可见的工程目录");
    };
    let root = worktree.read(cx).abs_path().to_path_buf();
    log::info!(
        "embedded_support: generate checking root={}",
        root.display()
    );
    if !is_embedded_project(&root) {
        return Ok(None);
    }

    let zed_dir = root.join(".zed");
    std::fs::create_dir_all(&zed_dir).context("创建 .zed 目录失败")?;

    let tasks_path = zed_dir.join("tasks.json");
    let debug_path = zed_dir.join("debug.json");
    let embedded_path = zed_dir.join("embedded.json");

    let cmake = which_or("cmake");
    let size = which_or("arm-none-eabi-size");
    let probe_rs = find_probe_rs();
    let speed = read_config(&embedded_path)
        .as_ref()
        .and_then(|cfg| cfg.get("speed").and_then(Value::as_u64));

    let mut generated = Vec::new();

    if !tasks_path.is_file() {
        let presets = discover_presets(&root);
        let tasks = if presets.is_empty() {
            default_tasks(&cmake, &size, &probe_rs, &read_config(&embedded_path))
        } else {
            preset_tasks(&presets, &cmake, &size, &probe_rs, speed)
        };
        std::fs::write(&tasks_path, serde_json::to_string_pretty(&tasks)?)?;
        generated.push("tasks.json");
    }
    if !debug_path.is_file() {
        let presets = discover_presets(&root);
        let scenarios = if presets.is_empty() {
            default_debug(&read_config(&embedded_path))
        } else {
            preset_debug(&presets)
        };
        std::fs::write(&debug_path, serde_json::to_string_pretty(&scenarios)?)?;
        generated.push("debug.json");
    }
    if !embedded_path.is_file() {
        let presets = discover_presets(&root);
        let chip = presets
            .iter()
            .find_map(|preset| preset.chip.clone())
            .unwrap_or_else(|| "STM32F103C8".to_string());
        let template = json!({
            "_readme": "由 Zed 嵌入式支持自动生成；chip 可用 `probe-rs chip list` 查询。",
            "backend": "probe-rs",
            "chip": chip,
            "buildDir": presets
                .first()
                .map(|preset| preset.build_dir_rel.clone())
                .unwrap_or_else(|| "build".to_string()),
            "autoDownload": true
        });
        std::fs::write(&embedded_path, serde_json::to_string_pretty(&template)?)?;
        generated.push("embedded.json");
    }

    if generated.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "已为 {} 生成 {}",
            root.display(),
            generated.join("、")
        )))
    }
}

// ---------- CMakePresets 发现 ----------

pub struct Preset {
    pub name: String,
    pub build_dir_rel: String,
    pub chip: Option<String>,
    pub elf_rel: String,
}

/// ST 命名尾段结构固定为 `[引脚码][闪存码][封装码][温度码]`（如 f407vgt6），
/// 取 family + 前两位即 probe-rs 目标名（STM32F407VG）。
pub fn infer_chip(raw: &str) -> Option<String> {
    let rest = raw.trim().to_ascii_lowercase();
    let rest = rest.strip_prefix("stm32")?;
    if rest.len() < 6 {
        return None;
    }
    let family = &rest[..4];
    if !family.starts_with(['a', 'c', 'f', 'g', 'h', 'l', 'u', 'w'])
        || !family[1..].chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let tail = &rest[4..];
    let core = match tail.len() {
        2 => tail,
        len if len >= 3 => &tail[..2],
        _ => return None,
    };
    if !core.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(format!(
        "STM32{}{}",
        family.to_uppercase(),
        core.to_uppercase()
    ))
}

fn discover_presets(root: &Path) -> Vec<Preset> {
    let Ok(text) = std::fs::read_to_string(root.join("CMakePresets.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let project = project_name(root);
    let Some(presets) = value.get("configurePresets").and_then(Value::as_array) else {
        return Vec::new();
    };
    presets
        .iter()
        .filter(|preset| {
            !preset
                .get("hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|preset| {
            let name = preset.get("name").and_then(Value::as_str)?.to_string();
            let build_dir_rel = preset
                .get("binaryDir")
                .and_then(Value::as_str)
                .map(|dir| {
                    dir.replace("${sourceDir}", &root.to_string_lossy())
                        .replace("${sourceDir}/", "")
                })
                .map(|expanded| {
                    Path::new(&expanded)
                        .strip_prefix(root)
                        .map(|rel| rel.to_string_lossy().into_owned())
                        .unwrap_or(expanded)
                })
                .unwrap_or_else(|| format!("build/{name}"));
            let chip = preset
                .get("cacheVariables")
                .and_then(|cv| cv.get("LY_MCU").or_else(|| cv.get("CHIP")))
                .and_then(Value::as_str)
                .and_then(infer_chip);
            let elf_rel = format!(
                "{build_dir_rel}/{}.elf",
                project.clone().unwrap_or_else(|| "fw".to_string())
            );
            Some(Preset {
                name,
                build_dir_rel,
                chip,
                elf_rel,
            })
        })
        .collect()
}

fn project_name(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("CMakeLists.txt")).ok()?;
    let line = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or(line))
        .find(|line| line.trim_start().starts_with("project("))?;
    let rest = line.trim_start().trim_start_matches("project(");
    let name = rest
        .split(|c: char| c == ')' || c.is_whitespace())
        .next()?
        .trim_matches(|c: char| c == '"' || c == '\'')
        .to_string();
    if name.is_empty() { None } else { Some(name) }
}

// ---------- 任务/场景生成 ----------

fn preset_tasks(
    presets: &[Preset],
    cmake: &str,
    size: &str,
    probe_rs: &str,
    speed: Option<u64>,
) -> Value {
    let mut tasks = Vec::new();
    for preset in presets {
        let name = &preset.name;
        tasks.push(json!({
            "label": format!("{name}: configure"),
            "command": cmake,
            "args": ["--preset", name],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        }));
        tasks.push(json!({
            "label": format!("{name}: build"),
            "command": cmake,
            "args": ["--build", "--preset", name],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        }));
        tasks.push(json!({
            "label": format!("{name}: clean"),
            "command": cmake,
            "args": ["--build", "--preset", name, "--target", "clean"],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        }));
        tasks.push(json!({
            "label": format!("{name}: size"),
            "command": size,
            "args": [preset.elf_rel],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        }));
        if let Some(chip) = &preset.chip {
            let mut flash_args = vec![
                json!("download"),
                json!("--chip"),
                json!(chip),
                json!("--reset"), // 烧录完成后复位并自动运行固件
            ];
            if let Some(speed) = speed {
                flash_args.push(json!("--speed"));
                flash_args.push(json!(speed));
            }
            flash_args.push(json!(preset.elf_rel));
            tasks.push(json!({
                "label": format!("{name}: flash"),
                "command": probe_rs,
                "args": flash_args,
                "cwd": "$ZED_WORKTREE_ROOT",
                "reveal": "always"
            }));
            tasks.push(json!({
                "label": format!("{name}: erase"),
                "command": probe_rs,
                "args": ["erase", "--chip", chip],
                "cwd": "$ZED_WORKTREE_ROOT",
                "reveal": "always"
            }));
            tasks.push(json!({
                "label": format!("{name}: reset"),
                "command": probe_rs,
                "args": ["reset", "--chip", chip],
                "cwd": "$ZED_WORKTREE_ROOT",
                "reveal": "always"
            }));
        }
    }
    Value::Array(tasks)
}

fn preset_debug(presets: &[Preset]) -> Value {
    Value::Array(
        presets
            .iter()
            .map(|preset| {
                let mut scenario = json!({
                    "label": format!(
                        "{}: debug {}",
                        preset.name,
                        preset.chip.clone().unwrap_or_else(|| preset.name.clone())
                    ),
                    "adapter": "yz61-embedded",
                    "request": "launch",
                    "program": preset.elf_rel,
                    "stop_on_entry": false,
                    "build": format!("{}: build", preset.name)
                });
                if let Some(chip) = &preset.chip {
                    scenario["chip"] = json!(chip);
                }
                scenario
            })
            .collect(),
    )
}

fn default_tasks(cmake: &str, size: &str, probe_rs: &str, config: &Option<Value>) -> Value {
    let config = config.clone().unwrap_or_else(|| json!({}));
    let build_dir = config
        .get("buildDir")
        .and_then(Value::as_str)
        .unwrap_or("build")
        .to_string();
    let target = config
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("fw")
        .to_string();
    let chip = config
        .get("chip")
        .and_then(Value::as_str)
        .unwrap_or("STM32F103C8")
        .to_string();
    let elf = format!("{build_dir}/{target}.elf");
    let toolchain = config
        .get("toolchainFile")
        .and_then(Value::as_str)
        .unwrap_or("cmake/arm-gcc-toolchain.cmake")
        .to_string();
    json!([
        {
            "label": "stm32: configure",
            "command": cmake,
            "args": [
                "-G", "Ninja", "-S", ".", "-B", build_dir,
                "-DCMAKE_BUILD_TYPE=RelWithDebInfo",
                format!("-DCMAKE_TOOLCHAIN_FILE={toolchain}")
            ],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        },
        {
            "label": "stm32: build",
            "command": cmake,
            "args": ["--build", build_dir],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        },
        {
            "label": "stm32: clean",
            "command": cmake,
            "args": ["--build", build_dir, "--target", "clean"],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        },
        {
            "label": "stm32: size",
            "command": size,
            "args": [elf],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        },
        {
            "label": "stm32: flash",
            "command": probe_rs,
            "args": ["download", "--chip", chip, "--reset", elf],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        },
        {
            "label": "stm32: erase",
            "command": probe_rs,
            "args": ["erase", "--chip", chip],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        },
        {
            "label": "stm32: reset",
            "command": probe_rs,
            "args": ["reset", "--chip", chip],
            "cwd": "$ZED_WORKTREE_ROOT",
            "reveal": "always"
        }
    ])
}

fn default_debug(config: &Option<Value>) -> Value {
    let config = config.clone().unwrap_or_else(|| json!({}));
    let build_dir = config
        .get("buildDir")
        .and_then(Value::as_str)
        .unwrap_or("build");
    let target = config.get("target").and_then(Value::as_str).unwrap_or("fw");
    let chip = config.get("chip").and_then(Value::as_str);
    let mut scenario = json!({
        "label": format!("stm32: debug {}", chip.unwrap_or("STM32")),
        "adapter": "yz61-embedded",
        "request": "launch",
        "program": format!("{build_dir}/{target}.elf"),
        "stop_on_entry": false,
        "build": "stm32: build"
    });
    if let Some(chip) = chip {
        scenario["chip"] = json!(chip);
    }
    Value::Array(vec![scenario])
}

// ---------- 工具辅助 ----------

fn read_config(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn which_or(program: &str) -> String {
    which::which(program)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| program.to_string())
}

/// probe-rs 定位：PATH → `~/.yz61-embedded/tools/probe-rs/<版本>/`（yz61-embedded 扩展的托管目录）。
fn find_probe_rs() -> String {
    if let Ok(path) = which::which("probe-rs") {
        return path.to_string_lossy().into_owned();
    }
    let Some(home) = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
    else {
        return "probe-rs".to_string();
    };
    let managed = home.join(".yz61-embedded/tools/probe-rs");
    if let Ok(entries) = std::fs::read_dir(&managed) {
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        versions.sort();
        if let Some(latest) = versions.pop() {
            let exe = if cfg!(windows) {
                "probe-rs.exe"
            } else {
                "probe-rs"
            };
            if let Some(found) = find_file(&latest, exe, 4) {
                return found.to_string_lossy().into_owned();
            }
        }
    }
    "probe-rs".to_string()
}

fn find_file(dir: &Path, name: &str, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in &paths {
        if path.is_file() && path.file_name().is_some_and(|n| n == name) {
            return Some(path.clone());
        }
    }
    for path in &paths {
        if path.is_dir() {
            if let Some(found) = find_file(path, name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// 在工程里定位含 `int main(` 的 C 源文件，返回 worktree 相对的 unix 风格路径。
/// 优先常见路径与 `User/` 子树（本脚手架约定 CubeMX 的 main.c 不含 main()，
/// 入口由 User/ 提供），最后浅层递归（跳过 build/Examples 等目录）。
pub fn find_main_source(root: &Path) -> Option<String> {
    let candidates = [
        "User/app/app_main.c",
        "User/main.c",
        "User/src/main.c",
        "Core/Src/main.c",
        "src/main.c",
        "Src/main.c",
        "main.c",
    ];
    for candidate in candidates {
        let path = root.join(candidate);
        if path.is_file() && source_contains_main(&path) {
            return Some(candidate.to_string());
        }
    }
    // User/ 子树浅搜：入口约定在 User/ 下，但文件名/层级可能不同。
    let mut found = None;
    walk(&root.join("User"), root, 4, &mut found);
    if found.is_some() {
        return found;
    }
    // 其余目录浅搜，跳过构建产物与三方库中的示例/测试源（常含诱饵 main）。
    walk(root, root, 5, &mut found);
    return found;

    fn walk(dir: &Path, root: &Path, depth: u8, found: &mut Option<String>) {
        if depth == 0 || found.is_some() || !dir.is_dir() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut names: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        names.sort();
        let mut subdirs = Vec::new();
        for path in &names {
            let file_name = path.file_name();
            let name = file_name
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if path.is_dir() {
                if matches!(
                    name.as_str(),
                    "build"
                        | ".git"
                        | ".zed"
                        | "Drivers"
                        | "Middlewares"
                        | "node_modules"
                        | "Examples"
                        | "examples"
                        | "Test"
                        | "Tests"
                        | "templates"
                ) || name.starts_with('.')
                {
                    continue;
                }
                subdirs.push(path.clone());
            } else if name.ends_with(".c") && source_contains_main(path) {
                if let Ok(rel) = path.strip_prefix(root) {
                    *found = Some(rel.to_string_lossy().replace('\\', "/"));
                }
                return;
            }
        }
        for sub in subdirs {
            walk(&sub, root, depth - 1, found);
            if found.is_some() {
                return;
            }
        }
    }
}

fn source_contains_main(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines()
        .any(|line| line.contains("int main(") || line.contains("void main("))
}

/// 返回 main 函数体内第一条语句所在的 0 基行号。
/// main 签名行本身不是可执行语句，断点打在那里可能不绑定；从签名行向后
/// 找到 `{`，再跳过空行、注释与预处理行，落在第一条语句上。
/// 找不到 `{` 或语句时回退（签名行 / `{` 行）。
pub fn main_body_first_statement_row(text: &str) -> Option<u32> {
    let lines: Vec<&str> = text.lines().collect();
    let main_row = lines
        .iter()
        .position(|line| line.contains("int main(") || line.contains("void main("))?;
    let brace_row = lines
        .iter()
        .enumerate()
        .skip(main_row)
        .find(|(_, line)| line.contains('{'))
        .map(|(i, _)| i)
        .unwrap_or(main_row);
    let mut in_block_comment = false;
    for (i, line) in lines.iter().enumerate().skip(brace_row + 1) {
        let mut trimmed = line.trim();
        if in_block_comment {
            match trimmed.find("*/") {
                Some(end) => {
                    in_block_comment = false;
                    trimmed = trimmed[end + 2..].trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                }
                None => continue,
            }
        }
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        if trimmed.starts_with("/*") {
            if !trimmed[2..].contains("*/") {
                in_block_comment = true;
            }
            continue;
        }
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        // 函数体结束（空函数体等）：回退到 `{` 行。
        if trimmed.starts_with('}') {
            break;
        }
        return Some(i as u32);
    }
    Some(brace_row as u32)
}

#[cfg(test)]
mod tests {
    use super::{find_main_source, main_body_first_statement_row};
    use std::fs;

    /// 复刻 ly3015 布局：CubeMX main.c 无 main()，真入口在 User/app/app_main.c，
    /// Packages/rtt/Examples 里还有个（被 gitignore 的）诱饵 main。
    #[test]
    fn find_main_source_prefers_user_entry_over_decoys() {
        let root = tempfile::tempdir().unwrap();
        let write = |rel: &str, body: &str| {
            let path = root.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        };
        write(
            "MCUs/stm32f407vgt6/Core/Src/main.c",
            "/* CubeMX: no main() here */\nvoid SystemClock_Config(void){}\n",
        );
        write(
            "Packages/rtt/Examples/Main_RTT_SpeedTestApp.c",
            "int main(void){}\n",
        );
        write(
            "User/app/app_main.c",
            "#include \"main.h\"\nint main(void){}\n",
        );

        let found = find_main_source(root.path());
        assert_eq!(
            found.as_deref(),
            Some("User/app/app_main.c"),
            "must pick the User entry, not the Packages example decoy"
        );
    }

    #[test]
    fn find_main_source_plain_layouts() {
        let root = tempfile::tempdir().unwrap();
        let write = |rel: &str, body: &str| {
            let path = root.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        };
        assert_eq!(find_main_source(root.path()), None);
        write("Core/Src/main.c", "int main(void){}\n");
        assert_eq!(
            find_main_source(root.path()),
            Some("Core/Src/main.c".into())
        );

        let root2 = tempfile::tempdir().unwrap();
        let path = root2.path().join("src/main.c");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "int main(void){}\n").unwrap();
        assert_eq!(find_main_source(root2.path()), Some("src/main.c".into()));
    }

    /// app_main.c 的真实形状（此处压缩到文件头部）：签名行后是 `{`、
    /// 块注释、第一条语句 HAL_Init()。
    #[test]
    fn main_body_row_lands_on_first_statement() {
        let text = "\
#include \"main.h\"

int main(void)
{
    /* 1. HAL 与时钟 */
    HAL_Init();
    SystemClock_Config();
}";
        assert_eq!(main_body_first_statement_row(text), Some(5));
    }

    #[test]
    fn main_body_row_edge_cases() {
        // `{` 与签名同行：第一条语句在下一行。
        assert_eq!(
            main_body_first_statement_row("int main(void) {\n    foo();\n}"),
            Some(1)
        );
        // 没有独立语句（空函数体）：回退到 `{` 行。
        assert_eq!(
            main_body_first_statement_row("int main(void)\n{\n}\n"),
            Some(1)
        );
        // 没有 main：None。
        assert_eq!(main_body_first_statement_row("int foo(void){}\n"), None);
        // 语句前的多行注释与预处理行都被跳过。
        assert_eq!(
            main_body_first_statement_row(
                "int main(void)\n{\n/* a\nb */\n#ifdef X\n#endif\nrun();\n}"
            ),
            Some(6)
        );
    }
}
