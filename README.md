# Colab Rust 客户端

用 Rust 实现 Google Colab CLI 与 colab-ext 的功能合集。长期运行策略、VM 自动重建与任务检查点由任务脚本或外部调度器负责。当前提供多账户、OAuth、部分 ADC、本地存储，以及 Colab 控制面的会话查询、分配、释放、跨账户汇总、CCU 用量查询，Contents 文件操作，以及 Python/Notebook 与一次性脚本执行、历史查询和导出、VM 授权与官方 Drive 挂载、交互 REPL 与 Unix SSH；扩展 DriveFS 的独立凭据已接入，远端控制器已离线实现，挂载 CLI、默认 drivemount 策略与恢复入口已接入。原版 WebSocket 通信、心跳、状态与断线处理继续属于客户端范围。

## 构建与验证

本地应用不依赖 Python。`Cargo.toml` 声明 Rust 1.85+；当前在 macOS、Rust 1.99.1 验证，最低版本和其他平台尚待实测。

```sh
cargo build --locked
cargo run --locked -- --help
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --no-default-features
# 以下只测试远端 Python 组件，不是本地客户端的运行依赖：
python3 -B tools/tests/test_drivefs_shim.py
python3 -B tools/tests/test_drivefs_controller.py
```

已实现子命令的官方/Rust 自动对比方法、报告及兼容差异见 [CLI 自动对比](docs/14-cli-comparison.md)。

默认启用系统凭据库。`--no-default-features` 可构建不包含原生凭据库的版本，适合离线开发；它不能替代系统凭据库的互操作测试。Linux 原生后端需要可用的 Secret Service。

## 多账户

使用独立配置目录体验本地功能，避免与 Python 客户端同时修改同一份配置：

```sh
cargo run -- --config-dir /tmp/colab-rust-demo account add work --configure-only
cargo run -- --config-dir /tmp/colab-rust-demo account use work
cargo run -- --config-dir /tmp/colab-rust-demo account current
cargo run -- --config-dir /tmp/colab-rust-demo account list --json
cargo run -- --config-dir /tmp/colab-rust-demo account status --json
cargo run -- --config-dir /tmp/colab-rust-demo account add storage --configure-only
cargo run -- --config-dir /tmp/colab-rust-demo -a work account set-drive storage
```

`account add` 默认登录，成功后尝试独立 Drive 授权并切换 active；`--no-drive` 跳过 Drive，`--no-use` 保留 active。`--url-only` 保存待完成授权后返回；只登记资料请显式加 `--configure-only`。详见 [联合授权与失败处理](docs/34-account-login-and-drive-consent-implementation.md)。支持 `rename`、`remove`、全局 Drive 默认绑定，以及中断事务的 `account repair`。删除只清理本地资料，不释放 VM。无 `--config-dir` 时使用 `~/.config/colab-cli`；环境变量为 `COLAB_CLI_CONFIG_DIR`。

`account status`（或 `acct status`）并发查询全部账户的 CCU、会话与 Drive 绑定，不受当前单账户选择限制；每行保留错误和已取得的余额，不能把退出 0 视为所有账户成功。它可刷新已有凭据，但不弹出交互登录；成功的会话查询会同步本地记录。`drive_ready` 只表示文件存在。详见 [账户状态说明](docs/29-account-status-implementation.md)。

## 认证

OAuth 需要用户提供 Google OAuth client 配置，支持 `installed`、`web` 或直接字段对象；仓库不包含 client secret。默认读取 `~/.colab-cli-oauth-config.json`，也可用 `-c` 指定：

```sh
cargo run -- -a work -c /absolute/path/client.json account login --url-only
cargo run -- -a work account login --code 'AUTHORIZATION_CODE'
```

两次调用必须使用相同配置目录和账户。也可用 `--callback-url '完整回调URL'` 兑换并校验 state；尚无自动回调监听器。client、redirect、scope 是否被 Google 接受仍需真实授权验证。

`--token-storage auto|keyring|file` 控制凭据存储。`auto` 在系统凭据库写入失败后使用受限权限文件，并保持文件为权威来源；文件内容未加密。ADC 当前支持 `authorized_user`、`service_account`、`impersonated_service_account`、文件/URL 来源的 `external_account` 及 `external_account_authorized_user` JSON：

```sh
cargo run -- account add gcloud --configure-only --adc-file /absolute/path/application_default_credentials.json
cargo run -- account login gcloud
```

