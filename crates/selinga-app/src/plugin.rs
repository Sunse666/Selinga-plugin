//! StarPie 插件入口（仅本仓库存在，独立版没有这个模块）。
//!
//! ## 为什么是命令行而不是 IPC
//!
//! 宿主当前能给进程插件的只有「按一下扇区 → 拉起一个进程」。在 JSON-RPC 协议落地之前，
//! 最稳的契约就是**参数 + 退出码**：宿主说做什么，Selinga 做什么，框选与译文全部留在
//! Selinga 自己的进程里。于是同一个二进制既能被 StarPie 当插件调用，也依然能双击当独立程序用。
//!
//! ## 插件版与独立版的差异（判断点只有 `Option<PluginMode>` 一处）
//!
//! - **不建托盘、不注册全局热键**：触发权归宿主。两套全局热键共存只会互相抢键，
//!   而「宿主里运行的插件」也不需要额外的托盘图标。
//! - **不同步开机自启**：`HKCU\\...\\Run` 是独立版的意图；插件的启停由宿主管理。
//! - **重复触发 = 重新框选**：单实例的第二份进程不再只是把旧窗口拎到前台，
//!   而是通过命名事件让已运行实例重新进入框选，见 `selinga_platform::request_reselect`。
//!
//! ## 参数
//!
//! ```text
//! selinga.exe --starpie-action translate [--target-lang zh-CN] [--display-mode bilingual]
//! ```
//!
//! 目前只有一个动作 `translate`：启动即框选，选完翻译，框常驻到用户关闭为止。
//! 这是「一次触发完成一件事」的最小形态；将来若改走 JSON-RPC，这里演进成握手即可，
//! 命令行参数保持向后兼容，宿主的 v0.5 调用方式不用改。

use crate::{DisplayMode, TARGET_LANGS, display_mode_from};

/// 宿主经 `--starpie-action` 传入的动作名。
pub const ACTION_TRANSLATE: &str = "translate";

/// 打开配置文件。宿主把第二个扇区绑到这里，就是一个"插件自己的设置入口"。
pub const ACTION_SETTINGS: &str = "settings";

#[derive(Debug, Clone)]
pub struct PluginMode {
    /// 动作名。未知动作按框选翻译处理 —— 宁可多做一次框选，也不要静默什么都不做。
    pub action: String,
    pub target_lang: Option<String>,
    pub display_mode: Option<DisplayMode>,
    /// 宿主给定的矩形（物理像素）：<c>x,y,w,h</c>。给了就跳过框选层。
    pub region: Option<(i32, i32, u32, u32)>,
}

impl PluginMode {
    /// 解析命令行。**没有 `--starpie-action` 就返回 `None`**，
    /// 独立版的启动路径因此完全不受影响 —— 这是插件化唯一必须守住的前提。
    pub fn from_args<I>(args: I) -> Option<Self>
    where
        I: IntoIterator<Item = String>,
    {
        let mut action: Option<String> = None;
        let mut target_lang = None;
        let mut display_mode = None;
        let mut region = None;
        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--starpie-action" => action = iter.next(),
                "--target-lang" => target_lang = iter.next(),
                "--display-mode" => {
                    if let Some(value) = iter.next() {
                        display_mode = Some(display_mode_from(&value));
                    }
                }
                "--region" => {
                    if let Some(value) = iter.next() {
                        region = parse_region(&value);
                    }
                }
                _ => {}
            }
        }
        let action = action?;
        Some(Self {
            action,
            target_lang,
            display_mode,
            region,
        })
    }

    /// 目标语言在 `TARGET_LANGS` 里的下标；宿主给了未知值时返回 `None`，交给设置兜底。
    pub fn target_lang_index(&self) -> Option<usize> {
        let lang = self.target_lang.as_deref()?;
        TARGET_LANGS
            .iter()
            .position(|candidate| candidate.eq_ignore_ascii_case(lang))
    }

    /// 宿主给定矩形时转成截屏用的区域。宽高为 0 视为未给。
    pub fn region(&self) -> Option<selinga_capture::Region> {
        self.region.map(|(x, y, w, h)| selinga_capture::Region { x, y, w, h })
    }

    pub fn describe(&self) -> String {
        format!(
            "action={} target_lang={:?} display_mode={:?} region={:?}",
            self.action, self.target_lang, self.display_mode, self.region
        )
    }
}

/// 解析 `x,y,w,h`（物理像素）。任何一段不合法都整体作废 —— 半个矩形比没有矩形更糟。
fn parse_region(raw: &str) -> Option<(i32, i32, u32, u32)> {
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() != 4 {
        return None;
    }
    let x: i32 = parts[0].trim().parse().ok()?;
    let y: i32 = parts[1].trim().parse().ok()?;
    let w: u32 = parts[2].trim().parse().ok()?;
    let h: u32 = parts[3].trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((x, y, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn absent_action_keeps_standalone_behaviour() {
        assert!(PluginMode::from_args(args(&[])).is_none());
        assert!(PluginMode::from_args(args(&["--target-lang", "ja-JP"])).is_none());
        // 动作名后面没有值：同样不算插件模式，避免把空动作当成有效请求。
        assert!(PluginMode::from_args(args(&["--starpie-action"])).is_none());
    }

    #[test]
    fn parses_action_and_overrides() {
        let mode = PluginMode::from_args(args(&[
            "--starpie-action",
            "translate",
            "--target-lang",
            "ja-JP",
            "--display-mode",
            "bilingual",
        ]))
        .expect("plugin mode");
        assert_eq!(mode.action, "translate");
        assert_eq!(mode.target_lang_index(), Some(2));
        assert_eq!(mode.display_mode, Some(DisplayMode::Bilingual));
    }

    #[test]
    fn unknown_language_falls_back_to_settings() {
        let mode = PluginMode::from_args(args(&[
            "--starpie-action",
            "translate",
            "--target-lang",
            "xx-YY",
        ]))
        .expect("plugin mode");
        assert_eq!(mode.target_lang_index(), None);
    }

    #[test]
    fn parses_region_and_rejects_bad_input() {
        let mode = PluginMode::from_args(args(&[
            "--starpie-action",
            "translate",
            "--region",
            "100,200,800,400",
        ]))
        .expect("plugin mode");
        assert_eq!(mode.region, Some((100, 200, 800, 400)));
        assert_eq!(
            mode.region(),
            Some(selinga_capture::Region {
                x: 100,
                y: 200,
                w: 800,
                h: 400
            })
        );

        for bad in ["1,2,3", "a,b,c,d", "1,2,0,4", "1,2,3,-4"] {
            let mode = PluginMode::from_args(args(&[
                "--starpie-action",
                "translate",
                "--region",
                bad,
            ]))
            .expect("plugin mode");
            assert!(mode.region.is_none(), "非法区域 {bad} 不应被接受");
        }
    }

    #[test]
    fn language_matching_is_case_insensitive() {
        let mode = PluginMode::from_args(args(&[
            "--starpie-action",
            "translate",
            "--target-lang",
            "ZH-cn",
        ]))
        .expect("plugin mode");
        assert_eq!(mode.target_lang_index(), Some(0));
    }
}
