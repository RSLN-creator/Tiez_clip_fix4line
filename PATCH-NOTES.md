# TieZ 鼠标滚轮修复补丁（Windows）

Fork 自 [jimuzhe/tiez-clipboard](https://github.com/jimuzhe/tiez-clipboard) `v0.3.3` tag。
修复：快捷键/鼠标热键呼出主窗口后，**鼠标滚轮无法滚动列表**（触摸板双指正常），
以及滚轮**穿透到底下应用**的问题（上游 [issue #1](https://github.com/jimuzhe/tiez-clipboard/issues/1)）。

## 根因

主窗口以"不抢焦点"方式呼出（`set_focusable(false)` + `WS_EX_NOACTIVATE` + `SW_SHOWNA`），
保持用户输入焦点在原应用——这是产品特性。但 Windows 把：

- **鼠标滚轮** `WM_MOUSEWHEEL` 投递给**键盘焦点窗口**；
- **触摸板** `WM_POINTERWHEEL` 按光标下 hit-test 投递。

于是无焦点时滚轮消息全部发给了前台的其他应用：表现为 TieZ 滚不动（或穿透），
触摸板正常。托盘点击呼出正常是因为托盘路径显式调用了 `set_focus()`。

## 修复方式（不改变"不抢焦点"设计）

`WH_MOUSE_LL` 低级鼠标钩子在系统路由前拦截滚轮事件：光标命中主窗口树时，
把 `WM_MOUSEWHEEL` 直接投递给 `WindowFromPoint` 命中的 WebView2 子窗口并吞掉；
否则放行。同时过滤 `LLMHF_INJECTED`（不吞 AutoHotkey 等工具注入的滚轮），
并重建 wParam 中的 Ctrl/Shift 修饰键（保留 Ctrl+滚轮等语义）。

## 改动文件

| 文件 | 改动 |
|---|---|
| `src-tauri/src/infrastructure/windows_ext.rs` | 新增 `install_wheel_forward_hook()` 与 `wheel_ll_proc()`（约 +60 行） |
| `src-tauri/src/app/setup.rs` | `setup_main_window` 内安装钩子（1 行调用） |

## 构建

```bash
npm install
npm run tauri:build -- --no-bundle
# 产物：src-tauri/target/release/tiez-app.exe，替换原 exe 即可
```

依赖：Node 18+、Rust (MSVC)、VS Build Tools（C++ 桌面开发负载）。无需 NSIS/WiX。

## 已知边界

- 若 TieZ 主线程长时间卡顿，Windows 可能静默摘除低级钩子（修复失效但不影响稳定性），
  重启 TieZ 恢复。
- 窗口边缘纯透明区域的滚轮现在会被 TieZ 消费（此前穿透给底层应用）。
