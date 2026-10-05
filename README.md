# Codex Auto Resume

管理 Codex Desktop 本地会话，在会话明确因 5 小时额度耗尽而中断后，等待额度恢复并发送你配置的原文消息。支持按次数恢复、持续监控、托盘、历史记录、简体中文与 English。

## 下载与平台支持

公开版本从本仓库 **Releases** 下载。发布流程先生成草稿预发布 Release，由维护者检查后公开；公开前可从 Actions → **Build installers** 的成功运行下载产物（需要 GitHub 登录）。

| 平台 | 架构 | 安装包 | 自动恢复 |
| --- | --- | --- | --- |
| Windows | x64 | `.exe`（NSIS，当前用户安装） | 支持，受版本及身份校验限制 |
| macOS | Apple Silicon / arm64 | `.dmg` | 实验版，暂不支持 |
| macOS | Intel / x64 | `.dmg` | 实验版，暂不支持 |
| Linux | x64 | `.deb`、`.AppImage` | 实验版，暂不支持 |

Windows 包未做发行者签名；macOS 包仅做 ad-hoc 签名，未做 Apple Developer 签名或公证，系统可能提示来源无法验证。Release 附带 `SHA256SUMS`，用于核对下载完整性。

macOS/Linux 已补齐主目录解析，使界面不因缺少 Windows 环境变量而退出；目标系统安装、启动、托盘及通知仍待验收。Desktop 接口明确阻断，不能开启自动恢复。系统自启动与界面“打开日志和数据目录”目前仅适用于 Windows。

## 使用（Windows）

1. 运行候选安装包或 `npm run desktop:dev`，保持 Codex Desktop 登录并打开。
2. 在“最近会话”选择目标，设置原文恢复消息及 Count(N) / Continuous，点击开启。
3. 仅在最新 turn 明确因 5h 中断，且同 turn 历史额度证据成立时，等待额度恢复并提交一次消息。正常结束、人工停止及周额度耗尽均不触发。
4. Watch 暂停/停止只阻止后续发送，不终止 Codex 正在运行的任务。
5. 关闭窗口保留托盘监听；退出或重新启动应用后旧 Watch 均暂停，必须重新开启。

**当前是候选版本。按用户要求，本次开发没有执行任何真实 resume 发送效果测试。单 Watch 实发上下文、完整 Desktop 执行流同步、自然 5h 周期仍待验收。**

## 兼容性

- 自动恢复只支持 Windows Native，Desktop / Codex Runtime 版本仅用于诊断，不设置固定版本白名单；Windows 需安装 PowerShell 7 和 WebView2，安装包不包含 Codex Desktop。
- 从实际 Windows 命名管道发现 Desktop 消息入口，校验工具能力；不依赖当前 Agent 的 CODEX 环境，不运行 CLI writer，不使用键鼠自动化。
- 当前账号来自 Desktop 只读额度接口；只有本地 rollout 的 creator account 与之匹配，且会话列在 Desktop 中，才允许管理。
- 从 `tools/list` 校验四个所需工具及其参数 schema，每次调用前检查实际参数，发送前检查原文消息参数与目标会话读取响应。schema 校验仅在本地运行，不获取网络或文件引用；接口缺失、参数/响应不兼容或账号/上下文不明确时停止发送，显示具体诊断。版本未知不阻断；重新检查恢复兼容后不会自动重新授权 Watch。
- 额度后台检查每5分钟一次，失败也按此间隔重试；手动刷新及准备发送前强制核实最新额度。会话列表仍按配置的轮询间隔读取。临时额度或会话列表通信失败跳过本轮，保留上次成功值、原更新时间和 Watch 授权，本轮不发送；成功核实后继续判断。没有缓存时显示未知；账号/协议/版本等硬失败和发送结果不确定仍阻断，历史“需要处理”需手动重新启用。
- 提交前持久化 Attempt，同一中断来源永不重复发送。响应超时/结果未知时保持 NeedsAttention，不以 CLI fallback 重发。
- 已知发送响应只返回 threadId。成功响应后进入“已提交，等待执行确认”，每 5 秒只读核对发送前基线之后的 Desktop 工具输入：记录类型、来源、原文摘要、唯一输入和明确 turn_id 都匹配，且已观察到执行证据，才扣一次并恢复监控。原文不会附加标记。
- 后续用户轮次不抹掉已经执行的 resume：只要唯一匹配的结构化输入及执行证据成立，就扣本次一次；随后用户输入撤销后续自动发送授权。多个匹配候选、候选内部手动干预、多条输入、身份不完整仍不扣、不重发。两分钟内未确认、超出16MiB累计读取或128轮上限、文件替换/连续性变化均进入 NeedsAttention；超时与历史未知记录不会自动补扣。Pause/Stop 不取消已提交 Desktop 任务，后续证据不会重新授权；重启撤销待确认资格。
- 普通展示仍有界读取最后8MiB；缺少开始事件的最新有效结束事件可以显示状态，但不制造执行或额度资格。待确认Attempt从发送前基线增量读取，保存进度，每次约4MiB，单行最多1MiB，未读完整不提前确认；不向基线之前扫描历史。
- Desktop 未回传请求编号；结构化记录关联不能保证跨客户端的原子 exactly-once。已完成一次授权的诊断实发，但新版自动记账与自然 5h 连续恢复仍需独立实际验收，fixture 不能替代。