ADC 登录只验证取得 token，不代表具有 Colab 权限。service_account 使用未加密 PKCS#8 RSA 私钥签署 JWT，按 Colab 的公开 scope 集合交换 token，不回写密钥文件。impersonation 支持内嵌的用户或服务账户来源及 IAM delegates，源 token 与目标 token 分别缓存，不把源身份传给 Colab。external_account 支持 STS（含可选 HTTP Basic 客户端认证）、可选 IAM 目标与 workforce user project，来源文件不会被回写。Workforce 用户 ADC 用客户端 Basic 认证刷新令牌，轮换只在当前进程缓存。ADC 在没有显式文件且默认 gcloud 文件不存在时可探测 GCE/GKE metadata；NO_GCE_CHECK=true 可禁用。Unix 可执行令牌来源需显式 GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES=1，支持非交互 JSON 和输出缓存。AWS aws1 支持环境快照和 EC2 IMDSv1/v2，生成签名主体供 Google STS 验证。X.509 workload ADC 支持证书主体与 STS/IAM mTLS；当前不支持 domain-wide subject、其他 universe、递归 impersonation 或 JWT/SAML 证书绑定 token；metadata 支持 GCE_METADATA_MTLS_MODE 的 none/strict/default 模式。支持矩阵与离线证据见 [服务账户](docs/30-adc-service-account-implementation.md)、[IAM impersonation](docs/31-adc-impersonation-implementation.md)、[外部身份](docs/36-adc-external-account-implementation.md)、[Workforce 用户](docs/37-adc-workforce-user-implementation.md)、[metadata](docs/38-adc-metadata-implementation.md)、[可执行来源](docs/39-adc-executable-implementation.md) 、[AWS](docs/40-adc-aws-implementation.md) 与 [STS 客户端认证](docs/70-external-account-client-auth.md) 与 [X.509/mTLS](docs/71-x509-adc-and-mtls.md) 与 [metadata mTLS](docs/72-metadata-mtls.md)。

## 会话控制面

先登录所选账户，再显式分配或释放 VM：

```sh
cargo run -- -a work new -s cpu-demo --json
cargo run -- -a work new -s gpu-demo --gpu T4 --high-mem
cargo run -- -a work sessions --json
cargo run -- status -s cpu-demo --json
cargo run -- -a work usage --json
cargo run -- stop -s cpu-demo
```

`sessions` 查询后端全部会话，未命名资源以 `?` 显示；`status` 显示本地命名会话的状态及上次执行信息。两者都会刷新本地 token，并裁剪已失效会话。失败分配可用相同硬件参数恢复；未暂存 endpoint 的普通分配可改用 `new -s NAME` 命名恢复，始终复用原 nbh，无需删除日志。`new` 仅使用显式/环境/active 账户，不按同名会话路由；`status -s` 和 `stop -s` 支持会话归属路由。JSON 和终端输出不包含 OAuth、proxy 或 XSRF token。

分配中断后，使用同一账户、名称和硬件参数重试 `new`；释放失败后重试 `stop`。逐账户操作日志保留 notebook identity 和清理意图，禁止手动删除；有待恢复操作时拒绝账户重命名或删除，包括 `--force`。详见 [控制面实现与恢复约定](docs/10-control-plane-implementation.md)。这些示例会操作真实 VM，离线回归使用 loopback HTTP；单次真实 CPU 验收见 [线上记录](docs/15-live-cpu-validation.md)。

## 文件操作与多账户汇总

以下命令只使用已有会话，不会自动分配 VM：

```sh
cargo run -- ls -s cpu-demo
cargo run -- upload ./local.bin content/remote.bin -s cpu-demo
cargo run -- download content/remote.bin ./copy.bin -s cpu-demo
EDITOR='code --wait' cargo run -- edit content/script.py -s cpu-demo
cargo run -- rm content/remote.bin -s cpu-demo
cargo run -- sessions --all --json
cargo run -- usage --all --json
```

上传/下载文件上限 32 MiB；下载校验成功后才原子替换本地文件。编辑只对 404 创建空文件，未修改时不上传；上传失败会保留本地编辑文件并显示恢复路径。`--all` 最多并发查询四个账户，逐账户保留结果或错误，不触发交互式登录。详细协议、兼容差异与未完成部分见 [文件接口与账户汇总实现](docs/11-contents-and-inventory-implementation.md)。

