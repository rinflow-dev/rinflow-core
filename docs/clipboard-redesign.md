# 剪贴板重构建议：轻量同步、按需取内容、独立传输任务

日期：2026-10-27。状态：设计建议，未修改运行时代码。依据是本工作区当前源码；`example` 中的行为不代表上游最新版本。

## 1. 建议结论

保留现有加密数据连接和输入优先级机制，重构剪贴板的数据模型与生命周期：

- 小文本、HTML 和小图片继续即时同步。
- 文件默认只同步一个可读取的引用；目标应用请求文件内容，或用户主动选择接收时，才开始传输。
- 剪贴板表示“现在可以粘贴什么”；传输任务表示“一次已经开始的复制”。两者分别管理。
- Windows 优先验证原生虚拟文件粘贴；macOS 先验证 Finder 兼容性，不能把拖放文件承诺当成普通粘贴已经可用的结论。
- 大文件不因鼠标越过边缘自动下载，不因大小弹出抢焦点的接收对话框。

例如复制一个 20 GB 视频后切到另一台设备：先同步文件名、大小、类型等元数据，不传 20 GB 内容；在支持的目标中粘贴后，才出现复制进度。期间再复制一段文字，视频任务继续完成，文字仍是当前剪贴板。

## 2. 当前实现为何产生不自然的体验

当前链路是：

```text
复制本地文件
  → 核心同步递归生成完整文件清单
  → 控制权交接时发送清单
  → 接收方自动接收或弹窗确认
  → 下载到 Downloads/LinkCursor
  → 全部完成后把接收路径写入系统剪贴板
  → 用户在目标目录粘贴，系统再复制一次
```

这不是“文件整个塞进剪贴板内存”的问题。发送端已经按 64 KiB 分块读取，也已经将输入与数据分成两条 QUIC 连接；这些值得保留。主要问题在触发时机、状态归属和接收侧背压。

| 现状 | 用户或性能上的后果 | 源码依据 |
| --- | --- | --- |
| 收到文件清单立即进入接收决策 | 切屏等同于下载；尚未粘贴也占带宽、磁盘 | `runtime/data.rs::clipboard_chunk/start_download` |
| Windows 对不超过 512 MiB 的文件自动接受，超过则 MessageBox；macOS 使用 NSAlert | 大小阈值改变操作方式；切屏可能抢焦点 | Windows `clipboard.rs::files_offered`；macOS `AppModel.swift::askToReceiveFiles` |
| 完成后无条件发出写剪贴板事件 | 这条完成路径没有核对剪贴板版本，存在覆盖较新本地复制的风险 | `runtime/data.rs::finish_download` |
| 接收状态是全局一个 `Option<Download>`；新文件 offer 取消旧下载 | 传输任务被下一次复制操作替换 | `runtime.rs::Runtime`；`runtime/data.rs::start_download` |
| 先下载，再以 CF_HDROP / 本地文件 URL 发布 | 粘贴前没有可用的文件；还要承担缓存到最终目录的复制成本 | 两端 ClipboardBridge；`finish_download` |
| `Offer::build` 同步递归目录并排序 | 大目录、慢盘、网络目录可能阻塞核心事件循环 | `runtime/data.rs::offer_files`；`files.rs::Offer::build/add` |
| 接收网络事件和写盘任务使用无界队列 | 发送队列虽有限，网络快于磁盘时接收侧仍可能不断堆积内存 | `transport.rs::read_loop`；`runtime/data.rs::DiskWriter::spawn` |
| 普通内容合计限 8 MiB，平台会舍弃部分图片/HTML | 较大截图可能丢失表示或完全不能同步 | `clipboard.rs::MAX_CLIPBOARD_SIZE`；两端读取逻辑 |
| 协议有 offset，但正常请求始终从 0 开始，取消会清理 partial | 不能把现有字段等同于已经实现断线续传 | `runtime/data.rs::advance_download`；`files.rs::FileWriter` |

