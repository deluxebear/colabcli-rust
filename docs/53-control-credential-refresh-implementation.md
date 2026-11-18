# 控制面凭据续期与有限认证重试

本文记录设计 [账户上下文、错误与恢复策略](05-rust-architecture.md) 的后续实现。输入是命令已解析的账户、OAuth/ADC 来源、取消标记和控制面配置。客户端保留该来源，按请求检查凭据；不修改 active account 或进程环境，不重新登录，不启动后台保活。

## 来源与状态变化

[ControlCredentials](../src/auth/control.rs) 持有单一来源。OAuth 使用命令内的 OAuthCredentials 缓存，沿用既有 60 秒到期余量；有效缓存不重新访问 keyring。需要刷新时仍走账户级互斥、refresh pending、保存与恢复协议，从持久层重新读取最新 refresh token。只有成功后才替换内存缓存。ADC 保留同一个 AdcProvider，刷新时继续使用该来源的用户、服务账户、IAM、联邦或 metadata 分支，token 和配额项目一起返回，不写回源 ADC 文件。两个来源均保留获准 scopes 与自身原有刷新语义。

[ColabApi](../src/colab_api.rs) 新增 with_credentials_source；命令入口改用该构造方式，首次认证仍在构造期间完成。每次控制面请求先查询来源，过期或接近到期时先刷新再发送。现有 new(config, bearer) 静态库接口保留，其调用者继续管理 bearer 的生命周期。动态来源的项目不可通过 with_quota_project 覆盖；项目由来源提供，规则见 [52](52-adc-quota-project-implementation.md)。runtime REST、Jupyter/SSH 仍使用独立 proxy token，不把 OAuth/ADC bearer 或项目头注入运行时。

仅 GET /tun/m/assignments 和 /tun/m/ccu-info 在返回 HTTP 401 后允许再取得凭据，并发送一次新请求。若共享来源已经换过 bearer，直接使用更新的凭据；否则强制刷新。第二次 401 直接返回，不继续循环。403、重定向、其他状态和传输错误不触发此认证重试。没有把所有 GET 都当成只读：assign GET 可能直接分配 VM，故分配 GET/POST、释放 GET/POST、凭据传播 GET/POST 均不因 401 自动重放。既有分配 502–504/可重试传输策略未扩展。

每个请求自身仍使用既有 HTTP timeout；一次安全查询可以包含两次 HTTP 请求及一次凭据刷新，没有引入覆盖整个认证与请求链的新总期限。并发相同 bearer 的拒绝由同一来源串行协调，不承诺跨独立命令/独立 provider 去重，也不承诺刷新服务返回相同 token 时去重。

## 锁与失败责任

[SessionService](../src/sessions.rs) 在分配、同步、释放、用量与 Drive 清理校验期间原本就持有账户生命周期锁。现在将真实 FileLock 的 Arc 传给短期控制面客户端副本，OAuth 刷新核对锁对应的配置根目录和 profile，然后复用该锁，不再次获取同一排他锁。FileLock 的路径凭证只在库内部可读，释放规则仍由最后一个锁持有者负责。临时客户端不返回给调用方，不缓存于 SessionService 或持久化。

未持有会话锁的 OAuth 控制面请求先取得生命周期锁，再取得内存锁和 refresh 锁，避免传播 worker 与会话操作的锁序反转。等待结束后重新取得当前时间并检查取消。不同账户的缓存/刷新互斥独立；删除、迁移事务或错配锁仍受控失败，不借用另一账户身份。ADC 继续由自己的 provider 保护缓存。

刷新失败不发送后续目标请求，不替换仍有效的 token，不回显服务端敏感正文，不切换其他账户。会话/任务清理仍遵循原有日志责任：已登记的控制面事务不会因认证失败而被删除。run 在分配、执行、清理阶段原本会重新建立服务；若清理阶段首次认证失败，尚未创建释放事务，保留 run intent 和会话供 run cleanup 恢复，并不要求一定存在 control_operation.json。任务退出码与清理错误分别记录，脚本成功但清理未完成时 CLI 返回失败。

## 离线验证

[十项凭据续期测试](../tests/control_refresh.rs) 使用合成 token、内存凭据后端、临时文件与 loopback，覆盖 OAuth/ADC 的 401 后刷新、配额配对和源文件不写回；第二次 401 上限、403 不刷新；分配 GET/POST、释放和传播不重放；刷新失败后原凭据保留；共享克隆并发去重；缓存有效时禁止后端读取仍能请求；真实会话锁复用、错配锁拒绝、取消前不联网；短生命周期凭据在同一保留来源上无 401 主动续期后释放；两个账户并发时一方刷新失败不影响另一方 token 或 refresh token。

[实际 CLI/Jupyter 回归](../tests/jupyter.rs) 另有一项测试覆盖四组组合：OAuth/ADC × 清理刷新成功/失败。它分配合成 VM、通过真实 loopback WebSocket 执行合成脚本，等待凭据跨过到期余量后结束脚本，验证清理的新 bearer/项目、成功释放、失败保留 run intent、退出码、OAuth 轮换持久化与 ADC 文件原样。此测试也保留了 run 原本逐阶段认证的行为，不将其描述为本次首次实现。

```sh
cargo test --locked --test control_refresh
cargo test --locked --test jupyter run_refreshes_its_bound_oauth_or_adc_before_cleanup_and_retains_failed_cleanup
```

没有创建真实云资源，没有实际等待 Google token 失效或验证服务端权限/Colab 401 语义。没有新增 runtime proxy token 长期轮换、VM 自动重建或任务恢复机制；真实 OAuth/ADC 与跨平台验收仍按 [验收文档](06-validation-and-open-questions.md) 保留未完成状态。

全量回归曾检出释放恢复路径遗漏锁传递，已补齐该路径；原有 CLI run-cleanup 回归验证清单确认资源已消失后只清理日志和本地状态，不重复分配或执行脚本。

本机 release 的 [16 项发行检查](../research/native-release-validation.json) 与 [两组非 TTY 图片对照](../research/release-image-comparison.json) 通过，未新增优化版 401/续期专项验收；不能将打包成功解释为真实 OAuth/ADC 或跨平台通过。其他合并专项报告保留对应阶段证据，本轮未重新捕获；没有推送或发布。

最终默认特性全量 458 项、无默认特性全量 456 项 Rust 测试通过；两种特性的 Clippy 与格式检查通过。既有官方 CLI 365 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）。本机 release 为 9,140,416 字节，默认特性测试用 debug 为 44,653,160 字节。研究文档检查覆盖 58 篇 Markdown、646 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。没有推送、发布或创建真实云资源。
