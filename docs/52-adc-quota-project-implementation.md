# ADC 配额项目与请求身份

## 输入与来源

固定原版以 AuthorizedSession 发送 ADC 请求，底层 google-auth 会将配额项目加入 x-goog-user-project。此前 Rust 只取 bearer，忽略这部分元数据。现在 [Environment/AccountContext](../src/context.rs) 捕获 GOOGLE_CLOUD_QUOTA_PROJECT，[AdcProvider](../src/auth/adc.rs) 将项目与来源、token 缓存在同一实例中。

```sh
GOOGLE_CLOUD_QUOTA_PROJECT=example-project colab -a compute sessions --json
```

有效项目优先取非空环境快照，再取所选 ADC JSON 的 quota_project_id；环境空字符串不覆盖文件，缺失/null/空的文件值表示不发头。不把 project_id、GOOGLE_CLOUD_PROJECT 或账户名称推断成配额项目，不调用 gcloud 补全项目，也不写回 ADC 文件。环境覆盖只作用于 ADC，OAuth2 模式不采用此环境变量。

[输入校验](../src/auth/quota.rs) 要求项目为最多 256 字节的非空 ASCII graphic 字符串，不接受空白、控制字符或非字符串文件值；有效环境值可覆盖文件中的未选中字段。非 UTF-8 环境覆盖也作为非法值拒绝，不静默退回文件项目。非法输入在获取/刷新 token 或 metadata/IAM 请求前返回固定错误，不回显项目正文。这是 Rust 的校验约束，不宣称畸形值的错误时机/文案与 Python 完全一致。quota_project_id 是请求元数据，不修改项目配置或 IAM 权限；服务端是否接受该项目尚未实测。

## 状态与传输边界

request_credentials 返回同一 provider、同一缓存锁下的 RequestCredentials { token, quota_project_id }。access_token/refresh_access_token 保留旧返回类型；已有库调用不需改 token 接口。初始化后修改上下文或源文件不会使当前 provider 中途切换项目；新 provider 重新读取源文件；环境改变需要重新捕获上下文，下一次 CLI 调用会执行此步骤。新公开上下文字段 quota_project 用于显式快照，库调用者构造结构体时需补该字段。

[ColabApi](../src/colab_api.rs) 的 with_quota_project 校验并保存该客户端的项目，每次控制面 GET/POST 显式设置敏感 HeaderValue；不使用进程级默认认证头。Bearer 与项目由 [命令入口](../src/main.rs) 一起传入，因此并发账户的 token 和项目不串用。请求仍禁重定向，错误不显示请求头或服务端敏感正文。

IAM 需要区分两种来源：

- 普通 impersonated_service_account：根项目/环境覆盖用于目标 token 的控制面请求；generateAccessToken 用嵌入 source_credentials 的项目。根环境覆盖不改写嵌入来源。
- external_account 自带 IAM impersonation：STS 来源和目标采用有效外层项目，generateAccessToken 带该来源项目，控制面带同一有效项目。独立的 workforce_pool_user_project 仍只按原协议编码为 STS options，不把两个字段混同。

授权码/refresh/JWT token 交换、STS 与 metadata 的请求不带应用配额头；JWT claim 不增加项目字段。runtime REST、Jupyter 和 SSH 使用运行时 proxy 身份，也不继承控制面项目头。没有增加自动重试或认证回退。

失败不替换仍有效的 token 缓存，不改变项目归属；本配额元数据层不写 token/keyring/ADC 或账户注册表，上层账户新增/登录仍沿用既有生命周期约定。源文件解析与缓存仍受原有大小、互斥和取消规则约束；没有新增资源分配、清理日志或后台任务。新 metadata 只在内存中释放，无需存储迁移。

## 对照与测试

[纯内存探针](../tools/probe_adc_quota.py) 用 -S 和显式依赖目录执行，构造合成 google-auth Credentials；不调用 default 发现或 refresh，禁用网络、DNS、子进程，并清空该探针自己的环境。结果 [adc-quota-probe.json](../research/adc-quota-probe.json) 记录 google-auth 2.62.2、四个依赖源码哈希和十组观察：用户字段/环境/空值六组、普通 impersonation 两组、external IAM 两组。Rust 优先级测试直接消费其用户样本。

这属于当前安装库的协议观察，不是固定三仓库的源码清单更新，也不是线上配额/计费证据。环境和请求头依据 [google-auth 环境变量](https://google-auth.readthedocs.io/en/latest/reference/google.auth.environment_vars.html) 与 [Google 配额项目说明](https://docs.cloud.google.com/docs/quotas/set-quota-project)，具体来源/IAM 语义以探针和对应版本实现为准。

[九项新增测试](../tests/adc_quota.rs) 使用临时文件、合成 token/临时 RSA 和 loopback，覆盖：

- 文件/环境/空值优先级、provider 快照、源文件不回写。
- 并发账户的 GET/POST、配对 bearer/项目和 XSRF 释放流程。
- refresh/JWT/STS 无项目头，JWT 精确 claim；刷新失败后保留原 token/项目，响应不能注入新项目。
- 非法/超长/控制字符/Unicode/错误类型、非对象 ADC 根与坏嵌入来源均受控失败，无联网或敏感正文回显。
- 普通与联邦 IAM 来源项目、目标控制面项目及环境覆盖。
- 实际 CLI 的多 ADC 账户 sessions --all、空环境与覆盖，以及 OAuth 模式忽略该环境值。
- 实际 CLI 的 runtime REST/WS 只带 proxy token，不带 ADC 项目头，执行结束没有 running/pending。

既有 [metadata 测试](../tests/adc_metadata.rs) 增加有效项目快照及 metadata 无头断言；原 IAM、external、Workforce 回归同步通过。

```sh
/path/to/official/python -S tools/probe_adc_quota.py --deps /path/to/site-packages --output research/adc-quota-probe.json
cargo test --locked --test adc_quota --test adc_metadata
```

没有验证真实项目的 serviceusage.services.use 权限、Google/IAM/Colab 接受性或账单结果，没有创建真实云资源，也不把这些离线结果标成整个 ADC 矩阵完成。

本机 release 的 [16 项发行检查](../research/native-release-validation.json) 和 [两组非 TTY 图片对照](../research/release-image-comparison.json) 通过。release 为 9,123,696 字节，默认特性测试用 debug 为 44,593,256 字节。发行检查未新增优化版配额分支验收，不替代本阶段的默认/无默认特性专项测试或线上接受性证据。其他合并专项报告保留对应阶段的历史证据；没有推送或发布。

默认特性全量 447 项、无默认特性全量 445 项 Rust 测试通过，两种特性的 Clippy 和格式检查通过。官方 CLI 全量 365 场景名称、命令和分类保持不变（92 一致、54 保留差异、219 Rust 扩展）。Python 工具回归 59 项中 58 项通过，1 项仅 Linux 适用而跳过；配额探针另行完成十组观察。研究文档检查覆盖 57 篇 Markdown、629 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。

后续控制面同一客户端保留来源和请求前续期见 [53](53-control-credential-refresh-implementation.md)，包含保留配额身份的有限只读查询重试；本阶段项目来源与请求隔离规则不变。
