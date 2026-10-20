# 剪贴板与文件传输方案

给各平台外壳（macOS app、Windows agent）的实现说明。**核心库已经实现**（ABI 6）：第 0 部分的数据连接、第 1 部分的剪贴板同步、第 3 部分的 v1 复制粘贴文件，外壳照着接入即可。第 2 部分和第 3 部分里的 v2/v3 是**建议，尚未实现**。

参考：Deskflow（剪贴板跟随控制权）、Mouse Without Borders（`example/powertoys-mwb`，MIT 许可，剪贴板/文件/拖放的做法和踩过的坑）。

---

## 0. 传输：控制连接 + 数据连接（已实现，ABI 6）

每对设备之间有**两条 TCP 连接**，用同一个端口：

| 连接 | 承载 | 要求 |
| --- | --- | --- |
| 控制连接（原有） | `Hello`、`Ping/Pong`、`InputBatch`、`InputReset`、`ScreenInfo`、`Placement`、`DataInvite` | 低延迟，只放小消息 |
| 数据连接（新增） | `ClipboardChunk`、`FileRequest`、`FileChunk`、`FileCancel` | 大块数据，绝不挡在输入前面 |

**为什么不能共用一条连接**：程序里给输入排优先级，只决定谁先交给操作系统。写入在系统发送缓冲区收下数据时就返回，而系统会把这个缓冲区自动调到几 MB；大数据会一直灌进去，之后的鼠标事件只能排在这几 MB 后面（TCP 严格按顺序送达）。在 20–50 Mbps 的 Wi-Fi 上就是 0.6–1.6 秒的卡顿，而且贯穿整个文件传输。所以按流量性质分成两条连接，并且**每条连接的系统发送缓冲都限制在 128 KiB**（局域网带宽仍然跑得满）。

**建立过程**（外壳不用关心）：

1. 控制连接握手完成后，**接受连接的一方**生成随机令牌，在控制连接上发 `DataInvite { token }`。只发给与本机共享 `CAPABILITY_CLIPBOARD` 或 `CAPABILITY_FILES` 的对端。
2. **发起连接的一方**向同一地址、同一端口再连一次，第一帧发 `DataHello { device_id, token }`。
3. 接受方按第一帧认出这是数据连接，用令牌把它挂到对应的会话上；令牌不对就关闭。所以局域网里别的设备连不上来。
4. 控制连接断开时数据连接一起关闭；数据连接单独断开时，接受方每 5 秒重新邀请一次。

数据连接上的发送顺序：小消息（`FileRequest`/`FileCancel`）→ 剪贴板 → 文件。文件按网络实际发送的速度读取（队列只有 4 块），不会一次读进内存。

能力位：`CAPABILITY_CLIPBOARD = 1 << 7`，`CAPABILITY_FILES = 1 << 6`。**第 5 位作废**：它曾表示"剪贴板走控制连接"（`512cb34`），新旧两种实现互相不发剪贴板，也就不会因为未知消息互相断开。

---

## 1. 剪贴板同步（已实现）

### 1.1 工作方式

剪贴板**跟着控制权走**（和 Deskflow 一样）：

- 本机开始控制某台设备时（设置输入目标后发出的第一个输入事件），把本机剪贴板发给它。
- 对方把控制权交还时（本机收到对方的 `InputReset`），对方把它那边的剪贴板发回来。所以"在 Windows 上复制，推回 Mac 粘贴"是成立的。
- 每台设备对每个对端记住上次交换内容的摘要，**内容没变就不发**，这同时防止了剪贴板来回回声。
- 收到的剪贴板会成为本机的剪贴板，会继续传给下一台被控制的设备（A → B → C 可以接力）。

传输细节（外壳不用关心，了解即可）：

- 消息 `ClipboardChunk`（类型 8），每块最多 64 KiB，走**数据连接**（见第 0 部分）。新的剪贴板会替换还在排队的旧剪贴板。数据连接还没建好时，同步会等它建好后自动补发。
- 只发给宣告了 `CAPABILITY_CLIPBOARD`（第 7 位）的对端；外壳在配置里打开剪贴板时核心库才会宣告。
- 全部格式合计最多 **8 MiB**，最多 16 种格式。超过时核心库发 `error` 事件（`clipboard not shared: …`），不发送。

