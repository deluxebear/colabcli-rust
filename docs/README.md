# Colab Rust 客户端研究与实现规格

研究日期：2026-11-16。目标是在本地不安装 Python 的条件下，用 Rust 覆盖 Google Colab CLI 与 colab-ext 的功能合集，并增加保活监控与 VM 自动恢复。本文档集用于实现规划、协议开发和验收；完整客户端和线上兼容验证尚未完成。

当前已实现多账户、OAuth、authorized_user ADC、本地存储、会话控制面、Contents、跨账户汇总与执行核心、run 与历史导出，默认 139 项离线测试通过。实际命令、恢复约定和限制见 [基础模块实现状态](09-foundations-implementation.md)，运行方式见 [项目 README](../README.md)。下列目标规格不等于功能均已交付。

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
| [保活监控与自动恢复](08-supervision-and-recovery.md) | VM 外 supervisor、回收判定、预算、重建与检查点恢复 |
| [验收与证据缺口](06-validation-and-open-questions.md) | 离线差分、协议测试、线上探针、风险与待定项 |
| [基础模块实现状态](09-foundations-implementation.md) | 当前 Rust 代码、命令、测试结果、迁移恢复和认证限制 |
| [控制面实现状态](10-control-plane-implementation.md) | 会话分配、查询、释放、用量与操作日志恢复约定 |
| [文件与汇总实现状态](11-contents-and-inventory-implementation.md) | Contents 命令、编辑恢复、runtime token 刷新及有界账户汇总 |
| [Jupyter 与执行实现状态](12-jupyter-and-execution-implementation.md) | DEFAULT 协议、exec/Notebook、kernel 恢复与并发生命周期 |
| [run 与历史实现状态](13-run-and-history-implementation.md) | 脚本退出码、清理与信号恢复、历史与四种导出 |
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
| 新增生命周期能力 | 显式启用监控与有预算的自动重建；可选检查点任务恢复，不保证无限防回收 |
| 协议 | 实现目标实际使用的 HTTP/WebSocket/SSH 桥接契约 |
| 本地系统组件 | 初版允许使用系统 OpenSSH、系统凭据库、浏览器和用户编辑器 |
| 远端辅助逻辑 | 初版由 Rust 生成并发送 Python payload，保留现有 Colab 环境依赖 |
| 远端全 Rust helper | 可选后续工程，不作为初版必需条件 |
| DriveFS / Google 后端 | 使用现有组件；不承诺自行复刻其内部实现或服务端可用性 |
| Python 扩展加载器 | 业务功能直接内建；`.pth` 注入、venv 修复按“不适用”处理 |

初版平台边界建议为 Linux 与 macOS。Windows 的上游 SSH 测试覆盖不等于整个客户端的 Windows 支持；若纳入，必须单独通过凭据库、编辑、锁、信号、安装与 SSH 参数转义验收。

## 研究覆盖与限制

已检查：命令注册和签名、账户与凭据逻辑、状态/历史结构、API 调用、消息序列化及队列、执行/SSH/Drive 生命周期、关键单元测试和集成脚本。

本轮没有读取用户凭据，没有执行 OAuth 登录，没有创建 Colab VM，没有挂载或访问用户 Drive。静态清单工具仅解析源码并计算哈希；它不导入目标模块。另对固定 KERNEL 源码提取的五个纯编解码函数执行了七项离线检查，结果与合成样本已保存；没有执行上游 CLI 或完整测试套件。

因此 OAuth client/redirect 是否仍被接受、账户与套餐权限、DriveFS 私有参数、长期刷新可靠性、各平台真实行为仍属于验收阶段。上游文档声称的“已测试”只作为其历史记录，不能替代本项目的实测结果。

保活与恢复的服务边界另参考 [Google Colab FAQ](https://research.google.com/colaboratory/faq.html)，核对日期 2026-11-16。上游历史保活接口是否仍有效尚未验证；新 supervisor 为本项目设计，不属于已确认存在的上游能力。

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
