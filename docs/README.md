# Colab Rust 客户端研究与实现规格

研究日期：2026-11-16。目标是在本地不安装 Python 的条件下，用 Rust 覆盖 Google Colab CLI 与 colab-ext 的功能合集；保留原版 WebSocket 通信能力，长期运行、自动重建和任务恢复由外部调度层负责。本文档集用于实现规划、协议开发和验收；完整客户端和线上兼容验证尚未完成。

当前已实现多账户、OAuth、authorized_user/service_account/impersonation/文件或URL及非交互可执行/AWS external_account/Workforce用户/metadata ADC、本地存储、会话控制面、Contents、跨账户汇总与执行核心、run 与历史导出、REPL、协议心跳、Unix SSH、独立 Drive 凭据与 DriveFS 挂载、只读核查和恢复；远端 metadata 和挂载控制器已离线实现，支持环境凭据、实验性计算 OAuth 来源与额外参数，见 [25](25-drivefs-inputs-implementation.md)。真实 Drive 读写、只读和服务端清理已完成受控验证，恢复经过及尚缺的跨账户/长期刷新证据见 [65](65-live-drive-validation-and-recovery.md)。实际命令、恢复约定和限制见 [基础模块实现状态](09-foundations-implementation.md)，运行方式见 [项目 README](../README.md)。下列目标规格不等于功能均已交付。

结论：本地客户端完整重写具有源码层面的可行性。主要工程成本在认证兼容、Jupyter 消息状态机、多账户隔离、DriveFS 运行环境和失败后的资源恢复。远端仍执行用户 Python，并依赖 Colab 服务、Google DriveFS 与 FUSE；重写这些服务端组件不在目标中。

## 阅读入口

