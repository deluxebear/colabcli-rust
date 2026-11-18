# Rust 依赖材料与当前本机发行复验

## 范围与数据来源

[收集工具](../tools/collect_dependency_licenses.py) 使用 cargo metadata --locked --offline，分别按四个原生发行目标解析默认特性依赖图。沿 normal/build 边遍历，排除纯 dev 边；不把整个 Cargo.lock 的测试、Windows、WASI 等无关包都当作发行依赖。首次缓存不全时先运行 cargo fetch --locked，候选工作流已接入。

四目标依赖包数分别为 macOS ARM64 222、macOS x86_64 223、Linux ARM64 262、Linux x86_64 263；合并 267 个包。包归档内递归收集 LICENSE/LICENCE、COPYING、COPYRIGHT、NOTICE、UNLICENSE 及显式 license_file，保留原文、相对路径、包名/版本/来源和声明的许可表达式；198 份不同文本按 SHA-256 去重，生成 [原始材料](../licenses/rust-dependencies.json)，792,690 字节。没有读取用户凭据或执行依赖包程序。

缺少材料、包外路径、符号链接、空文件、非 UTF-8 或超过 1 MiB 的单份文本会失败。--check 重建并逐字节比较，CI 因材料过期而失败；打包还核对材料内 Cargo.lock 摘要，拒绝旧锁文件生成的材料。原有候选目录不覆盖，任何失败不留下部分发行目录。

这是一份实际依赖材料汇集，不代表已对全部许可分支、静态链接义务、嵌入原生代码逐一作出法律结论。整个项目的自身许可证、colab-ext 的 U12 复用权利确认仍独立待办；不因本轮收集而关闭。

## 验证

收集工具新增四项测试，验证混合边和循环、normal/build 与 dev 的筛选、去重而保留包归属、缺失材料与包外路径/符号链接拒绝。发行工具共十一项测试，新增锁摘要过期失败；全量 Python 工具测试 183 项，182 通过、1 平台跳过。生成后 --check 通过。

从当前 Rust 源码完成 cargo build --release --locked，macOS ARM64 二进制为 9,223,936 字节，SHA-256 为 a66f3226016afd11cf50671208d43907a25b2c3276339f952bcf1e124ed85c07。优化二进制经过十八项本机临时目录/loopback 验证，包括实际包内所有声明的原文字节及完整校验和集合、原生格式/版本/帮助/补全、账户边界、可执行 ADC 与 AWS STS 路径。没有读取系统钥匙串、访问真实 Google、创建云资源或发布 Release。

本轮没有修改 Rust 实现，因此没有再次运行此前 497/495 项 Rust 全量测试；它们仍是独立的历史回归证据。当前范围与输入/日志摘要见 [本轮记录](../research/dependency-licenses-release-validation.json)。其他平台构建与真实远程下载不能由单机检查推定通过。
