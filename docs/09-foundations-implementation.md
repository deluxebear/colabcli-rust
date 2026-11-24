# Rust 基础模块实现状态

更新日期：2026-11-17。本文记录实际代码与离线验证结果；01–08 仍是完整产品的研究与目标规格。当前交付包含 P1 基础模块、P2 控制面/Contents/汇总与 P3 执行核心、run 与历史导出，不代表全部 ADC 类型、线上验收或完整客户端已经完成。

## 1. 已实现范围

| 模块 | 当前能力 | 代码 |
| --- | --- | --- |
| CLI | `version`；`account`/`acct` 下的 list、current、status、add、use、set-drive、rename、remove、login、repair | [main.rs](../src/main.rs) |
| 账户 | 注册表、名称/email 选择、active、全局及逐账户 Drive 绑定、旧配置复制、事务恢复 | [accounts.rs](../src/accounts.rs) |
| 上下文 | 显式参数、环境、会话归属、active 的优先级；Drive 独立解析；ADC 路径解析 | [context.rs](../src/context.rs) |
| 认证 | OAuth 两步授权、state/PKCE、刷新与轮换、失败暂存恢复；authorized_user/service_account/impersonation ADC | [auth.rs](../src/auth.rs) |
| 凭据 | `auto`/`keyring`/`file`、旧 keyring 惰性迁移、系统后端等待期限 | [credentials.rs](../src/credentials.rs) |
| 脚本与历史 | run、信号取消、清理计划恢复；log 过滤与四种导出 | [runs.rs](../src/runs.rs)、[history.rs](../src/history.rs) |
| 执行 | Jupyter REST/DEFAULT WS、exec Python/Notebook、kernel 复用与显式恢复 | [jupyter](../src/jupyter/mod.rs)、[execution.rs](../src/execution.rs) |
| Drive 凭据 | 原生 consent/import/show/token/clear、PKCE、独立账户、脱敏与刷新恢复 | [drive_credentials.rs](../src/drive_credentials.rs) |
| DriveFS 组件 | 远端 metadata、挂载控制器、私密输入、mount/umount 编排与轮换回收 | [drivefs.rs](../src/drivefs.rs) |
| DriveFS 核查 | drive status/probe-cleanup、独立只读 kernel、未知执行旁核查、冻结/采用/finalize 与恢复租约 | [probe.rs](../src/drivefs/probe.rs) |
| SSH | Unix 交互/命令/proxy、WS 字节桥接、系统 OpenSSH、信号与清理恢复 | [ssh.rs](../src/ssh.rs) |
| VM 自动化 | auth、官方 drivemount、install、dfs_ephemeral 凭据传播与人工确认 | [automation.rs](../src/automation.rs) |
| 文件 | `ls/upload/download/rm/edit`、限额、路径编码、下载原子替换、编辑失败恢复 | [contents.rs](../src/contents.rs) |
| 汇总 | `status/sessions --all`、`usage --all`，最多四账户并发与逐账户错误 | [inventory.rs](../src/inventory.rs) |
| 控制面 | `new`、`status`/`sessions`、`stop`、`usage`；逐账户分配/释放日志与故障恢复 | [colab_api.rs](../src/colab_api.rs)、[sessions.rs](../src/sessions.rs) |
| 存储 | JSON 模型、未知字段保留、受限权限、原子写入、进程锁、会话与设置读改写 | [storage.rs](../src/storage.rs)、[models.rs](../src/models.rs) |

命令行为有明确的阶段差异：`account add` 默认登录、尝试 Drive 授权并切换 active；离线登记使用 --configure-only，见 [34](34-account-login-and-drive-consent-implementation.md)；`list --json` 输出账户摘要数组，列表及账户写回保持文件顺序，见 [41](41-account-inventory-implementation.md) 和 [42](42-account-mutations-implementation.md)；业务错误通常退出 1，参数错误退出 2；文本 exec 的 Python 错误沿用官方退出 0，JSON 扩展仍退出 1，run 保留脚本退出码。官方 CLI 离线差分见 [14](14-cli-comparison.md)，合并版 JSON 的逐阶段契约及保留差异见 41–44，Rust 专属详情另行说明。Drive 绑定只写入配置，不执行挂载。

账户名限 64 字符以内的 ASCII，首字符为字母或数字，后续允许字母、数字、`.`、`_`、`@`、`-`，禁止 `..`；忽略大小写后不能重名。email 可作为选择器，多个账户匹配时明确报错。

## 2. 本地使用

```sh
cargo build --locked
cargo run -- --config-dir /tmp/colab-rust-demo account add work --configure-only
cargo run -- --config-dir /tmp/colab-rust-demo account use work
cargo run -- --config-dir /tmp/colab-rust-demo account list --json
cargo run -- --config-dir /tmp/colab-rust-demo account add storage --configure-only
cargo run -- --config-dir /tmp/colab-rust-demo -a work account set-drive storage
cargo run -- --config-dir /tmp/colab-rust-demo account set-drive storage --default
cargo run -- --config-dir /tmp/colab-rust-demo account set-drive --default --clear
```

配置目录优先 `--config-dir` / `COLAB_CLI_CONFIG_DIR`，否则为 `~/.config/colab-cli`。账户解析优先显式参数、`COLAB_ACCOUNT`、适用的会话归属、active；同名会话优先 active，多账户歧义报错。Drive 环境变量为 `COLAB_DRIVE_ACCOUNT`，兼容 `COLAB_DFS_ACCOUNT`。会话路由已接入 status/stop/exec，也识别待恢复操作；drivemount 默认接入独立 DriveFS，显式 --upstream 选择官方流程，见 [28](28-drivemount-strategy-implementation.md)。

## 3. 存储与恢复约定

配置根目录包含 `accounts.json`、`settings.json`；逐账户文件在 `accounts/<name>/`，包括 `sessions.json`、`control_operation.json`（仅有待恢复资源操作时存在）、`oauth_token.json`、授权 pending 和复制的历史/Drive 凭据。默认账户初始化会非破坏性复制旧布局，保留源文件，不覆盖已存在的目标文件。未知 JSON 字段保留；损坏 JSON、未来注册表版本直接报错，避免覆盖原件。

Unix 目录为 0700、写入文件为 0600，JSON 写入采用同目录临时文件、fsync 和原子替换。锁默认等待 5 秒；账户生命周期锁放在独立 `locks/` 目录，避免重命名目录导致两个锁域。路径检查拒绝目录穿越与配置树内符号链接，但未声称能抵御同用户恶意进程的全部文件系统竞争。

重命名和删除通过 `account-transaction.json` 记录进度。中断时拒绝后续普通账户操作，运行以下命令继续完成事务：

```sh
cargo run -- --config-dir /absolute/path/config account repair
```

重命名保留源目录直至目标凭据与注册表提交，更新 active、默认 Drive 和所有绑定。旧认证上下文与会话写入器不能重新创建已删除/改名的资料。删除仅处理本地状态：已有 sessions 需 `--force`，待恢复控制面操作或非空 managed 目录即使 force 也拒绝，保留未知状态保护，需显式处置，不能手删日志或忽略目录。

Python 与 Rust 的共同写锁兼容性尚未建立，**禁止同时修改同一配置目录**。Rust 授权 pending 与事务日志为本项目格式，不承诺恢复 Python 的未完成授权。

## 4. 凭据与认证

系统 keyring 使用 service `google-colab-cli`、username `oauth_token:<profile>`；旧 `oauth_token` 在 default 需要认证时惰性导入并保留源值。列表和注册不访问系统凭据库。显式 file 模式的旧凭据迁移只读取旧文件，不访问或导入旧 keyring；新建 file 模式 rename/remove 只处理文件；旧事务和 auto/keyring 仍保持跨后端清理保护。

`auto` 优先尝试系统后端；写入失败后保存私有权限文件和 `oauth_keyring_fallback.json` 标记，之后继续使用文件，避免后端超时但延迟完成的写入覆盖最新凭据。文件内容未加密；显式 `keyring` 成功写入后才清除文件和标记。原生调用的调用方等待期限默认 30 秒，可用全局 `--keyring-timeout 1–300` 或 `COLAB_KEYRING_TIMEOUT` 调整（整数秒）；无法取消 OS 操作，未结束前拒绝同一后端的新调用。等待超时、无访问权限、凭据条目歧义/编码错误及未编译原生后端分别给出脱敏原因，禁止打印 keyring 原始错误内容。

新建 file 重命名/删除不访问 keyring，事务日志记录 file 策略；auto/keyring 以及没有策略字段的旧日志仍严格检查和清理 keyring。关闭 `native-keyring` 特性不会跳过旧事务的保护。前置检查失败可修复后重试；若已留下事务日志则运行 repair。详见 [42](42-account-mutations-implementation.md)。

OAuth client 配置由用户提供，默认 `~/.colab-cli-oauth-config.json`，或 `-c /absolute/path/client.json`，支持 installed/web/直接字段对象。client 与默认 SDK redirect 的配对仍需 Google 线上验证。例：

```sh
cargo run -- -a work -c /absolute/path/client.json account login --url-only
cargo run -- -a work account login --code 'AUTHORIZATION_CODE'
```

两步流程绑定账户、client、redirect、state、PKCE 和 15 分钟有效期。`--callback-url` 校验完整回调与 state；裸授权码流程无法验证返回 state，依靠 pending 绑定与 PKCE。暂无自动监听回调或浏览器启动。授权码交换开始后即禁止重放；网络结果未知时须重新授权。成功响应在凭据提交前暂存，存储失败可再次 finish，无需重复兑换。

访问 token 到期前 60 秒刷新，以逐账户进程锁合并并发刷新；保存轮换后的 refresh token 和实际 scopes，不主动扩大 scopes。刷新结果也会暂存，存储失败可重试提交。HTTP 默认总超时 20 秒、连接超时 5 秒，禁止跟随重定向，限制响应体大小；错误不输出 token 响应正文。

ADC 路径顺序为 `GOOGLE_APPLICATION_CREDENTIALS`、账户指定文件、`CLOUDSDK_CONFIG`、用户 gcloud 默认位置；建议使用绝对路径。当前支持 **authorized_user、service_account、impersonated_service_account、文件/URL external_account 与 external_account_authorized_user**，每个 provider 独立持有凭据与刷新缓存，不修改进程环境，不回写 gcloud 文件。ADC 刷新返回的新凭据只在 provider 生命周期内缓存。service_account 的 JWT 交换与测试见 [30](30-adc-service-account-implementation.md)；IAM impersonation 的来源与目标缓存见 [31](31-adc-impersonation-implementation.md)；外部身份 STS 与可选 IAM 见 [36](36-adc-external-account-implementation.md)；证书来源、递归 impersonation、metadata mTLS 和 domain-wide subject 尚未实现；metadata 最后发现路径见 [38](38-adc-metadata-implementation.md)。成功取 token 不代表已确认 Google 身份或 Colab 权限。

## 5. 验证结果

在本机 macOS、Rust 1.99.1 执行：

| 检查 | 结果 |
| --- | --- |
| `cargo test --locked` | 225 项通过：账户 17、认证 17、CLI 6、兼容格式 3、控制面 27、Contents 17、凭据 5、汇总 3、Jupyter 57、SSH 21、Drive 凭据 16、历史 10、run 7、存储 14、终端输入 2、REPL 高亮 1、原生后端逻辑 2 |
| `cargo test --locked --no-default-features` | 223 项通过；不包含两项原生后端等待与错误脱敏测试 |
| `cargo fmt --check` | 通过 |
| 默认及 no-default-features 的 `cargo clippy --locked --all-targets -- -D warnings` | 均通过 |
| `cargo build --locked` | 通过 |

测试使用临时目录、注入的内存凭据库和 loopback HTTP；覆盖 8 个 CLI 子进程并发写入、真实 CLI 两步兑换、账户与 ADC 隔离、PKCE 报文、刷新并发、迁移中断和凭据写入失败。测试入口见 [accounts](../tests/accounts.rs)、[auth](../tests/auth.rs)、[cli](../tests/cli.rs)、[storage](../tests/storage.rs)、[control_plane](../tests/control_plane.rs)、[contents](../tests/contents.rs)、[inventory](../tests/inventory.rs)、[jupyter](../tests/jupyter.rs)、[history](../tests/history.rs)、[runs](../tests/runs.rs)。原生后端逻辑测试使用注入闭包，未读写真实系统凭据库。