## 显示语言

设置页支持“跟随系统”、简体中文与 English。显式选择立即保存；跟随系统时英文系统使用英文，其余系统语言回退简体中文。界面、状态、日期格式、托盘菜单与后续通知使用同一显示语言，日期保持本地时区。安装器语言与应用语言分别处理。

切换语言不会改写默认恢复消息、已有 Watch 消息、会话标题或路径，不会改变授权、发送资格与扣次。新 Watch 原因、历史及 Attempt 诊断使用稳定消息代码与参数；旧记录保留原文，未知、缺参或第三方消息按安全回退原文显示。

语言资源随应用打包，位于 `src/locales/ui.json` 与 `src/locales/backend.json`。新增文案必须同时维护两种语言及插值参数；`npm test` 检查资源完整性和显示合同，Rust fixture 检查设置/数据库兼容及分类。项目长期要求见 `AGENTS.md`。

浏览器预览可切换语言，选择仅在本次预览有效；不会连接 Codex 或发送消息。原生托盘/通知及重启持久化的实际验收须单独记录。

多语言 fixture 覆盖资源键/插值一致性、系统及未知语言回退、嵌套/缺参回退、用户原文、旧设置/数据库、语言保存与重开、终态审计保留及业务状态不变。已有浏览器预览核查中英切换、未保存用户原文保持，以及 850×600 英文设置页无水平溢出；这些结果不替代原生托盘、通知及重启持久化验收。

## 本地数据