| 文档 | 用途 |
| --- | --- |
| [功能与命令契约](01-features-and-cli.md) | 功能边界、完整命令分组、参数兼容、输入输出与差异决策 |
| [账户 认证与存储](02-accounts-auth-storage.md) | 账户解析、OAuth/ADC、配置结构、锁和迁移 |
| [控制面与文件接口](07-control-plane-and-contents.md) | 会话分配、请求响应 schema、重试、用量与 Contents |
| [运行时与通信协议](03-runtime-and-wire-protocols.md) | Jupyter、执行状态机与 SSH |
| [DriveFS](04-drivefs.md) | 远端凭据服务、授权与挂载模式、清理和安全边界 |
| [Rust 架构与实施顺序](05-rust-architecture.md) | 模块责任、上下文、错误与恢复、阶段交付条件 |
| [生命周期职责边界](08-supervision-and-recovery.md) | CLI 通信与清理职责、外部任务调度及已归档方案 |
| [验收与证据缺口](06-validation-and-open-questions.md) | 离线差分、协议测试、线上探针、风险与待定项 |
| [真实执行断线验收](61-live-execution-disconnect-validation.md) | TLS 透明故障注入、未知状态、不重放与显式恢复 |
| [Drive 真实验收准备](62-drive-live-validation-preparation.md) | 离线受限目录计划、二进制/只读探针与精确清理 |
| [Drive 真实验收与恢复](65-live-drive-validation-and-recovery.md) | 实际读写/只读、独立云端字节与清理核对、进程归属和上传后卸载 |
| [恢复记录检查点合并](66-recovery-checkpoint-merge.md) | 旧执行快照丢更新复现、字段差量合并与冲突保护 |
| [真实 DEFAULT 帧与重放](67-live-codec-capture-and-replay.md) | 真实文本/二进制采集、双 decoder 核对、脱敏样本和 U06 关闭 |
| [真实 kernel 崩溃与恢复](68-live-kernel-death-validation.md) | 生命周期 Unknown、部分输出、禁止重放、显式恢复及两次清理 |
| [macOS 钥匙串代码身份](69-macos-keychain-code-identity.md) | 只读签名检查、重复构建身份变化与未证明范围 |
| [External account 客户端认证](70-external-account-client-auth.md) | STS Basic、workforce 项目规则与请求身份隔离 |
| [X.509 ADC 与 mTLS](71-x509-adc-and-mtls.md) | 证书材料快照、双向 TLS、轮换、账户隔离和本地验收 |
| [Metadata mTLS](72-metadata-mtls.md) | none/strict/default、证书轮换、HTTP 回退及模式接线 |
| [基础模块实现状态](09-foundations-implementation.md) | 当前 Rust 代码、命令、测试结果、迁移恢复和认证限制 |
| [控制面实现状态](10-control-plane-implementation.md) | 会话分配、查询、释放、用量与操作日志恢复约定 |
| [文件与汇总实现状态](11-contents-and-inventory-implementation.md) | Contents 命令、编辑恢复、runtime token 刷新及有界账户汇总 |
| [Jupyter 与执行实现状态](12-jupyter-and-execution-implementation.md) | DEFAULT 协议、exec/Notebook、kernel 恢复与并发生命周期 |
| [run 与历史实现状态](13-run-and-history-implementation.md) | 脚本退出码、清理与信号恢复、历史与四种导出 |
| [CLI 自动对比](14-cli-comparison.md) | 已实现入口的离线双端对比、覆盖边界与差异 |
| [真实 CPU 验收](15-live-cpu-validation.md) | 单次线上会话、文件往返、执行、重启、导出与释放结果 |
| [VM 授权与官方 Drive](16-colab-automation-implementation.md) | Colab 专用消息、凭据传播、人工授权与 kernel 差异 |
| [远端软件包安装](17-install-implementation.md) | uv/pip、requirements 上传、独立 kernel 与恢复指纹 |
| [REPL 与 WebSocket 心跳](18-repl-and-websocket-implementation.md) | 持久连接、多行编辑、中断确认、断线保护与验证边界 |
| [SSH 实现](19-ssh-implementation.md) | 交互/命令/proxy、字节桥接、系统 OpenSSH 与清理恢复 |
| [DriveFS 独立凭据](20-drive-credentials-implementation.md) | Drive consent/import/show/token/clear、账户隔离和刷新恢复；后续远端挂载见 24 |
| [DriveFS metadata 组件](21-drivefs-metadata-implementation.md) | 远端 shim、私有配置、串行刷新和轮换；后续控制器与挂载见 22–24 |
| [DriveFS 挂载控制器](22-drivefs-controller-implementation.md) | 挂载/状态/卸载远端 API、所有权、私密输入、未知禁止重放；后续 CLI 接入见 24 |
| [DriveFS 只读与凭据回收](23-drivefs-probe-and-recovery.md) | status/probe-cleanup CLI、独立只读恢复、冻结/采用/确认清理 API；后续核心编排见 24 |
| [DriveFS 挂载编排与租约](24-drivefs-lifecycle-implementation.md) | mount/umount、持久计划、跨账户租约、凭据回收与 VM 清理；真实恢复证据见 65 |
| [DriveFS 输入与参数](25-drivefs-inputs-implementation.md) | 环境/计算 OAuth 来源、身份覆盖、轮换保留、额外 argv 与原生传输兼容 |
| [扩展诊断与兼容入口](26-extension-diagnostics-implementation.md) | ext doctor/status/version/install、隐藏 drive 别名、无钥匙串/网络/写入的本地诊断 |
| [URL 与工具命令](27-utility-commands-implementation.md) | url/pay、whoami、readme/skill 资源与别名、强制刷新及脱敏 tokeninfo |
| [根挂载策略](28-drivemount-strategy-implementation.md) | 默认 DriveFS、显式官方策略、交互回退与共享操作锁 |
| [账户联合状态](29-account-status-implementation.md) | account/acct status、CCU 与会话联合汇总、部分失败和 Drive 文件存在语义 |
| [合并版在线汇总](44-merged-inventory-implementation.md) | 会话/用量 JSON、全账户文本、独立身份与错误处理 |
| [账户选择校验](43-account-selection-implementation.md) | 根级/局部作用域、变更前校验、空值与环境选择 |
| [账户变更](42-account-mutations-implementation.md) | 合并版变更输出、保序回退与 file 生命周期恢复 |
| [账户离线清单](41-account-inventory-implementation.md) | 合并版 list/current 业务 JSON、离线摘要与安全差异 |
| [AWS ADC](40-adc-aws-implementation.md) | aws1 环境/EC2 凭据、SigV4 与独立签名对照 |
| [ADC 可执行来源](39-adc-executable-implementation.md) | 显式 opt-in、输出缓存、进程组与取消；真实 IdP 待验收 |
| [Metadata ADC](38-adc-metadata-implementation.md) | 最后发现、文件优先、标记与缓存；真实 GCE/GKE 待验证 |
| [Workforce 用户 ADC](37-adc-workforce-user-implementation.md) | STS Basic 刷新、只读缓存、轮换与失败恢复 |
| [ADC 外部身份](36-adc-external-account-implementation.md) | 文件/URL 主体、STS、workforce 与可选 IAM；真实 IdP 待验证 |
| [ADC 服务账户](30-adc-service-account-implementation.md) | PKCS#8 RSA、JWT bearer 交换、独立缓存与隔离测试；线上身份仍待验证 |
| [补全与 Drive 状态选择](35-completion-and-drive-status-implementation.md) | 离线 shell 脚本、安全安装与只读 Drive 账户展示 |
| [账户联合授权](34-account-login-and-drive-consent-implementation.md) | add/login 默认流程、active 切换、Drive 失败与账户隔离 |
| [合并 help 与别名](33-merged-help-and-aliases-implementation.md) | 固定三仓库运行时契约、隐藏入口与汇总别名 |
| [原生更新与发行候选](32-native-update-and-release-implementation.md) | GitHub Releases 清单、校验安装、一代回滚与离线打包 |
| [ADC IAM impersonation](31-adc-impersonation-implementation.md) | 独立源/目标 token、IAM delegates、失败不换身份及隔离验证 |
| [源码清单](../research/source-inventory.json) | 三个仓库固定提交、逐文件 SHA-256、静态入口和测试函数清单 |
| [离线协议样本](../research/kernel-codec-probe.json) | 七项纯函数检查结果与可重放的合成帧 |