另外，当前发送端的 `files_send_ended(done)` 在最后一块进入发送队列后就可能发出，不代表接收端已经写盘成功。进度和完成语义应一起修正。

## 3. example 项目的实际做法

### Deskflow：借鉴所有权和内容去重

服务端记录 clipboard owner、sequence、dirty 标记，控制权切换时向活跃设备更新剪贴板，避免反复发送相同内容。

不过本快照的 `IClipboard::Format` 是 Text、HTML、Bitmap，`Clipboard` 会把表示汇总到内存再序列化。它适合参考共享剪贴板的所有权管理，不能直接作为大文件按需粘贴的实现模板。

依据：[Server.cpp](https://github.com/deskflow/deskflow/blob/master/src/lib/server/Server.cpp)、[IClipboard.h](https://github.com/deskflow/deskflow/blob/master/src/lib/deskflow/IClipboard.h)、[Clipboard.h](https://github.com/deskflow/deskflow/blob/master/src/lib/deskflow/Clipboard.h)。

### Mouse Without Borders：借鉴轻量通知与独立取数

小内容可即时发送；超过 1 MiB 的普通内容先发 clipboard beat，再通过独立连接获取。文件同样先通知，接收流程按 post action 决定落盘和后续操作。

但本快照也会在切换设备时调用 `GetRemoteClipboard`，不能描述成“全部等粘贴才传”。剪贴板单文件路径有 100 MiB 限制，接收后还会设置本地 FileDropList。照搬它仍会保留我们目前的一部分体验问题。

依据：[Clipboard.cs](https://github.com/microsoft/PowerToys/blob/main/src/modules/MouseWithoutBorders/App/Core/Clipboard.cs)、[Event.cs](https://github.com/microsoft/PowerToys/blob/main/src/modules/MouseWithoutBorders/App/Core/Event.cs)、[Receiver.cs](https://github.com/microsoft/PowerToys/blob/main/src/modules/MouseWithoutBorders/App/Core/Receiver.cs)。

### Chrome Remote Desktop：借鉴传输状态机与接收确认

本快照 Windows/macOS host 的系统剪贴板实现以 UTF-8 文本为主；文件通过独立的 `filetransfer-` 数据通道和文件操作接口处理，并非透明跨平台文件剪贴板。

值得借鉴的是 Metadata → Data → End → Success/Error 的生命周期、取消处理、异步文件操作，以及限制发送侧排队块数。其缓冲写入器本身仍有队列，不能据此认定整条接收链路天然具有严格内存上限。

依据：[clipboard_win.cc](https://source.chromium.org/chromium/chromium/src/+/main:remoting/host/clipboard_win.cc)、[clipboard_mac.mm](https://source.chromium.org/chromium/chromium/src/+/main:remoting/host/clipboard_mac.mm)、[file_transfer_message_handler.cc](https://source.chromium.org/chromium/chromium/src/+/main:remoting/host/file_transfer/file_transfer_message_handler.cc)、[file_transfer.proto](https://source.chromium.org/chromium/chromium/src/+/main:remoting/proto/file_transfer.proto)。

### Lan Mouse：本快照没有跨设备剪贴板可借鉴

README 的 Clipboard support 仍未勾选；GTK 中出现的 clipboard 调用用于复制主机名和指纹，不是跨设备剪贴板协议。

依据：[README.md](https://github.com/feschber/lan-mouse/blob/main/README.md)。

## 4. 用户交互与同步范围

| 内容 | 复制或切换设备时 | 请求内容时 |
| --- | --- | --- |
| 小文本、HTML | 同步完整内容，让粘贴保持即时 | 使用本地已有内容 |
| 小图片 | 可在预算内预取，异步转换格式 | 使用已取内容或请求目标格式 |
| 大图片、大文本 | 同步格式与大小，按平台能力延迟取数 | 有超时、失败反馈；不静默截断 |
| 文件、文件夹 | 默认只同步元数据和引用，不读取文件正文 | 创建独立任务，按应用请求或明确接收操作拉取 |

起始参数可设为：普通表示合计不超过 256 KiB 时直接携带；图片预取总预算 2 MiB；文件正文默认不预取。这些是待实测的策略参数，不是新的硬性文件大小限制。

同步范围沿用“当前控制关系”：在交接时发布当前快照；控制关系持续期间发生复制，也及时更新这对设备。其他已连接设备的后台剪贴板变化不抢占当前活动关系，不全网广播正文。实际发布仍需满足双方共享设置与会话授权。

用户主动粘贴/接收后，用一张不抢焦点的进度卡展示设备、文件、速度和取消入口。大小本身不触发额外确认；磁盘不足、目标不支持等情况才给出具体操作提示。应用原生复制进度可用时优先使用，避免两个界面都宣称同一个“完成”。

文件语义先限定为复制。跨机剪切不自动删除来源文件。

## 5. 架构与状态边界

```mermaid
flowchart LR
    A[系统剪贴板适配器] --> B[ClipboardService：快照、格式、所有权]
    B --> C[Offer：小内容或内容引用]
    C --> D[对端系统剪贴板 Provider]
    D -->|请求内容| E[TransferManager：独立任务]
    E --> F[有界数据管道]
    F --> G[目标应用流 / 临时文件]
    G --> H[校验与接收结果]
```

建议拆成三层：

1. **ClipboardService**：观察本地变化、发布当前快照、按格式提供数据、控制同步范围、防回声。
2. **TransferManager**：传输任务、读写、取消、重试、续传、缓存和实际进度。多个任务可以存在，活跃并发先保守限制；不以“每次复制”代替任务取消。
3. **Platform provider**：Windows OLE 数据对象，macOS pasteboard / file promise，以及不支持按需读取时的接收入口。

核心模型建议：

```text
ClipboardSnapshot {
  id: origin_device + process_epoch + monotonic_revision,
  representations: [{ format, size: optional, inline_data | content_ref }],
  files: optional top_level_summary
}

TransferJob {
  job_id, snapshot_id, peer_id, source_lease,
  state, entry_progress, received_bytes, written_bytes, verified_bytes
}
```

`snapshot_id` 管理版本和回声，摘要只用于内容去重/完整性；不要把相同内容摘要当成同一次复制。接收方另维护本地 generation 和系统 changeCount/sequence，发布 provider 和任何异步补写都必须确认 generation 仍匹配。操作系统剪贴板不是跨进程事务，应在所属线程串行操作，并在能取得的写入锁内复核。

关键状态规则：

- 新复制替换当前快照，撤销旧快照尚未开始的取数权限。
- 已开始任务持有受限的 source lease；下一次复制不终止它，也不让它完成后重写当前剪贴板。
- 显式取消、禁用共享、解除信任或来源失效会停止相关任务。
- “收到 offer”“已安装 provider”“内容可用”“接收成功”分别报告，不能混为一个已同步状态。
- 程序读取文件内容不一定来自 Ctrl+V：剪贴板管理器或目标预览也可能请求。查询格式、枚举文件、读取大小不得触发正文传输；正文读取才触发任务，并接受这仍不是严格的“用户按下粘贴”信号。
- A → B → C 保留 origin 和 snapshot 身份。只有 C 获得来源授权且可连接时才直取；否则明确提示，或由 B 在确有本地完整副本后发布新来源。第一版不引入隐式大文件中继。

## 6. 两个平台如何落地

### Windows：OLE 虚拟文件作为首选路线

由专门 STA 线程维护 OLE 剪贴板，通过 `OleSetClipboard` 发布 `IDataObject`：

- `CFSTR_FILEDESCRIPTORW` 提供名称、大小、属性等描述。
- `CFSTR_FILECONTENTS` 通过 `TYMED_ISTREAM` 提供每个文件的内容，背后连接核心的有界读取管道。
- 实现并验证 `IDataObjectAsyncCapability`，供支持的目标协商异步读取。
- 实现取消、超时、重复 GetData、IStream Seek、并行读取与 COM 生命周期；网络操作不能占用输入捕获线程或核心事件循环。
- 保留 CF_HDROP 接收路径以支持现有源文件；接收端只有在完整本地缓存确实存在时才发布 CF_HDROP。

原型必须分别验证 Explorer、桌面、文件夹与空文件夹、大于 4 GB 文件、目标取消及一个常用第三方程序。系统接口存在不等于所有目标都支持相同的读取行为。

微软文档明确区分 CF_HDROP 的现有文件路径与 FILEDESCRIPTOR/FILECONTENTS 的文件表示，后者可用 IStream 避免一次装入整个文件：[Shell Clipboard Formats](https://learn.microsoft.com/en-us/windows/win32/shell/clipboard)。异步提取需由目标协商：[IDataObjectAsyncCapability](https://learn.microsoft.com/en-us/windows/win32/api/shldisp/nn-shldisp-idataobjectasynccapability)。

### macOS：先验证 Finder 粘贴，再确定对外能力

普通表示可以研究 `NSPasteboardItemDataProvider` 的延迟供数，但它的回调不能被视为适合等待数分钟网络下载的通用异步文件管道。

`NSFilePromiseProvider` 适用于文件承诺，官方说明以拖放为主要场景。应以最小原型分别验证 general pasteboard + Finder ⌘V、拖到 Finder、多文件、文件夹、取消和超时。不能因为拖放成功，就把 Finder 普通粘贴也标记为支持。

如目标不支持可靠的按需文件粘贴，第一版提供明确的“保存到…”接收入口：用户选择最终目录后直接传入该目录的临时文件，完成后提交并可显示文件。只有用户选择“接收后用于粘贴”时，才生成受管理的缓存并写入本地文件 URL，且需确认剪贴板未被新内容替换。这条兼容路径需要额外操作，也仍可能有二次复制，不能宣传成完全透明的文件粘贴。

不采用全局拦截 ⌘V、等待下载后向当前窗口重放的默认方案：菜单粘贴和其他读取入口无法覆盖，等待期间焦点与剪贴板也可能改变。

依据：[NSFilePromiseProvider](https://developer.apple.com/documentation/appkit/nsfilepromiseprovider)、[NSPasteboardItemDataProvider callback](https://developer.apple.com/documentation/appkit/nspasteboarditemdataprovider/pasteboard(_:item:providedatafortype:))。

## 7. 大文件传输必须一起调整的部分

### 端到端背压

每个传输从网络接收、分发、写盘到平台读取均有字节预算。例如先以每任务 4 MiB 在途预算、全局 32 MiB 为起点，再实测调整。限制应按字节计算，不能只限定块数。

磁盘或目标消费变慢时停止发放接收额度；发送端随之暂停读取。不能把有界通道的阻塞发送直接放回核心主循环，否则只是把内存问题变成输入卡顿。发送窗口、独立接收任务、核心控制事件队列需要共同设计。

### 元数据与文件一致性

复制时先生成轻量顶层摘要；完整目录枚举放到后台任务，并可分页交付、取消和限制资源，允许总量处于“计算中”。目录拓扑在清单冻结后固定；目录清单扫描不是复制时刻的文件系统快照，应明确这点。

开始读取时绑定文件身份、大小与修改信息，读取中/结束再检查变化。内容哈希用于传输校验，不能单独保证读取期间源文件从未改变。第一版检测到变化就失败并提示重新复制；严格快照一致性需要文件系统快照或本地暂存，作为后续能力。

### 完成、续传与缓存

- 明确 End / Result：发送完毕和接收成功是不同状态；落盘模式需在校验、提交后确认。虚拟文件流模式若拿不到目标提交结果，只能报告内容交付完成，不能声称最终文件已经安全落盘。
- 续传记录 snapshot、文件身份、清单版本、已确认的区间及校验信息。重连后重新授权并核对来源，再从有效边界继续；不能只增加一个 offset 参数。崩溃恢复只认可已持久化并可校验的边界。
- 继续使用临时文件和路径校验；提交需实现平台上的原子不覆盖语义，避免“先检查目标是否存在，再 rename”的竞争窗口。
- 缓存放入受管理的 Cache 目录，不默认污染 Downloads。只缓存确实需要重读/Seek/兼容性落盘的内容，有配额和过期回收；当前 provider、正在传输和被目标持有的条目不得回收。
- 优先流式交付；不强制提前缓存整个大文件，也不承诺所有目标下都能做到零临时磁盘开销。

### 传输通道

保留现有控制连接 + 数据连接、TLS、证书绑定与 QoS。少量 offer 元数据可走数据侧高优先级消息；大清单分页，大正文走有界任务管道。

初期可复用现有 FileChunk 编码，先消除生命周期和队列问题。并行流式消费者增多后，再在数据连接内按任务使用独立 QUIC stream，并验证调度、取消和拥塞控制；无需为每个任务新建连接。

现有文档“同一 QUIC 连接所有数据严格顺序送达”的表述需要修订：有序性在 stream 内，多个 stream 仍共享连接的拥塞与流控资源。保留两连接的理由包括现有架构和套接字 QoS，不应建立在 QUIC 不支持多流的前提上。

## 8. 协议与迁移

建议增加按需剪贴板能力协商，具体位号和消息编号在实现时分配：

```text
ClipboardOffer(snapshot, representations, top_level_summary)
ClipboardRequest(snapshot, representation, request_id)
FileManifestRequest(snapshot, page)
TransferStart(snapshot, request_id, lease)
TransferData(job, entry, offset, bytes)
TransferEnd(job, verification)
TransferResult(job, outcome)
OfferRevoke(snapshot)
TransferCancel(job, reason)
```

文件读取只接受内容引用，不接受对方任意指定本机路径。引用/lease 绑定会话身份、允许的条目与寿命，旧引用不能因重连自动恢复权限。

双方支持新能力时启用新流程；旧客户端保留小内容同步。新端对旧端的文件操作应显示为显式的兼容接收，避免静默退回“过边缘就下载”；旧端未升级的接收体验则无法由新端单方面修复。同步更新 Rust API、C ABI、Swift 包装和 Windows agent，避免仅核心升级后声称支持原生 provider。

## 9. 推荐实施顺序与验收

1. **先验证平台接口**：Windows 虚拟文件粘贴、macOS Finder 粘贴/拖放分别做最小原型，输出兼容性矩阵。这一步决定哪些路径可称为透明按需粘贴。
2. **修复当前状态边界**：快照版本和 generation 检查，传输任务独立；完整扫描移出主循环，接收链路加字节预算，完成改为接收端确认。
3. **接入新 offer/request 协议与平台 provider**：小内容即时同步，文件默认不预取；先完成 Windows 与 macOS 已验证的路径，并提供明确的兼容接收入口。
4. **补全续传、缓存与重读**：断线恢复、同文件重复粘贴、多个消费者、受控并发与回收。跨屏拖放随后复用同一套传输引擎。

必须覆盖的验收场景：

- 复制 20 GB 文件并切屏，不发文件正文；仅请求格式和文件描述也不发正文。
- 在支持的目标中一次粘贴即可启动复制，无需等后台预下载后再粘贴一次。
- 文件任务进行时复制文字；文字能正常粘贴，旧文件完成后不覆盖它。
- 人为限制接收磁盘或目标读取速度，记录内存峰值，确认符合全链路字节预算。
- 传输中持续移动鼠标和键入，测量输入延迟分位值，并与无传输基线比较；不能仅凭分连接就宣称无影响。
- 检查大于 4 GB 文件、多文件/空目录、同名冲突、来源改变、目标取消、空间不足和断线重连。
- 从菜单粘贴、使用第三方目标、读取多次或 Seek，均按兼容性矩阵得到成功或明确失败。
- 只收到网络数据、写盘尚未结束或目标尚未报告结果时，界面不提前宣称接收成功。

以上是待实施的验收标准，不是本次已经运行或通过的测试。
