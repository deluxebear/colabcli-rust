# Colab Rust 客户端

用 Rust 实现 Google Colab CLI 与 colab-ext 的功能合集，并增加保活监控、VM 自动重建及可选检查点恢复。当前提供多账户、OAuth、部分 ADC、本地存储，以及 Colab 控制面的会话查询、分配、释放、跨账户汇总、CCU 用量查询，Contents 文件操作，以及Python/Notebook 与一次性脚本执行、历史查询和导出；交互 REPL、SSH、DriveFS 和 supervisor 尚未实现。

## 构建与验证

本地应用不依赖 Python。`Cargo.toml` 声明 Rust 1.85+；当前在 macOS、Rust 1.99.1 验证，最低版本和其他平台尚待实测。

```sh
cargo build --locked
cargo run --locked -- --help
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --no-default-features
```

默认启用系统凭据库。`--no-default-features` 可构建不包含原生凭据库的版本，适合离线开发；它不能替代系统凭据库的互操作测试。Linux 原生后端需要可用的 Secret Service。

## 多账户

使用独立配置目录体验本地功能，避免与 Python 客户端同时修改同一份配置：

```sh
cargo run -- --config-dir /tmp/colab-rust-demo account add work
cargo run -- --config-dir /tmp/colab-rust-demo account use work
cargo run -- --config-dir /tmp/colab-rust-demo account current
cargo run -- --config-dir /tmp/colab-rust-demo account list --json
cargo run -- --config-dir /tmp/colab-rust-demo account add storage
cargo run -- --config-dir /tmp/colab-rust-demo -a work account set-drive storage
```

`account add` 仅登记资料，不登录或切换账户。支持 `rename`、`remove`、全局 Drive 默认绑定，以及中断事务的 `account repair`。删除只清理本地资料，不释放 VM。无 `--config-dir` 时使用 `~/.config/colab-cli`；环境变量为 `COLAB_CLI_CONFIG_DIR`。

## 认证

OAuth 需要用户提供 Google OAuth client 配置，支持 `installed`、`web` 或直接字段对象；仓库不包含 client secret。默认读取 `~/.colab-cli-oauth-config.json`，也可用 `-c` 指定：

```sh
cargo run -- -a work -c /absolute/path/client.json account login --url-only
cargo run -- -a work account login --code 'AUTHORIZATION_CODE'
```

两次调用必须使用相同配置目录和账户。也可用 `--callback-url '完整回调URL'` 兑换并校验 state；尚无自动回调监听器。client、redirect、scope 是否被 Google 接受仍需真实授权验证。

`--token-storage auto|keyring|file` 控制凭据存储。`auto` 在系统凭据库写入失败后使用受限权限文件，并保持文件为权威来源；文件内容未加密。ADC 当前仅支持 `authorized_user` JSON：

```sh
cargo run -- account add gcloud --adc-file /absolute/path/application_default_credentials.json
cargo run -- account login gcloud
```

ADC 登录只验证取得 token，不代表具有 Colab 权限。

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

分配中断后，使用同一账户、名称和硬件参数重试 `new`；释放失败后重试 `stop`。逐账户操作日志保留 notebook identity 和清理意图，禁止手动删除；有待恢复操作时拒绝账户重命名或删除，包括 `--force`。详见 [控制面实现与恢复约定](docs/10-control-plane-implementation.md)。这些示例会操作真实 VM，自动测试仅使用 loopback HTTP。

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
cargo run -- exec -s cpu-demo -f experiment.ipynb --output-image plot.png
cargo run -- kernels -s cpu-demo --json
cargo run -- restart-kernel -s cpu-demo
```

exec 复用已有 kernel；普通命令退出保留 VM/kernel。Notebook 输出写入 `<stem>_output.ipynb`，失败时保留已有输出。超时表示本次相关消息的空闲期限，0 为无限等待；断线或超时后不会自动重放代码。结果未知时保存消息 ID 并阻止新执行，先核查或显式重启 kernel。`stop` 可以从另一终端释放运行中的 VM。详见 [执行核心实现](docs/12-jupyter-and-execution-implementation.md)。

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

`src/` 包含账户、上下文、认证、凭据后端、存储、控制面协议、会话服务、Contents 和有界账户汇总模块；`tests/` 使用临时目录、内存凭据库和本地 HTTP 服务。默认测试 139 项通过，关闭原生凭据库后 138 项通过；未使用真实 Google 账户或创建 VM。

- [基础模块实现状态与边界](docs/09-foundations-implementation.md)
- [完整研究与实现规格](docs/README.md)
- [Rust 架构与实施顺序](docs/05-rust-architecture.md)
- [保活监控与自动恢复设计](docs/08-supervision-and-recovery.md)

研究工具在 `tools/`，固定上游提交与离线证据在 `research/`。GitHub Actions 已配置 Linux/macOS 检查，尚未远程执行。