## 执行 Python 与 Notebook

```sh
printf 'x = 7\nprint(x)\n' | cargo run -- exec -s cpu-demo
cargo run -- exec -s cpu-demo -f task.py --env MODE=train --timeout 30
cargo run -- exec -s cpu-demo -f interactive.py --allow-stdin --timeout 120
cargo run -- exec -s cpu-demo -f experiment.ipynb --output-image plot.png
cargo run -- kernels -s cpu-demo --json
cargo run -- restart-kernel -s cpu-demo
```

exec 复用已有 kernel；普通命令退出保留 VM/kernel。Notebook 输出写入 `<stem>_output.ipynb`，失败时保留已有输出。默认超时表示本次相关消息的空闲期限；显式 `--allow-stdin` 需要 Unix 交互终端和 `-f`，允许远端 `input/getpass`，超时改为单次执行的总期限（包含输入等待）。0 为无限等待；断线或超时后不会自动重放代码。结果未知时保存消息 ID 并阻止新执行，先核查或显式重启 kernel。`stop` 可以从另一终端释放运行中的 VM。详见 [执行核心实现](docs/12-jupyter-and-execution-implementation.md)。

## VM 授权与官方 Drive 挂载

```sh
cargo run -- auth -s cpu-demo
cargo run -- drivemount -s cpu-demo /content/drive --upstream
```

这两个命令使用已有 VM 和所选算力账户。已授权流程可以无终端运行；需要人工输入时使用交互终端。auth 默认总期限 600 秒，drivemount 默认 30 秒，不隐式重放结果未知的执行。每次新操作使用独立 kernel，保留普通 exec 的变量环境；结果未知时使用 `automation-cleanup -s cpu-demo` 显式清理待恢复 kernel，创建 ID 未知时先用 kernels 核查。这里显式选择官方 Colab Drive 流程；专用协议与恢复约定见 [16](docs/16-colab-automation-implementation.md)。

`drivemount` 默认复用 `drive mount` 的独立 DriveFS 编排，采用计算账户绑定的 Drive 凭据，支持 `--readonly`、`--dry-run` 和跨账户。自动化建议加 `--no-fallback`；只有缺少凭据且没有恢复责任的普通交互终端可回退官方流程。显式 `--upstream` 或 `COLAB_DRIVE_DEFAULT=off` 选择官方策略，不能与 DriveFS 专用参数或受管挂载责任混用。两条路径都只操作已有 VM；现有 Drive 资料的真实 shim 挂载、读写、只读及云端清理已验证，首次 Google 授权、跨账户和长期刷新仍待验收，见 [真实验收](docs/65-live-drive-validation-and-recovery.md)。详见 [策略与测试](docs/28-drivemount-strategy-implementation.md)。

## 远端软件包安装

```sh
cargo run -- install -s cpu-demo numpy pandas
cargo run -- install -s cpu-demo -r requirements.txt
```

只操作已有 VM。requirements 在创建独立安装 kernel 前上传，执行结果未知时不自动重放；默认不限时，可用 `--timeout` 设置空闲期限。用法与未完成的真实安装验收见 [安装说明](docs/17-install-implementation.md)。

## 一次性脚本与历史

```sh
cargo run -- run --gpu T4 --env MODE=train -s training train.py --epochs 5
cargo run -- run --keep -s retained job.py
cargo run -- runs
cargo run -- run-cleanup RUN_UUID
cargo run -- log -s training --type execution --lines 10 --json
cargo run -- log -s training -o training.ipynb
```

run 的客户端选项放在脚本路径前，后续参数原样传给脚本。默认申请新 VM、执行并释放；--keep 保留 VM。清理失败保留 run 计划，可用 run-cleanup 恢复，恢复不会重放脚本。脚本非零退出码独立于清理结果；SIGINT/SIGTERM/SIGHUP 会请求取消等待并进入清理。历史还支持 `.md/.txt/.jsonl`，停止会话后仍可离线导出。详见 [run 与历史实现](docs/13-run-and-history-implementation.md)。

## 代码与文档

`src/` 包含账户、上下文、认证、凭据后端、存储、控制面协议、会话服务、Contents 和有界账户汇总模块；`tests/` 使用临时目录、内存凭据库和本地 HTTP 服务。完整默认/无默认特性测试结果见 [验收说明](docs/06-validation-and-open-questions.md)；远端 metadata、挂载控制器和包装器另有 41 项脚本测试通过，1 项 Linux 专属跳过，核心挂载 CLI 已接入，见 [挂载与租约说明](docs/24-drivefs-lifecycle-implementation.md)。另已执行单次真实 CPU 验收，范围与资源清理见 [记录](docs/15-live-cpu-validation.md)。

