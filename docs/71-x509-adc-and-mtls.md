# X.509 ADC 与证书认证传输

2026-11-18（Asia/Shanghai）。补齐 external_account 的 certificate 来源和 `urn:ietf:params:oauth:token-type:mtls`。本阶段只使用临时合成证书、本地 TLS 服务和 fake request；没有访问真实 Google、系统钥匙串、用户私钥或云资源。

## 配置、输入和状态

Google 的 [X.509 federation 配置](https://docs.cloud.google.com/iam/docs/workload-identity-federation-with-x509-certificates) 使用独立的证书配置文件，workload 下声明 cert_path/key_path；[官方 Python 指南](https://googleapis.dev/python/google-auth/latest/user-guide.html) 描述叶证书和中间链的主体编码。

[证书实现](../src/auth/adc/certificate.rs) 支持 certificate_config_location 或 use_default_certificate_config 二选一，以及可选 trust_chain_path。默认位置从启动时的 [显式环境快照](../src/context.rs) 获取：GOOGLE_API_CERTIFICATE_CONFIG、CLOUDSDK_CONTEXT_AWARE_CERTIFICATE_CONFIG_FILE_PATH、gcloud 配置目录下 certificate_config.json；gcloud 目录使用 CLOUDSDK_CONFIG 或平台默认目录。默认选中配置存在但缺少 workload 时，可回查 gcloud 默认文件；显式位置不回退。配置位置的 `~`/`~/` 使用快照 home 展开，其他相对路径保持当前工作目录语义。

证书配置格式为 `{"cert_configs":{"workload":{"cert_path":"...","key_path":"..."}}}`。config、cert、key 和 trust 文件均需为普通文件，每份读取最多 1 MiB；Unix 非阻塞打开以拒绝 FIFO。允许投影链接及合并 cert/key bundle；同路径只读一次，不同路径读到不配对的轮换会失败。PEM/DER、链长度和叶证书/私钥配对在请求前核对；不接受加密私钥。额外链中首个叶证书去重，其他位置出现叶证书拒绝。每份 PEM 最多 16 张证书。

主体是叶证书 DER 的标准 Base64 数组，随后追加 trust_chain_path 中的中间证书；未提供 trust_chain_path 时仅发叶证书。TLS 使用 cert_path 中解析出的完整证书链和配对私钥。两者来自同一次 material 读取，避免主体与握手分别读取出不同版本；私钥缓冲使用现有清零包装，不承诺 HTTP/TLS 库全部内部副本清零。

## 传输、缓存与失败

[external 实现](../src/auth/adc/external.rs) 将证书来源与 mtls 主体类型匹配，要求独立 workload 来源和 HTTPS STS，不混用 file/url/executable/AWS 来源。允许 Google STS 公共和 mTLS 域名；IAM 目标校验也接受 IAM Credentials 公共及 mTLS 域名。测试只允许既有 loopback 规则下的端点，不增加任意生产目标。

[TokenClient](../src/auth.rs) 为本次快照创建带 identity 的专用客户端，保持 TLS 服务器验证、禁止重定向和原有连接/请求期限。STS 与可选 IAM 都使用该客户端，STS token 仍仅用作 IAM Bearer，Colab 使用最终 token；不把证书私钥、客户端认证或源 token 写回 ADC。配置或文件错误、配对失败、TLS 拒绝均直接失败，不尝试无证书连接或其他身份。

仍有效的最终 token 由原有 provider 缓存返回；需要刷新时重新读取证书配置和材料。目标 token 强制刷新可复用有效 STS，但 mTLS 材料重新读取。失败不覆盖仍有效的旧 token；不同账户的配置、材料和缓存独立。证书选项在 provider 中装箱以控制外层认证枚举大小，没有放宽 Clippy 检查。

## 证据和范围

七项 [本地证书测试](../src/auth/adc/certificate/tests.rs) 包括：真实双向 TLS、握手叶证书与主体一致、两账户 12 线程缓存隔离、成功与失败轮换、IAM mTLS/Bearer 隔离和 STS 复用、默认选择/快照/后备配置、合并 bundle 别名、链顺序/配对/超限/目录/FIFO 拒绝，以及 HTTP/混合来源/类型不符时零请求和不受信任服务器拒绝。暂时不使用新 mTLS 客户端后，IAM 握手测试按预期失败；恢复后通过，证明断言依赖真实证书握手。

[可重跑的原版探针](../tools/probe_x509_adc.py) 通过已安装 google-auth 环境运行。它生成临时合成证书，fake request 核对主体和传入的证书路径，无实际网络。当前 [四组结果](../research/x509-adc-official-probe.json) 对应 google-auth 2.62.2，保存探针及三个库文件哈希，覆盖只有叶证书、链含首位叶证书、链不含叶证书和叶证书位于后续位置拒绝。安装依赖证据不替换固定研究基线。

最终默认/无默认 489/487 项 Rust、两种全目标 Clippy、fmt、工具 175 通过/1 平台跳过及默认 debug 构建均通过；169 份输入收尾哈希一致。完整结果见 [回归报告](../research/x509-adc-regression-validation.json)。首次编译因测试 fixture 未启用 rcgen PEM 方法失败，随后替身继承非阻塞 socket 导致三项失败；修正 fixture 后的结果独立记录，不把早期失败记为成功。第一次 Clippy 暴露枚举大小增量，已通过装箱选项解决。

本阶段没有真实 X.509 workload/组织/Colab 权限验收，也没有 metadata mTLS、JWT/SAML 证书绑定 token、其他 universe、任意 `~username` 展开或原生 Windows 验收。对不匹配的绑定来源继续明确拒绝。这里的 mTLS 功能不代表所有 ADC 身份都能使用 Colab；V03.8/U03 和总体目标仍未完成。旧线上报告保留原构建哈希，不外推到本次新构建。
