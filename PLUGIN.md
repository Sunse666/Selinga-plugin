# Selinga 插件版

这是 [Selinga](https://github.com/Sunse666/Selinga) 的**插件版分支**：同一套截图 / OCR / 翻译 / 覆盖层，
但启动方式从「托盘常驻 + 全局热键」改成「由 [StarPie](https://github.com/Star-Pie/StarPie) 触发」。

独立版继续在 `D:\IO\Rust_obj\Selinga` 维护，两边互不阻塞；本目录只承载插件相关的改动。

## 为什么拆成两个目录

插件版要动的是**触发路径**，不是翻译质量：

- 托盘图标、三个全局热键、开机自启项，在「宿主里运行的插件」语境下要么无意义、要么会和宿主抢键；
- 重复触发时的语义要变（重新框选，而不是把旧窗口拎到前台）；
- 将来若改走 JSON-RPC，协议层也只属于插件版。

翻译链、OCR 预处理、行配对、覆盖层渲染全部与独立版保持同源，改动会双向同步。

## 命令行契约（v0.5，参数 + 退出码）

```text
selinga.exe --starpie-action translate [--target-lang <bcp47>] [--display-mode <bilingual|translated>]
```

| 参数 | 必填 | 说明 |
|---|---|---|
| `--starpie-action` | **是** | 动作名。**只要出现就进入插件模式**；不出现则完全按独立版启动。`translate` = 框选翻译；`settings` = 打开配置文件后立即退出（不建窗口）。 |
| `--target-lang` | 否 | 目标语言，取值来自 `TARGET_LANGS`（`zh-CN`/`en-US`/`ja-JP`/`ko-KR`），大小写不敏感。给未知值时回落到设置里的值，不会静默改成默认语言。 |
| `--display-mode` | 否 | `bilingual`（双语对照）或 `translated`（纯译文）。缺省用设置里的值。 |
| `--region x,y,w,h` | 否 | 宿主直接给定的矩形（物理像素）。给了就**跳过框选层**，第一帧进框态。任何一段不合法（段数不对、非数字、宽或高为 0）都整体作废，退回现场框选 —— 半个矩形比没有矩形更糟。 |

退出码：

| 码 | 含义 |
|---|---|
| `0` | 正常结束（用户关闭框，或本进程是重复触发的第二份、已把信号递给已运行实例） |
| 非 `0` | 启动失败（由 eframe / 平台层返回） |

> **宿主注意**：这是一个常驻到用户关闭的 UI 进程。v0.5 的调用方必须 `Process.Start` 后**不等待**
> 直接返回；若同步等退出，会把动作线程卡到用户关框为止。

## 与独立版的行为差异

| 行为 | 独立版 | 插件版 |
|---|---|---|
| 托盘图标 | 有 | **无** |
| 全局热键（`Ctrl+Alt+H/P`、`F2`） | 注册 | **不注册**（触发权归宿主） |
| 开机自启（`HKCU\...\Run`） | 按设置同步 | **不触碰**（启停由宿主管理） |
| 重复启动 | 激活已有窗口 | **通知已运行实例重新框选**，本进程直接退出 |
| 目标语言 / 显示模式 | 读设置 | 参数覆盖优先，未给才读设置 |
| 设置热加载 | 有 | 有（两版一致） |

### 重新框选是怎么做的

单实例用命名互斥量 `Local\selinga-single-instance` 判定。插件版额外创建一个自动复位的命名事件
`Local\selinga-plugin-reselect`：

1. 宿主再次触发扇区 → 第二份进程启动 → `CreateMutexW` 发现 `ERROR_ALREADY_EXISTS`；
2. 它 `OpenEventW` + `SetEvent` 打信号，然后 `exit 0`；
3. 已运行实例每帧 `WaitForSingleObject(handle, 0)` 轮询，读到信号就 `force_foreground()` + `enter_select_mode()`。

信号没被接住（也就是跑的是独立版，没人建监听器）时，才退回老的「激活旧窗口」行为 —— 独立版行为不变。

## 构建

```bash
cargo build --release -p selinga-app
# 产物：target/release/selinga.exe
```

验证：

```bash
cargo test -p selinga-app      # plugin::tests 覆盖参数解析的四种边界
cargo clippy --workspace --all-targets -- -D warnings
```

## 已知限制

- **宿主拿不到结果**：v0.5 只有参数 + 退出码，StarPie 不知道译文内容，也不知道是否成功。
  需要结果回流时再上 JSON-RPC（`initialize` / `action.execute`）。
- **`--region` 还没有宿主来源**：宿主当前的轮盘会话只暴露圆心、角度与扇区序号
  （`IHostWheelSessionService`），没有矩形。参数与行为已经就绪，等宿主那边能给出几何信息时
  直接接上即可。
- **单实例是全局的**：插件版与独立版共用同一个互斥量，所以两者不能同时运行。
  这是有意的 —— 两套覆盖层同时存在只会互相抢焦点。

## 与宿主的对接形态（目标）

```json
{
  "id": "starpie.plugin.selinga",
  "runtime": "process",
  "command": ["selinga.exe", "--starpie-action", "translate"],
  "capabilities": ["ScreenCapture", "Ui"]
}
```

`runtime: "process"` 目前宿主还不认识，见 StarPie 仓库的 `plugin/docs/plugin-process-runtime.md`。
在它落地之前，这个插件可以用现有的「运行命令」动作手工接入验证。