## 固定基线

| 代号 | 仓库 | 固定提交 |
| --- | --- | --- |
| CLI | [googlecolab/google-colab-cli](https://github.com/googlecolab/google-colab-cli) | `02d2321c38dd215b7804eebf78b61d98b856bcd3` |
| EXT | [deluxebear/colab-ext](https://github.com/deluxebear/colab-ext) | `980a39748badc3168a5af8734b2467351bdafcbc` |
| KERNEL | [googlecolab/jupyter-kernel-client](https://github.com/googlecolab/jupyter-kernel-client) | `f18e982c3265df5e923aa9def101ab3fd737e139` |

KERNEL 的固定提交取自 CLI 的 [uv.lock](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/uv.lock#L557)。不能用另一个 PyPI 版本的默认行为替代这份基线。后续上游更新应重新生成清单、比较变化并更新规格。

## 证据分级

- **源码事实**：固定提交的实际分支、数据结构、序列化或测试断言可直接支持。
- **静态风险**：存在具体触发路径，但本轮未在真实 Google 环境或并发运行中复现。
- **Rust 设计**：为了实现功能、隔离故障或修正风险而提出的目标行为，不代表上游已经如此实现。
- **待验证**：需要服务端、依赖默认配置、平台或真实凭据才能建立的事实。

正文的 C、A、R、D、H 编号分别标记命令、账户、运行时、Drive 和 HTTP 接口证据。每篇文档保留固定提交的源码链接。文档内测试编号 V 标记验收要求，不代表测试已经执行。

## 完整实现的定义

| 层次 | 本项目目标 |
| --- | --- |
| 本地应用 | Rust 实现；无本地 Python 运行时依赖 |
| 命令与业务 | 覆盖 CLI 与 EXT 的有效业务能力、兼容别名和自动化输出 |
| 生命周期能力 | 原版 WebSocket、心跳与连接状态、凭据刷新、资源清理恢复；长期保活策略、自动重建和任务检查点由外部负责 |
| 协议 | 实现目标实际使用的 HTTP/WebSocket/SSH 桥接契约 |
| 本地系统组件 | 初版允许使用系统 OpenSSH、系统凭据库、浏览器和用户编辑器 |
| 远端辅助逻辑 | 初版由 Rust 生成并发送 Python payload，保留现有 Colab 环境依赖 |
| 远端全 Rust helper | 可选后续工程，不作为初版必需条件 |
| DriveFS / Google 后端 | 使用现有组件；不承诺自行复刻其内部实现或服务端可用性 |
| Python 扩展加载器 | 业务功能直接内建；`.pth` 注入、venv 修复按“不适用”处理 |

初版平台边界建议为 Linux 与 macOS。Windows 的上游 SSH 测试覆盖不等于整个客户端的 Windows 支持；若纳入，必须单独通过凭据库、编辑、锁、信号、安装与 SSH 参数转义验收。

## 研究覆盖与限制

已检查：命令注册和签名、账户与凭据逻辑、状态/历史结构、API 调用、消息序列化及队列、执行/SSH/Drive 生命周期、关键单元测试和集成脚本。

初期静态研究没有读取用户凭据、登录 Google 或创建 VM；后续授权的单次真实 CPU 验收见 [15](15-live-cpu-validation.md)。尚未挂载或访问用户 Drive。静态清单工具仅解析源码并计算哈希；它不导入目标模块。另对固定 KERNEL 源码提取的五个纯编解码函数执行了七项离线检查，结果与合成样本已保存；初期未执行上游 CLI 或完整测试套件；后续已增加固定官方源码的隔离运行与 Rust 完整回归，结果及限制见 [CLI 自动对比](14-cli-comparison.md)。

因此 OAuth client/redirect 是否仍被接受、账户与套餐权限、DriveFS 私有参数、长期刷新可靠性、各平台真实行为仍属于验收阶段。上游文档声称的“已测试”只作为其历史记录，不能替代本项目的实测结果。

保活与恢复的服务边界另参考 [Google Colab FAQ](https://research.google.com/colaboratory/faq.html)，核对日期 2026-11-16。上游历史保活接口是否仍有效尚未验证；原 supervisor 方案已归档，不属于当前实现或验收范围。

新增实现：[45 每日原生发行检查](45-daily-native-update-implementation.md)、[46 执行富文本显示](46-rich-text-output-implementation.md)、[47 终端图片](47-terminal-image-output-implementation.md)、[48 WebSocket HTTP 代理](48-websocket-http-proxy-implementation.md)、[49 SOCKS5 代理](49-websocket-socks-proxy-implementation.md)、[50 HTTPS 代理](50-websocket-https-proxy-implementation.md)、[51 JPEG 终端显示](51-jpeg-terminal-projection-implementation.md)、[52 ADC 配额项目](52-adc-quota-project-implementation.md)。

## 复现研究基线

分别检出上述三个提交后执行：

```sh
python3 tools/collect_source_inventory.py \
  --cli /absolute/path/google-colab-cli \
  --ext /absolute/path/colab-ext \
  --kernel /absolute/path/jupyter-kernel-client \
  --output research/source-inventory.json
```

工具要求 HEAD 与固定提交一致；输出记录所有受 Git 跟踪文件的 SHA-256。命令清单是 AST 静态入口，不会展开运行时 monkey-patch、Typer 默认参数或 Annotated 类型别名。最终 CLI 契约仍需要在隔离环境中生成合并后的 help 并核对。

## 开发前必须关闭的缺口

1. 合并版真实 help、关键输出与 JSON fixtures 尚未采集。
2. 三类 Drive 授权及主 OAuth 的实际 redirect/state/PKCE 行为尚需离线依赖验证和线上授权验证。
3. 所有 ADC 身份种类的 Colab 访问能力尚未建立。
4. DriveFS 当前版本、私有参数与 metadata shim 兼容性尚需真实 VM 验证。
5. 扩展仓库基线根目录未发现 LICENSE；直接复用其源码前需确认许可范围。本文只登记这个缺口，不作授权结论。

上述缺口不阻止开发离线模块，但阻止宣称“已与线上完全等价”。完整测试与风险处置见 [验收文档](06-validation-and-open-questions.md)。

控制面命令内凭据续期、只读查询的有限认证重试、锁复用和失败责任见 [53](53-control-credential-refresh-implementation.md)。

Linux ARM64 的非 root、离线容器验证及最低 Rust 版本检查见 [54](54-linux-container-validation.md)。其结果覆盖本机 Linux 文件/终端/SSH/锁/发行路径，不替代真实系统钥匙串或 Google 验收。

Linux 原生 Secret Service 与独立 Python keyring 的互通、锁定、无响应、服务故障和恢复证据见 [55](55-native-secret-service-validation.md)。

真实 SSH 测试的资源账本、条件清理和未满足的认证前提见 [56](56-live-ssh-validation-and-guarded-cleanup.md)。当前交付脚本和离线保护，尚未新增真实 SSH 通过证据。

合并版 Drive 凭据 show/clear 的输出、路由对照及旧 JSON 包装迁移见 [57](57-merged-drive-credentials-implementation.md)。

Drive consent 开始阶段的命名账户自动登记、邮箱提示、并发及失败边界见 [58](58-drive-consent-profile-implementation.md)。

macOS x86_64 通过 Apple Silicon/Rosetta 的执行验证与 Intel 原生硬件边界见 [59](59-macos-x86-rosetta-validation.md)。

全范围验收的 53 个 V01–V07 条目、阶段门和外部前提逐项核对见 [60](60-acceptance-audit.md)；有测试文件或帮助快照不等于该项完整验收通过。

Drive 受控执行器、运行时 endpoint 条件、失败恢复与本地验证边界见 [63](63-drive-live-validation-executor.md)。本阶段没有真实云盘写入或新建 VM。

Linux x86_64 隔离模拟执行、EINTR 修正与失败记录见 [64](64-linux-x86-container-validation.md)；不能代替原生 x86 硬件或真实 Google 验收。

候选上游声明见 [73](73-release-notices.md)；四目标 Rust 依赖许可材料与当前 macOS ARM64 release 的十八项复验见 [74](74-rust-dependency-licenses.md)。

文件命令的实际合并版十二组请求/字节对照与两处兼容修正见 [75](75-merged-files-and-json-bytes.md)。

JSON 下载数值精度与十八组合并版对照见 [76](76-json-number-download-compatibility.md)。
