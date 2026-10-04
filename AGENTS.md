# Auto Resume 项目约定

继承本会话全局 AGENTS 指令，使用 PowerShell 7、rg 和有界输出。

## 确认合同
- `.codex/plans/windows-mvp.operation-path.md` 为已确认 LARGE 开发计划；最新执行状态见相邻 checkpoint。
- 最新用户约束：先按已验证 Desktop IPC 方案开发，**不做真实 resume 发送效果测试**。无需为此重复发送测试消息；不得将 fixture 成功当作 Desktop 实发验收。
- 工具仅发送用户配置的原文消息，不处理审批、不切换模型/权限/cwd，不通过 CLI writer 或键鼠绕过 Desktop。
- 任何兼容/身份/额度/发送结果不确定均阻断后续发送，原因必须可见。
- 重启撤销授权；Pause/Stop 不取消 Codex 任务；同一来源中断不得重发。

## 项目结构
- `src/`：React 管理界面，使用 Tauri invoke；没有直接发送或调试发送命令。
- `src-tauri/src/engine.rs`：SQLite 状态机、一次性来源、授权、Attempt 与确认计数。
- `desktop.rs`：只读能力发现、版本范围、Desktop 私有 IPC 消息适配。
- `sessions.rs`：只读有界解析本地 rollout，历史额度绑定 turn，最新用户事件立即废止旧终止证据。
- `lib.rs`：单实例、串行后台 supervisor、命令、诊断、原生生命周期。

## 实际验证入口
1. `npm run build`：TypeScript + Vite。
2. `npm test`：纯界面决策 fixture。
3. `cargo test --manifest-path src-tauri/Cargo.toml`：纯本地协议/解析/状态/SQLite fixture；不连接真实发送入口。
4. `cargo run --manifest-path src-tauri/Cargo.toml -- --observe-only`：只读 Desktop 发现/会话/账号额度核实，不创建 Watch、不发送消息。
5. `npm run desktop:build`：Windows NSIS 候选包，不自动安装。

长输出保留 `.codex/logs/`（已忽略），只返回命令、结果、数量和最短失败原因。
测试是写操作，不与同文件实现并发运行。实发、托盘、通知、系统自启动及自然 5h 人工验收必须单独记录，不得自动修改实际系统自启动配置来测试。

## 发布与多平台构建
- `.github/workflows/release.yml` 构建 Windows x64、macOS arm64/x64、Linux x64；macOS/Linux 为实验安装包，Desktop 自动恢复仍须明确阻断，不以跨平台构建宣称功能支持。
- 发布相关默认检查增加 `node scripts/check-release.mjs` 与 `node --test scripts/release-checks.mjs`；版本必须与 package、lockfile、Cargo、Tauri 配置及 `v<version>` 标签一致。
- 手动/PR 构建仅上传 artifacts，标签构建全部成功后生成草稿预发布 Release；不得自动公开发布或覆盖已公开 Release。
- 不在 CI 执行真实 resume、自启动修改或 Desktop 实发测试。跨平台安装启动、签名、公证、托盘及通知另行验收。

## 多语言支持（持续维护合同）
- 多语言是项目始终支持、持续维护的功能。当前支持简体中文（zh-CN）和英文（en），提供跟随系统及显式语言选择；实现与验收证据见 `docs/multilingual-validation.md`，计划与检查点见 `.codex/plans/multilingual.operation-path.md` 及相邻 checkpoint。
- 新增或修改用户可见文本时，同步维护全部支持语言，覆盖界面、状态、错误、无障碍标签、托盘和通知；使用统一的本地语言资源，不依赖在线翻译。
- 业务判断、状态流转、错误分类、诊断和通知去重使用稳定代码及必要参数，不能依赖翻译后的文本。结构化消息用于展示，未知代码仍须显示安全的回退原因，不能隐藏故障或放宽发送门禁。
- 语言选择持久化并兼容缺少语言字段的旧设置；日期/数字按显示语言格式化，日期保持本地时区。应用运行语言与安装器语言分别处理。
- 会话标题、路径、用户配置的默认恢复消息及 Watch 消息保持原文；语言切换不能改写消息、改变授权、触发发送或改变扣次。
- 新历史记录保存稳定消息代码及参数，保留审计原文；旧历史记录保留原文，不通过模糊匹配推测翻译。新增结构化字段必须兼容旧数据库。
- 统一语言资源位于 `src/locales/ui.json` 和 `src/locales/backend.json`，Rust 与前端共享后端资源；新 Watch 原因、History 和 Attempt 诊断均须带稳定元数据。缺失/损坏元数据或插值参数时显示完整安全原文。
- `npm test` 中的多语言检查为默认验证流程：覆盖语言资源键/参数一致性、回退、旧原文和显示切换；Rust fixture 覆盖旧设置/数据库兼容及稳定分类。英文长文本布局、原生托盘/通知须单独记录实际验收；不得为多语言验收执行真实 resume 发送测试。
