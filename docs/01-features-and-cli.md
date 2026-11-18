# 功能与命令契约

本文定义官方 CLI 与扩展的合并目标。命令名称、默认值和别名以固定提交实际注册为依据；README 示例仅辅助说明。完整静态参数记录见 [源码清单](../research/source-inventory.json) 的 `command_entrypoints`。

当前 Rust 已实现账户、会话控制、文件、执行/run、历史、install、REPL、Unix SSH、VM auth/官方 drivemount，以及 Drive 独立凭据授权；已实现行为及阶段差异见 [09](09-foundations-implementation.md)、[16](16-colab-automation-implementation.md)。Drive 凭据阶段见 [20](20-drive-credentials-implementation.md)，远端控制器已离线实现，见 [22](22-drivefs-controller-implementation.md)，DriveFS 核心挂载 CLI 与恢复入口已接入，见 [24](24-drivefs-lifecycle-implementation.md)。URL、身份诊断和内嵌资源已接入，见 [27](27-utility-commands-implementation.md)。根 drivemount 的默认 DriveFS 与回退策略见 [28](28-drivemount-strategy-implementation.md)。account status 的联合汇总已接入，见 [29](29-account-status-implementation.md)。本篇完整矩阵仍是目标契约，不代表所有命令已实现。

## C01 命令来源