### 1.2 外壳需要做的

1. **打开开关**：配置 JSON 里加 `"clipboard": true`（默认关闭）。
2. **每次系统剪贴板变化后报告给核心库**：

   ```c
   lc_core_set_clipboard(core,
     "{\"formats\":[{\"mime\":\"text/plain;charset=utf-8\",\"data\":\"<base64>\"}, …]}");
   ```

   - 格式按"最具体的在前"排列（例如 HTML 在纯文本前）。
   - 剪贴板里没有可共享的内容时报告 `"formats":[]`，例如密码管理器标记为"隐藏"的内容。
3. **处理收到的剪贴板**：事件

   ```json
   {"event":"clipboard","peer_id":"…","formats":[{"mime":"…","data":"<base64>"}, …]}
   ```

   把它写入系统剪贴板。**这次写入不能再当作变化报告回核心库**（核心库也会按摘要去重，但外壳自己跳过可以省掉一次编码）。
4. 可选：`{"event":"clipboard_sent","peer_id":"…","size":字节数}` 表示本机剪贴板刚发给了对方，可以用来在界面上提示"已同步剪贴板"。
5. 不认识的 MIME 类型直接忽略。

### 1.3 格式对照

线上只用这三种 MIME 类型（核心库里的常量 `MIME_TEXT` / `MIME_HTML` / `MIME_PNG`）：

| 线上 MIME | macOS（NSPasteboard） | Windows |
| --- | --- | --- |
| `text/plain;charset=utf-8` | `NSPasteboard.PasteboardType.string`（`public.utf8-plain-text`） | `CF_UNICODETEXT`（UTF-16，与 UTF-8 互转） |
| `text/html;charset=utf-8` | `.html`（`public.html`） | `"HTML Format"`（CF_HTML：写入时要生成带字节偏移的头部，读取时按 `StartFragment`/`EndFragment` 取出片段） |
| `image/png` | `.png`；只有 `.tiff` 时用 `NSBitmapImageRep` 转成 PNG 再报告 | 注册格式 `"PNG"`；没有时把 `CF_DIBV5`/`CF_DIB` 转成 PNG。写入时同时放 `"PNG"` 和 `CF_DIB`，兼容只认位图的程序 |

### 1.4 macOS 实现要点

- **检测变化**：macOS 没有剪贴板变化通知，轮询 `NSPasteboard.general.changeCount`，建议间隔约 250 ms。另外**在开始控制对方的那一刻（越过边缘时）再检查一次**，避免"刚复制就推过去"时还没轮询到的情况。
- **隐私内容**：剪贴板包含 `org.nspasteboard.ConcealedType` 或 `org.nspasteboard.TransientType`（密码管理器使用）时报告 `"formats":[]`，不共享。
- **写入**：`clearContents()` 后按收到的格式逐个 `setString` / `setData`，写完记下新的 `changeCount`，下次轮询看到这个值就跳过，不报告回去。
- **只读需要的类型**：只取上表三种，不要把剪贴板里所有类型都读出来（大文件拷贝时剪贴板里可能有很大的表示）。
- **线程**：核心库回调在它自己的线程上，按现有 `RustCore` 的做法转到主线程再操作 `NSPasteboard`。

### 1.5 Windows 实现要点（Windows agent 侧，供对照）

- 用 `AddClipboardFormatListener` 收 `WM_CLIPBOARDUPDATE`，在一个隐藏窗口所在的线程上处理。
- 剪贴板包含 `ExcludeClipboardContentFromMonitorProcessing`，或 `CanIncludeInClipboardHistory` 为 0 时，视为隐私内容，报告空。
- 自己写入后会收到一次 `WM_CLIPBOARDUPDATE`：用 `GetClipboardSequenceNumber()` 记下写入后的序号，跳过这一次。

