# Rust 基础模块实现状态

更新日期：2026-11-16。本文记录实际代码与离线验证结果；01–08 仍是完整产品的研究与目标规格。当前交付包含 P1 基础模块、P2 控制面/Contents/汇总与 P3 执行核心、run 与历史导出，不代表全部 ADC 类型、线上验收或完整客户端已经完成。

## 1. 已实现范围

| 模块 | 当前能力 | 代码 |
| --- | --- | --- |
| CLI | `version`；`account`/`acct` 下的 list、current、add、use、set-drive、rename、remove、login、repair | [main.rs](../src/main.rs) |
| 账户 | 注册表、名称/email 选择、active、全局及逐账户 Drive 绑定、旧配置复制、事务恢复 | [accounts.rs](../src/accounts.rs) |
| 上下文 | 显式参数、环境、会话归属、active 的优先级；Drive 独立解析；ADC 路径解析 | [context.rs](../src/context.rs) |
| 认证 | OAuth 两步授权、state/PKCE、刷新与轮换、失败暂存恢复；authorized_user ADC | [auth.rs](../src/auth.rs) |
| 凭据 | `auto`/`keyring`/`file`、旧 keyring 惰性迁移、系统后端等待期限 | [credentials.rs](../src/credentials.rs) |
| 脚本与历史 | run、信号取消、清理计划恢复；log 过滤与四种导出 | [runs.rs](../src/runs.rs)、[history.rs](../src/history.rs) |
| 执行 | Jupyter REST/DEFAULT WS、exec Python/Notebook、kernel 复用与显式恢复 | [jupyter](../src/jupyter/mod.rs)、[execution.rs](../src/execution.rs) |
| 文件 | `ls/upload/download/rm/edit`、限额、路径编码、下载原子替换、编辑失败恢复 | [contents.rs](../src/contents.rs) |
| 汇总 | `status/sessions --all`、`usage --all`，最多四账户并发与逐账户错误 | [inventory.rs](../src/inventory.rs) |
| 控制面 | `new`、`status`/`sessions`、`stop`、`usage`；逐账户分配/释放日志与故障恢复 | [colab_api.rs](../src/colab_api.rs)、[sessions.rs](../src/sessions.rs) |
| 存储 | JSON 模型、未知字段保留、受限权限、原子写入、进程锁、会话与设置读改写 | [storage.rs](../src/storage.rs)、[models.rs](../src/models.rs) |

命令行为有明确的阶段差异：`account add` 不自动登录或切换 active；`list --json` 输出注册表；列表按账户名排序；错误统一退出码 1。完整上游 help、JSON 与退出码差分尚未验收。Drive 绑定只写入配置，不执行挂载。

账户名限 64 字符以内的 ASCII，首字符为字母或数字，后续允许字母、数字、`.`、`_`、`@`、`-`，禁止 `..`；忽略大小写后不能重名。email 可作为选择器，多个账户匹配时明确报错。

## 2. 本地使用

```sh
cargo build --locked
cargo run -- --config-dir /tmp/colab-rust-demo account add work
cargo run -- --config-dir /tmp/colab-rust-demo account use work
cargo run -- --config-dir /tmp/colab-rust-demo account list --json
cargo run -- --config-dir /tmp/colab-rust-demo account add storage
cargo run -- --config-dir /tmp/colab-rust-demo -a work account set-drive storage
cargo run -- --config-dir /tmp/colab-rust-demo account set-drive storage --default
cargo run -- --config-dir /tmp/colab-rust-demo account set-drive --default --clear
```

配置目录优先 `--config-dir` / `COLAB_CLI_CONFIG_DIR`，否则为 `~/.config/colab-cli`。账户解析优先显式参数、`COLAB_ACCOUNT`、适用的会话归属、active；同名会话优先 active，多账户歧义报错。Drive 环境变量为 `COLAB_DRIVE_ACCOUNT`，兼容 `COLAB_DFS_ACCOUNT`。会话路由已接入 status/stop，也识别待恢复操作；执行与挂载命令尚未实现。

## 3. 存储与恢复约定

配置根目录包含 `accounts.json`、`settings.json`；逐账户文件在 `accounts/<name>/`，包括 `sessions.json`、`control_operation.json`（仅有待恢复资源操作时存在）、`oauth_token.json`、授权 pending 和复制的历史/Drive 凭据。默认账户初始化会非破坏性复制旧布局，保留源文件，不覆盖已存在的目标文件。未知 JSON 字段保留；损坏 JSON、未来注册表版本直接报错，避免覆盖原件。

Unix 目录为 0700、写入文件为 0600，JSON 写入采用同目录临时文件、fsync 和原子替换。锁默认等待 5 秒；账户生命周期锁放在独立 `locks/` 目录，避免重命名目录导致两个锁域。路径检查拒绝目录穿越与配置树内符号链接，但未声称能抵御同用户恶意进程的全部文件系统竞争。

