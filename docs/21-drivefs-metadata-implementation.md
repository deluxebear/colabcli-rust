# DriveFS 远端 metadata shim 实现

更新日期：2026-11-17。本阶段提供可打包的远端组件，尚未接入 `drive mount/umount/status`，没有向真实 VM 部署、挂载或访问 Drive。凭据模块见 [20](20-drive-credentials-implementation.md)，上游事实和完整挂载设计见 [04](04-drivefs.md)。

## 输入、状态与边界

[Rust 模块](../src/drivefs.rs) 内嵌 [远端脚本](../src/drivefs/metadata_shim.py)，不调用本地 Python。脚本只使用 VM 的 Python 标准库，按既有研究的 HTTP 契约独立实现。`metadata_config` 要求完整 Drive scope 和 Gaia ID，只序列化版本、client、refresh token、token URI、身份和 scopes；不会复制 access token 或未知凭据字段。序列化结果使用 Debug 脱敏、析构清零的 Secret 包装，限制 64 KiB。这不代表所有分配或传输副本都能被清零。

调用者必须通过私密输入通道发送配置，不能将返回值插入源码、argv、history 或诊断。该传输与挂载控制器尚未接入。没有新增用户可调用的挂载命令，也不会因加载 Rust 模块而创建进程或远端文件。

脚本输入为 `--config` 私有文件路径与 `--port`，默认从 8012 起尝试最多 30 个端口，仅绑定 127.0.0.1；0 表示分配空闲端口。路径各级不得为符号链接，配置所在目录必须属于当前 UID 且权限为 0700；配置必须为同 UID、0600、单硬链接的普通文件，读取上限 64 KiB。远端操作目录由未来挂载控制器创建和清理，不能使用上游共享 `/tmp/dfs_creds.json` 路径。

启动后写同目录的私有 `shim-ready.json`，仅含版本、PID 和端口。它不含 token，也不构成进程所有权或挂载成功证明；崩溃后可能残留。控制器仍须检查 PID 启动身份、HTTP 探针和实际挂载表。组件不删除配置或挂载点，不杀其他进程，不提供 VM 保活、重建或任务恢复。

## HTTP 与刷新

实现 token、user-id、email、scopes 四类 GET 后缀，沿用研究基线的后缀匹配；query 不被剥离，因此 `/email?x=1` 不匹配。成功含 Metadata-Flavor: Google、准确的字节 Content-Length 和 Cache-Control: no-store。未知路径 404 空 body，非 GET 返回固定错误。请求日志关闭；异常只返回固定的 500 信息，启动错误也不输出 traceback、原始 OAuth body 或凭据。

刷新使用互斥锁，缓存按单调时钟计算，在到期前 120 秒刷新，POST 为 form 编码，20 秒网络超时，响应上限 64 KiB。禁用环境代理与 HTTP 重定向。token URI 只允许 HTTPS，loopback 测试可用 HTTP，禁止 userinfo、query 和 fragment。HTTP 的有限 socket 超时不是涵盖 DNS 或持续慢速响应的整体期限，完整控制器仍需处理执行取消和结果未知。

实际 expires_in 不抬高到 60 秒；缺失时沿用 3599 秒估计值。显式空或缺少完整 Drive scope 的响应拒绝发放 token，不能回退到已配置的更高权限。响应未提供 scope 时使用现有 scope 元数据，仍不能将其视为服务端权限证明。刷新失败不回退到旧缓存。

轮换 refresh token 和显式返回的 scope 先原子写入原配置，文件与父目录均 fsync，成功后才返回 access token。写入失败保留内存中的新 refresh token，后续刷新继续用它并重试持久化；网络成功与首次落盘之间仍有故障窗口。远端轮换不会自动同步本地凭据，后续控制器必须明确这个恢复边界。access token 只保留在内存和 token HTTP 响应中。

loopback 不是同 VM 进程之间的授权隔离：VM 内能访问该端口的程序可以获得 Drive token，必须信任同 VM 的其他代码。此组件不实现完整 GCE metadata 服务，也不自行验证 Google tokeninfo。

## 验证与后续接入

运行 `python3 -B tools/tests/test_drivefs_shim.py`，直接测试 Rust 内嵌的同一份脚本。11 项离线测试覆盖四类路由、Unicode 字节长度、空 404、并发缓存、完整 refresh form、刷新边界、短有效期、轮换落盘、落盘失败恢复、上游错误与畸形响应脱敏、禁止重定向和环境代理、文件权限/符号与硬链接、配置校验、端口冲突/耗尽，以及真实子进程的就绪与启动失败输出。只使用临时目录和本机 loopback，没有真实 OAuth 或 DriveFS。

Rust 新增 3 项配置测试，覆盖必要字段与未知秘密隔离、身份/scope 前提及大小限制。CI 在 Linux/macOS 任务中加入脚本测试；本轮仅本机 macOS 实测，不能据 CI 配置宣称 Linux 已验收。

本阶段默认 228 项 Rust 测试、无默认特性 226 项通过，11 项脚本测试通过；两种特性的 Clippy、fmt、默认构建及研究文档检查通过。193 个既有官方差分与 Rust 扩展场景重新执行，断言失败为 0，报告二进制哈希已核对；本阶段未新增 CLI 行为。

本阶段交付时剩余接入包括：私密 stdin/input_reply 传输、每挂载独立目录与 PID 启动身份、DriveFS 启动参数、挂载表核查、未知结果禁止重放和只读恢复、安全卸载、清理后轮换凭据的回收策略，以及真实 Colab 挂载/读写/续期验收。后续已实现控制器与私密输入适配 API，见 [22](22-drivefs-controller-implementation.md)，CLI 恢复入口和真实验收仍未完成，P5 仍未完成。
