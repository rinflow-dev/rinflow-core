# 外壳接入 ABI 8 / ABI 9（macOS 对照清单）

核心库已经实现了下面所有协议层的工作（ABI 9，`lc_core_abi_version() == 9`）。这份清单列出 **每个外壳（macOS app）还要做的事**、用哪些 macOS API、以及 Windows 端可以参照的实现。新消息只发给声明了对应能力位的对端，所以 macOS 没跟上之前，两边照样能连。

接口细节以 [include/linkcursor_core.h](../include/linkcursor_core.h) 为准，事件字段见根目录 [README](../../README.md) 的 Events 表。

## 0. 先换库

- 用新编的核心库（含 ABI 9 头文件），`RustCore.swift` 里的 `lc_core_abi_version() == LC_CORE_ABI_VERSION` 检查自然变成 9。

## 1. ABI 8：保持连接、睡眠与网络变化

| 要做的 | macOS 做法 | Windows 参照 |
| --- | --- | --- |
| 删掉自己的重连：`AppModel.reconnectTimer` 和 `connectPlacedPeers()` | 核心的 `auto_connect`（默认开）会自己连已配对设备、断线后按 1–30 s 退避重连 | `agent/src/main.rs` 没有任何重连代码 |
| 系统要睡眠时调用 `lc_core_system_will_sleep` | `NSWorkspace.shared.notificationCenter` 的 `willSleepNotification` | `agent/src/power.rs`（`PBT_APMSUSPEND`） |
| 醒来、或网络变化时调用 `lc_core_network_changed` | `didWakeNotification`；`NWPathMonitor` 的 `pathUpdateHandler`（防抖约 0.8 s） | `power.rs`（`NotifyUnicastIpAddressChange`） |
| 新的 `disconnected` 原因 | `"disconnected on the other device"`、`"going to sleep"`、`"local request"` 不提示；`"refused by the other device"` 提示"对方拒绝了连接"；`"heartbeat timeout"` 提示"对方没有响应，回来后会自动重连" | `agent/src/state.rs` 的 `disconnect_message` |

## 2. 通过地址添加设备

- 配置键 `manual_addresses`（保存后启动时传入），运行时 `lc_core_set_manual_addresses(core, "[\"192.168.1.20\"]")` 整体替换。
- 事件 `manual_addresses`：`addresses` 每项 `{address, status}`，`status` 为 `searching` / `found`（带 `peer_id`、`name`）/ `no_response` / `unresolved`（带 `error`）。
- UI：放在"添加设备"里，一个输入框 + "添加"，下面列出每个地址和状态，可删除。客户端先做基本校验（只允许字母数字 `.-_:[]%`，不重复，最多 32 个）。
- 只需在两台设备中的一台上添加；找到之后就和局域网发现的设备一样连接、配对、自动重连。
- 参照：`app/MainWindow.Transfers.cpp`（地址部分）、`agent/src/settings.rs`（校验）。

## 3. Mac 快捷键映射（⌘ ↔ Ctrl）

- 设置项"沿用 Mac 的快捷键"，启动时传 `shortcut_remap`，运行时 `lc_core_set_shortcut_remap(core, on)`。
- 它影响的是**本机收到**的按键：Mac 被 Windows 控制时，打开后 Windows 的 Ctrl 和 Win 互换，Ctrl+C 在 Mac 上就是 ⌘C。
- Windows 端默认打开；macOS 端建议也默认打开。

## 4. 锁屏与唤醒（presence）

| 要做的 | macOS 做法 |
| --- | --- |
| 有人在用本机时，每隔几秒调用 `lc_core_report_user_activity` | 每 5 s 检查 `CGEventSourceSecondsSinceLastEventType(.combinedSessionState, kCGAnyInputEventType) < 10` |
| 收到 `peer_active` 且用户开了"保持唤醒"：一分钟内保持唤醒 | `IOPMAssertionCreateWithName(kIOPMAssertionTypePreventUserIdleDisplaySleep)`，并每 30 s 调用一次 `IOPMAssertionDeclareUserActivity` 重置空闲计时（屏保和闲置锁屏都看它）；最后一次 `peer_active` 一分钟后释放 assertion |
| 用户锁屏时，若开了"一起锁定"：`lc_core_lock_peers` | `DistributedNotificationCenter` 的 `com.apple.screenIsLocked` |
| 收到 `lock_requested` 且开了"一起锁定"：锁屏 | `SACLockScreenImmediate()`（login.framework）；5 s 内由对方请求引起的锁屏不再转发 |