[CI](../.github/workflows/ci.yml) 已配置 Linux/macOS，尚未远程执行；Rust 1.85 最低版本尚未实测。基础阶段的回归使用合成凭据。后续单次真实 CPU 验收见 [15](15-live-cpu-validation.md)；新 OAuth 授权、跨工具钥匙串互通与其他账户权限仍待验证。

## 6. 后续接入边界

控制面 HTTP、会话查询/分配/释放与用量现已接入，详细行为、操作日志恢复与兼容差异见 [10](10-control-plane-implementation.md)。Contents 和有界账户汇总见 [11](11-contents-and-inventory-implementation.md)；P2 的单次真实 CPU/文件往返/释放验收见 [15](15-live-cpu-validation.md)。Jupyter 连接与非交互 exec 见 [12](12-jupyter-and-execution-implementation.md)。run、历史与导出见 [13](13-run-and-history-implementation.md)。显式 exec 终端 stdin 与 REPL 已接入，见 [18](18-repl-and-websocket-implementation.md)；Unix SSH 已见 [19](19-ssh-implementation.md)，扩展 DriveFS 仍需开发；Colab dfs_ephemeral 专用消息见 [16](16-colab-automation-implementation.md)。持续保活策略、自动重建和任务检查点恢复移至外部脚本或调度器，原 supervisor 方案归档见 [08](08-supervision-and-recovery.md)。完整验收编号继续以 [06](06-validation-and-open-questions.md) 为准，不能因本阶段离线通过而整体勾选完成。

## 7. sessions 凭据排障（2026-11-16）

本机 `target/debug/colab sessions` 已复现为系统钥匙串读取超过当时默认的两秒（现已调整为 30 秒），尚未发出控制面请求。当前账户没有文件凭据，默认 `~/.colab-cli-oauth-config.json` 也未配置；这不能证明钥匙串里没有旧凭据，因此 auto 读取失败仍报凭据库故障，不伪装为未登录。

先解锁/允许钥匙串访问，再运行 `./colab sessions`（默认等待 30 秒，也可通过 `--keyring-timeout` 覆盖）。若主动选择文件后端，需先以 `./colab --token-storage file -c /absolute/path/client.json account login --url-only` 发起授权，随后同一账户/配置执行 `./colab --token-storage file account login --code AUTHORIZATION_CODE`，再 `./colab --token-storage file sessions`。file 不会解锁或导出钥匙串凭据；没有登录时 sessions 会提示需要登录与 client 配置路径。不要在聊天或日志中分享授权码、client secret 或 token。

新增 [credentials](../tests/credentials.rs) 五项回归测试，覆盖 file 迁移隔离、旧文件迁移、auto 不将不可访问当作未登录、文件凭据优先及 CLI 参数/登录提示；原生后端另覆盖超时类别与敏感错误安全映射。file 迁移隔离测试修复前明确失败、修复后通过。

## 8. 分配恢复与会话显示兼容修复（2026-11-16）

移除准备响应 acc/nbh/variant 与请求逐字匹配的额外校验；保留响应类型、variant 枚举和非空 token 校验，POST 使用原请求参数与返回的 token。该额外校验是 `new` 报“分配准备信息与请求不匹配”的原因，与固定 Python 上游仅消费准备 token 的逻辑不符。

普通分配失败且尚未暂存 endpoint 时，允许相同硬件参数改用新名称恢复，例如无名 `new` 失败后执行 `new -s test`。日志先原子保存新名称，始终保留原 nbh；已有同名会话、已暂存 endpoint、run 所有者及不同硬件参数仍受保护。不要删除现有 control_operation.json 来解除阻塞。

`sessions` 与 `status` 拆为独立命令：前者包含后端未命名会话，以 `?` 显示；后者仅显示本地命名会话，增加状态和上次执行信息。补齐原版空列表提示、CPU 与 Standard/High-RAM 标签；JSON 保留安全字段，不输出凭据。`--all` 仍是本项目的多账户扩展。详见 [10](10-control-plane-implementation.md)。

新增六项离线回归测试，覆盖非回显准备元数据、无名失败后命名恢复、恢复再次失败后的 nbh 保留、已有会话保护、已暂存/有所有者操作保护及真实 CLI sessions/status 文本差分。只使用临时配置和 loopback HTTP，未创建真实 VM；完整线上兼容仍待验收。

## 9. 空 POST 的 HTTP 411 修复（2026-11-16）

控制面 assign/unassign 的 POST 原先未显式声明 Content-Length；严格 HTTP 前端可因此拒绝请求并返回 411。已用要求 `Content-Length: 0` 的 loopback 服务复现分配失败，再在共享控制面请求层为两种空 POST 显式发送该头。query、XSRF token 与空请求体契约保持一致，不用 JSON `{}` 替代空 body。

新增一项测试同时覆盖分配和释放，确认长度为零、没有 Transfer-Encoding、请求体为空，且分配成功响应和释放 204 均正确处理。修复前测试收到 411，修复后通过。原操作日志及 nbh 无需清除，重新构建后可继续 `new -s test`；本次未发出真实 Google 分配或释放请求。

## 10. 已实现命令的自动对比（2026-11-16）

新增临时配置与 loopback HTTP/WebSocket 驱动的官方/Rust 差分工具，覆盖 root help 的所有已实现入口、account 子命令及 acct 别名，并可运行完整 Rust 回归。官方源码与 KERNEL 逐文件核验固定基线；没有导入 colab-ext，没有使用真实凭据或创建 VM。测试方式、扩展命令边界和已确认差异见 [14](14-cli-comparison.md)，逐场景观察值见 [完整报告](../research/cli-comparison.json)。测试通过只说明场景断言通过，报告保留已有的退出码、格式、默认参数及请求时序差异。

## 11. 官方命令兼容对齐（2026-11-16）

共有命令的文本提示、参数与导出按隔离执行的官方样本对齐：new 使用六位十六进制名称、run 使用 run-加六位名称；GPU/TPU 同时存在时优先 TPU，裸硬件选项报参数错误。stop 可自动选择唯一会话，未知名称提示并返回 0。文件命令、usage、日志与 Notebook 进度使用官方格式；文本 exec 的 Python 错误返回 0，JSON 模式继续以非零标记失败。

Notebook 文本模式与历史导出遵循 nbformat 的多行字段和 execution_count 表示；默认历史导出不带内部 account/session/event_id，原始脱敏元数据保留在持久化及 log --json，原增强库导出接口也保留。新增三项官方格式 golden 样本与脱敏测试。

文本执行结果未知时清除旧 running 表示，另存 execution_pending，并继续阻止代码自动重放；restart 确认成功后清除。新增真实协议回归确认未知结果不会被重复执行。正常 VM 释放直接 GET/POST unassign；释放意图在资源请求前落盘 attempted=true，旧日志默认按可能已请求处理，恢复时仍先查 assignments。run 依靠整个 VM 释放清理 kernel，保持所有者与 endpoint 校验。

对比工具继续保留原始观察值；自动名称格式先断言，再归一化随机值，Notebook cell ID 与 Jupyter session ID 仅在比较时消除随机差异。help 的框架布局、独立包版本号、Python 异常堆栈与 Rust 脱敏错误展示不要求伪造一致，详见 [14](14-cli-comparison.md)。


## 12. 单次真实 CPU 验收（2026-11-16）

用户授权后创建一个 CPU VM，完成 33 项真实会话、文件、执行、绘图、内核、历史与导出检查，再释放本次资源并查询确认消失。线上发现 Contents 与图片 MIME 的 base64 带换行，已修复并补充失败先行的回归。当时默认测试增至 157 项、无默认特性 155 项；范围、输入、失败与清理证据见 [15](15-live-cpu-validation.md)。


## 13. 生命周期范围调整（2026-11-16）

按用户确认，原版 WebSocket 通信、协议心跳、交互与专用消息、断线状态核对，以及凭据刷新和资源清理恢复仍属于 CLI。持续保活策略、自动重建和任务检查点恢复由任务脚本或外部调度器决定；P7 和相关命令草案退出当前路线图，08 保留为历史参考。此变更仅调整文档，不改变已实现的 new/stop/run-cleanup/repair 行为或持久化保护。


## 14. WebSocket 终端输入（2026-11-16）

新增可轮询 InputHandler、stdin 请求/回复路由与 exec --allow-stdin Unix 终端入口。等待输入期间继续处理心跳、期限和终止信号；密码关闭回显，受控返回恢复终端，回复不额外写入历史。新增四项 Jupyter/CLI 集成测试及两项伪终端单元测试，覆盖 Unicode、连续普通/密码请求、无关 parent、PING/PONG、期限、取消、错误脱敏、非 TTY 拒绝、完整 CLI JSON 与持久化隔离。当时默认 163 项、无默认特性 161 项通过。该新增输入路径尚未真实 Colab 验收；Colab 专用消息、REPL 和 SSH 仍未完成。详细约定见 [12](12-jupyter-and-execution-implementation.md)。


## 15. Colab 专用请求与自动化（2026-11-16）

新增 auth、官方 drivemount、dfs_ephemeral 非阻塞处理器与控制面凭据传播，接入人工确认、事件历史和绑定 endpoint 的账户隔离。新增六项测试，扩展已有伪终端测试到三个命令，当时累计默认 169 项、无默认特性 167 项。尚未做真实授权或 Drive 挂载；当时发现的独立 kernel 差异现已对齐，见 [16](16-colab-automation-implementation.md)。


## 16. 独立自动化 kernel 与恢复（2026-11-16）

auth/drivemount 已对齐原版的独立 kernel 生命周期，不覆盖普通 exec 的 ID/session。增加六阶段持久意图、严格的未执行连接恢复、未知/已完成执行禁止重放、automation-cleanup 显式清理及状态展示。新增六项协议/故障/并发测试，当时默认回归 175 项、无默认特性 173 项；四个此前不匹配的官方自动化场景已匹配。恢复语义与仍未完成的真实授权/挂载验收见 [16](16-colab-automation-implementation.md)。


## 17. install 安装入口（2026-11-16）

新增 install、requirements 预检与同 VM 上传、JSON 参数向量、uv/pip 回退、独立 automation kernel、含文件内容的恢复指纹和省略私有参数的源码历史。新增五项 Rust 协议/CLI 回归，累计默认 180 项、无默认特性 178 项；实际 Python 模板另有七项离线替身检查。没有真实安装包，详情与验收边界见 [17](17-install-implementation.md)。

## REPL 与协议心跳

新增 repl 的管道和交互入口，持久 kernel/WS、内存行历史、中文、多行快捷键、流式输出与图片保存。WebSocket 默认主动 PING 间隔 60 秒，等待编辑时持续轮询；中断需 REST 请求和 execute_reply/idle 双重确认，未知结果继续保留防重放状态。新增七项 Jupyter 协议/CLI 测试与一项高亮测试；行为、平台限制和真实验收缺口见 [18](18-repl-and-websocket-implementation.md)。

## SSH 与资源清理

新增 Unix ssh 的交互、一次性命令和 proxy 三种入口，系统 OpenSSH 及公钥导出、严格握手 query/header、非阻塞字节桥接、精确 argv 与 ProxyCommand 转义。按模式区分自动创建和 --rm；创建使用独立 ssh_owner，公钥/握手失败也进入 endpoint 绑定清理，释放失败保留已有事务日志。新增 21 项协议/CLI 测试及 23 个差分/扩展场景；完整约定、修复差异和未验收范围见 [19](19-ssh-implementation.md)。

## DriveFS 凭据与授权

新增 drive/dfs 的 consent、import、show、token、clear，隔离计算与存储账户，支持自带 client、URL 两步及 loopback、PKCE/state、授权码不回显和刷新轮换恢复。账户重命名同步迁移 Drive 刷新暂存。新增 16 项测试，涵盖失败恢复、并发、身份与凭据隔离；详细约定和来源/权限验证边界见 [20](20-drive-credentials-implementation.md)。远端 shim 部署、DriveFS 挂载、只读核查与安全卸载继续保留在完整目标中，不能因凭据模块通过而宣称 P5 完成。

## DriveFS metadata 组件

