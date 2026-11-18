# 内建扩展诊断与注入兼容入口

更新日期：2026-11-17。本阶段接入设计中的 ext doctor/version/install、ext status 别名，以及隐藏的 drive/dfs doctor/version/install。它们使用 Rust 原生实现，不依赖本地 Python，不查询 Google 或系统钥匙串。

## 命令

```sh
colab ext version
colab ext doctor --json
colab ext doctor -a compute --drive-account storage --json --strict
colab ext status --json
colab ext install --site-dir /absolute/path/legacy-site --quiet
colab drive doctor --json
colab dfs version
```

version 返回 Rust 包版本、目标 OS/架构、内建业务能力、是否编译 native-keyring、无需本地 Python 的说明。远端 DriveFS 与线上兼容性始终标记 not_checked。信息不宣称 Google、FUSE 或系统凭据库已经可用。

install 接受旧 --site-dir/--quiet，并新增 --json；返回 builtin=true、changed=false、python_injection=not_applicable。它不创建 .pth、安装包或目标目录，也不修复 Python 环境。--quiet 抑制人类文本；显式 --json 时仍输出 JSON。version/install 不读取账户或配置，也不初始化默认目录。

## 只读本地诊断

[diagnostics.rs](../src/diagnostics.rs) 在普通 bootstrap/认证分派之前运行。使用 [Storage::open_existing](../src/storage.rs) 打开已有目录，不创建文件、不改变权限、不获取会创建锁文件的生命周期锁，也不恢复任何暂存记录。

doctor 对已有注册表使用原上下文解析，支持显式账户、COLAB_ACCOUNT、active 与 Drive 绑定/覆盖，遵守已有凭据文件与全局 Drive 绑定的优先级。未初始化目录和旧单账户配置报告缺失/未迁移，不自动引导迁移。账户事务存在时，展示 account_repair_required 与 blocked_pending_account_repair，停止按尚未稳定的账户归属诊断，不调用 repair。

输出仅含允许字段：

- 注册表状态、账户数量、所选计算和 Drive 账户。
- 本地 OAuth 缓存/刷新需求，或 ADC 类型与完整性；不证明服务端仍接受授权。
- Drive 凭据是否满足本地挂载前提，不展示 client、Gaia ID、邮箱、scope 原文或秘密。
- 待恢复控制面/OAuth/Drive 暂存文件的存在状态，不展开其原始内容。
- 会话名称、正常 kernel 是否已知、是否有待处理操作、经归属校验的挂载记录数量。
- 所选 Drive 账户的有效租约数量及计算账户的 run 清理记录数量；坏记录、符号链接和超出扫描上限分别报告未完成诊断，不跳过后宣称健康。
- 本地 PATH 中 SSH 可执行文件是否存在；只检查元数据，不执行它。

file 模式检查本地 OAuth JSON；auto 模式本地文件不存在时报告 keyring_not_inspected；keyring 模式报告 not_inspected，连本地 fallback 文件也不作为其认证依据。实现使用禁止访问凭据后端的替身，不能由诊断调用引发系统密码弹窗。

ADC 最多读取 64 KiB，识别 authorized_user，以及后续 [30](30-adc-service-account-implementation.md) 增加的 service_account 基本配置（仅 email/key 字段非空，不验证密钥或签名）。后续 [31](31-adc-impersonation-implementation.md) 增加 impersonated_service_account 的 URL/内嵌来源类型存在性检查，不解析或认证其来源。external_account 或不完整类型报告 unsupported_or_incomplete，不执行 credential_source 命令，不访问 metadata/impersonation/token URL。原始字节和 refresh token 使用零化容器，不输出文件内容或底层解析异常。

默认输出简明文本，--json 只输出结构化 JSON。默认即使有诊断问题也返回 0，以便查看完整报告；--strict 在本地问题、缺失或不能检查的认证状态下返回 1。它不把离线检查升级为在线验证。账户选择无效等参数错误仍正常失败。

这是无锁、非原子的本地快照：并发操作可能改变后续读取，不能用于授权删除资源或证明远端完成。诊断仅列出恢复责任，实际恢复仍使用 account repair、原操作重试和显式清理命令。运行中的正常挂载不是错误；未知执行或待清理记录会提示检查。

## 验证

CLI 集成测试覆盖所有新入口及旧别名、信息命令不创建配置、install 不创建 Python 目录、quiet、JSON/strict、计算/Drive 路由、钥匙串不检查、事务不恢复、旧格式不迁移、ADC 不执行外部来源、坏注册表/会话/租约与符号链接。测试比较调用前后整个临时配置树的文件字节和权限，确认诊断没有持久化副作用；秘密、未知字段和原始诊断不出现在输出。

CLI 差分工具为这些原生扩展入口增加场景，并把 ext 可见子命令加入完整覆盖核对。固定官方 CLI 没有 ext，结果只计 Rust 扩展；不宣称复现 Python 注入内部实现。真实凭据库互通、Google 权限、DriveFS 挂载及跨 token 到期验收仍独立待验证。

最终验证：默认特性 263 项、无默认特性 261 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 240 个场景为 88 一致、39 保留差异、113 Rust 扩展，无断言失败；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。
