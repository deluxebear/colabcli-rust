---
name: colab-native-cli
description: 使用 Rust Colab CLI 管理多账户、会话、文件、远端执行和 DriveFS。
---

# Colab 原生 CLI

本地只需 colab 二进制；Python 代码在远端 Colab VM 执行。用 `colab help COMMAND` 查看已安装版本的参数，`colab readme` 阅读内嵌说明。

## 账户和认证

`account add NAME` 默认登录、尝试独立 Drive 授权并切 active；离线初始化必须使用 `--configure-only`。`--url-only` 只准备计算授权，后续 `login --code` 如需 Drive 应显式加 `--with-drive`。`--no-drive` 跳过 Drive，`--no-use` 保留 active。add 的普通 Drive 失败为警告，login 显式 Drive 失败返回非零；计算授权可能已经保存，应核对状态再重试。

- `colab account list --json`、`account current --json` 查询本地配置。
- `colab account status --json` 联网查询全部账户的余额和会话，不受 -a/COLAB_ACCOUNT 单账户选择限制。逐行检查 error；退出 0 允许部分失败，drive_ready 仅表示 Drive 凭据文件存在。
- 用 `-a PROFILE` 或 COLAB_ACCOUNT 选择计算账户；显式账户优先。不要修改进程环境切换其他任务的身份。
- 首次授权需用户同意：`colab -a PROFILE account login --url-only`，随后同一账户用 `account login --code CODE` 完成。
- `colab whoami --json` 会刷新所选 OAuth/ADC 并查询 Google 身份；`colab ext doctor --json` 只读本地配置，不请求 Google 或钥匙串。
- ADC 当前支持 authorized_user、service_account（未加密 PKCS#8 RSA）和 impersonated_service_account（内嵌前两种来源，可带 IAM delegates）。external_account 支持文件/URL 主体、STS 和可选 IAM 目标；external_account_authorized_user 支持 Workforce 用户 STS Basic 刷新，轮换仅在 provider 内存，不回写 gcloud。不支持递归 impersonation、domain-wide subject、证书来源或 metadata mTLS。无显式 ADC 且默认 gcloud 文件缺失时才允许 metadata 发现，可用 NO_GCE_CHECK=true 禁用；坏文件不能降级为机器身份。可执行来源只在 Unix 且显式 GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES=1 时运行，非交互、默认30秒、输出受限；doctor 不执行。AWS aws1 支持启动时环境快照和 EC2 IMDSv1/v2；仅生成并发送签名主体给 Google STS，不直接调用 AWS STS，不记录 key 或签名主体。取 token 成功不代表 Colab 接受该身份。不要输出、提交或写入脚本日志的 token、授权码、client secret、私钥或 JWT assertion。

## 会话、文件和执行

```sh
colab -a compute new -s work --json
colab -a compute sessions --json
colab -a compute url -s work
colab -a compute upload script.py content/script.py -s work
colab -a compute exec -s work -f script.py
colab -a compute download content/result.json result.json -s work
colab -a compute stop -s work
```

未指定账户时，可按唯一会话归属路由；歧义时必须显式指定。`sessions/status/usage --all --json` 汇总多个账户，不能把部分失败当作全部成功。

exec/REPL 复用正常 kernel，run 使用新 VM 并默认清理。`run --keep` 才保留 VM。脚本参数放在脚本路径之后，`--` 后的参数属于脚本。交互 SSH、一次性命令和 proxy 支持不同的创建/清理策略，先核对 `ssh --help`。

连接断开可能表示执行结果未知。先检查 status/kernels/待恢复记录，不重放未知代码、不自动创建替代 VM。长期运行、自动重建和任务检查点由用户脚本或外部调度器决定，CLI 不承诺无限保活。

## 独立 DriveFS

```sh
colab -a storage drive consent --url-only
colab -a storage drive consent --code CODE
colab -a compute account set-drive storage
colab -a compute drive mount -s work --dry-run
colab -a compute drive mount -s work --readonly
colab -a compute drive status -s work --json
colab -a compute drive umount -s work --json
```

根命令 `drivemount` 默认使用同一 DriveFS 编排；自动化加 `--no-fallback`。`--upstream` 显式选择计算账户的官方 Colab 授权流程，不能与跨 Drive 账户或 DriveFS 专用参数组合，也不能覆盖有恢复责任的路径。缺少凭据时仅普通交互终端可自动回退；发送后失败或结果未知不会换策略。默认等待 30 秒。

计算 OAuth 与 Drive 凭据分开。DriveFS 要求完整 drive scope 和 Gaia ID，drive.file 不足。支持环境来源和实验性 --cli-token，挂载前预览参数；不要把 token 放入 --drive-extra。

卸载会回收轮换凭据并确认远端清理。保留 active 租约时不得绕过保护更换凭据、重命名或强制删除账户。只在确认 VM 已释放后，使用 `drive lease-cleanup --endpoint ENDPOINT` 清理匹配租约；它不能从已销毁 VM 回收 token。

## 恢复与证据

`ext doctor --json` 列出本地待恢复责任，`account repair` 恢复账户事务；不要手删日志绕过保护。automation-cleanup 和 drive probe-cleanup 仅用于核实归属后的 kernel 清理，不清除用户普通 kernel。

真实测试只清理本次创建的资源。离线测试不能证明 Google 授权、DriveFS/FUSE 或原生凭据库互通；遇到未知结果应保留现场并说明证据边界。

原生更新使用 `update --manifest URL` 检查，显式 `--install` 才替换文件；源可由 COLAB_CLI_UPDATE_MANIFEST 配置。只使用可信发行源，校验后会执行发行物的 version。`update --status` 离线核查，`update --rollback` 恢复一代备份；存在待恢复操作时先核查，禁止删除更新日志绕过保护。GitHub 仓库为 https://github.com/deluxebear/colabcli-rust，Release 尚未发布；发行构建可内嵌公开清单地址。

`--show-completion SHELL` 只输出静态补全，不访问账户或 VM；`--install-completion` 会写用户 shell 配置，自动化应按用户明确需要使用。`drive status --drive-account PROFILE` 的 drive_account 是本地所选存储账户，local_credentials_present 仅表示文件存在，不证明凭据有效或与远端挂载身份一致。

`account list --json` 的 accounts 是数组；`current --json` 包含账户/Drive 来源和路径。auth.ready 仅说明本地文件存在状态；auto/keyring 无文件时为 null，离线清单不会打开钥匙串。不要将该摘要当作线上认证验证。

新建 file 模式账户 rename/remove 不访问钥匙串，也不删除原有钥匙串槽。repair 按事务中记录的策略恢复；没有策略字段的旧事务仍按双后端处理，切换到 file 不能绕过其保护。变更会保留账户顺序，删除 active 时选择首个剩余账户；这些命令只处理本地状态，不停止 VM。

账户清单与 use/set-drive/rename/remove 会先校验根级账户选择。set-drive 的根级 -a 与局部 -a 独立：前者必须有效，后者可覆盖绑定目标。重复根级 -a 取最后值；空根级值回退到 COLAB_ACCOUNT 或 active，账户环境变量会修剪首尾空白。

sessions/usage --json 的顶层是账户对象，--all --json 是账户数组；会话在 assignments，用量在 usage。即使账户认证或网络失败也可能退出 0，必须检查 error；无效全局选择和命令级配置错误仍非零。需要本地 kernel/执行详情时使用 status --json。--all 使用各账户自己的凭据，合法全局 -a 不限制查询范围。
