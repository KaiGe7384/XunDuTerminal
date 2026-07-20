# Wave-style 后端并发重构计划

## 目标

让 XunDuTerminal 支持像 Wave 一样同时打开大量服务器、终端、文件和监控窗口时仍然流畅。

核心原则：

- 前端只负责展示和交互，不直接承受远程 IO 风暴。
- 每台服务器有一个连接 Broker，统一管理 SSH Client、终端 Channel、文件 Channel、监控 Command。
- 文件、监控、终端输出都要支持节流、缓存、取消、分阶段加载。
- 先在现有 Rust/Tauri 后端里验证 Broker 思路，再决定是否把 Broker 独立成 Go sidecar。

## 沙盒模拟结果

脚本：`tools/perf-sandbox/go-broker-sim.go`

默认 30 台服务器、终端+文件+监控同时打开：

- 当前模型：90 次 SSH 握手，耗时约 1.47s，模拟 UI 阻塞约 1.18s。
- Broker 模型：30 次 SSH 握手，耗时约 0.61s，模拟 UI 阻塞约 0.19s。

80 台服务器放大测试：

- 当前模型：240 次 SSH 握手，耗时约 3.55s，模拟 UI 阻塞约 3.15s。
- Broker 模型：80 次 SSH 握手，耗时约 1.01s，模拟 UI 阻塞约 0.50s。

结论：连接复用 + UI 分阶段加载值得推进。

## 分阶段路线

### Phase 1：现有 Rust 后端低风险降压

- 文件目录读取优先复用 `remote-aux-worker` 的 SSH 会话。
- 如果远程没有 `python3` 或执行失败，自动回退旧 SFTP 路径。
- 文件和监控同开时，尽量共享同一个 aux SSH helper，减少额外握手。
- 保留现有 Tauri API，不动前端协议。

### Phase 2：Go sidecar POC

- 新建 Go sidecar，提供 `connect`、`openTerminal`、`listDir`、`stats`、`cancel`。
- 每台服务器一个 Broker goroutine，内部复用 `golang.org/x/crypto/ssh.Client`。
- 文件目录、监控采集加 TTL 缓存和请求合并。
- 用 JSONL/stdin-stdout 或本地 TCP 与 Tauri 通讯。

### Phase 3：前端数据节流

- 文件列表只渲染可视区域，继续保留虚拟列表。
- 远程文件和监控只在窗口激活或靠近可视区域时加载。
- 终端数据、监控数据、文件列表更新走批处理，不在同一帧刷多个重组件。

### Phase 4：迁移策略

- 如果 Phase 1 已明显稳定，可以继续强化 Rust Broker。
- 如果 Go sidecar POC 在真实服务器上明显更稳定，再把 SSH/文件/监控逐步迁移到 Go。
- 前端 API 保持兼容，避免一次性大迁移。

## 验证指标

- 30 台服务器同时打开终端+文件+监控不出现窗口未响应。
- 真实日志中 SSH handshake 次数明显减少。
- `remote-list-broker` 成功率高于 SFTP fallback。
- 前端 `ui-lag` 日志持续低于 200ms。