### 1.6 已知限制

- **目前流量没有加密**：剪贴板里可能有密码、令牌等敏感内容，会以明文在局域网里传输。在加密和配对完成之前，界面上的剪贴板开关建议**默认关闭**，打开时提示只在可信网络中使用。
- 8 MiB 上限对截图足够，但大图片和文件不在这个机制里（见第 3 部分）。

---

## 2. 剪贴板后续改进（建议，未实现）

按价值排序：

1. **加密完成后再默认开启**（见 1.6）。
2. **增加 RTF**：`text/rtf` ↔ macOS `.rtf` / Windows `"Rich Text Format"`。富文本在 Office、备忘录、文本编辑之间很常用。核心库不需要改，两端外壳各加一行映射即可（接收方忽略不认识的类型，所以可以先由一端开始发）。
3. **大内容改为"先通知，粘贴时再取"**：现在剪贴板在交接控制权时整个推过去，上限 8 MiB。以后可以对大内容只发一个"有新内容"的通知，在对方真正粘贴时再拉取：
   - macOS：`NSPasteboardItem` + `NSPasteboardItemDataProvider`（承诺数据）。
   - Windows：延迟渲染（`SetClipboardData(format, NULL)` + `WM_RENDERFORMAT`）。

   无界鼠标是"通知 + 切换过去时再拉取（30 秒内）"，比它更好的是粘贴时再拉取。这一项可以和文件传输一起做，因为两者用的是同一套拉取机制。

**不要学无界鼠标的地方**：它防止来回回声靠的是 1 秒的时间间隔，按内容比对去重的那段代码因为一个类型判断错误从来没生效过。我们已经用"每个对端的内容摘要"去重，这一点比它稳。

---

## 3. 文件传输（v1 已实现，v2/v3 为建议）

**v1 的实现与下面 3.2–3.6 一致**，区别只有：传输走数据连接（第 0 部分），不走"批量通道"；磁盘剩余空间检查还没做（写入失败时会报 `files_failed`）。外壳接入要点：

- 配置：`"files": true`（需要同时 `"clipboard": true`）、`"download_directory"`（必填）、`"max_auto_receive_bytes"`（默认 512 MiB）。
- 发送：系统剪贴板里有文件时，在 `lc_core_set_clipboard` 里加一项 `text/uri-list`（`file://` URL，每行一个），其他格式照常（比如文件名文本）。核心库会把本地路径换成清单，本地路径不会离开本机。
- 接收：`files_received` 之后会紧跟一个 `clipboard` 事件，格式列表第一项是 `text/uri-list`（接收方的本地路径）。外壳要把它写成系统剪贴板里的**文件**（macOS：`writeObjects([NSURL])`；Windows：`CF_HDROP`），粘贴就能得到文件。
- 超过自动接收上限时收到 `files_offered`：问用户，同意调用 `lc_core_accept_files(core, transfer)`，拒绝调用 `lc_core_cancel_files`。

### 3.1 目标与分阶段

| 阶段 | 用户看到的 | 说明 |
| --- | --- | --- |
| **v1：复制粘贴文件** | 在一台设备上复制文件（Finder ⌘C / 资源管理器 Ctrl+C），控制权移到另一台后粘贴（⌘V / Ctrl+V），文件出现在当前目录或下载目录 | 先做这个，价值最大、实现最简单 |
| v2：跨设备拖放 | 把文件从一台设备的屏幕拖过边缘，在另一台设备上松开 | 需要检测"拖动中越过边缘"，两端实现差异大 |
| v3：粘贴/松开时才传输 | 文件内容在对方真正粘贴或松开时才传，大文件不提前占带宽 | 用系统的"文件承诺"接口，最复杂 |

### 3.2 v1 流程