重命名和删除通过 `account-transaction.json` 记录进度。中断时拒绝后续普通账户操作，运行以下命令继续完成事务：

```sh
cargo run -- --config-dir /absolute/path/config account repair
```

重命名保留源目录直至目标凭据与注册表提交，更新 active、默认 Drive 和所有绑定。旧认证上下文与会话写入器不能重新创建已删除/改名的资料。删除仅处理本地状态：已有 sessions 需 `--force`，待恢复控制面操作或非空 managed 目录即使 force 也拒绝，等待 supervisor 接入后的显式资源处置。

Python 与 Rust 的共同写锁兼容性尚未建立，**禁止同时修改同一配置目录**。Rust 授权 pending 与事务日志为本项目格式，不承诺恢复 Python 的未完成授权。

## 4. 凭据与认证

系统 keyring 使用 service `google-colab-cli`、username `oauth_token:<profile>`；旧 `oauth_token` 在 default 需要认证时惰性导入并保留源值。列表和注册不访问系统凭据库。显式 file 模式的旧凭据迁移只读取旧文件，不访问或导入旧 keyring；账户 rename/remove 的跨后端清理保护仍保持原有要求。

`auto` 优先尝试系统后端；写入失败后保存私有权限文件和 `oauth_keyring_fallback.json` 标记，之后继续使用文件，避免后端超时但延迟完成的写入覆盖最新凭据。文件内容未加密；显式 `keyring` 成功写入后才清除文件和标记。原生调用的调用方等待期限默认 2 秒，可用全局 `--keyring-timeout 1–300` 或 `COLAB_KEYRING_TIMEOUT` 调整（整数秒）；无法取消 OS 操作，未结束前拒绝同一后端的新调用。等待超时、无访问权限、凭据条目歧义/编码错误及未编译原生后端分别给出脱敏原因，禁止打印 keyring 原始错误内容。

重命名和删除会严格检查或清理 keyring，避免残留秘密；**即使选择 file 模式，系统后端不可用时也可能失败**。关闭 `native-keyring` 特性不会悄悄跳过此保护。前置检查失败可修复后重试；若已留下事务日志则运行 repair。

OAuth client 配置由用户提供，默认 `~/.colab-cli-oauth-config.json`，或 `-c /absolute/path/client.json`，支持 installed/web/直接字段对象。client 与默认 SDK redirect 的配对仍需 Google 线上验证。例：

```sh
cargo run -- -a work -c /absolute/path/client.json account login --url-only
cargo run -- -a work account login --code 'AUTHORIZATION_CODE'
```

两步流程绑定账户、client、redirect、state、PKCE 和 15 分钟有效期。`--callback-url` 校验完整回调与 state；裸授权码流程无法验证返回 state，依靠 pending 绑定与 PKCE。暂无自动监听回调或浏览器启动。授权码交换开始后即禁止重放；网络结果未知时须重新授权。成功响应在凭据提交前暂存，存储失败可再次 finish，无需重复兑换。

访问 token 到期前 60 秒刷新，以逐账户进程锁合并并发刷新；保存轮换后的 refresh token 和实际 scopes，不主动扩大 scopes。刷新结果也会暂存，存储失败可重试提交。HTTP 默认总超时 20 秒、连接超时 5 秒，禁止跟随重定向，限制响应体大小；错误不输出 token 响应正文。

ADC 路径顺序为 `GOOGLE_APPLICATION_CREDENTIALS`、账户指定文件、`CLOUDSDK_CONFIG`、用户 gcloud 默认位置；建议使用绝对路径。当前**仅支持 authorized_user**，每个 provider 独立持有凭据与刷新缓存，不修改进程环境，不回写 gcloud 文件。ADC 刷新返回的新凭据只在 provider 生命周期内缓存。service account、external account、impersonation、metadata server 尚未实现。成功取 token 不代表已确认 Google 身份或 Colab 权限。

## 5. 验证结果

在本机 macOS、Rust 1.99.1 执行：

| 检查 | 结果 |
| --- | --- |
| `cargo test --locked` | 152 项通过：账户 17、认证 17、CLI 6、控制面 24、Contents 16、凭据 5、汇总 3、Jupyter 31、历史 10、run 7、存储 14、原生后端逻辑 2 |
| `cargo test --locked --no-default-features` | 150 项通过；不包含两项原生后端等待与错误脱敏测试 |
| `cargo fmt --check` | 通过 |
| 默认及 no-default-features 的 `cargo clippy --locked --all-targets -- -D warnings` | 均通过 |
| `cargo build --locked` | 通过 |

测试使用临时目录、注入的内存凭据库和 loopback HTTP；覆盖 8 个 CLI 子进程并发写入、真实 CLI 两步兑换、账户与 ADC 隔离、PKCE 报文、刷新并发、迁移中断和凭据写入失败。测试入口见 [accounts](../tests/accounts.rs)、[auth](../tests/auth.rs)、[cli](../tests/cli.rs)、[storage](../tests/storage.rs)、[control_plane](../tests/control_plane.rs)、[contents](../tests/contents.rs)、[inventory](../tests/inventory.rs)、[jupyter](../tests/jupyter.rs)、[history](../tests/history.rs)、[runs](../tests/runs.rs)。原生后端逻辑测试使用注入闭包，未读写真实系统凭据库。