应用数据目录为 `%APPDATA%\com.local.codex-auto-resume\`（实际路径由 Tauri app_data_dir 决定）。SQLite 保存 Watch 配置、Prompt、资格来源、发送尝试和历史；不会修改 Codex 的 thread 数据库或安装文件。

`diagnostics.jsonl` 记录版本、检查阶段、故障和发送状态转换，单文件最大约 2MB，保留一份轮转日志。临时 `desktop_tool_failure` 诊断按用户授权优先定位：记录失败工具名、调用上下文、调用编号、耗时和 Desktop 实际失败返回文本（最多16Ki字符）/RPC错误（最多8Ki字符），不记录成功响应或发送请求体。定位并修复后可移除此诊断。界面“打开日志和数据目录”可定位文件，“导出诊断”只导出兼容状态与管理数量。

系统自启动默认关闭，仅用户保存相应设置时写入当前用户的 Windows Run 项。自启动不会恢复旧 Watch 授权。

## 开发与验证

需要 Node.js 22、PowerShell 7、Rust stable 及平台构建依赖。Windows 使用 MSVC 和 WebView2；macOS 使用 Xcode Command Line Tools；Linux 需要 WebKitGTK 4.1、AppIndicator 等库，完整依赖见 workflow。

```powershell
npm ci
node scripts/check-release.mjs
node --test scripts/release-checks.mjs
npm run build
npm test
cargo test --manifest-path src-tauri/Cargo.toml
npm run desktop:dev
npm run desktop:build
```

Windows 只读集成检查（不会打开 Watch 数据库或发送消息）：

```powershell
cargo run --manifest-path src-tauri/Cargo.toml -- --observe-only
```

候选安装包位于 `src-tauri/target/release/bundle/nsis/`；指定目标构建时位于 `src-tauri/target/<target>/release/bundle/nsis/`。构建不代表真实恢复或原生托盘验收通过。真实 resume、自然 5 小时恢复、系统自启动及完整 Desktop 执行流仍待单独验收。

macOS/Linux 本地打包须在对应系统上显式覆盖默认 Windows bundle：

```powershell
# macOS
node node_modules/@tauri-apps/cli/tauri.js build --bundles dmg
# Linux
node node_modules/@tauri-apps/cli/tauri.js build --bundles deb,appimage
```

## GitHub Actions 与发布

[release.yml](.github/workflows/release.yml) 在四个构建任务中生成上表的五个安装包。任务执行版本校验、发布脚本测试、前端测试、Rust fixture 和 Tauri 打包。Intel Mac 在 Apple Silicon runner 上交叉编译，其 Rust fixture 只编译、不执行。

1. **手动检查**：Actions → Build installers → Run workflow，仅上传产物，不创建 Release。相关文件的 PR 也运行构建，使用只读权限。
2. **同步版本**：修改 `package.json`、`package-lock.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`、`src-tauri/tauri.conf.json`，更新本文“发布说明”章节，发布脚本直接读取该章节作为草稿 Release 正文。运行上述版本及发布脚本检查。
3. **标签触发**：提交发布文件后，由维护者推送与应用版本完全一致的 `v<version>` 标签。当前版本为 `v0.1.1`；不匹配则失败。
4. **草稿上传**：只有全部平台构建成功后才生成草稿预发布 Release，上传五个安装包及 `SHA256SUMS`。重跑允许更新草稿，拒绝覆盖已公开 Release。
5. **人工发布**：检查产物、校验值、平台限制、签名状态和实际验收后，手动公开草稿。

```powershell
# 由维护者确认发布提交及版本后执行
git tag v0.1.1
git push origin v0.1.1
```

构建任务使用 `contents: read`，草稿上传任务使用内置 `GITHUB_TOKEN` 的 `contents: write`，不需要个人访问令牌。当前流程未配置发行者签名密钥、Apple 公证或自动更新，也不执行任何真实 resume 或系统自启动操作。

跨平台构建以 GitHub runner 的实际结果为准，本地 Windows 检查不能证明 macOS/Linux 构建和运行成功。配置参考 [Tauri GitHub Actions 文档](https://v2.tauri.app/distribute/pipelines/github/) 和 [tauri-action](https://github.com/tauri-apps/tauri-action)。

## 发布说明

- Desktop 升级兼容：移除固定版本白名单，改为检查实际工具能力、参数 schema 和响应；版本仅用于诊断。
- resume 发送前校验原文参数及本地会话身份；协议或执行结果不明确时继续停止、不扣次、不重发。临时只读失败的提示与重试行为保持。
- Windows x64：NSIS 安装包，自动恢复使用 Windows Desktop IPC。
- macOS Apple Silicon / Intel：DMG 实验包，自动恢复暂不支持。
- Linux x64：DEB / AppImage 实验包，自动恢复暂不支持。
- SHA256SUMS：所有安装包的 SHA-256 校验值。

当前为候选版本。Windows 使用实际能力、参数和响应检查，升级版本本身不会阻断。发送接口声明通过不能替代真实发送及原会话执行验收；结果不明确仍不扣次、不重发。macOS/Linux 已补齐界面启动的主目录解析，但目标系统运行仍待验收；Desktop 接口明确阻断，不能开启自动恢复，系统自启动暂不支持。

临时读取额度/会话列表失败跳过本轮并保留授权，本轮不发送；额度后台每5分钟检查，手动刷新及发送前仍强制核实。重启撤销授权，Pause/Stop 不取消 Codex 任务，同一来源不重发。

Windows 包未做发行者签名；macOS 包仅使用 ad-hoc 签名，未做 Apple Developer 签名或公证。系统可能提示来源无法验证。

构建成功不代表功能验收通过：真实 resume、自然5h恢复、跨平台安装/启动、原生托盘/通知仍须单独验收。

## 已执行验证与验收边界

2026-10-05 能力兼容更新：应用版本保持 `0.1.1`，Windows Rust 68 项、前端15项及发布脚本3项检查通过，Windows NSIS 构建通过。Desktop `26.930.4958.0` 的真实只读能力检查通过；未执行真实 resume 发送或安装验收。

2026-10-04 发布准备的本地检查结果：

- 版本校验通过，当前应用版本为 `0.1.0`。
- 发布脚本的3个集成测试通过，覆盖错误版本/标签、缺失安装包、产物命名、完整五包集合、拒绝覆盖公开 Release、拒绝草稿额外资产以及草稿上传和校验文件。
- 前端15项测试通过，包含双语资源与回退检查；Windows Rust本地62项fixture通过。
- actionlint v1.7.12、YAML/JSON及脚本语法检查通过。
- 使用与CI相同的 Windows x64目标和参数打包成功，生成约3.02MiB NSIS安装包；收集后的安装包SHA-256与原文件一致。
- 独立审查发现并修复草稿残留额外资产问题，复审无新增可操作问题。

GitHub Actions 尚未实际运行，macOS/Linux构建、安装、启动及原生托盘/通知未验收。Intel Mac测试在arm64 runner只交叉编译、不执行。真实resume、自然5h恢复、系统自启动及跨客户端一致性仍未验收，本地fixture不能替代这些结果。