新增独立实现的远端 Python 标准库 shim，由 Rust 内嵌打包；包含四类 metadata GET、串行缓存刷新、禁止重定向、固定脱敏错误、私有配置与轮换持久化。新增 3 项 Rust 配置测试和 11 项脚本离线测试，后者加入 CI。组件尚未接入 CLI 或远端部署，挂载/核查/安全卸载仍待实现；输入、清理责任、轮换故障窗口及验证边界见 [21](21-drivefs-metadata-implementation.md)。

## DriveFS 挂载控制器与私密输入

新增 per-operation 私有目录/HOME、挂载 ID 与进程启动身份核查、只读状态、已知所属挂载的卸载清理，以及独立 automation kernel 的一次性密码输入适配。未知执行不重放，卸载失败不删除挂载点或凭据。新增 4 项 Rust 适配测试、1 项 WebSocket 成功/断线测试；脚本另有 27 项控制器/包装器测试，本机 26 通过、1 项 Linux 专属跳过。仍需 CLI 本地计划、结果验证、未知执行下的只读 probe journal、轮换凭据回收和真实挂载；详细阶段、失败与清理规则见 [22](22-drivefs-controller-implementation.md)。

## DriveFS 只读 CLI 与凭据回收协议

接入 drive/dfs status 和 probe-cleanup，使用独立只读 kernel/journal，不加载 Drive 凭据、不修改未知 mutation 或用户执行；创建未知时拒绝再分配，已知目标支持删除失败恢复。新增严格结果边界与脱敏展示，待恢复 probe 阻止账户重命名和强制删除。远端 shim 卸载改为冻结 credentials_pending、本地 CAS 采用、带摘要确认的 finalizing/finished 两步清理 API，保留 token 轮换和崩溃恢复责任。新增 11 项 Rust 测试及两项脚本回收用例；挂载/卸载 CLI、本地计划和跨账户租约仍未接入，见 [23](23-drivefs-probe-and-recovery.md)。

## DriveFS 挂载编排与跨账户租约

接入 drive/dfs mount、umount、leases 和 lease-cleanup；持久保存挂载计划与跨账户租约，未知执行先独立核查，仅清理精确归属的 kernel，避免重发挂载或凭据。卸载自动完成私有 Contents 回收、CAS 采用与带摘要的 finalize；force 替换先回收旧挂载。活动租约保护凭据更新及两端账户生命周期，stop 确认 VM 释放后精确清理，失败保留恢复记录。DriveFS 原始输出不落历史。实现与验证边界见 [24](24-drivefs-lifecycle-implementation.md)，尚未做真实 Drive 挂载验收。

最终离线回归：默认特性 252 项、无默认特性 250 项 Rust 测试通过；两种特性 Clippy、fmt 和默认构建通过。远端脚本 40 项通过、1 项 Linux 专属跳过。完整 CLI 差分 214 个场景为 88 一致、39 保留差异、87 Rust 扩展，无断言失败；root/account/drive 清单一致，报告二进制哈希与当前 debug 文件相符。

## DriveFS 来源与附加参数

接入 --from-env、--cli-token、--user-id/--email、--drive-extra 与 --via-cli。挂载源在独立 Drive 私有存储中持久化，以来源指纹保留回收后的轮换 token；活动租约拒绝来源或身份改写，预览不保存新配置，计算 OAuth 文件不改写。额外 argv 在两端校验，禁止覆盖受管认证和路径；--via-cli 使用 Rust 原生传输。行为、测试及实验边界见 [25](25-drivefs-inputs-implementation.md)。

最终回归：默认特性 257 项、无默认特性 255 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。远端脚本 41 项通过、1 项 Linux 专属跳过。完整 CLI 差分 221 个场景为 88 一致、39 保留差异、94 Rust 扩展，无断言失败；root/account/drive 清单一致，报告二进制哈希与当前 debug 文件相符。

## 内建扩展诊断与兼容入口

接入 ext doctor/version/install、status 别名和隐藏 drive/dfs 对应入口。doctor 使用已有目录的只读快照，在 bootstrap 之前检查所选账户、认证文件和恢复责任，不查询钥匙串/Google、不恢复日志、不创建锁或改变权限；仅展示白名单字段。version/install 不读取或创建配置，Python 注入按内建/不适用处理。测试与行为边界见 [26](26-extension-diagnostics-implementation.md)。

最终验证：默认特性 263 项、无默认特性 261 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 240 个场景为 88 一致、39 保留差异、113 Rust 扩展，无断言失败；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

## URL、身份诊断与内嵌资源

接入只读 url、显式浏览器打开、pay/--url-only、隐藏 whoami 与 JSON、readme/README、skill/SKILL。URL 保留官方双重后端信号和扩展 authuser，只读取本地会话且不输出 proxy token。whoami 使用带锁的强制刷新与白名单 tokeninfo，错误脱敏；OAuth 邮箱回填核对当前 bearer，ADC 来源独立。README 与 Agent 指引编译时内嵌，无本地 Python/网络依赖。协议、测试及保留差异见 [27](27-utility-commands-implementation.md)。

最终验证：默认特性 273 项、无默认特性 271 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 262 个场景为 92 一致、53 保留差异、117 Rust 扩展，无断言失败，原有 240 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

## 24. 根挂载策略

根 drivemount 默认复用 DriveFS 挂载编排；显式 --upstream/环境关闭开关选择官方策略。只有无凭据、无恢复责任、同计算账户且没有专用参数的交互终端可自动回退，--no-fallback 禁止这一回退。两条路径共享 endpoint 操作锁，官方执行前重新核查归属与相交路径的计划/挂载/租约，持锁直到执行结束。发送后失败或未知结果绝不切换策略；默认 timeout 为 30 秒。测试与限制见 [28](28-drivemount-strategy-implementation.md)。

最终验证：默认特性 278 项、无默认特性 276 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 270 个场景为 92 一致、53 保留差异、125 Rust 扩展，无断言失败，原有 262 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

## 25. 账户联合状态

接入 account status/acct status，最多四账户并发，结合 CCU、命名/孤立 assignment、有效 provider 与 Drive 绑定，支持扩展文本和扁平 JSON 行。单账户失败保留该行元数据与已取得余额，不发起交互式登录，不修改 active/绑定；仅成功的完整 assignment 查询同步本地会话。Drive 就绪字段只说明文件存在。账户隔离、ADC 来源与失败路径见 [29](29-account-status-implementation.md)。

最终验证：默认特性 282 项、无默认特性 280 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 277 个场景为 92 一致、53 保留差异、132 Rust 扩展，无断言失败，原有 270 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

## 26. ADC 服务账户

AdcProvider 保留公开接口并拆分到账户认证子模块；新增 service_account 未加密 PKCS#8 RSA 加载、ring RS256 签名、固定 Google audience 与公开 scope 的 JWT 交换。内存缓存按 provider 串行刷新，强制刷新失败保留仍有效的旧 token，原 authorized_user 刷新与轮换行为保持。源文件最多 64 KiB，不回写密钥、JWT 或 token；错误脱敏。doctor 仅报告基本配置存在，不签名或联网。协议、密钥与缓存边界及 CLI 测试见 [30](30-adc-service-account-implementation.md)。

最终验证：默认特性 287 项、无默认特性 285 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 281 个场景为 92 一致、53 保留差异、136 Rust 扩展，无断言失败，原有 277 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

## 27. ADC IAM impersonation

新增 impersonated_service_account 内嵌来源（authorized_user/service_account）、IAM 目标与 delegates 校验、JSON token 交换和 RFC3339 到期解析；源/目标缓存独立，源轮换只在内存采用。目标强制刷新失败保留仍有效的旧目标，但本次显式刷新仍返回错误，不回退源身份。doctor 仅识别基本配置，不联网或展开来源。并发、失败恢复、凭据隔离和 CLI 全流程见 [31](31-adc-impersonation-implementation.md)。

最终验证：默认特性 294 项、无默认特性 292 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 287 个场景为 92 一致、53 保留差异、142 Rust 扩展，无断言失败，原有 281 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

## 28. 原生更新与发行候选

接入独立 update 入口，显式发行源、默认 30 秒请求、SemVer 检查、长度/SHA-256/架构/版本自检、原子替换和一代离线回滚；私有操作锁和 Prepared/Committed/RollingBack 日志隔离安装，外部修改拒绝覆盖。渠道已选择 GitHub Releases，离线工具与四平台候选 workflow 已配置，公开仓库已创建并关联 origin，尚未推送代码或真实发布。测试及失败清理责任见 [32](32-native-update-and-release-implementation.md)。

最终验证：默认特性 309 项、无默认特性 307 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 298 个场景为 92 一致、54 保留差异、152 Rust 扩展，无断言失败，原有 287 场景分类不变；root/account/drive/ext 清单一致。离线打包工具 8 项测试通过。

## 29. 合并 CLI 帮助与别名

隔离加载固定 CLI/EXT/KERNEL，采集 83 个包含隐藏入口的运行时命令路径、参数声明以及实际 --help/-h。据此新增 account/acct ls、rm、switch 与 sessions/usage -A、--all-accounts，复用原状态更新和保护。十项差分验证与账户状态/拒绝删除测试见 [33](33-merged-help-and-aliases-implementation.md)。账户联合授权等差异仍有明确缺口，合并版业务等价尚未完成。

最终验证：默认特性 310 项、无默认特性 308 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整官方 CLI 差分 308 个场景为 92 一致、54 保留差异、162 Rust 扩展，无断言失败，原有 298 场景分类不变。合并版 83 路径长短 help 均成功，Rust 无缺失命令路径；五项研究工具测试通过。两份 CLI 报告的二进制哈希与当前 debug 文件相符。

## 30. 账户联合授权

默认 add 接入计算登录、可选 Drive consent 与最后 active 切换；URL-only 延后 Drive，configure-only 保留离线登记。login 支持创建缺失命名账户和邮箱提示，完成授权码时不自动创建；计算/Drive 失败分层处理，显式传 profile、不修改环境。八项 Rust 测试含真实子进程双 token 交换与本地 state callback。行为和边界见 [34](34-account-login-and-drive-consent-implementation.md)。

最终验证：默认特性 318 项、无默认特性 316 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 318 个场景为 92 一致、54 保留差异、172 Rust 扩展，无断言失败，原有 308 场景分类不变（account-add 已按新的离线参数迁移）。合并版 83 路径长短 help 均成功，账户参数缺口已消除；两份报告哈希与当前 debug 二进制相符。真实 Google/Drive 授权与平台验收仍未完成。

## 31. 补全与 Drive 状态账户选择

接入 show/install-completion 与统一命令树，生成五种 shell 格式，显式安装 Bash/Zsh/Fish；不进入账户 bootstrap。安装检查文件归属、链接、权限与旧内容，持锁并原子替换文件，rc 追加幂等受管块。Drive status 接入 drive-account，只用计算账户探测并展示本地选择/文件存在。测试与写入边界见 [35](35-completion-and-drive-status-implementation.md)。

最终验证：默认特性 326 项、无默认特性 324 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 332 个场景为 92 一致、54 保留差异、186 Rust 扩展，无断言失败，原有 318 场景的输入与分类不变。合并版 83 路径长短 help 均成功，剩余未展示项仅为已有隐藏参数；两份报告哈希与当前 debug 二进制相符。release 重建、打包校验和、version 和五种补全输出通过；真实平台/Google 验收仍未完成。

## 32. ADC 外部身份

接入文件/URL 主体令牌、workload/workforce STS 与可选 IAM 目标；按 provider 隔离源和最终 token 缓存，失败不降级身份、不改写外部文件。doctor 仅报告基本配置。范围、失败恢复、十项 Rust 测试与六项 CLI 扩展证据见 [36](36-adc-external-account-implementation.md)。真实 Google/IdP 和 Azure 验收尚未完成。

最终验证：默认特性 336 项、无默认特性 334 项 Rust 测试通过；两种特性 Clippy、fmt 和默认构建通过。完整 CLI 差分 338 个场景为 92 一致、54 保留差异、192 Rust 扩展，零断言失败，旧 332 场景输入和分类不变；业务场景 274 个。合并帮助 83 路径无缺失、短 help 无失败；研究工具 55 项中 54 通过、1 项 Linux 专属跳过。

本机 release 重建及打包后 version、账户别名帮助、联合授权参数和五种补全输出通过；manifest、SHA256SUMS、打包字节一致性与两份 debug 报告哈希均已核对。未推送代码或发布 Release。

## 33. Workforce 用户 ADC