```
发送方（复制了文件）                         接收方
1. 外壳在剪贴板里看到文件 URL
   → lc_core_set_clipboard 里报告
     text/uri-list（本地 file:// 路径）
2. 核心库读取文件信息，生成"文件清单"
   （只有相对路径、大小，不含本地完整路径），
   替换掉 text/uri-list
3. 控制权移到接收方时，清单随剪贴板发过去 ──▶ 4. 核心库收到清单：总大小不超过自动接收
                                               上限时开始拉取；超过时发事件请外壳确认
                                 ◀── FileRequest（逐个文件，可续传）
5. 按请求读取文件，FileChunk 分块发送 ──────▶ 6. 写入 .partial 临时文件，长度核对无误后
   （走批量通道，排在输入后面）                  原子改名到目标目录
                                            7. 全部完成：发 clipboard 事件，格式为
                                               text/uri-list（接收方本地路径），外壳写入
                                               系统剪贴板，用户粘贴即得到这些文件
```

为什么在交接时就下载、而不是粘贴时才下载：v1 先保持简单，和无界鼠标一样。控制权移过去，通常意味着用户接下来就要在那台设备上操作；配合自动接收上限，大文件不会悄悄占满磁盘。真正按需传输留到 v3。

### 3.3 职责划分

**核心库负责**（两端共用一份逻辑）：
- 生成文件清单；
- 读取和写入文件内容；
- 临时文件、文件名清洗、同名处理；
- 续传和取消；
- 进度事件。

macOS app 没有开沙盒（需要辅助功能权限），核心库可以直接读写文件。

**外壳负责**：
- 系统剪贴板和文件 URL 的读写；
- 提供下载目录；
- 大文件确认对话框；
- 进度显示；
- 接收完成后在 Finder 或资源管理器里显示文件（可选）。

### 3.4 协议草案

- 新能力位 `CAPABILITY_FILES = 1 << 6`，只对宣告了这一位的对端发送文件相关消息（和剪贴板一样，旧版本收到未知消息类型会断开连接）。
- 文件清单作为剪贴板里的一种格式 `application/x-linkcursor-files`，内容是 JSON：

  ```json
  {"transfer": 17, "total_size": 4823311,
   "entries": [{"path": "报告/第一章.pdf", "size": 4800000},
               {"path": "报告/图片", "dir": true},
               {"path": "备注.txt", "size": 23311}]}
  ```

  `path` 一律是相对路径，用 `/` 分隔，接收方必须重新清洗（见 3.6）。
- 新消息（编号接着现有的 8 往后排）：

  | 消息 | 方向 | 内容 |
  | --- | --- | --- |
  | `FileRequest` | 接收方 → 发送方 | `transfer`、`entry`（清单里的序号）、`offset`（从这里继续，用于续传） |
  | `FileChunk` | 发送方 → 接收方 | `transfer`、`entry`、`offset`、数据（每块最多 64 KiB，和剪贴板一致） |
  | `FileCancel` | 双向 | `transfer`、原因（用户取消、文件已变化、磁盘空间不足等） |

  发送方收到请求时如果文件的大小或修改时间和生成清单时不一致，回复 `FileCancel("文件已变化")`，不发送半新半旧的内容。
- 走数据连接（第 0 部分）。一次只进行一个传输；新的剪贴板文件清单会取消还没完成的旧传输。

### 3.5 C 接口草案

配置：

| 键 | 默认 | 说明 |
| --- | --- | --- |
| `files` | `false` | 外壳实现了文件传输 |
| `download_directory` | 无 | 接收文件的目录，例如 `~/Downloads/LinkCursor`、`%USERPROFILE%\Downloads\LinkCursor` |
| `max_auto_receive_bytes` | `536870912`（512 MiB） | 不超过这个大小自动接收，超过时先问用户 |

发送：沿用 `lc_core_set_clipboard`，格式里放 `text/uri-list`（`file://` URL，每行一个，`\r\n` 分隔）。

接收方的新事件：

| 事件 | 字段 | 外壳该做的 |
| --- | --- | --- |
| `files_offered` | `peer_id`、`transfer`、`count`、`total_size` | 超过自动接收上限时才发：弹窗确认，同意后调用 `lc_core_accept_files(core, transfer)` |
| `files_progress` | `transfer`、`received`、`total` | 显示进度 |
| `files_received` | `transfer`、`paths` | 完成；随后还会收到一个 `clipboard` 事件（`text/uri-list`），照常写入剪贴板 |
| `files_failed` | `transfer`、`reason` | 提示失败原因 |