- [基础模块实现状态与边界](docs/09-foundations-implementation.md)
- [完整研究与实现规格](docs/README.md)
- [Rust 架构与实施顺序](docs/05-rust-architecture.md)
- [生命周期职责边界与历史方案](docs/08-supervision-and-recovery.md)

研究工具在 `tools/`，固定上游提交与离线证据在 `research/`。GitHub Actions 已配置 Linux/macOS 检查，尚未远程执行。

REPL 支持 `colab repl -s NAME` 和管道输入；Enter 提交，Ctrl-J/Alt-Enter 换行，EOF 或 `/quit` 退出。底层 WebSocket 每 60 秒发送协议心跳；这不保证 VM 不被回收。执行中断必须收到远端确认，断线或结果未知时禁止自动重放。详见 [REPL 与心跳实现](docs/18-repl-and-websocket-implementation.md)。

SSH 支持 `colab ssh -s NAME`、`colab ssh -s NAME -- CMD` 和 `--proxy-mode`。需要系统 OpenSSH；自动创建矩阵、--rm 清理与字节桥接约定见 [SSH 实现](docs/19-ssh-implementation.md)。Windows 原生 SSH 路径及真实 Colab SSH 验收仍待完成。

Drive 凭据支持 `drive consent/import/show/token/clear`，使用独立存储账户和私有文件；首次授权仍需用户同意。[授权与恢复说明](docs/20-drive-credentials-implementation.md)。远端控制器和私密输入适配已有离线实现；核心挂载 CLI 与本地恢复计划已接入，现有资料的真实读写/只读和上传后卸载已验证。各次失败、显式恢复及剩余缺口见 [真实验收](docs/65-live-drive-validation-and-recovery.md)，实现接口见 [控制器说明](docs/22-drivefs-controller-implementation.md)。

`drive status -s cpu-demo --path /content/drive --json` 只读核查已有 VM，不需要 Drive 凭据，可与未知挂载操作隔离运行。`drive probe-cleanup -s cpu-demo` 只清理其待恢复核查 kernel；创建结果未知时需先核实 ID，再显式传 --kernel-id。只读恢复见 [只读与凭据回收](docs/23-drivefs-probe-and-recovery.md)，核心挂载/卸载编排见 [挂载与租约](docs/24-drivefs-lifecycle-implementation.md)。

`drive mount -s cpu-demo --dry-run` 预览核心挂载参数；去掉 --dry-run 执行，`drive umount -s cpu-demo` 卸载并回收轮换凭据。`drive leases` 离线列出受管引用；VM 已释放但记录仍在时用 `drive lease-cleanup --endpoint ...`，服务端确认不存在后才清理。

DriveFS 也支持 `--from-env`、实验性 `--cli-token`、`--user-id/--email` 和 `--drive-extra=--flag`；输入优先级、持久化与轮换保护见 [来源与参数说明](docs/25-drivefs-inputs-implementation.md)。

`colab ext doctor --json` 只读检查本地账户、认证文件与恢复记录，不查询钥匙串或网络；`ext version` 显示内建能力，`ext install` 兼容旧入口但不修改 Python 环境。见 [诊断说明](docs/26-extension-diagnostics-implementation.md)。

`colab url -s work` 只读取本地会话并输出单行浏览器 URL，`--open` 才启动系统浏览器。`whoami --json` 会刷新所选账户并查询 Google 身份；`readme/README` 和 `skill/SKILL` 输出内嵌文档。详见 [工具命令说明](docs/27-utility-commands-implementation.md)。