接入 external_account_authorized_user 的 STS Basic 刷新、token-only 缓存、内存轮换和失败保留状态；不扩大 scopes、不回写 gcloud。新增七项认证测试和六项 CLI 扩展场景，并补测 external_account 的两种 ID token 拼写；协议、诊断和真实身份缺口见 [37](37-adc-workforce-user-implementation.md)。

最终验证：默认特性 344 项、无默认特性 342 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建通过。完整 CLI 差分 344 个场景为 92 一致、54 保留差异、198 Rust 扩展，零断言失败，旧 338 场景输入与分类不变；业务场景 280 个。合并帮助 83 路径无缺失、短 help 无失败；研究工具 55 项中 54 通过、1 项 Linux 专属跳过，另有四项纯内存参考库协议核对通过。

本机 release 重建、打包字节/manifest/SHA256SUMS、version、账户别名与联合授权帮助、五种补全输出通过；两份 debug 报告哈希与当前二进制一致。未推送代码或发布 Release。

## 34. Metadata ADC

接入默认文件缺失后的 metadata 发现、Google 标记、PUBLIC_SCOPES 请求、独立缓存与失败恢复；显式或坏文件不降级，环境只在启动快照。doctor 展示 fallback 策略但不探测。八项协议测试、账户解析和八项 CLI 扩展证据及真实平台缺口见 [38](38-adc-metadata-implementation.md)。

最终验证：默认特性 353 项、无默认特性 351 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建通过。完整 CLI 差分 352 个场景为 92 一致、54 保留差异、206 Rust 扩展，零断言失败，旧 344 场景输入与分类不变；业务场景 288 个。合并帮助 83 路径无缺失、短 help 无失败；研究工具 55 项中 54 通过、1 项 Linux 专属跳过。

本机 release 重建、打包字节/manifest/SHA256SUMS、version、账户别名与联合授权帮助、五种补全输出通过；两份 debug 报告哈希与当前二进制一致。未推送代码或发布 Release。

## 35. ADC 可执行来源

新增显式 opt-in 的 Unix 非交互插件、输出缓存、标准库兼容分词、进程组期限与取消回收；scope/身份仍经过原 STS/IAM。账户和跨账户汇总不再吞掉插件取消。13 项子进程测试、21 项分词对照与六项 CLI 扩展见 [39](39-adc-executable-implementation.md)，真实 IdP 和其余 ADC 来源未验收。

最终验证：默认特性 367 项、无默认特性 365 项 Rust 测试通过；完整 CLI 差分 358 个场景为 92 一致、54 保留差异、212 Rust 扩展，零断言失败，旧 352 场景输入与分类不变；业务场景 294 个。修订后的无默认特性全量复跑通过，不以首次失败计通过。研究工具 55 项中 54 通过、1 项 Linux 专属跳过；另有 21 项 Python 标准库分词观察由 Rust 单元测试逐一核对。

上述测试结果已在“执行期间主体过期”修复后重跑。release 打包后的 11 项检查及过期修复前后证据见 [39](39-adc-executable-implementation.md)；当前两份 CLI 报告哈希与 debug 一致，未发布或推送。

## 36. AWS 外部身份 ADC

接入 aws1 环境快照、EC2 IMDSv1/v2、区域/role 选择、SigV4 主体与 STS/可选 IAM；目标 audience 纳入签名，v2 失败不降级。10 项协议测试、三组双参考签名样本与七项 CLI 扩展见 [40](40-adc-aws-implementation.md)。未验证真实 AWS/Google 权限。

最终验证：默认特性 378 项、无默认特性 376 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和 Python 语法检查通过。完整 CLI 差分 365 个场景为 92 一致、54 保留差异、219 Rust 扩展，零断言失败，旧 358 场景输入与分类不变；业务场景 301 个。合并 help 83 路径无缺失、短 help 无失败，两份报告哈希与当前 debug 一致。研究工具 55 项中 54 通过、1 项 Linux 专属跳过；三组双参考 SigV4 样本由 Rust 单元测试核对。

本机 release 的 12 项打包/运行检查通过，包括 AWS 环境凭据签名→loopback STS、可执行 ADC 过期拒绝、version、别名/联合授权帮助、补全、manifest/SHA256SUMS 和字节一致性。release 哈希与发行记录一致，未推送或发布；真实身份和跨平台尚未验收。

## 37. 账户离线清单

`account list/current` 的业务 JSON 和文本摘要接入固定合并版契约，补齐认证/Drive/会话及路径，保留离线查询不打开钥匙串的边界。新增 16 项真实合并命令差分和五项 Rust 集成测试；范围、输出迁移和未验证项见 [41](41-account-inventory-implementation.md)。

最终验证：默认特性 383 项、无默认特性 381 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建通过。新增合并版账户差分 16 项全部一致；原有完整官方差分 365 个场景的输入/分类不变（92 一致、54 保留差异、219 扩展），83 个合并帮助路径无缺失。本机 release 的 14 项打包/运行检查通过，三份 debug 报告哈希一致。未发布或推送，真实 Google/钥匙串及合并版其余业务不在本轮验收结论内。

## 38. 合并版账户变更

补齐 use/set-drive/rename/remove 的确认和常见错误输出；注册表更新保持账户顺序，删除 active 选择首个剩余账户。修复 file 生命周期仍触发系统凭据库的问题，按事务持久化后端策略并兼容旧日志。27 组合并版实际变更对照、四项 Rust 回归和工具安全门见 [42](42-account-mutations-implementation.md)。

最终验证：默认特性 387 项、无默认特性 385 项 Rust 测试通过，两种特性 Clippy、fmt 通过。工具测试 57 项中 56 通过、1 项 Linux 专属跳过。27 组变更对照及此前 16 项清单对照全部一致；完整官方 CLI 差分仍为 365 项（92 一致、54 保留差异、219 Rust 扩展），原有命令输入和分类不变；83 个合并帮助路径无缺失。三份默认 debug 报告哈希匹配，变更报告单独匹配无钥匙串构建，本机 release 的 14 项检查通过。未推送代码或发布 Release。

## 39. 根级账户选择

账户清单与变更前校验根级资料，区分 set-drive 的根级与局部 -a，支持重复参数取末值、空值回退和环境空白修剪。无效根级选择先于业务写入报错；新增 21 组合并命令对照及五项副作用/解析集成测试，范围及保留差异见 [43](43-account-selection-implementation.md)。

最终验证：默认特性 392 项、无默认特性 390 项 Rust 测试通过；两种特性 Clippy、fmt 通过。工具测试 57 项中 56 通过、1 项 Linux 专属跳过。21 组选择对照、27 组既有变更对照和 16 项清单对照全部一致；原有完整 CLI 365 场景的命令输入及分类不变（92 一致、54 保留差异、219 Rust 扩展），83 个合并帮助路径无缺失。三份默认 debug 报告、两份无钥匙串报告分别匹配对应构建哈希。本机 release 的 15 项检查通过，新增“无效根级＋有效局部目标”被拒绝且注册表字节不变的打包后验证。未推送或发布；授权及其余顶层入口仍需继续对照。

## 40. 合并版在线汇总

将 sessions/usage 的 JSON、--all 文本与 account status 复用共享账户行；按原版返回逐账户结果和错误，单账户 JSON 也需检查 error。补齐根级校验与查询模式隔离，保留 status 的本地详情扩展。22 组实际合并 loopback 对照、三项 Rust 回归及网络隔离测试见 [44](44-merged-inventory-implementation.md)。

最终验证：默认特性 395 项、无默认特性 393 项 Rust 测试通过，两种特性 Clippy、fmt 通过。工具测试 59 项中 58 通过、1 项 Linux 专属跳过。合并在线汇总 22 组为 15 一致、7 组仅保留错误文本差异，零未解释差异；既有 27 组账户变更、21 组选择和 16 项清单对照仍一致。官方单端基线的完整 CLI 365 场景保持命令输入和分类（92 一致、54 保留差异、219 Rust 扩展）；九项 JSON 错误的退出约定和对应断言按本次契约迁移。83 个合并帮助路径无缺失。三份默认 debug、三份无钥匙串报告分别匹配对应二进制哈希；本机 release 的 16 项打包/运行检查通过。没有推送或发布，也未进行真实 Google/ADC 合并版并发验收。

## 41. 每日原生发行检查

接入交互命令的每日原生检查，使用独立源/平台缓存，保留 PyPI 字段；失败节流、2 秒 HTTP 超时与短锁等待不阻断业务，JSON/管道/SSH 和信息命令抑制提示。总开关沿用 enable_update_check，安装仍需显式 update --install。输入、状态、清理责任和验证见 [45](45-daily-native-update-implementation.md)。

验证：默认特性全量 403 项、无默认特性全量 401 项通过。随后将检查入口移到命令成功之后，并补上无效账户的 PTY 断言；两种特性各重跑 23 项原生更新/每日检查测试及 Clippy。fmt 和研究文档检查通过；49 篇 Markdown、511 个相对链接、123 个固定源码引用、143 个源码哈希无错误。原有业务差分和 release 报告未重新捕获，其哈希保留为前一阶段历史证据。未推送或发布。

## 42. 执行富文本显示

补齐 Markdown、HTML、纯文本的 MIME 优先级和可读文本转换；共用流式钩子覆盖 append/update，JSON、Notebook 和历史继续保留原始 MIME。固定上游 16 组显示样本、三项投影回归与一项双模式 CLI/WS/Notebook 集成验证见 [46](46-rich-text-output-implementation.md)。未知/坏类型和超限降级不触发远端代码重放，没有新增 Google 资源。

最终验证：默认 407 项、无默认特性 405 项 Rust 全量回归通过，两种特性的 Clippy、fmt 通过。16 组固定显示样本为 13 组排版空白规范化后一致、3 组保留布局差异并验证内容。既有完整 CLI 365 场景重新通过（92 一致、54 保留差异、219 Rust 扩展），场景名称、命令和分类保持不变。当前本机 release 的 16 项打包/运行检查通过；其他合并版专项报告保留为各自阶段的历史证据，本轮未重新采集。没有推送或发布，也没有新增 Google 资源或宣称真实富文本/跨平台验收完成。

## 43. 终端图片与默认产物

共用执行输出接入终端 PNG 的 Kitty 分包传输和默认 PNG/JPEG 临时产物；显式 --output-image 路径避免额外副本。JSON 与管道抑制终端控制序列，默认图片文件为私有且退出后保留。解码或默认保存失败不改变已完成执行的恢复状态；三项图片测试、10 组实际 CLI/PTY/WS 配置和固定上游 7 组样本见 [47](47-terminal-image-output-implementation.md)。没有新增 Google 资源，尚无真实图形终端显示或 JPEG 转码验收。

最终默认特性 411 项、无默认特性 409 项 Rust 全量回归通过，两种特性的 Clippy、fmt 通过。既有 365 个 CLI 场景全部通过断言，名称、命令及分类保持不变（92 一致、54 保留差异、219 Rust 扩展）；新图片样本 7 组独立计数。当前本机 release 的 16 项打包/运行检查及两组非终端 exec/repl 图片差分通过。其他合并版专项报告仍是对应阶段证据，本轮未重新采集。没有推送、发布或创建 Google 资源，也未完成真实图形终端视觉、JPEG 转码或跨平台验收。

## 44. WebSocket HTTP CONNECT 代理

Jupyter 与 SSH 共用 TCP/CONNECT 连接层，读取环境代理并复用当前 HTTP 的 NO_PROXY 匹配引擎；Basic 仅发送到 CONNECT，端点握手和 TLS 保持原身份。失败不跟随重定向、不重试认证、不退回直连。10 项 loopback/实际 CLI 测试覆盖隧道、头隔离、滴流超时及 407 后复用 kernel，见 [48](48-websocket-http-proxy-implementation.md)。没有新增真实 Google 资源；HTTPS 代理、SOCKS、真实企业代理及成功的代理 TLS 仍未验收。

最终默认特性 421 项、无默认特性 419 项 Rust 全量回归通过，两种特性 Clippy、fmt 和文档检查通过。既有完整 CLI 365 场景保持名称、命令和分类（92 一致、54 保留差异、219 Rust 扩展）；代理专项 10 项单独计数。本机 release 的 16 项基础/打包检查与两组无代理图片差分通过，不作为代理分支或成功代理 TLS 的优化版验收。其他专项报告保留各自阶段证据。没有推送、发布或新建 Google 资源；真实代理、成功的代理 TLS、HTTPS 代理、SOCKS、系统代理自动发现仍有缺口。