新调用：`lc_core_accept_files(core, transfer)`、`lc_core_cancel_files(core, transfer)`。

### 3.6 安全规则（接收方必须做到）

参考无界鼠标的 `ReceivedDestinationFile.cs`，并补上它缺的部分：

- **文件名清洗**：
  - 去掉开头的 `/`；拒绝 `..` 和带盘符的路径；
  - 去掉控制字符；
  - Windows 上替换 `<>:"|?*`，避开 `CON`、`NUL`、`COM1` 等保留名，去掉结尾的点和空格；
  - 最终路径必须仍在下载目录之内。
- **先写临时文件**：写到同目录下的 `.<随机>.partial`，长度和清单完全一致后才原子改名；失败、取消或断线时删除临时文件。
- **不覆盖**：同名时加序号，例如 `报告 (1).pdf`。无界鼠标会静默覆盖，这一点不要学。
- **接收方自己限额**：不能只相信发送方报的大小。清单总大小、单个文件大小、实际写入字节数都要检查，并且检查剩余磁盘空间。
- **只接受当前交接方的文件**：清单只从正在与本机交接控制权的对端接收。
- **不执行、不改权限**：收到的文件只按普通文件保存。
- **加密前默认关闭**：和剪贴板一样，文件现在是明文传输。

### 3.7 v2 拖放要点（以后做）

- **发送方检测"拖动中越过边缘"**：
  - macOS：越过边缘时如果左键按着，读 `NSPasteboard(name: .drag)` 里的文件 URL。
  - Windows：无法直接读取别的程序正在拖动的数据。无界鼠标的做法是在光标下放一个隐藏窗口，接住资源管理器的 `DragEnter`，从数据对象里取出文件。这个思路可以借鉴。
- **接收方松开鼠标时**：
  - v2：先下载到下载目录，再在 Finder 或资源管理器里显示。无界鼠标就是这样做的。
  - v3：真正落到鼠标下的程序里，需要发起本地拖动并使用文件承诺：macOS 用 `NSFilePromiseProvider`，Windows 用 `CFSTR_FILEDESCRIPTOR` + `CFSTR_FILECONTENTS`。

---

## 4. 从 Mouse Without Borders 借鉴与避开的点（摘要）

| 点 | 无界鼠标 | 我们 |
| --- | --- | --- |
| 小内容即时推送，大内容先通知再拉 | ✓ | 剪贴板目前在交接时整体推送（≤8 MiB）；大内容的"先通知再拉"放进第 2 部分第 3 条和文件传输 |
| 大数据另走通道 | 另开一条 TCP 连接，另一个端口 15101 | 另开数据连接，但用同一个端口（第一帧区分，令牌绑定会话），不用多开防火墙端口；发送缓冲压到 128 KiB |
| 防回声 | 1 秒时间间隔（内容去重有 bug） | 每个对端的内容摘要 ✓ |
| 文件落地 | `.partial` + 原子改名 + 长度核对 ✓ | 照做 |
| 文件名 | 只取文件名 | 支持文件夹，所以要完整清洗相对路径 |
| 同名 | 静默覆盖 ✗ | 加序号 |
| 接收方限额 | 没有 ✗ | 必须有 |
| 一次只能一个文件，不支持文件夹 | ✗ | 清单支持多个文件和文件夹 |
| 加密 | AES-CBC，没有完整性校验 ✗ | 等配对加密完成（另行设计），之前默认关闭 |

---

## 5. 待决定

1. 下载目录和自动接收上限的默认值（建议 `下载/LinkCursor`、512 MiB）。
2. v1 是否需要"接收完成后自动在 Finder/资源管理器中显示"，还是只放进剪贴板。
3. RTF 是否现在就加。
4. 加密完成之前，剪贴板和文件是否允许在设置里打开（建议允许，但默认关闭并提示风险）。
