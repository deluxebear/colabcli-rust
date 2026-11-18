# DriveFS 凭据来源与附加参数

更新日期：2026-11-17。本章补充 [24](24-drivefs-lifecycle-implementation.md) 的核心挂载编排，接入环境凭据、实验性计算 OAuth 来源、身份覆盖、额外 DriveFS argv 与原生传输兼容选项。验证仍为合成凭据和 loopback 服务，未做真实 Drive 授权或挂载。

## 来源优先级和输入

```sh
# 由 CI 安全注入 COLAB_DRIVE_*，预览不会保存新凭据
colab drive mount -s cpu-demo --from-env --dry-run
# 已保存的 Drive 凭据，可补充 Gaia ID 与邮箱
colab drive mount -s cpu-demo --user-id GAIA_ID --email USER_EMAIL
# 额外参数作为一个 argv 元素传递；使用等号避免选项歧义
colab drive mount -s cpu-demo --drive-extra=--curl_logging
# 兼容入口使用 Rust 原生传输，不启动 Python CLI
colab drive mount -s cpu-demo --via-cli --dry-run
```

优先级是显式 --cli-token、环境 refresh token（或 --from-env）、已有 Drive 存储。--cli-token 与 --from-env 互斥；环境中存在 COLAB_DRIVE_REFRESH_TOKEN 时自动选择环境来源。--from-env 不会在输入缺失时静默回退已有文件。

环境来源要求 COLAB_DRIVE_REFRESH_TOKEN、COLAB_DRIVE_CLIENT_ID、完整 COLAB_DRIVE_SCOPES 和 Gaia ID；支持 COLAB_DRIVE_CLIENT_SECRET、COLAB_DRIVE_TOKEN_URI、COLAB_DRIVE_USER_ID、COLAB_DRIVE_EMAIL。Gaia ID 与邮箱可以用 --user-id/--email 覆盖。token URI 默认 Google OAuth endpoint，继续执行原有端点校验。scopes 按空白切分，完整 Drive scope 必须精确匹配，drive.file 不等价。输入的 scopes 只标记 configured_unverified，不作为服务端授权证明。

rt 模式仅接受匹配 DriveFS client ID 的环境来源，并建立 dfs 来源标签；其他 client 仍拒绝。--cli-token 强制 shim，即使同时写了 --mode rt。该实验路径读取已选计算账户的 OAuth 凭据，要求 refresh token、完整 Drive scope 和调用者提供的 Gaia ID；不会自动向 Google 查询身份或升级 scope。支持旧 DFS_UID/DFS_EMAIL 作为该路径的身份补充。它固定使用计算账户对应的独立 Drive 文件，拒绝 --drive-account，忽略原 Drive 绑定；列举其租约可显式使用 `drive leases --drive-account COMPUTE_PROFILE`。

--cli-token 不修改计算 OAuth 文件，也不会在 Drive 卸载时向计算 OAuth 写入轮换结果。两者仍可能共享同一 Google grant，线上 token 轮换互通尚未验证，因此这是实验能力；通常官方 CLI 的有限 Drive scope 会被本客户端提前拒绝。

## 持久化、轮换与租约

[configure_mount](../src/drive_credentials.rs) 在 Drive 账户生命周期锁下解析来源。实际挂载前，把完整匹配的 refresh token/client/身份保存到所选账户的私有 dfs_credentials.json，沿用原子刷新暂存和恢复协议，后续卸载可按 [24](24-drivefs-lifecycle-implementation.md) 回收。显式环境来源会替换该账户已有挂载凭据；未知扩展字段保留，旧 access token/expiry 清除，旧 scope 确认不继承。原计算 OAuth 始终独立。

保存配置时记录原始注入来源的指纹。相同 CI 输入再次运行时，优先保留该来源已经回收的轮换 token，避免把仍留在环境中的旧 token 写回文件；用户变更来源或身份时重新验证。活动租约拒绝这些变更，同一来源可重试。没有自动刷新、测试性 token 请求或新的用户授权步骤。

--dry-run 完成输入、scope、rt、身份与租约冲突校验，展示非秘密参数、extra_flags 和 transport=native；不会保存新来源、分配 VM 或访问运行时。读取时仍可能恢复此前已经存在的刷新暂存，这与原有 load 行为一致。错误不回显原始 token、client secret 或候选凭据对象。

## 额外 argv 与兼容选项

--drive-extra 可重复，每项必须为非秘密的 --name 或 --name=value，名称使用小写字母、数字、下划线。最多 32 项，每项最多 4096 UTF-8 字节，不允许控制字符。值中的空格、引号和 shell 元字符保持一个 argv 元素，不经 shell 执行。

客户端和远端控制器分别校验。禁止覆盖 features、preferences、身份、认证/token/secret/password、挂载路径、私有配置与日志位置，以及 help/version 等控制器管理字段。违反时在派生进程前失败。附加参数属于可持久记录的非秘密意图，调用者不得把 token 等秘密放入其值。最终路径继续由控制器追加；额外参数保存在租约和远端状态中，健康复用要求参数一致，修改需先卸载或 --force。

--via-cli 是兼容标志，两种取值均使用 Rust REST/WebSocket 原生路径，不寻找 colab/Python 可执行文件，也不吞掉子进程退出码。实际行为和 dry-run 的 transport 字段一致。

## 验证及未完成项

新增测试覆盖来源预览不保存、无效 scope 拒绝、活动租约拒绝来源/身份变更、轮换后重复环境不回滚、未知字段保留、scope 来源标记，以及 --cli-token 的强制 shim、隔离和拒绝路径。完整 loopback WebSocket 测试覆盖环境与计算 OAuth 两类挂载、凭据私密输入、卸载采用/finalize 及历史脱敏。Rust/远端参数测试覆盖特殊字符 argv、受管参数覆盖拒绝和不同参数不误复用。

后续隐藏诊断兼容入口见 [26](26-extension-diagnostics-implementation.md)。根 drivemount 的扩展回退策略已接入，见 [28](28-drivemount-strategy-implementation.md)。合并版 help，以及真实 DriveFS/Google 授权与跨 token 到期读写验收仍未完成；本章不把这些缺口视为已实现。

最终回归：默认特性 257 项、无默认特性 255 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。远端脚本 41 项通过、1 项 Linux 专属跳过。完整 CLI 差分 221 个场景为 88 一致、39 保留差异、94 Rust 扩展，无断言失败；root/account/drive 清单一致，报告二进制哈希与当前 debug 文件相符。
