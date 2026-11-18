# External account ADC 的 STS 客户端认证

2026-11-18（Asia/Shanghai）。补齐原先明确拒绝的 external_account client_id/client_secret 路径。输入仍为显式账户下的 ADC 配置；主体读取、STS、IAM 的身份和失败边界独立，没有访问真实 Google 或系统凭据库。

## 协议和状态变化

[实现](../src/auth/adc/external.rs) 与 google-auth 的外部账户行为一致：非空 client_id 启用 HTTP Basic，缺失或空 client_secret 使用空密码；空/缺失 client_id 不启用 Basic，即使存在 secret。客户端字段类型错误在读取主体前失败。请求正文不添加 client_id/client_secret。

Basic 仅附加到既有已验证 STS URL，来源 URL 仍只使用自己的 headers；IAM 继续使用 STS Bearer，Colab 继续使用最终目标 token。没有跟随重定向、降级身份或向其他请求复制 Basic。workforce_pool_user_project 仍验证为 workforce 的数字项目号；启用客户端认证时省略 options.userProject，以 client ID 决定项目；没有客户端认证时保留原编码。

客户端信息保存在 provider 内部，secret 使用已有敏感包装。成功刷新复用原有缓存和账户隔离；强制目标刷新可复用有效 STS。失败返回静态原因/HTTP 状态，不输出 secret 或服务端响应；仍有效的旧最终 token 保留，原 ADC 和主体文件不回写。HTTP 库的全部内部副本仍不承诺清零。

## 原版对照与回归

Google 的 [external_account 文档](https://google-auth.readthedocs.io/en/latest/reference/google.auth.external_account.html) 声明客户端字段；[官方实现](https://github.com/googleapis/google-auth-library-python/blob/main/google/auth/external_account.py) 使用 Basic 并在客户端认证时省略 workforce user project。

可重跑的 [离线探针](../tools/probe_external_client_auth.py) 应由原版 CLI 安装环境的 Python 运行，不加载用户 ADC。它只创建临时合成主体文件，并以传入的 fake request 捕获请求，不访问 Google。当前 [原版结果](../research/external-client-auth-official-probe.json) 对应 google-auth 2.62.2，保存四个库文件和探针 SHA-256；五组缺失/空值及 Unicode 凭据都满足相同规则。这是本机安装依赖版本证据，不替换研究固定上游基线。

Rust [14 项 external 测试](../tests/adc_external.rs) 包含三项新增测试：来源/STS/IAM 认证隔离与目标缓存、可选字段和失败后缓存保护、两个不同客户端账户的 12 线程并发隔离。首次两项在临时恢复旧拒绝条件时均失败，恢复新实现后通过；原有不带认证的 workforce options、重定向、超限响应、来源和错误保护继续覆盖。完整默认/无默认特性 482/480 项 Rust、两种全目标 Clippy、格式、176 项工具（175 通过/1 平台跳过）及默认 debug 构建均通过；报告记录各输入及日志哈希。详见 [回归报告](../research/external-client-auth-regression-validation.json)。

本阶段没有证书来源、其他 universe、递归 impersonation、metadata mTLS 或真实外部身份的 Colab 接受性证据。ADC 能交换合成 STS token 不等于该身份获得 Colab 权限，V03.8/U03 继续保持未完成。生产 debug 已随本轮全量回归重建，旧真实 kernel/Drive/SSH 报告仍只证明各自记录的历史构建，不重写为新构建线上通过。
