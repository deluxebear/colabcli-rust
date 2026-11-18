# Metadata ADC 发现与刷新

## 选择顺序和输入

ADC 现在能在没有显式文件且默认 gcloud 文件不存在时，使用 GCE/GKE 风格 metadata 端点。文件优先级仍为环境 GOOGLE_APPLICATION_CREDENTIALS、账户 adc_file、CLOUDSDK_CONFIG、HOME 下默认 gcloud 路径；前两者标记为 required，缺失也不能降级为机器身份。默认路径存在但内容无效、不可读、未知类型或刷新失败时同样不回退。代码见 [context.rs](../src/context.rs)、[adc.rs](../src/auth/adc.rs) 和 [metadata.rs](../src/auth/adc/metadata.rs)。

Environment 启动时快照 metadata 配置，AccountContext 和 provider 持有副本；不靠修改进程环境切换账户。已有 AdcProvider::new(path, client) 仍只接受该显式文件；CLI 的 account login、普通计算请求和 whoami 改用 from_context，允许最后的 metadata 发现。OAuth 默认账户不自动切换 ADC；需账户选择 ADC 或 --auth adc。

| 环境配置 | 行为 |
| --- | --- |
| GCE_METADATA_HOST | token 服务器 authority；未设置时尝试兼容 GCE_METADATA_ROOT，再使用 metadata.google.internal |
| GCE_METADATA_IP | 探测服务器 authority，默认 169.254.169.254 |
| GCE_METADATA_TIMEOUT | 探测请求秒数；1–30，有效值外或无法解析则默认 3 |
| NO_GCE_CHECK=true | 禁用 metadata 发现，适合离线或受管配置 |

authority 可带端口（含 loopback 测试），不能含 scheme、路径、userinfo、query、fragment、反斜杠或空白。显式主机配置是受信任输入，允许自定义 HTTP metadata 服务，并非任意来源的网络沙箱。未使用的 metadata 配置不会使正常文件 ADC 或 OAuth 失效。

## 网络、状态与失败

协议核对 Google 的 [metadata 实现](https://github.com/googleapis/google-auth-library-python/blob/main/google/auth/compute_engine/_metadata.py) 及 [AIP-4115 token 端点](https://google.aip.dev/auth/4115)。先 GET 探测 authority 的 /；响应须为 200 并有 Metadata-Flavor: Google。探测超时、HTTP 失败、缺失标记均返回需要授权，之后显式重试可以重新探测。当前每次发现只探测一次，没有 SDK 的多次探测重试或 Linux DMI 判定。

成功后 GET /computeMetadata/v1/instance/service-accounts/default/token，scopes query 是 PUBLIC_SCOPES 的逗号连接值；scope 是否被环境接受由服务端决定，客户端不将请求值标为实际已授予 scopes。请求均带 Metadata-Flavor: Google，不发账户 Authorization，不携带 OAuth refresh/client secret；独立客户端禁用代理和重定向。连接期限 3 秒，探测使用配置期限，取 token 请求期限 20 秒，均非命令全过程总期限。

token 响应也要求 Google 标记，最多读取 64 KiB；必须含非空、无空白/控制字符的 access_token、Bearer 类型和正整数 expires_in，日期溢出拒绝。每个 provider 自有缓存、由原 mutex 合并并发刷新，提前 60 秒刷新；显式强刷失败对本次报错，保留仍有效的旧 token。发现成功后 token 失败可重试 token 请求，无需重新探测，不改用其他账户身份。

不同本地 profile 的缓存互不共享；它们在同一机器上可能取得同一个附加服务账户身份，profile 隔离并不创造不同云端身份。错误不输出配置 URI、响应原文或 token。没有凭据文件写入、钥匙串写入、持久事务或远端资源创建；进程结束释放缓存，无云资源清理责任。

## 诊断和边界

doctor 始终离线。ADC 诊断新增 metadata_fallback：blocked_explicit_file、disabled 或 not_probed，分别表示选择策略，不表示 metadata 存在或能授权；online_validity 仍为 not_checked。默认文件缺失时 state 仍为 missing，并保留未验证 issue，不假装已经取得机器身份。

本阶段初期只实现普通 HTTP metadata bearer 路径；后续 [72](72-metadata-mtls.md) 已接入 none/strict/default mTLS 与 HTTPS metadata。仍没有 Secure Session Agent、绑定 token、服务账户枚举/身份发现、DMI 发现、非默认服务账户选择或独立 metadata JSON 凭据类型。不能因此宣称所有 GCE/GKE/Cloud Run 模式或真实 Colab 权限均已支持。

## 验证

[八项 metadata 测试](../tests/adc_metadata.rs) 使用 loopback，覆盖默认文件缺失后的发现、显式/损坏文件不降级、并发与 provider 缓存隔离、禁用/无效配置、标记和重定向、到期/强刷失败保留缓存、无效/超大响应、探测超时释放锁与后续恢复。账户测试检查 required 标记和 metadata 快照。八个 CLI 扩展场景覆盖 login（设置不可达代理以证明绕过）、sessions（ROOT 兼容变量，其他场景验证 HOST 优先）、whoami、account status、禁用、显式文件缺失、伪造探测响应和离线 doctor。均未访问真实 metadata、OS 凭据库或 Google 资源。

原有 344 个 CLI 场景输入与分类须保持。真实 GCE/GKE、metadata TLS 模式和 Colab 接受性待平台验收；其余 ADC 来源及设计文档中的剩余验收项仍未完成。

最终验证：默认特性 353 项、无默认特性 351 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建通过。完整 CLI 差分 352 个场景为 92 一致、54 保留差异、206 Rust 扩展，零断言失败，旧 344 场景输入与分类不变；业务场景 288 个。合并帮助 83 路径无缺失、短 help 无失败；研究工具 55 项中 54 通过、1 项 Linux 专属跳过。

本机 release 重建、打包字节/manifest/SHA256SUMS、version、账户别名与联合授权帮助、五种补全输出通过；两份 debug 报告哈希与当前二进制一致。未推送代码或发布 Release。

后续 [52](52-adc-quota-project-implementation.md) 已接入 ADC quota_project_id / GOOGLE_CLOUD_QUOTA_PROJECT 的请求元数据，区分 IAM 来源与目标、保留 metadata 请求隔离。本篇此前的配额项目限制保留为当时范围；实际项目权限与服务接受性仍未验收。