## 45. SOCKS5/SOCKS5h 环境代理

HTTP 启用 reqwest 的 SOCKS 特性，Jupyter/SSH 共用连接层增加 SOCKS5 CONNECT，区分本地与代理端目标 DNS，支持无认证和 percent 编码用户名/密码。握手精确读取有界字段，使用共同 deadline，拒绝认证降级与失败后直连。新增六项协议回归，五项既有集成测试补 SOCKS 分支，合计 16 项代理测试通过；实际 exec 的 REST/WS 和 ssh --proxy-mode 均覆盖。输入、状态、失败和清理责任见 [49](49-websocket-socks-proxy-implementation.md)。没有创建真实云资源；HTTPS 代理、成功代理 TLS、系统代理发现和真实企业代理仍有验收缺口。

最终默认特性 427 项、无默认特性 425 项 Rust 全量回归通过。既有官方 CLI 365 场景保持名称、命令与分类（92 一致、54 保留差异、219 Rust 扩展）；代理专项合计 16 项独立计数。本机 release 的 16 项发行检查与两组无代理图片差分通过，不作为优化版 SOCKS 或成功代理 TLS 验收。其他合并版专项报告保留各阶段证据，本轮未重新采集。没有推送、发布或新增真实云资源。

两种特性的 Clippy 与格式检查通过；研究文档检查覆盖 54 篇 Markdown、571 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。

## 46. HTTPS 代理与双层 TLS

WebSocket 共享连接层支持 https:// 代理，在验证代理 TLS 后发送 CONNECT，再按原目标配置建立端点 TLS。Stream 保留底层 TCP 的超时/非阻塞控制，代理 TLS 与 CONNECT 共用 deadline；失败不降级、不直连、不重放执行。库调用者可添加代理 CA，端点信任保持独立，没有关闭证书或主机名验证。八项新测试覆盖成功 Jupyter/SSH、双层 TLS 及失败验证；既有 HTTP/SOCKS/SSH 回归同步验证，输入、清理和边界见 [50](50-websocket-https-proxy-implementation.md)。没有创建真实云资源，真实企业/公开 CA 代理和跨平台仍有验收缺口。

最终默认特性 435 项、无默认特性 433 项 Rust 全量回归通过。原有官方 CLI 365 场景的名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）；新增八项 HTTPS 代理测试独立计数。本机 release 的 16 项发行检查和两组无代理图片对照通过，不作为优化版代理或线上 TLS 验收。其他合并版专项报告保留各阶段证据，本轮没有重新采集。没有推送、发布或创建真实云资源。

两种特性的 Clippy、格式检查通过。研究文档检查覆盖 55 篇 Markdown、591 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。

## 47. JPEG 终端显示转换

补齐 V05.6 的 JPEG 内联显示：仅 TTY 文本模式将 JPEG 解码为 PNG 后使用既有 Kitty 分包，文件、显式导出、JSON 和 Notebook 保留原始 JPEG。转换设输入/尺寸/像素/解码/输出限制，常规转换失败只警告，不改已完成执行状态。新增三项图像回归，既有实际 CLI/PTY 测试扩展至 16 组配置，见 [51](51-jpeg-terminal-projection-implementation.md)。没有新增真实 Google 资源；真实图形终端像素、特殊 JPEG 变体及跨平台验收仍未完成。

最终默认特性 438 项、无默认特性 436 项 Rust 全量回归通过，原有官方 CLI 365 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）。三项新增编解码回归和 16 组 CLI/PTY 配置独立计数。本机 release 的 16 项发行检查及两组非 TTY 图片对照通过，不作为优化版 JPEG 转码或真实终端视觉验收。其他合并专项报告保留各阶段证据，本轮未重新采集；没有推送、发布或创建真实云资源。

两种特性的 Clippy 和格式检查通过。研究文档检查覆盖 56 篇 Markdown、606 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。

## 48. ADC 配额项目与请求身份

补齐 ADC quota_project_id / GOOGLE_CLOUD_QUOTA_PROJECT 的请求元数据；provider 原子返回 token 与配额项目快照，控制面每次 GET/POST 显式添加头，不使用全局 client 默认身份。普通 impersonation 的 IAM 采用嵌入来源项目，联邦 IAM 采用有效来源项目；token/STS/metadata 和 runtime REST/WS 不携带该头。九项新增 Rust 回归、metadata 补充断言及十组 google-auth 纯内存观察见 [52](52-adc-quota-project-implementation.md)。无新增真实云资源，也未取得真实项目权限或 Colab 接受性证据。

默认特性全量 447 项、无默认特性全量 445 项 Rust 测试通过，两种特性的 Clippy 和格式检查通过。官方 CLI 全量 365 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）。Python 工具回归 59 项中 58 项通过，1 项仅 Linux 适用而跳过；配额探针另行完成十组观察。研究文档检查覆盖 57 篇 Markdown、629 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。

## 49. 控制面凭据续期与有限认证重试

控制面保留账户绑定的 OAuth/ADC 来源，每次请求前按原有到期余量检查并刷新。会话清单与用量 GET 在 401 后最多刷新重试一次，分配 GET/POST、释放与传播均不因 401 自动重放。OAuth 复用会话实际持有的生命周期锁，缓存有效时不重复读取 keyring；锁序、取消、失败保存和账户隔离继续受测。十项新测试与一项四组合 CLI/WS 清理回归见 [53](53-control-credential-refresh-implementation.md)。

最终默认特性全量 458 项、无默认特性全量 456 项 Rust 测试通过；两种特性的 Clippy 与格式检查通过。既有官方 CLI 365 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）。本机 release 为 9,140,416 字节，默认特性测试用 debug 为 44,653,160 字节。研究文档检查覆盖 58 篇 Markdown、646 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。没有推送、发布或创建真实云资源。

## 50. Linux ARM64 容器与最低 Rust 版本验证

新增隔离验证环境与逐阶段报告，并为 CI 添加独立 MSRV job。Rust 1.85.1 的默认/无默认特性 all-targets 编译通过；Rust 1.100.0 的 Linux ARM64 两种特性 Clippy、458/456 项 Rust 测试、59 项 Python 测试及 16 项原生发行检查全部通过。源码只读、外网关闭、UID 1000，144 份受检文件哈希保持一致；已清理本次两个容器和三个临时卷。详情与真实平台边界见 [54](54-linux-container-validation.md)。没有改动产品 Rust 代码或重写 macOS 证据。

研究文档检查最终覆盖 59 篇 Markdown、660 个相对链接、125 个固定源码引用和 143 个固定仓库源码哈希，无错误；固定研究清单与本阶段 144 份工作区输入快照分别计数，不混同。

## 51. Linux 原生 Secret Service 与 Python 互通

新增五项默认忽略的原生凭据测试，专用私有 D-Bus 容器中逐阶段执行正常互通/迁移、锁定、无响应、服务停止及恢复。12 个验证阶段通过，包含 Rust 1.85.1 编译与稳定版 Clippy；146 份输入哈希一致。实际使用 GNOME Keyring 42.1、Python keyring 23.9.3 和 Rust keyring 3.6.3，未修改产品 Rust 或依赖。报告、默认隔离保护及未验收边界见 [55](55-native-secret-service-validation.md)。本次一个容器与两个临时卷已清理；没有访问用户钥匙串或 Google。

研究文档检查覆盖 60 篇 Markdown、673 个相对链接、127 个固定源码引用和 143 个固定仓库源码哈希，无错误；本次原生测试的 146 份工作区输入另行记录，不合并计数。macOS 的专用目标实际运行 0 项，确认平台门控；现有产品来源和二进制/报告哈希保持一致。

## 52. 真实 SSH 验证入口与条件清理

提供默认只预检查、显式执行/清理模式的开发验证脚本；隐藏条件 stop 和 SSH 父入口复用已有 endpoint 保护，避免误连/误停替代 VM。三项新增 CLI 回归、六组 SSH 拒绝配置、八项脚本安全回归已通过，见 [56](56-live-ssh-validation-and-guarded-cleanup.md)。文件后端当前需要登录，真实 SSH 尚未开始，创建资源为 0；没有将预检查或替身测试记为线上通过。

最终默认特性全量 461 项、无默认特性全量 459 项 Rust 测试通过；两种特性的 Clippy 与格式检查通过。原有 365 个 CLI 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）；Python 工具 67 项中 66 项通过、1 项 Linux 专属在 macOS 跳过。本机 release 为 9,140,576 字节，默认测试用 debug 为 44,658,168 字节。研究文档检查覆盖 61 篇 Markdown、687 个相对链接、127 个固定源码引用和 143 个固定源码哈希，无错误。

## 53. SSH 验证脚本完整流程

新增六项跨子进程的合成流程测试，覆盖成功、分配后握手失败清理、未知结果调和与 cleanup-only 恢复；另补畸形清单行拒绝。修复成功预检查退出 0 但报告 passed=false 的不一致。SSH 脚本 15 项测试通过，全部 Python 工具 74 项中 73 项通过、1 项 Linux 专属跳过，详见 [56 后续记录](56-live-ssh-validation-and-guarded-cleanup.md)。本次未改产品 Rust 或重编译；现有真实认证失败证据不变，未创建 VM，整体目标尚未完成。

## 54. 固定依赖 session 传递证据

新增实际构造探针，校验固定包源码及导入路径，使用 Python -S 避免安装环境的 .pth 预载错误版本。三组输入确认固定依赖忽略调用方 session/extra_params，消息 session 与 query 仍相同，显式 token query/header 保留。Rust 既有重连回归加强为直接比较持久化、两次握手 query 和消息头；历史跨命令变量验收另行保留，范围见 [运行时传递证据](03-runtime-and-wire-protocols.md)。本次仅修改研究工具、测试和文档，没有改产品 Rust，真实 SSH 的文件认证预检查仍失败，创建 VM 为 0。

Jupyter 测试组 75 项通过，目标 Clippy 与 fmt 通过；构造探针重复运行结果相同。未重跑全量 Rust、官方 CLI 对照、其他特性或发行构建，沿用对应阶段报告，不扩大本轮验证范围。

## 55. 合并版 Drive 凭据输出

show 的脱敏元数据移到顶层、空凭据使用单行文本，clear 提示对齐原版；空的扩展元数据字段省略，已知字段白名单和 pending/租约保护不变。12 组实际合并 CLI 对照中 11 组输出/退出码/状态相同；缺凭据 token 原版触发 ValueError 且无诊断，Rust 明确提示缺凭据，两端退出 1。新增工具只接收无原生凭据库 Rust，使用临时合成凭据并禁止原版外部操作，逐文件核对变更；迁移与范围见 [57](57-merged-drive-credentials-implementation.md)。

默认/无默认特性 461/459 项 Rust 全量通过，两种特性 Clippy、fmt 通过，Python 工具 74 项中 73 项通过、1 项平台跳过。原有 365 个 CLI 场景名称/命令/分类不变（92 一致、54 保留差异、219 Rust 扩展）。本机 release 的 17 项检查及两组非 TTY 图片对照通过，release 9,140,704 字节、默认测试 debug 44,668,280 字节；四份报告与对应二进制身份核对通过。真实 SSH 预检查仍缺认证，没有创建资源；其余专项保留历史证据，整体目标未完成。

## 56. Drive token/身份查询的实际协议对照

新增七组固定合并版与 Rust 的实际 HTTP loopback 对照，验证绑定/显式 Drive 身份、刷新与 whoami 请求、不同客户端凭据隔离、失败不改其他账户、轮换持久化和 userinfo 失败后 show 恢复且无网络重放。确认原版丢失轮换结果、短有效期被抬高、naive expiry 受本地时区影响，以及 whoami 注册表策略差异；Rust 保留已有正确处理，不为模拟错误改变生产行为。七组契约全部通过，无未解释差异，详见 [57 后续](57-merged-drive-credentials-implementation.md)。本次只增加开发工具、合成报告和文档，没有改产品 Rust 或创建真实资源，也不关闭真实授权/挂载验收。

## 57. Drive consent 自动登记与邮箱提示

