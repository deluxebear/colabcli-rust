# HTTPS 代理与双层 TLS

## 输入与连接

[Jupyter](../src/jupyter/runtime.rs) 与 [SSH](../src/ssh.rs) 的共享 [连接层](../src/websocket_transport.rs) 接受 https:// 代理，选择与 NO_PROXY 沿用 [48](48-websocket-http-proxy-implementation.md)，HTTP/SOCKS 模式沿用 [49](49-websocket-socks-proxy-implementation.md)。HTTPS_PROXY 表示目标使用的环境变量，其值可为 http:// 或 https://；https:// 表示先加密到代理，省略端口时使用 443。

```sh
HTTPS_PROXY=https://proxy.example:8443 colab exec -s work -f script.py
ALL_PROXY=https://proxy.example:8443 colab ssh -s work
```

[Stream](../src/websocket_transport/stream.rs) 封装直接 TCP 或代理 TLS，实现 Read/Write，并通过 tcp() 暴露底层连接以设置超时、SSH 非阻塞和轮询。KernelRuntime、SSH 的流类型相应从 MaybeTlsStream<TcpStream> 变为 MaybeTlsStream<Stream>；现有库调用者若直接操作底层描述符，需改为访问 stream.tcp()。端点 tungstenite TLS 套在此流之外，允许目标 TLS 经代理 TLS 传输。

连接依次执行：解析并拨代理、验证代理 TLS、加密发送 CONNECT、处理有界回复，再进入原端点 TLS/WS。原目标 host:port 才是 CONNECT authority；代理身份与目标身份独立。代理认证 Basic 仅进入加密 CONNECT，不写入端点 WS；runtime token、公钥等仍只在端点握手中发送。ws 目标在代理 TLS 终止处可见明文，不将此模式称为端到端加密。

## 信任、期限与失败

代理端使用锁定 rustls 与 WebPKI 公共根，验证证书链、名称和有效期；以代理主机名构造 ServerName，DNS 名支持 SNI，IP 字面量使用 IP SAN 验证。没有关闭验证的参数。库调用者可通过 ProxyEnvironment::with_proxy_root_certificate 添加 DER CA，扩充代理根集合；它不修改目标端 TLS 信任，不修改进程环境，也不写入用户配置。CLI 尚无自定义代理 CA 的配置入口，不声明自动导入系统/企业 CA。

代理 TLS 的底层每次读写扣除同一绝对 deadline，随后 CONNECT 继续使用这一期限；TLS 握手和持续滴流的加密响应均不能不断延长连接。CONNECT 最多 16 KiB 头，逐字段/字节读至结束，不丢弃紧随其后的隧道数据，即使它已在 TLS 内部缓冲。建立隧道后解除代理协商 deadline，保留原端点 TLS/upgrade 和运行时各自的超时；系统 DNS 仍是阻塞解析，没有独立取消能力。

代理 TLS 失败只显示固定错误，CONNECT 拒绝只显示状态码，不输出证书内容、代理 URI、密码或正文。失败释放本次 TCP/TLS；不回退 HTTP、关闭验证、绕过代理或重试执行。Jupyter kernel 持久化与 SSH owner/--rm 清理规则不变，也不新增后台恢复、会话重建或检查点。

rustls 0.23.46 与 webpki-roots 1.0.12 原本已在锁文件中，本阶段声明为直接运行依赖。测试新增 rcgen 0.13.2、time 及五个间接包，生成内存临时 CA/服务器证书/密钥；time 固定 0.3.44 以保留 Rust 1.85 兼容路径，不把测试生成器打入发行程序。时间类型与证书生成器的 MSRV 依据锁定源码和 [time 0.3.44 清单](https://docs.rs/crate/time/0.3.44/source/Cargo.toml.orig)。

## 验证与边界

[八项 HTTPS 测试](../tests/https_proxy.rs) 与既有 [16 项 HTTP/SOCKS 代理](../tests/websocket_proxy.rs)、[21 项 SSH 回归](../tests/ssh.rs) 使用临时账户、合成 token、loopback 和内存证书：

- Jupyter 经过已验证的代理 TLS 与加密 CONNECT 执行，localhost SNI 正确；Bearer 与代理 Basic 不混用。
- SSH 经 HTTPS 代理双向传输二进制，验证含 NUL 的 2 MiB 帧在非阻塞写入/flush 后完整到达。
- 代理证书不受信任、主机名不匹配、过期时，在 CONNECT 之前失败，不直连可达目标，不泄露凭据。
- 成功的目标 WSS 嵌套在代理 TLS 内；目标 CA 不受信任或名称不匹配时仍被拒绝，即使代理 TLS 已成功。
- 加密 CONNECT 的成功回复后保留首段隧道字节；407/302 保持拒绝，不返回敏感正文。
- 滴流 TLS record 在绝对期限内失败，代理 TLS 与后续加密 CONNECT 共用期限。
- NO_PROXY 绕过 HTTPS 代理，既不打开代理 TLS，也不发送代理认证。

```sh
cargo test --locked --test https_proxy --test websocket_proxy --test ssh
```

证据证明本地证书验证、双层 TLS 和库级 Jupyter/SSH 路由；不是实际 CLI 在公开 CA 代理上的线上证据。没有完成真实企业代理、系统 CA 导入、代理客户端证书认证、PAC/系统代理发现、跨平台全链路，也没有创建真实 Google 资源或发布发行物。

默认特性全量 435 项、无默认特性全量 433 项 Rust 测试通过。官方 CLI 365 场景的名称、命令和分类与上一阶段一致（92 一致、54 保留差异、219 Rust 扩展）。本机 release 的 [16 项发行检查](../research/native-release-validation.json) 与 [两组无代理图片对照](../research/release-image-comparison.json) 通过；release 为 8,854,768 字节，默认测试用 debug 为 41,852,040 字节。打包/直连检查不作为优化版 HTTPS 代理或线上 TLS 的验收；其他合并版专项报告保留为对应阶段的历史证据。没有推送或发布。

两种特性的 Clippy、格式检查通过。研究文档检查覆盖 55 篇 Markdown、591 个相对链接、125 个固定源码引用和 143 个源码哈希，无错误。