注意：用 `IOPMAssertionDeclareUserActivity` 保持唤醒，不会被算作输入事件，所以不会反过来触发 `report_user_activity`。Windows 端用零位移的鼠标移动来保持唤醒，因此要专门把它过滤掉（`agent/src/presence.rs` 的 `someone_active`）。两个默认值：保持唤醒开，一起锁定关。

## 5. 游戏锁定指针（被控端）

- Mac 被控时，如果前台游戏隐藏并锁定了光标（`CGAssociateMouseAndMouseCursorPosition(false)` 加 `CGDisplayHideCursor`），检测到后调用 `lc_core_set_pointer_locked(core, true)`；结束时传 `false`。检测方法：`CGCursorIsVisible() == false`，并且注入相对移动后光标位置不变。每 250 ms 检查一次，收到移动事件时也检查。
- 锁定期间按相对移动注入：发送 `kCGEventMouseMoved` 并设置 `kCGMouseEventDeltaX/Y`，不要去设置光标位置。
- 参照：`agent/src/inject.rs`（`pointer_captured`、`check_pointer_lock`）。

## 6. 游戏锁定指针（控制端）

- 收到 `peer_pointer_lock {peer_id, locked:true}`，而且正在控制这台设备时：所有移动原样发过去，边缘不再切回。释放快捷键照常有效。
- 收到 `locked:false` 时，补发一次绝对位置，把对方光标拉回本机跟踪的位置。
- 开始控制一台设备时，清掉之前记下的锁定状态，由对方重新告知。
- 参照：`agent/src/capture.rs`（`edge::pointer_held`、`pointer_released`）。

## 7. 文件传输进度

- 接收：`files_receiving`（名称、项数、文件数、总大小）→ 若干 `files_progress` → `files_received` 或 `files_failed`。
- 发送：`files_sending`（含 `sent`、`total`，约每 250 ms 一次）→ `files_send_ended`（`reason` 为 `done` 表示完成）。
- 取消：两个方向都用 `lc_core_cancel_files(core, transfer)`。
- UI：主窗口里每个传输一张卡片，显示进度条、"已传 / 总量"和取消按钮。收到文件后发系统通知（`UNUserNotificationCenter`），点通知在 Finder 里打开下载目录。
- `files_failed` 和 `files_send_ended` 的原因为 `cancelled`、`replaced by a newer copy`、`files are no longer on the clipboard` 时不算错误，不要提示。
- 参照：`agent/src/state.rs`（`transfers`）、`app/MainWindow.Transfers.cpp`。

## 8. 按设备调指针和滚动（控制端）

- 每台被控设备单独保存三项：指针速度（0.25–4×）、滚动速度（0.25–5×）、反转滚动方向。
- **只在控制端生效**：先把移动量乘上速度，再更新本机对对方光标的跟踪，然后发出去，这样两边对光标位置的判断不会有偏差。滚动量直接乘速度，需要时再取反。
- 参照：`agent/src/capture.rs`（`input.pointer_speed` 等）、`agent/src/settings.rs`（`PeerInput`）。

## 9. 三台以上设备串联

- 核心现在允许把设备放在另一台已放置设备的旁边（`lc_core_set_placement` 的吸附规则已经改了），也就是说 macOS 的排列界面可能收到不贴着本机的 `offset`。绘制时照常按 offset 画即可。
- 控制端需要在光标离开当前设备、进入相邻已放置设备时直接切换目标：
  1. `lc_core_set_input_target(next)`，旧设备会自动收到 reset。
  2. 发一个绝对位置事件，落点是本机坐标减去对方的 offset。
  3. 修饰键继续保持按下，其他已按下的键作废。
- 逻辑参照核心里的 `layout::RemotePointer::step` 和 `Motion::Neighbor`（Rust，C ABI 没有暴露）。Swift 端需要照着移植；如果更想共用代码，可以加一个 C 接口。

## 10. 快捷键切换设备

- 默认打开，组合键为 Control+Option+Shift+方向键（可改为 Control+Option+方向键）。本机拥有光标时用 Carbon 的 `RegisterEventHotKey` 注册；正在控制其他设备时，在已有的 event tap 里识别。
- 目标设备：从当前设备的中心出发，选那个方向上最近的设备（主方向距离 + 2 × 横向偏差最小），落到它离当前设备最近的那块屏幕的中心。
- 注册失败（被其他程序占用）时在设置里提示。
- 参照：`agent/src/arrangement.rs`（`World::toward`、`World::landing`，有单元测试）、`agent/src/edge.rs`。

## 11. 语言

- 设置项 `language`：`system` / `zh-CN` / `en-US`。macOS 用 `Localizable.strings`（en、zh-Hans），覆盖语言时设置 `AppleLanguages`，重启 app 后生效。
