# Drive 授权起始阶段的账户登记

## 输入与路由

此前 drive consent 只能使用已存在账户，与目标中的 allow_create 不符。现仅在开始授权（未提供 --code 或 --callback-url）时支持自动登记命名账户；账户参数位于根、drive 组或 consent 子命令均适用，也支持 COLAB_ACCOUNT 和两种 Drive 环境变量。显式参数优先于计算账户环境；已有邮箱选择继续解析为原账户，不另外建立邮箱名称资料。

consent 将结果写入显式 Drive 环境账户，否则写入本次所选计算账户本身，不追随普通 show/token 的 Drive 绑定或全局默认。新登记账户沿用默认 OAuth provider，但不获取计算 token、不改变 active 或默认 Drive 绑定，也不把登记视为已登录。独立 Drive OAuth 的 client/redirect 校验、state/PKCE 和恢复日志继续适用。

授权提示按显式 login_hint、显式 email、所选 Drive 账户已存 email 取值；空值回退。只用于提示，不代表 Google 已确认身份，也不更改注册表或用计算账户邮箱替代另一 Drive 账户的提示。

## 状态、失败与清理

计算和 Drive 环境选择先共同校验，再在一个注册表锁/原子写入事务中补齐缺失账户；重复并发登记复用已有账户，不覆盖元数据或丢失其他账户。非法路径、模糊邮箱和忽略大小写后冲突会在提交前失败，不留下半次登记。锁顺序保持注册表操作结束后再进入 Drive 账户锁，不在持有 Drive 锁时反向获取注册表锁。

账户登记后，client/config 或授权开始失败会保留空账户供重试，与原版的显式登记意图一致；不自动删除，因为其他调用可能已使用该账户。没有成功写入 pending 的失败不会制造 token 或“已授权”状态。URL-only 成功仅写所选账户的受限 pending；取消或后续失败仍按已有授权恢复语义处理。

完成授权必须找到已有账户和其 pending，不能因代码、callback 或名称错误新建资料。show/token/clear/import 等其他路径不因此获得隐式创建能力。与原版不同，两个新选择中的后一个非法时不保留第一个空资料；原版 wrapper 对完成阶段也可能提前建立空账户，Rust 保留上述明确边界。

## 验证与边界

五项新增 Rust 回归覆盖八组登记/路由、已有邮箱、active/绑定/既有元数据不变、错误选择原子拒绝、完成与普通操作不创建、开始失败后重试、两个真实 CLI 子进程并发，以及四组提示来源优先级。全部使用临时配置、file 后端及本地测试 URL；没有 Google 或用户凭据。

[实际合并版对照工具](../tools/compare_merged_drive_consent.py) 运行九组离线 consent URL-only 流程，记录见 [merged-drive-consent.json](../research/merged-drive-consent.json)。Rust 必须通过无原生凭据库 capability 门；Python 使用 -S 和固定源码，禁止网络、浏览器、钥匙串及子进程。验证账户顺序、active、pending 归属、实际授权 URL 关键参数、login_hint、state 与已有文件不变，不保留随机 state/verifier。

对照先在旧 Rust 二进制上确认新账户路径失败，再确认新实现的九组路由契约通过；已有账户的邮箱提示差异也先被对照捕获，再修复。原版与 Rust pending schema、进度文案不同。本次两端均实测到与持久化 verifier 匹配的 S256 challenge，说明此固定源码/依赖组合的 custom URL-only 路径实际启用了 PKCE；没有仅凭 code_verifier 字段名作推断，也不外推到其他 flavor 或依赖版本。九组为路由/URL 契约通过，不是九组完整输出等价，也不是实际授权或 Drive 挂载成功。

本次 Python 3.14.0，google-auth-oauthlib 1.5.0、oauthlib 4.0.1、requests-oauthlib 2.0.0，版本在每个原版子进程核对一致并随报告记录。

打包后二进制的同一原生发行检查也加入新账户登记、state/S256、默认账户不变和存储邮箱提示验证，仍只使用临时合成配置。真实 OAuth client/scopes 接受性、独立授权与持续 Drive 挂载仍待验收。

最终默认/无默认特性全量 Rust 测试分别为 466/464 项通过，两种特性的 Clippy、fmt 通过。Python 工具 74 项中 73 项通过、1 项 Linux 专属跳过。既有完整 CLI 365 场景名称、命令与分类不变（92 一致、54 保留差异、219 Rust 扩展）；12 组凭据输出、七组刷新/whoami 和九组 consent 均用当前无钥匙串构建重跑，结论保持。

本机 release 的 17 项打包运行检查与两组非 TTY 图片对照通过；release 9,157,216 字节、默认测试 debug 44,693,592 字节。六份报告分别匹配对应默认/无钥匙串/release 构建；其他合并专项、Linux 和 Secret Service 报告保留历史快照，没有重新执行或改称当前全平台通过。没有推送、发布或新建真实 VM。