仅授权起始阶段支持命名账户自动登记，计算/Drive 选择共同校验并在单个注册表事务中提交，不切换 active 或改已有绑定。完成阶段与其他 Drive 操作不创建；开始失败保留空资料供重试。补齐所选存储账户邮箱作为授权提示，显式参数保持优先。五项新增回归与九组合并版实际 URL-only 对照覆盖路由、并发、原子拒绝、失败重试和提示优先级；两端本次均验证到匹配 verifier 的 S256，不能外推到真实 Google 接受性。输入、状态与清理责任见 [58](58-drive-consent-profile-implementation.md)。

默认/无默认特性 466/464 项 Rust 全量通过，两种特性 Clippy、fmt 通过，Python 工具 74 项中 73 项通过、1 项平台跳过。原有 365 场景名称/命令/分类不变（92 一致、54 保留差异、219 Rust 扩展）；12/7/9 组三份 Drive 专项重跑通过。本机 release 的 17 项检查及两组非 TTY 图片对照通过，release 9,157,216 字节、默认测试 debug 44,693,592 字节，六份报告匹配对应二进制身份。其他平台/专项仍保留历史证据，尚未发布或完成整体真实验收。

## 58. 当前实现的 Linux ARM64 复验

同一隔离镜像中重新完成 15 阶段：最低 Rust 1.85.1 两种特性编译、稳定版两种特性 Clippy、466/464 项全量测试、74 项 Python 工具及 17 项发行检查均通过。默认特性的五项专用 Secret Service 测试 ignored 不计通过；无默认 ignored 为 0，Python 没有跳过。155 份输入及日志哈希复核一致，见 [54 当前复验](54-linux-container-validation.md)。本次容器和两个临时卷已清理，只保留镜像缓存与已校验的本地 Linux release；没有修改产品或创建 Google 资源，真实授权与完整平台验收仍未完成。

## 59. 合并版显式会话 URL 对照

新增 14 组实际合并 CLI 与 Rust 的离线 URL 对照，覆盖账户/邮箱/环境与会话归属、authuser、host 和 endpoint，以及三类明确失败。七组完全一致；路由提示、全局选项位置、origin 规范化、特殊 endpoint 编码及三类错误文案分别登记并断言，没有归一化成假一致。所有已有配置/凭据 payload 字节不变，原版外部操作禁止，见 [27 后续](27-utility-commands-implementation.md)。本次只增加研究工具、合成报告和文档，未改产品或重编译，也未进行真实前端连接。

## 60. macOS x86_64 的 Rosetta 执行

增加独立工具链/target/HOME 的验证脚本，实际使用 Rosetta 运行 x86_64 测试和打包产物。六阶段全通过：两种特性 Clippy、466/464 项测试、恢复默认构建及 release；120 份输入和日志哈希一致，临时 HOME/构建目录已删除。10,258,384 字节的单平台候选及校验和留在忽略目录，未发布。初次试跑中断不计通过，修正后完整重跑，见 [59](59-macos-x86-rosetta-validation.md)。本轮没有改产品；翻译执行不替代 Intel 原生硬件或真实授权验收。

## 61. 全范围验收核对

将 V01–V07 的 53 个条目全部映射到当前证据与局限，17 个 U 问题和 P0–P6 交付条件保持原范围，见 [60](60-acceptance-audit.md)。明确真实 Drive、SSH、各认证来源、原生平台及发行/许可缺口，不能用合成测试关闭。修正早期 F08/F11 目标表与最终文本 exec/TPU 优先行为的矛盾。结构核对确认无 V 条目遗漏、证据文件存在且记录哈希；这不代表语义覆盖自动通过。本轮文件认证只读预检查仍失败，创建 VM 为 0，没有重编译或声称整体完成。

## 62. 真实 Colab CPU/SSH

用户选择现有系统钥匙串后认证预检查成功，基线 assignments=0。当前默认 debug 真实创建一台 CPU，五项 SSH 检查全部通过：raw banner/EOF、root 与 /content、特殊 argv、1024 字节二进制 stdio、退出码 23。条件 stop 后重新查询后端确认 endpoint 消失，其他新增会话为 0，临时密钥已删除；结果见 [56](56-live-ssh-validation-and-guarded-cleanup.md)。没有访问 Drive、重新登录或修改产品，不以单次 macOS 成功关闭其他平台/真实交互/信号故障和整体目标。

## 63. 真实 run 非零退出与自动释放

以系统钥匙串执行一次默认 CPU 请求的真实 run，合成脚本验证 shebang/future、main/file/argv、特殊字符与类似选项的尾参、Unicode/等号环境值及 /content。实际进程和脚本结果均为 23，run 自动释放后独立查询后端确认 endpoint 消失，无新增其他 VM 或本次遗留计划。脱敏证据与未覆盖 keep/信号/故障范围见 [13 后续](13-run-and-history-implementation.md)。没有重放、访问 Drive、改产品或重新编译，整体目标继续保留未完成项。

## 64. 真实 run keep 与 SIGTERM

系统钥匙串下顺序完成两台默认 CPU 请求：keep 成功后后端保留且独立 exec 能读取脚本写入的随机文件；另一次实际观察远端脚本开始后向 CLI 发送 SIGTERM，返回 143 并自动释放。两项均独立查询后端确认最终释放，无其他新增会话或本次遗留计划，见 [13 后续](13-run-and-history-implementation.md)。未重放脚本、访问 Drive、改产品或重编译；真实断线、释放失败及长期运行仍未验收，整体目标未完成。

## 65. 真实 SSH PTY 与命令 SIGTERM

开发验证脚本新增 opt-in 扩展检查和私有失败诊断，20 项脚本安全/流程回归包含 PTY 回显不能误报成功、实际标记后才发信号及诊断权限。三次顺序真实尝试中，首次 binary stdio 返回 255，第二次未识别交互结果，第三次七项全通过，新增远端登录 shell 三流 TTY、/content 与命令 SIGTERM=143 证据。前两次失败原因未定位、报告分别保留；三台专属 CPU 均后端确认释放，临时密钥已删，见 [56 后续](56-live-ssh-validation-and-guarded-cleanup.md)。未改产品或重编译，其他信号/故障、长期连接、其他平台与完整目标仍未验收。

## 66. 真实执行中断线与显式恢复

新增只作用于测试子进程的 TLS 透明 CONNECT relay，在远端实际输出标记后切断 runtime 连接。真实单台 CPU 验证未知状态持久化、无自动重连、再次 exec 被拒绝、独立下载确认脚本继续且仅一次、显式重启后恢复；另从实际历史确认终止原因为连接丢失且部分输出保留。VM 已后端确认释放，其他新增资源为 0。十项安全回归、全工具 89 项中 88 项通过/1 项平台跳过，见 [61](61-live-execution-disconnect-validation.md)。未改产品或重编译；dead/restarting 的 Failed 与 V05.5 Unknown 目标仍有明确缺口，不能据此关闭完整目标。

## 67. kernel 生命周期广播的 Unknown 保护

修正 iopub dead/restarting 从 Failed 到 Unknown，保留部分输出、具体终止原因与持久化重放保护，非 iopub 的同名 status 不提前终止请求；普通 Python 异常语义不变。定向九项通过，新增一项跨四组状态/输出模式的持久化回归，默认/无默认特性全量 467/465 项通过，两种特性 Clippy 和 fmt 通过。完整 365 CLI 场景无失败，分类统计保持 92 一致、54 保留差异、219 Rust 扩展，见 [61 后续](61-live-execution-disconnect-validation.md)。

本机 release 的 17 项发行检查和两组非 TTY 图片对照通过，release 9,140,704 字节、默认测试 debug 44,693,592 字节，对应构建哈希各自核对。其他平台和合并专项报告继续是各阶段证据，没有发布。新构建真实广播验收在系统钥匙串预检查超时，创建 VM 为 0；不把旧构建真实断线成功改写成新构建线上通过。另只读检查到一份本地 Drive 资料，未刷新、未访问云盘；完整验收仍未结束。

## 68. Drive 受限目录验收准备

新增离线准备工具、14 步非执行命令清单和三个标准库远端探针，覆盖新随机 MyDrive 子目录的二进制读写、只读写拒绝与标记/内容验证后的非递归清理。目录 fd、FUSE 设备/路径/类型核对、O_NOFOLLOW/O_NONBLOCK 和条目白名单限制误操作；已有目录、未知结果或额外用户内容不自动接管/重放。17 项新回归通过，完整 Python 工具 108 项中 107 项通过、1 项平台跳过，见 [62](62-drive-live-validation-preparation.md)。

对本机候选资料生成了私有计划，原配置/凭据字节不变，没有调用 CLI、钥匙串、网络或云盘写入。没有改 Rust 或重复发行验证；真实执行器、独立存储身份、挂载/只读、持续刷新与恢复仍未验收，整体目标保持未完成。

## 69. Drive 受控执行器与运行时 endpoint 条件

新增私有账本执行器、精确云盘目录范围开关和隐藏运行时 endpoint 条件。在认证前及账户锁内核对归属，未知修改不重放，凭据未回收不释放 VM，探针以校验后固定字节经 stdin 提交。20 项合成流程测试通过；全 Python 128 项中 127 通过、1 项平台跳过，默认/无默认 Rust 471/469 项、两种 Clippy 和 fmt 通过。365 CLI 场景无失败，本机 17 项发行与两组图片对照通过，见 [63](63-drive-live-validation-executor.md)。

本地真实资料只读准备核对通过，注册表/凭据字节不变，未调用钥匙串、网络、VM 分配或云盘写入。真实云盘范围尚待确认，独立存储身份、实际挂载/只读、刷新与恢复仍缺证据；整体目标未完成。

## 70. Drive v3 只读独立核查

新增受限 GET 观察器并接入真实验收工具：分配前核对同 token 的 Drive 当前用户与 MyDrive 根目录所有权，首次卸载后独立核对随机目录、两文件元数据和服务端下载字节。拒绝重定向、额外条目、所有权/父目录不符、分页不完整与超限回复；不将 permission ID 等同 OAuth subject。十项 loopback API 和两个执行器失败流程通过，全工具 140 项中 139 通过、1 项平台跳过，见 [63 后续](63-drive-live-validation-executor.md)。未改 Rust、未重编译、未调用真实凭据或 Google；实际云盘范围仍待确认，不能关闭真实验收。

## 71. Drive 服务端清理观察

补齐只读 API 的已知对象清理核对：成功读取后仅在内存捕获三个已验证 ID，最终卸载后重新确认同一用户/root，逐对象核对 404 或相同所有者且 trashed，分别记录计数，不声称永久删除。API 未确认时完整验证失败，即使本地凭据已回收并成功释放 VM；不重放脚本。API 17 项、执行器 23 项，全工具 148 项中 147 通过、1 项平台跳过，见 [63 后续](63-drive-live-validation-executor.md)。未改 Rust 或访问真实云盘，V07 仍缺线上结果。

## 72. 中断读取修正与平台复验

真实执行语义保持不重放：生产 Jupyter 在读取 EINTR 后回到原期限/取消轮询；底层 WebSocket 读取在原预算内继续，覆盖握手，不重发消息或写入。共用测试 TCP 适配器在有限预算内处理 EINTR，修复 x86_64 模拟执行暴露的 Jupyter、SSH、代理替身失败。目录探针在同一已验证 fd 上 rewind 后枚举，保留严格白名单/内容检查；此前构建卷首次枚举为空的失败通过实际复现定位。

[macOS 最终回归](../research/eintr-verified-regression-validation.json) 默认/无默认 474/472 项、两种 Clippy 和 fmt 通过，[当前工具](../research/eintr-tools-validation.json) 153 项中 152 通过、1 项平台跳过。新构建 [365 CLI 对照](../research/eintr-cli-comparison.json) 保持 92 一致、54 保留差异、219 Rust 扩展，[17 项发行](../research/eintr-native-release-validation.json) 与 [两组图片](../research/eintr-release-image-comparison.json) 通过；源码和各构建哈希独立核对。此前中断或输入变化的回归不计最终成功，失败报告保留。

Linux x86_64 的隔离方式、架构/ELF 验证、各次失败与修正见 [64](64-linux-x86-container-validation.md)。模拟执行不替代原生 x86 硬件。当前 Drive 私有计划已随构建/模板更新并只读核对，未调用真实凭据或云盘，用户仍需确认测试目录写入范围；真实认证、跨账户、长期刷新及完整目标未完成。

