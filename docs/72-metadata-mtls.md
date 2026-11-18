# Metadata ADC 的 mTLS 模式

2026-11-18（Asia/Shanghai）。接入 Google metadata 的 none/strict/default 选择与双向 TLS，保留普通 HTTP metadata 行为。仅使用临时合成证书和 loopback，不访问实际 VM metadata、系统证书材料、Google 或用户凭据。

## 输入和模式

[MetadataOptions](../src/auth/adc/metadata.rs) 新增 MetadataMtlsOptions，Environment 启动时快照 GCE_METADATA_MTLS_MODE；不修改进程环境切换身份。未使用 metadata 的 OAuth/文件 ADC 不解析该模式，也不检查 metadata 证书文件。大小写不敏感，空串/非法值在选择 metadata 后报错；NO_GCE_CHECK 禁用仍先于探测。

| 模式 | 行为 |
| --- | --- |
| none（默认） | 普通 HTTP，不检查证书材料 |
| strict | 两份材料必须存在；token host 只能为默认 metadata.google.internal 或 169.254.169.254；必须使用 mTLS，网络/HTTP 错误不回退 |
| default | 两份材料存在时尝试 mTLS，否则 HTTP；mTLS 连接、TLS、超时或 HTTP 4xx/5xx 后请求同一地址的 HTTP；材料加载错误不回退 |

固定路径与 [Google metadata 文档](https://docs.cloud.google.com/compute/docs/metadata/overview) 一致：Unix `/run/google-mds-mtls/root.crt`、`client.key`；Windows `C:/ProgramData/Google/ComputeEngine/mds-mtls-root.crt`、`mds-mtls-client.key`。公共 Rust 配置对象允许显式测试路径，CLI 没有新增路径覆盖环境变量。代码只读取文件，不安装系统根证书或修改 VM 配置。

选择/回退规则对照 [官方 adapter](https://github.com/googleapis/google-auth-library-python/blob/main/google/auth/compute_engine/_mtls.py)。严格模式拒绝自定义 token authority，包括带端口的测试主机；不会为测试放开 CLI 策略。默认模式继续接受已有受信任的自定义 metadata authority。

## 传输、轮换和错误

探测和 token GET 都复用既有 Metadata-Flavor、禁用代理、禁止重定向和响应验证规则，不携带账户 Authorization 或 quota。TLS 验证服务器身份，使用 metadata 提供的根证书和配对 client bundle；材料解析抽取自 [X.509 共享读取](../src/auth/adc/certificate.rs)，有普通文件、1 MiB、证书数量、PEM/DER 和私钥配对限制。合并 bundle 同次读取，不混配两次轮换。

每次真正请求都重新选择材料并构建 mTLS 客户端，支持根/客户端证书轮换。默认模式文件随后缺失可用 HTTP，严格模式立即失败；已存在但损坏或不配对的材料不作为网络错误吞掉。失败保留有效 token 缓存；普通缓存命中不触碰文件或网络。多账户持有独立配置快照和缓存，不创建不同机器云身份。

默认模式在每次网络失败或 HTTP 4xx/5xx 后最多回退一次 HTTP；不跟随重定向，不把缺少 Google 标记或无效 token JSON 当作可回退的 TLS 故障。回退请求只使用 Metadata-Flavor，不发送 TLS 私钥、账户 Bearer 或 quota。连接期限仍为三秒，探测使用 GCE_METADATA_TIMEOUT，token 为二十秒；期限按每次尝试计，不是包含回退的统一总预算。发现阶段网络不可达仍映射需要授权，材料/模式错误保留静态原因，不输出路径、密钥或响应原文。

## 验证与局限

七项 [新本地测试](../src/auth/adc/metadata/tests.rs) 覆盖默认模式完整 provider 的 mTLS 发现/取 token、两个账户十二线程隔离、证书轮换与失败后保留缓存；strict 主机/材料/非法模式零请求拒绝及严格传输不回退；default 在 TLS 与 HTTP 错误后的实际 HTTP 请求；Google 标记伪造拒绝；none/无材料的 HTTP 路径。strict 自定义主机由纯策略检查拒绝，严格传输本身通过本地 TLS 服务单独验证；不冒充真实默认主机的完整线上验收。

[CLI metadata 集成](../tests/adc_metadata.rs) 增加一项四场景测试，子进程单独配置环境，验证非法模式、strict 主机限制、none 成功及文件 ADC 不受未使用的非法模式影响。整个 metadata 集成九项与 ADC 单元专项十六项通过。共享 TLS fixture 移至 [test_tls](../src/auth/adc/test_tls.rs)，原七项 X.509 测试继续验证，GET 解析允许无正文。暂时停用 metadata identity 后，严格握手测试失败，恢复后通过。

[原版离线探针](../tools/probe_metadata_mtls.py) 不修改父进程环境：从安装版本提取两段策略函数，注入合成环境和临时文件存在状态；adapter 使用 fake base/fallback send，不初始化宿主证书或发网络请求。[结果](../research/metadata-mtls-official-probe.json) 记录 google-auth 2.62.2、源/探针哈希、十二组模式/材料选择和六组 adapter 成功/HTTP/TLS 回退对照，以及三组显式端口生成规则。

新增端口回归先复现旧实现将显式 HTTP `:80` 错变 HTTPS 443；现分别从原始 authority 构造 HTTP/HTTPS 地址，保留显式端口、路径和 query。发现该问题后已停止旧回归，部分结果不计最终通过。

最终默认/无默认 497/495 项 Rust、两种全目标 Clippy、fmt、工具 175 通过/1 平台跳过及默认 debug 构建通过；172 份输入收尾哈希一致。完整结果见 [回归报告](../research/metadata-mtls-regression-validation.json)。当前仍没有真实 GCE/GKE mTLS 或 Colab 权限验收，没有运行时证书绑定 token、SSA/Agent Identity、DMI 或所有 metadata 变种。Windows 固定路径已接线，不等于原生 Windows 验收；总体目标继续保持未完成。