[CI](../.github/workflows/ci.yml) 已配置 Linux/macOS，尚未远程执行；Rust 1.85 最低版本尚未实测。本阶段没有使用真实 Google 凭据、登录 Google 或创建 VM，因此系统 keyring 的 Python 互通、Google OAuth 接受情况、账户身份和 Colab 权限均待验证。

## 6. 后续接入边界

控制面 HTTP、会话查询/分配/释放与用量现已接入，详细行为、操作日志恢复与兼容差异见 [10](10-control-plane-implementation.md)。Contents 和有界账户汇总见 [11](11-contents-and-inventory-implementation.md)；P2 仍缺真实 CPU/文件往返/释放验收。Jupyter 连接与非交互 exec 见 [12](12-jupyter-and-execution-implementation.md)。run、历史与导出见 [13](13-run-and-history-implementation.md)。交互 stdin、REPL、SSH、DriveFS、VM 保活、自动重建、任务检查点恢复均未实现；supervisor 设计见 [08](08-supervision-and-recovery.md)。完整验收编号继续以 [06](06-validation-and-open-questions.md) 为准，不能因本阶段离线通过而整体勾选完成。

## 7. sessions 凭据排障（2026-11-16）

本机 `target/debug/colab sessions` 已复现为系统钥匙串读取超过默认两秒，尚未发出控制面请求。当前账户没有文件凭据，默认 `~/.colab-cli-oauth-config.json` 也未配置；这不能证明钥匙串里没有旧凭据，因此 auto 读取失败仍报凭据库故障，不伪装为未登录。

先解锁/允许钥匙串访问，再运行 `./colab --keyring-timeout 30 sessions`。若主动选择文件后端，需先以 `./colab --token-storage file -c /absolute/path/client.json account login --url-only` 发起授权，随后同一账户/配置执行 `./colab --token-storage file account login --code AUTHORIZATION_CODE`，再 `./colab --token-storage file sessions`。file 不会解锁或导出钥匙串凭据；没有登录时 sessions 会提示需要登录与 client 配置路径。不要在聊天或日志中分享授权码、client secret 或 token。

新增 [credentials](../tests/credentials.rs) 五项回归测试，覆盖 file 迁移隔离、旧文件迁移、auto 不将不可访问当作未登录、文件凭据优先及 CLI 参数/登录提示；原生后端另覆盖超时类别与敏感错误安全映射。file 迁移隔离测试修复前明确失败、修复后通过。

## 8. 分配恢复与会话显示兼容修复（2026-11-16）

移除准备响应 acc/nbh/variant 与请求逐字匹配的额外校验；保留响应类型、variant 枚举和非空 token 校验，POST 使用原请求参数与返回的 token。该额外校验是 `new` 报“分配准备信息与请求不匹配”的原因，与固定 Python 上游仅消费准备 token 的逻辑不符。

普通分配失败且尚未暂存 endpoint 时，允许相同硬件参数改用新名称恢复，例如无名 `new` 失败后执行 `new -s test`。日志先原子保存新名称，始终保留原 nbh；已有同名会话、已暂存 endpoint、run 所有者及不同硬件参数仍受保护。不要删除现有 control_operation.json 来解除阻塞。

`sessions` 与 `status` 拆为独立命令：前者包含后端未命名会话，以 `?` 显示；后者仅显示本地命名会话，增加状态和上次执行信息。补齐原版空列表提示、CPU 与 Standard/High-RAM 标签；JSON 保留安全字段，不输出凭据。`--all` 仍是本项目的多账户扩展。详见 [10](10-control-plane-implementation.md)。

新增六项离线回归测试，覆盖非回显准备元数据、无名失败后命名恢复、恢复再次失败后的 nbh 保留、已有会话保护、已暂存/有所有者操作保护及真实 CLI sessions/status 文本差分。只使用临时配置和 loopback HTTP，未创建真实 VM；完整线上兼容仍待验收。

## 9. 空 POST 的 HTTP 411 修复（2026-11-16）

控制面 assign/unassign 的 POST 原先未显式声明 Content-Length；严格 HTTP 前端可因此拒绝请求并返回 411。已用要求 `Content-Length: 0` 的 loopback 服务复现分配失败，再在共享控制面请求层为两种空 POST 显式发送该头。query、XSRF token 与空请求体契约保持一致，不用 JSON `{}` 替代空 body。

新增一项测试同时覆盖分配和释放，确认长度为零、没有 Transfer-Encoding、请求体为空，且分配成功响应和释放 204 均正确处理。修复前测试收到 411，修复后通过。原操作日志及 nbh 无需清除，重新构建后可继续 `new -s test`；本次未发出真实 Google 分配或释放请求。