本阶段最终 Linux x86_64 的 15 阶段全通过：474/472 项 Rust（默认另有 5 ignored，不计通过）、153 项 Python 无跳过和 17 项发行检查。168 份输入/所有阶段日志/ELF 架构与产物哈希均核对，专属容器与两个卷已删除，报告见 [64 最终结果](64-linux-x86-container-validation.md)。这仍是 ARM64 daemon 的模拟执行，不代表原生 x86 硬件或真实 Google 认证验收。

## 73. Drive 真实读写、只读与上传后卸载

用户明确授权一个随机 MyDrive 目录及两份测试文件后，已实际验证 shim 挂载、二进制往返、独立 Drive API 下载字节、只读写入拒绝与原内容不变、受限清理、上传后卸载及凭据租约回收，详见 [65](65-live-drive-validation-and-recovery.md)。API 已确认三个本次对象进入回收站，不表示永久删除；VM 最终释放按独立后端确认单列。

真实故障促成启动快照恢复、精确独立租约恢复、同一 child 的启动身份观察与 zombie 处理、`--single_process`、所属 IPC 核查及 `push_changes_and_quit` 后才卸载的修正。显式会话已有有效 proxy token 时延迟主凭据读取，需要控制面/刷新时仍严格认证。各次失败和后续显式恢复分别保留，不改记原执行器一次无中断通过。跨真实账户、长期刷新、rt、新授权及其他平台仍未关闭；首次本地 session 恢复字段缺失的原因仍未定位。

本轮最终默认／无默认特性 476／474 项 Rust、两种 Clippy 和 fmt 通过，工具 163 项中 162 通过、1 项平台跳过；365 CLI 场景无测试失败（92 一致、54 保留差异、219 扩展），17 项本机发行检查通过。三台本次 CPU 均独立后端确认释放，其他新增为零；云端三个本次对象已确认进入回收站，精确同名存活目录为零。证据及各阶段构建范围见 [65](65-live-drive-validation-and-recovery.md)。

## 74. 恢复记录检查点的丢更新修正

稳定复现旧执行快照清掉其他写入者新增 Drive plan/mount/probe 的路径；检查点现只合并相对上次成功提交有变化的恢复字段，同字段冲突拒绝整次 sessions 写入，成功后才推进基线。明确删除与重复提交仍有效，endpoint/generation 和代理刷新保护保留。修复前失败、修复后三项定向及默认／无默认 478／476 项 Rust、两种 Clippy、fmt 与工具 162 通过／1 跳过均有证据；365 CLI 对照分类不变，17 项本机发行检查通过，详见 [66](66-recovery-checkpoint-merge.md)。首次真实 Drive 字段缺失的具体根因仍未被证明；本轮没有 Google 操作或新 VM，整体目标保持未完成。

## 75. 真实 DEFAULT 文本和二进制帧

在一台专属 CPU 上采集固定输出的真实文本和二进制帧，Rust 与固定上游 decoder 对原始响应核对一致，非 UTF-8 与空 buffer 均完整保留，成功 reply/idle 已观察。公开样本经过身份/metadata 脱敏和重新编码，两个样本在 Rust 重放通过；VM 已独立确认释放，其他新增为零，无 Drive 访问。U06 采集与重放缺口已补齐，详见 [67](67-live-codec-capture-and-replay.md)。

产品源码及最终 CLI/探针分别与既有完整回归/真实采集哈希核对一致；两种特性各三项 codec 与一项新增样本测试、两种全目标 Clippy 和 fmt 通过，工具 169 通过、1 项平台跳过。执行器失败退出码调整单独保留版本指纹和零创建认证预检查。其他真实认证、跨账户、长期刷新与发行仍未关闭，整体目标继续进行。

## 76. 真实 kernel 崩溃、禁止重放与恢复

新增验收工具的控制终端认证选项、所有 kernel-death 运行时命令的 endpoint 条件、预热和输出发送窗口。首次真实广播 Unknown 因缺输出未通过，第二次四项断言通过，并从原始回执独立核对结果。两台本次 CPU 均确认释放，其他新增为零，无 Drive 操作；具体证据、工具回归及限制见 [68](68-live-kernel-death-validation.md)。产品未重编译，钥匙串根因、其他时序与总体目标仍未关闭。

## 77. macOS 凭据代码身份排查

新增只读 codesign 检查工具；实际 debug/release/原版解释器均为 cdhash 身份，同一路径两次编译的身份变化已在临时目录复现。工具不运行目标或访问钥匙串，生产 CLI 未重编译。该结果解释重复构建可能重新授权的机制，但真实条目 ACL、非终端超时和完整互通未证明，见 [69](69-macos-keychain-code-identity.md)。

随后同一构建/账户在 30 秒凭据上限下，继承与脱离控制终端的两次真实 inventory 分别约 1.4/0.9 秒成功；当前未复现非终端超时，不把终端方式推定为根因或修复。完整工具 175 项通过、1 项平台跳过，本轮没有分配/释放 VM。

## 78. External account STS 客户端认证

补齐非空 client_id 的 HTTP Basic、缺省/空 secret、无 client ID 时不启用认证，以及认证启用时省略 workforce userProject。Basic 只送 STS，来源/IAM/最终身份维持隔离，缓存失败恢复和并发账户隔离有新增回归，原 ADC 不回写。原版安装环境 google-auth 五组离线捕获对照通过；具体两种特性回归及未覆盖范围见 [70](70-external-account-client-auth.md)。没有真实外部身份或新 VM 操作，整体目标继续进行。

本阶段最终默认／无默认 482／480 项 Rust、两种全目标 Clippy、fmt、工具 175 通过／1 平台跳过及默认 debug 构建通过；166 份源码/测试/工具输入收尾哈希一致。旧拒绝条件下两项新增测试失败，修正后 external 专项 14 项通过；本轮未重建 release 或声称新构建真实 Google 通过。

## 79. X.509 workload ADC 与 mTLS

接入 certificate 来源、默认证书位置的显式环境快照、workload 配置、合并 bundle 和中间链主体编码；同份材料用于主体和 STS/IAM mTLS，配对/读取/TLS 失败不降级，轮换失败保留有效缓存。七项本地证书专项及原版四组合成对照已通过；关闭新 mTLS 客户端时握手测试失败。完整回归、构建范围及真实身份权限缺口见 [71](71-x509-adc-and-mtls.md)。没有真实凭据、Google 或新 VM 操作，整体目标保持未完成。

本阶段最终默认／无默认 489／487 项 Rust、两种全目标 Clippy、fmt、工具 175 通过／1 平台跳过及默认 debug 构建通过；169 份输入收尾哈希一致。真实本地 mTLS 验证使用临时 CA，没有向系统安装根证书或修改钥匙串；不代表真实 Google 接受性或新发行物已验收。

## 80. Metadata mTLS 模式与轮换

接入 none/strict/default 环境快照、系统固定证书路径、每次请求的材料重读及配对验证；strict 默认主机约束和不回退、default 网络/HTTP 错误的一次 HTTP 回退、材料加载失败不回退均有本地测试。七项新 metadata 单元、九项 metadata 集成（含四个 CLI 场景）和原版 18 组离线策略/adapter 对照及三组端口生成通过，原七项 X.509 测试继续通过。完整回归及真实 GCE/绑定 token 等剩余范围见 [72](72-metadata-mtls.md)。没有真实凭据、系统证书修改或云资源操作，整体目标继续进行。

最终默认／无默认 497／495 项 Rust、两种全目标 Clippy、fmt、工具 175 通过／1 平台跳过及默认 debug 构建通过，172 份输入收尾一致。另复现并修复显式 `:80` 被 HTTP URL 归一化后错转 443 的问题，IPv4/IPv6、默认端口、路径和 query 均有回归；原版三组端口构造对照通过。修正前已停止旧回归，其部分结果不记最终通过。

## 81. 发行候选上游声明

候选打包增加已知上游来源、三份许可文本与校验和，声明缺失或无效时原子失败并清理暂存目录。发行专项 10 项通过，全量工具 177 通过／1 跳过；本轮未修改 Rust 或访问云资源。源码快照扫描及尚未完成的授权、依赖和远程发行范围见 [73](73-release-notices.md)，整体目标未完成。

## 82. 四目标依赖许可材料与当前 release

新增锁定依赖图材料收集与 CI 逐字节复核，267 个实际 normal/build 包、198 份去重原文随候选分发并纳入校验和，旧 Cargo.lock 材料拒绝打包。工具 182 通过／1 跳过，当前 macOS ARM64 release 构建与十八项本机检查通过，详见 [74](74-rust-dependency-licenses.md)。完整许可义务、U12 与实际远程发行仍未关闭；整体目标继续进行。

## 83. 合并版文件命令与 JSON 字节对齐

实际加载固定官方 CLI 与扩展，新增十二组文件命令 loopback 对照，覆盖账户/认证边界、单次路径编码、上传 payload、下载字节及失败不覆盖。发现并修正 JSON 中文/emoji/DEL 未按原版 ASCII 转义和目录下载错误输出差异，最终十二组全部一致。三项探针反向测试通过，完整回归及仍未覆盖范围见 [75](75-merged-files-and-json-bytes.md)。没有真实凭据或云资源操作，整体目标未完成。

## 84. JSON 下载数值精度

修正下载 JSON 的超 64 位整数精度丢失及浮点指数格式，局限于文档下载处理，不修改全局 JSON 解析特性。最终十八组合并版对照一致，含 1024 个有限浮点、4001-bit 整数、嵌套重复键、非规范数值和转义；五项 Rust 测试覆盖正常及失败边界。完整回归、有限浮点限制与未证明范围见 [76](76-json-number-download-compatibility.md)，整体目标未完成。

## 85. 当前源码的原生发行复验

纳入 JSON 下载修正后的 macOS ARM64 release 已重新构建并通过十八项打包和执行检查。二进制为 9,257,296 字节，源码、许可材料和二进制摘要见 [macOS 发行复验](../research/native-release-macos-current.json)。Linux 验证器已将发行声明和许可材料纳入前后输入哈希核对。

Linux ARM64 的两种特性 Rust 测试通过 502／500 项。首次 Python 回归定位到子进程就绪竞态，加入就绪同步后 187 项通过。Linux 优化构建通过十八项原生发行检查，两份 ARM64 二进制已合并打包并通过完整校验和检查。过程与首次失败记录见 [Linux 隔离复验](54-linux-container-validation.md)。

## 86. 四目标本地发行候选

同版 x86_64 macOS 和 Linux 均完成编译、全量 Rust 回归与优化二进制执行，各通过 502／500 项 Rust 测试。macOS 通过 Rosetta，Linux 在 ARM64 Docker daemon 上模拟执行。Linux x86_64 另通过 Rust 1.85.1 两种特性的编译检查和 187 项工具测试，默认特性有 5 项 Secret Service 专项按配置跳过。打包工具生成四份二进制、更新清单和保留内部许可路径的 ZIP，六项文件校验和均通过。详见 [四目标候选记录](../research/current-four-target-release-candidate.json)。

## 87. 真实候选自更新演练

macOS ARM64 使用四目标候选中的 0.1.0 二进制与临时构建的 0.0.0 客户端，验证 loopback 损坏下载拒绝、实际自替换、同版不下载和离线回滚。安装与回滚后的文件字节和权限均已核对。详见 [验证结果](../research/native-update-rehearsal.json)。

## 88. 历史行数与合并版对齐

`log --lines -N` 在事件类型过滤后跳过最早的 N 条。正数保留末尾，零不限制。合成历史覆盖旧版和 Rust 格式文件，并验证文本、JSON 和导出。与实际固定合并版对照，七组文本的退出码和输出一致，过滤后的 JSONL 字节一致。详见 [兼容结果](../research/log-lines-compatibility.json)。

## 89. Notebook 多行格式与附件

历史导出和文本模式执行写回共用 nbformat 的文本换行规则。SVG、JavaScript 和附件文本按行保存，PNG 与 JSON 保留原始表示。三组历史 Notebook 与固定合并版对照通过。兼容、历史和 Jupyter 测试共 93 项通过，更新后的 CLI 执行写回用例另行复验通过。详见 [验证记录](../research/notebook-line-compatibility.json)。

## 90. Notebook ID 与历史对应