`colab update --manifest URL` 检查原生发行，`--install` 校验并安装，`--status` 与 `--rollback` 离线核查和恢复。发行渠道选择 GitHub Releases，仓库为 [deluxebear/colabcli-rust](https://github.com/deluxebear/colabcli-rust)，尚未发布；可用 `COLAB_CLI_UPDATE_MANIFEST` 指定可信清单；预计清单地址为 `https://github.com/deluxebear/colabcli-rust/releases/latest/download/colab-update.json`。支持 Linux GNU/macOS 的 x86_64、aarch64，默认请求期限 30 秒。见 [更新与发行候选](docs/32-native-update-and-release-implementation.md)。

`colab --show-completion bash|zsh|fish|powershell|pwsh` 离线生成补全脚本；显式 `--install-completion bash|zsh|fish` 才安装到用户 shell 配置。`drive status --drive-account NAME` 可展示存储账户选择与本地凭据文件是否存在，仍只用计算账户查询 VM。见 [补全与状态选择](docs/35-completion-and-drive-status-implementation.md)。

`account list --json` 输出账户数组及本地认证、Drive、会话摘要；`current --json` 包含来源和文件路径。它们不刷新令牌或打开钥匙串；auto/keyring 缺少文件时 ready=null，表示未检查。[输出与兼容边界](docs/41-account-inventory-implementation.md)。

账户 `use/set-drive/rename/remove` 的确认输出与常见参数错误已按合并版对齐，写回保留账户顺序。新建 `--token-storage file` 重命名/删除仅处理文件；`account repair` 遵循日志中的后端策略，没有策略字段的旧日志仍需完成原有跨后端清理。[变更与恢复说明](docs/42-account-mutations-implementation.md)。

账户清单和变更会先校验根级 `-a`/`COLAB_ACCOUNT`；无效全局资料不会继续修改配置。`colab -a personal account set-drive storage -a training` 中，personal 是全局上下文，training 是绑定目标，两者独立。[选择规则与验证范围](docs/43-account-selection-implementation.md)。

`sessions/usage --json` 现在输出合并版账户行；`--all --json` 输出这些行的数组，包含 assignments、usage 和 error。账户查询失败也返回 JSON 和退出码 0，脚本必须检查 error；原有 kernel/执行详情仍可用 `status --json` 查询。[结构迁移与对照](docs/44-merged-inventory-implementation.md)。

配置原生发行源后，交互式账户清单/变更和 sessions/usage/status/new/stop 每日检查一次更新；2 秒请求超时，失败不阻断命令。settings.json 设置 `enable_update_check=false` 可关闭。JSON、管道与 SSH 不显示提示。[每日检查与缓存](docs/45-daily-native-update-implementation.md)。

执行输出现在按 Markdown、HTML、纯文本的顺序显示可读文本；JSON 与 Notebook 保留原始 MIME bundle。转换不抓取链接或执行网页脚本。[富文本显示与对照](docs/46-rich-text-output-implementation.md)。

文本执行输出支持终端 PNG 的 Kitty 分包协议；非终端不发送图片控制序列。没有 `--output-image` 时，PNG/JPEG 会保存到权限 0600 的系统临时文件，并输出路径，退出后保留供用户使用。JSON 不生成默认图片。[图片显示与清理责任](docs/47-terminal-image-output-implementation.md)。

Jupyter、SSH WebSocket 与 HTTP 请求支持环境变量指定的 HTTP/HTTPS CONNECT、SOCKS5/SOCKS5h 代理及 `NO_PROXY`；例如 `ALL_PROXY=socks5h://127.0.0.1:1080`，socks5 在本地解析目标域名，socks5h 交给代理解析。代理拒绝时返回错误，不退回直连。HTTPS 代理与目标端各自验证 TLS 证书。WebSocket 暂不支持 SOCKS4 或系统代理自动发现。[HTTP 选择规则](docs/48-websocket-http-proxy-implementation.md)与 [SOCKS5 验收边界](docs/49-websocket-socks-proxy-implementation.md)，以及 [HTTPS 代理与双层 TLS](docs/50-websocket-https-proxy-implementation.md)。

JPEG 输出现可在 TTY 中转换为 PNG 内联显示，默认/显式图片文件、JSON 与 Notebook 仍保留原始 JPEG；解码或超限错误只影响显示。[转换与限制](docs/51-jpeg-terminal-projection-implementation.md)。

ADC 现按 `GOOGLE_CLOUD_QUOTA_PROJECT` 和凭据文件的 `quota_project_id` 传递 `x-goog-user-project`，并区分 IAM 来源与目标项目；不会把该头传给 runtime REST/WS。[配额项目规则](docs/52-adc-quota-project-implementation.md)。

控制面客户端现保留账户绑定的凭据来源，请求前按需续期；会话清单和用量查询在 401 后最多刷新重试一次，分配、释放等操作不因 401 自动重放。[续期与失败恢复](docs/53-control-credential-refresh-implementation.md)。
