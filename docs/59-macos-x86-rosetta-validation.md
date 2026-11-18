# macOS x86_64 的 Rosetta 验证

本章区分 x86_64 可执行文件在 Apple Silicon 上的翻译执行与 Intel 原生硬件验收。宿主 hw.optional.arm64=1，显式 arch -x86_64 运行 sysctl.proc_translated 返回 1；Rosetta 已安装，本次没有安装系统翻译服务。

## 输入与隔离

[验证脚本](../tools/validate_rosetta.py) 显式定位 rustup 工具链的 cargo/rustc/rustdoc；本机 PATH 中 Homebrew Rust 与 rustup 版本不同，不能将 rustup 下载的 x86 标准库误当作 Homebrew sysroot 中的文件。新增 x86_64-apple-darwin 标准库仅用于对应 rustup 工具链，不修改全局默认工具链。

构建使用独立 target 临时目录和 HOME，清除计算/Drive/云认证环境与 SSH agent，Cargo offline；既有 ARM64 默认、无钥匙串及 release 产物保持独立。工具链与依赖缓存可以读取，不读取用户应用配置。测试使用临时账户、内存凭据库、loopback HTTP/WebSocket 和合成密钥；没有访问真实钥匙串、Google、Drive 或创建 VM。此宿主进程隔离不等同于关闭整个宿主网络。

六阶段依次执行两种特性的 Clippy/全量测试、恢复默认测试构建和 release。测试目标显式为 x86_64-apple-darwin，runner 使用 arch -x86_64；测试启动的同架构 CLI 由系统翻译执行。最低 macOS、Intel CPU 指令兼容、桌面凭据库、签名和 notarization 不在范围内。

## 打包与清理责任

打包工具校验 x86_64 Mach-O 文件头，生成单目标 manifest 和 SHA256SUMS，再通过 Rosetta 实际运行打包后的 version、五种 shell 补全、离线账户查询和 Drive consent 起始登记/state/S256。只保存合成状态的验证结论，临时 verifier 和 state 不提交。

构建阶段子进程持有独立进程组，异常/超时仅终止本次拥有的组。finally 清理本次独占临时 HOME 和构建目录，保留日志与已验证发行候选，不清其他 target 或用户配置。首次试跑发现验证脚本误用了不含版本的打包文件名，已主动中断并确认进程终止及 scratch 删除；修正后使用新输出目录重跑，不把中断算作通过。

## 结果

[Rosetta 验证记录](../research/macos-rosetta-validation.json) 包含六个阶段全部退出 0，Rust/Cargo 1.97.0、x86_64-apple-darwin。默认特性 466 项、无默认特性 464 项测试通过，两种特性的 Clippy 通过；两组 ignored 均为 0。Linux 专属 Secret Service 目标在此平台不执行，不能将其计为原生凭据验证。

120 份源码/测试/内嵌资源/验证工具输入前后及宿主复核一致，阶段日志哈希一致。release 为 10,258,384 字节，位于本地忽略目录 target/rosetta-validation-final/package/colab-0.1.0-x86_64-apple-darwin。原生架构头、打包前后字节与 manifest 哈希/大小、实际 Rosetta 运行检查均通过，启动方另逐项核对 SHA256SUMS 中的二进制和清单。未上传或发布。

本轮没有修改 Rust 产品、README 或内嵌资源，没有重跑 ARM64 默认/无钥匙串/发行或 Linux 测试；既有对应报告和二进制哈希复核一致。x86_64 的 Rosetta 证据是平台覆盖补充，不能关闭 Intel 原生硬件、最低 macOS、真实钥匙串、Google 授权/SSH/Drive 或四平台发行验收。

## 当前源码复验

JSON 下载修正后的同版 x86_64 构建通过两种 Clippy 和 502／500 项 Rust 测试。六阶段日志、源码快照和 10,390,616 字节的优化二进制已复核。打包产物在 Rosetta 上运行，构建临时目录已清理。此次依然使用 Apple Silicon 翻译执行。详见 [当前复验记录](../research/macos-rosetta-current-validation.json)。