输出 Notebook 至少为 nbformat 4.5，保留较新的次版本和合法 ID 的首次出现。缺失、重复或非法字符串 ID 获得新 ID。运行历史保存相同的 ID 和原 cell 索引。断线用例同时核对了部分输出和原始输入字节，另用 Python nbformat 验证三种实际 CLI 写出文件。详见 [验证记录](../research/notebook-cell-identity-validation.json)。

## 91. Notebook 标题与最近执行记录

Notebook 进度和最近执行记录共用上游 `@title` 语法与 cell ID 回退规则。七组标题样本与上游正则结果一致，CLI 的文本提示与持久状态通过本地 WebSocket 验证。详见 [标题对照](../research/notebook-title-compatibility.json)。

## 92. 释放前关闭已记录的 kernel

`stop` 和自有 `run` 会话清理先按 endpoint 获取最新的运行时代理凭据，再尝试关闭本地记录的 kernel。删除失败仍继续释放 VM。重试先确认原 endpoint 是否仍存在，只有控制面确认释放或确认资源已消失后才清理本地记录。loopback 验证了请求顺序、代理凭据、失败回退、替换 endpoint 隔离，以及执行中停止和 `run` 清理。真实 Colab 的新关闭顺序仍待复验。详见 [控制面释放流程](10-control-plane-implementation.md)。

使用合成会话直接运行固定官方 `02d2321` 的 `stop`，观察到一次 `shutdown_kernel=True` 调用、零次运行时 HTTP 请求和一次模拟 unassign。Rust 的显式 DELETE 对应功能设计的 kernel 关闭要求。

## 93. 分配拒绝与结果未知的恢复

分配 POST 前先原子记录尝试标记。首次明确拒绝（HTTP 400 或 412）允许重新选择硬件。更早的 POST 结果未知时，即使随后被拒绝也继续保留原 nbh。旧日志按可能已发出 POST 处理。GPU/TPU 的文本错误提示与固定官方 CLI 一致。loopback 验证了拒绝后改用 CPU、网关错误后保持原标识、旧日志恢复以及 `run` 清理结果。详见 [控制面操作日志](10-control-plane-implementation.md#操作日志与并发)。

## 94. 待恢复分配期间释放已有会话

同一账户的新分配结果未知时，仍可用 `stop -s NAME` 释放另一台已有会话。释放意图单独持久化，后端确认后可按原 nbh 恢复分配。loopback 测试覆盖释放 POST 返回 504、本地会话被裁剪后的 CLI 账户路由，以及暂存 endpoint 的别名保护。详见 [控制面操作日志](10-control-plane-implementation.md#操作日志与并发)。

## 95. 自动选择与停止会话共用一次查询

省略 `-s` 时，`stop` 在同一账户锁内同步、选择和释放会话，用同步结果提供运行时代理凭据。loopback 实际 CLI 测试核对了单次查询、过期记录裁剪、kernel DELETE、VM 释放，以及同名会话已换 endpoint 时的保护。详见 [控制面实现](10-control-plane-implementation.md#操作日志与并发)。

## 96. 当前构建的真实 CPU 控制面复验

使用现有系统钥匙串实际分配一台 CPU，核对后端持有的 endpoint、具名状态、远端执行结果和已持久化的 kernel ID。具名 `stop` 退出 0，独立后端查询确认 VM 消失，且本地会话与操作日志已清理。详见 [线上复验](15-live-cpu-validation.md#2026-11-18-控制面复验)。

## 97. 真实远端终端输入验收

现有构建在 macOS PTY 上完成真实 Colab CPU 的 Unicode `input()` 和合成密码 `getpass()` 往返。核对了密码回显关闭、退出后恢复及 JSON/历史输出隔离，测试 VM 已独立确认释放。详见 [执行验收](12-jupyter-and-execution-implementation.md#显式终端输入)。

## 98. REPL 的 endpoint 条件

交互与管道 REPL 接受与 exec 相同的显式 endpoint 条件。CLI 测试覆盖匹配后正常执行、同名 endpoint 不匹配时拒绝连接、缺少显式会话名时拒绝启动。六个常规管道 REPL 场景与固定官方 CLI 的结果一致。详见 [REPL 实现](18-repl-and-websocket-implementation.md)。

## 99. 历史优先跟随本地会话归属

`log -s NAME` 在未显式选择账户时优先读取当前本地会话所属账户的历史。会话已停止时保留按历史查找的能力。CLI 回归和固定合并版对照验证了同名隔离、选择优先级和歧义。详见 [历史账户路由](13-run-and-history-implementation.md#历史账户路由)。

## 100. 跨账户会话路由提示

会话从其他账户唯一命中时，常规会话命令、URL 和 Drive 运行时命令在 stderr 提示所选账户。显式账户、环境选择和 active 命中无需路由提示。SSH 仅在 stdout 为终端时显示提示。合并版 URL 对照十四组中八组一致，详见 [URL 对照](27-utility-commands-implementation.md#后续实际合并版-url-对照)。

## 101. 同名会话的候选账户诊断

多个非当前账户命中同名会话时，CLI 按注册表顺序列出候选账户，并提示通过 `-a` 明确选择。当前账户命中仍优先。固定合并版 URL 与历史命令的歧义场景均核对退出码、stdout 和 stderr。详见 [历史账户路由](13-run-and-history-implementation.md#历史账户路由)。

## 102. 省略会话名的 URL 同步

`colab url` 在存在本地会话时查询后端并从有效命名会话中自动选择。查询失败时保留本地记录，具名 URL 路径保持离线，成功的 stdout 保持单行。详见 [URL 同步对照](27-utility-commands-implementation.md#隐式会话同步对照)。

## 103. 浏览器链接的邮箱与会话诊断

省略会话名且有多个有效会话时，`colab url` 在 stderr 列出可用名称。具名会话缺失提示与固定合并版一致。新增合并版对照覆盖计算账户邮箱缺失和为空，均省略 authuser。详见 [URL 对照](27-utility-commands-implementation.md#后续实际合并版-url-对照)。

## 104. 真实 CPU 执行空闲期限验收

真实 CPU 验证通过持续输出超过空闲期限、有限期限内静默超时及 `--timeout 0` 三种场景。两次测试创建的 VM 均独立确认释放。新增 CLI 回归核对超时后的 msg_id 持久化和正常完成后的标记清除。详见 [执行验收](12-jupyter-and-execution-implementation.md#真实-cpu-空闲期限验收)。

## 105. 真实 CPU REPL 交互与中断复验

macOS PTY 验证通过中文多行、输入取消、Python 异常、远端 Ctrl-C、EOF 以及退出后由 `exec` 复用同一个 kernel。一次 63 秒静默执行正常完成。两次测试创建的 VM 均独立确认释放。详见 [REPL 复验](18-repl-and-websocket-implementation.md#真实-cpu-交互复验)。

## 106. 发行候选与原生验收报告核对

GitHub Actions 集中打包时核对四平台验收报告与实际产物的版本、目标、下载目录、大小和哈希。本地复用已有真实产物重打包，七份资产校验和与此前一致，单字节变更被拒绝且无部分输出。详见 [原生更新与发行](32-native-update-and-release-implementation.md#测试证据)。

## 107. GitHub Release 下载安装与回滚验收入口

原生更新演练支持以已核对的本地候选验证同一 GitHub Releases 仓库的公开清单，再从固定版本地址下载安装并验证离线回滚。本机候选演练通过，公开清单预检查返回 HTTP 404。详见 [发布后演练](32-native-update-and-release-implementation.md#github-release-发布后演练)。

## 108. 原生更新重定向与清单来源

原生更新在清单与发行物的重定向链中拒绝从公开主机进入回环地址。清单中的发行物地址按最终清单来源核对。测试覆盖公开 HTTPS 带签名参数、常见回环地址、HTTPS 降级和真实 loopback 签名参数下载。详见 [清单与校验](32-native-update-and-release-implementation.md#清单与校验)。

## 109. DriveFS 刷新结果诊断

受管 shim 记录最近的脱敏刷新结果，`drive status --json` 只在对应进程运行时读取。通过可控时钟与合成 OAuth 验证旧 token 到期、`invalid_grant` 诊断、重新刷新恢复、写入失败隔离及卸载清理。真实挂载的长时间读取与撤销仍待验收。详见 [Drive 状态协议](23-drivefs-probe-and-recovery.md)。

## 110. `exec` 文本超时诊断

执行器区分本地等待超时与其他结果未知的原因。文本 `exec` 提示设置的秒数、空闲或总期限以及 `--timeout` 调整选项，保留消息 ID 与重放保护。CLI 回归核对文本和 JSON 的恢复标记及输出通道。固定官方对照的三项中两项一致，静默超时保留 stderr 文案差异。详见 [执行空闲期限](12-jupyter-and-execution-implementation.md#真实-cpu-空闲期限验收)。

## 111. 当前构建的真实 CPU 文件验收

真实 CPU 运行时通过包含中文和保留字符的文件路径、目录和单文件元数据、二进制字节往返、缺失新建、无变更编辑、删除及删除后 404。测试会话已独立确认释放。详见 [文件命令复验](15-live-cpu-validation.md#2026-11-18-文件命令复验)。

## 112. Workforce 用户来源的服务账号模拟

ADC 支持将 `external_account_authorized_user` 作为 `impersonated_service_account` 的源凭据。回归核对 STS、IAM、配额分离、IAM 失败恢复及本地 ADC 文件字节。离线诊断可识别该来源。详见 [Workforce 实现](37-adc-workforce-user-implementation.md#workforce-用户作为服务账号模拟来源)。

## 113. 发行渠道随版本生成

候选工作流根据仓库和 Cargo 版本生成固定下载目录与公开清单地址，构建、原生验证和打包共用。通过本地工作流配置和四目标候选验证，七份发行资产哈希一致，输入旧 GitHub tag 在构建前被拒绝。详见 [打包与 GitHub Actions](32-native-update-and-release-implementation.md#打包与-github-actions)。

## 114. 全量 CLI 对照与改名后的参数提示

固定原版对照覆盖 370 个隔离场景。复测发现改名后二进制的 `run` 缺脚本提示有差异，现按缺失参数识别该场景。Rust 回归核对标准名称和改名后均返回原版提示、退出码 2，且无配置目录写入。[CLI 全量复验](14-cli-comparison.md#2026-11-18-全量复验)

## 115. `run` 的脚本模块与 traceback

`run` 在临时 `__main__` 模块中执行脚本，退出后恢复 kernel 的模块和参数，并移除包装器名称。UTF-8 BOM 与 shebang 处理保留剩余脚本物理行号。本机真实 IPython kernel 验证模块访问、dataclass/pickle、`SystemExit(23)`、traceback 与后续 kernel 状态。[运行与历史实现](13-run-and-history-implementation.md#验证与完整目标缺口)

## 116. `run` 共用账户认证客户端

分配、执行和释放共用本次命令的认证客户端，避免新阶段重新读取仍有效的钥匙串凭据或丢失 ADC 内存中的 refresh token 轮换状态。凭据刷新失败时保留释放日志供恢复。[钥匙串实测与剩余缺口](69-macos-keychain-code-identity.md#减少-run-生命周期内的重复访问)

## 117. Linux 原生更新自检

更新器在完成下载与同步后关闭写句柄。Linux 的临时 `ETXTBSY` 启动错误在同一自检期限内重试，文件校验、版本确认和原子替换仍是安装条件。Linux 容器通过 16 项更新测试，macOS 通过 15 项。[原生更新与发行](32-native-update-and-release-implementation.md)

## 118. DriveFS 元数据代理启动

元数据代理直接绑定数值地址 `127.0.0.1` 并记录内核分配的端口，启动过程中无需通过 HTTPServer 反查主机名。回归在反向查询被拒绝时验证绑定成功，进程测试继续核对私有就绪文件、实际 HTTP 响应及脱敏错误。[实现](../src/drivefs/metadata_shim.py) · [测试](../tools/tests/test_drivefs_shim.py)

## 119. 默认计算 OAuth 配置回退

`account add/login` 在默认配置文件不存在时获取固定上游公开客户端，核对 SHA-256 并保持 PKCE/SDK 回调流程。显式配置和已有默认文件失败不回退，兑换和刷新沿用已保存客户端；仓库不提交 client secret。联网边界、失败恢复和 DriveFS 区别见 [默认客户端说明](34-account-login-and-drive-consent-implementation.md#默认计算-oauth-客户端恢复2026-11-19)。