| 编号 | 源码 |
| --- | --- |
| C01 | [CLI 入口与全局回调](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/cli.py#L41) |
| C02 | [会话命令](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/session.py#L108) |
| C03 | [执行与 notebook](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/execution.py#L145) |
| C04 | [临时任务 run](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/run.py#L202) |
| C05 | [文件命令](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/files.py#L29) |
| C06 | [工具命令与别名](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/utility.py#L390) |
| C07 | [扩展注册](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/__init__.py#L27) |
| C08 | [扩展命令钩子](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L116) |
| C09 | [account 命令](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/account_commands.py) |
| C10 | [drive 与 ext 命令](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/commands.py#L279) |
| C11 | [SSH 分派](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/ssh.py#L706) |

## 全局参数

| 参数 | 基线语义 | Rust 要求 |
| --- | --- | --- |
| `--auth oauth2|adc` | 官方默认 oauth2，大小写不敏感 | 账户 provider 与显式覆盖的优先级应在上下文解析中统一 |
| `-c/--client-oauth-config` | 默认 `~/.colab-cli-oauth-config.json`，缺失时使用内嵌配置 | 保留配置入口，独立处理 OAuth client 与 redirect 的配对 |
| `--config` | 自定义 sessions.json 路径 | 不得被账户默认路径悄悄覆盖；相对路径传给 SSH 子进程前转绝对路径 |
| `--logtostderr` | 日志输出到 stderr | 不把敏感字段、SSH 字节流和业务 stdout 混在一起 |
| `-a/--account` | 扩展支持名称或邮箱 | 支持顶层及扩展现有入口；`--` 后属于用户脚本的参数不得被截取 |
| `COLAB_ACCOUNT` | 当前 shell 的账户覆盖 | 与持久 active profile 分开，不因一次调用修改 active |

根入口支持 --show-completion 和显式 --install-completion，范围见 [35](35-completion-and-drive-status-implementation.md)。

所有命令的 `-h/--help` 和 `help [COMMAND]` 均为兼容范围。合并版运行时命令树、参数与 help 已采集，见 [33](33-merged-help-and-aliases-implementation.md)；错误解析与完整业务 fixtures 仍待补齐，不能把静态 AST 当作完整 Click/Typer 语法。

## 会话和执行能力

| 命令 | 必需行为 | 关键边界 |
| --- | --- | --- |
| `new` | 创建 CPU/GPU/TPU；`-s`、`--gpu`、`--tpu`、`--high-mem` | 名称缺省随机；创建仅使用所选账户，不按旧会话名路由 |
| `sessions` | 显示本地与后端会话，识别后端孤立 assignment | 扩展增加 `--all [--json]`；单账户与全账户 JSON 需分别采样 |
| `status` | 显示会话与硬件状态 | 本地记录不等于后端仍存活，需刷新/裁剪 |
| `stop` | 关闭 kernel、释放 VM、更新本地历史与状态 | 失败不得误报“已释放”；恢复策略见架构文档 |
| `restart-kernel` | 重启所选 kernel | 重启中应结束正在等待的执行，防止挂起 |
| `url` | 输出可连接已有 VM 的浏览器 URL；支持 `--host`、`--open` | 扩展添加账户 authuser；stdout 必须保持单行 URL |
| `exec` | 从 stdin、`.py` 或 `.ipynb` 执行 | `-f`、`--env`、`--timeout`、`--output-image` |
| `repl` | 交互多行输入与非 TTY 管道执行 | EOF 干净退出；交互错误不中断整个 REPL |
| `run` | 新建 VM、执行本地脚本、退出时释放；支持 `--keep` | 脚本参数、环境变量、SystemExit、异常清理 |
| `ssh` | 交互 shell、一次性命令、ProxyCommand 桥接 | 自动创建矩阵与 `--rm` 语义必须按模式区分 |

GPU 接受 T4/L4/G4/H100/A100，TPU 接受 v5e1/v6e1，按大小写不敏感解析。基线 `resolve_runtime_options` 同时收到 gpu/tpu 时优先 TPU；Rust 已按兼容对齐要求采用同一 TPU 优先规则。L4/V5E1/V6E1 的高内存参数省略，其他支持选择的型号发送 `shape=hm`。这些是客户端映射，不保证账户有权限获得该硬件。[C02]

### exec 具体契约

1. `--timeout` 缺省 30 秒，是本次消息的空闲超时；0 或非正值表示无限等待的底层语义，负数的 CLI 接受情况需 fixture 确认。
2. `--env KEY=VALUE` 可重复，按第一个等号拆分，值可包含等号；非法变量先在本地拒绝。
3. 无文件且 stdin 为 TTY 时报告没有输入；空白输入返回成功。
4. 工作目录预处理为 `/content`。
5. notebook 只执行 code cells，保留其他 cell，补齐缺失的 cell ID。
6. 带输出 notebook 保存为 `<原文件名>_output.ipynb`；异常路径也进入保存 finally。
7. 保留图片输出、富文本转文本、stdout/stderr、traceback 与 output 类型。

基线 `execute_code` 将 Python 错误作为 output 返回，`exec` 命令没有像 `run` 那样统一根据 outputs 推导非零退出码；不能在文档中默认两者退出码相同。Rust 的文本 exec 已按官方返回 0，JSON 扩展保留错误返回 1；run 仍按脚本结果返回状态。[C03；runtime.py:260]

### run 具体契约

`run` 读取脚本后构造远端 `sys.argv`、`__name__='__main__'` 和环境变量，移除首行 shebang。应使用 JSON 数据传输或正确的 Python 字面量编码，禁止把脚本参数直接插入 Python 源码。

基线会从脚本参数中额外提取 `--env`，因此脚本本身名为 `--env` 的参数会产生歧义。Rust 需要保留兼容测试，或者仅在显式分隔符前解释 CLI 参数，并记录差异。[C04]

`SystemExit(None/0)` 返回 0，整数返回对应状态，字符串返回 1，其他 Python error 返回 1。传输异常也需失败，同时执行清理。`--keep` 保留 VM，断开本地客户端不删除 kernel。

README 将任务描述为包含产物获取，但基线 `run` 实际签名和执行路径没有通用的产物下载选项。自动下载如需增加，必须作为新增需求设计，不能声称已从上游完整移植。[C04]

## 文件 自动化与工具

| 命令 | 兼容内容 |
| --- | --- |
| `ls [PATH]` | 默认 content；目录优先、名称排序，目录加 `/` |
| `upload LOCAL REMOTE` | 先验证本地文件存在，Contents API base64 上传 |
| `download REMOTE LOCAL` | base64/text/json，目录下载拒绝 |
| `rm PATH` | 删除远端条目，记录历史 |
| `edit PATH` | 下载到临时文件，调用编辑器，仅内容变化后上传 |
| `auth` | 隐藏的 VM 内 GCP 授权；与 CLI 登录是两个层次 |
| `install [-r FILE] [PKG...]` | requirements 上传到 VM；优先 uv，失败回退 pip |
| `drivemount [PATH]` | 扩展默认改走已有凭据的 DriveFS；可 `--upstream` |
| `log` | 列出历史、按会话与事件过滤。正数 `--lines` 取末尾，负数跳过开头，零保留全部。支持导出 ipynb/md/txt/jsonl |
| `usage` | CCU 余额、速率；扩展 `--all [--json]` |
| `pay` | 打开订阅页面 |
| `version`、`update` | 已实现本地版本、原生清单检查、显式安装和离线一代回滚；GitHub 仓库已创建，发行物待发布，见 [32](32-native-update-and-release-implementation.md) |
| `whoami` | 隐藏的认证诊断；不得默认输出 token |
| `readme/README`、`skill/SKILL` | 内嵌文档，skill 含扩展业务说明 |

`edit` 基线把任何下载失败当作新空文件；Rust 设计仅将 404 作为新文件，认证/网络错误应中止，防止错误覆盖。[C05]

`install` 与 `drivemount` 的远端字符串构造需要数据化，不能照搬未经转义的包名和路径拼接。[官方自动化源码](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/commands/automation.py#L211)

## 扩展完整命令范围

| 命令组 | 子命令 | 语义 |
| --- | --- | --- |
| `account`，隐藏别名 `acct` | `list`（ls）、`current` | 离线列表与当前解析账户，支持 JSON |
| `account` | `add`、`login` | 默认认证、可选 Drive 联合授权；URL-only 两步流程及 configure-only 离线登记，见 [34](34-account-login-and-drive-consent-implementation.md) |
| `account` | `use`（switch）、`rename`、`remove`（rm） | 本地账户生命周期；remove 不等于停止 VM |
| `account` | `status` | 并发读取所有账户状态，单账户失败不拖垮整个结果 |
| `account` | `set-drive` | 指定账户或全局默认 Drive 绑定，支持 clear |
| `drive`，隐藏别名 `dfs` | `mount`、`umount`、`status` | 挂载、卸载与检查 |
| `drive` | `consent` | dfs/sdk/custom 授权，支持导入、两步流程和自定义参数 |
| `drive` | `show`、`token`、`clear` | 凭据元数据、显式获取 token、清除本地凭据 |
| `ext` | `doctor`、`version` | Rust 版提供内建能力与依赖诊断 |
| `ext` | `install` | Python `.pth` 修复不适用；Rust 兼容入口解释已内建，不操作 Python 环境 |

`drive` 隐藏的 install/doctor/version 与 `ext status` 是兼容入口，当前实现及只读诊断边界见 [26](26-extension-diagnostics-implementation.md)。`drive mount` 参数还包括 `--mode shim|rt`、`--port`、`--from-env`、`--cli-token`、`--user-id`、`--email`、`--drive-extra`、`--via-cli`、`--dry-run`。Rust 不再需要通过 Python CLI fallback 执行；兼容入口可映射为原生路径并说明差异。[C10]

Drive consent 的授权起始支持命名账户自动登记和所选存储账户邮箱提示，失败与完成阶段的创建边界见 [58](58-drive-consent-profile-implementation.md)。

文档中的 `connect` 示例不构成实现依据：固定 CLI 的命令注册中没有 connect；`new`/已有会话执行路径应作为真实入口。短参数 `-g` 也不能凭 README 示例默认存在，需以 help 及 AST 参数为准。[C01、C02]

## 输入输出和非交互规则

- `--json` 结果应只写 stdout；进度、账户路由、更新提示和警告写 stderr。
- SSH proxy stdout 必须逐字节透传，不能打印横幅、账户提示或换行。
- `url`、`version`、`readme`、`skill` 等信息命令应避免每日更新提示污染输出。
- 列表和汇总命令不得临时弹出 OAuth 等待；报告需要授权，并继续处理其他账户。
- Agent 自动化挂载使用 `--no-fallback`，缺少凭据直接失败；普通交互挂载可按现有规则回退官方流程。
- `--dry-run` 不分配 VM、不写入远端、不输出 refresh token 或含凭据的完整 payload。完整 payload 的调试输出应显式选择且对敏感字段脱敏。
- Rust 可新增统一 `--non-interactive` 和结构化错误，但这属于设计扩展，必须在 help 和版本变更中说明。

## 兼容等级

协议字段、数据 schema、参数转发与 SSH 字节流是严格契约。可供脚本消费的 JSON、URL、退出码需要逐项 fixture。彩色 help、进度文案和终端布局允许改变；保留清晰语义即可。涉及风险修复的差异集中记录在 [验收文档](06-validation-and-open-questions.md)，避免一边宣称逐字节兼容，一边悄悄改变行为。

本章已覆盖命令分组与核心语义；运行时合并 help、Typer 自动短选项和各错误路径的精确文案仍待离线采集，不应据本章宣称 CLI 解析兼容已经通过。

## 生命周期职责边界

客户端继续实现固定上游的 WebSocket 通信，包括握手、消息收发、输出流、协议心跳、交互输入、专用消息、断线检测与状态核对。当前已实现范围见 [12](12-jupyter-and-execution-implementation.md)；其余协议能力仍需开发和验收。连接重建不代表代码可以重放，执行结果未知时保留恢复线索并返回明确状态。

持续保活策略、自动申请替代 VM、任务重试和检查点恢复由任务脚本或外部调度器决定。原 `watch`、`supervisor run`、`new --supervise --restart-on-loss` 草案退出本项目当前范围；`run --keep` 仍仅表示跳过本次 VM 自动释放。分配/释放事务恢复、`run-cleanup` 和账户事务恢复继续属于 CLI。职责划分及历史草案见 [08](08-supervision-and-recovery.md)。

外部身份 ADC 已接入文件/URL 主体与 STS、可选 IAM，复用账户登录与计算命令；范围和失败恢复见 [36](36-adc-external-account-implementation.md)。

Workforce 用户 external_account_authorized_user 的原生 STS Basic 刷新与支持边界见 [37](37-adc-workforce-user-implementation.md)。

metadata ADC 作为默认 gcloud 文件缺失后的最后发现源，显式或坏文件不降级；实现、环境快照和平台限制见 [38](38-adc-metadata-implementation.md)。

Unix external_account 可执行来源现支持显式 opt-in、非交互执行和输出缓存，见 [39](39-adc-executable-implementation.md)；不会在 doctor 中执行。

后续已接入 AWS aws1 的环境/EC2 与 SigV4 主体，见 [40](40-adc-aws-implementation.md)；并未验证真实云端接受性。
