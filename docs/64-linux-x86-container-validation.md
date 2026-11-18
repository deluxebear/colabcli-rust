# Linux x86_64 容器执行验证

本章补充 [平台验收](06-validation-and-open-questions.md) 的 Linux x86_64 路径，沿用 [ARM64 隔离原则](54-linux-container-validation.md)。Docker daemon 实际为 ARM64，本次镜像、工具链和 ELF 为 amd64/x86_64，通过模拟执行；不能据此声称 Intel/AMD 原生硬件、所有发行版或完整 U10 验收通过。

## 架构与隔离证据

镜像构建显式指定 `--platform linux/amd64`，上下文只有 Dockerfile、Cargo.toml 和 Cargo.lock。联网只用于系统工具、Rust 工具链和锁定依赖下载；镜像不接收宿主 HOME、源码或凭据。验证时源码通过唯一只读 bind 挂载，目标、临时 HOME、Cargo 缓存与报告分别位于本次专属目录/匿名卷；外网关闭、根文件系统只读、UID 1000、capabilities 全移除、no-new-privileges、4 CPU/6 GiB/512 PID 限制。没有挂载 Docker socket、钥匙串、SSH 配置或其他服务数据。

启动方独立 inspect 检查实际镜像架构、daemon 架构、用户、网络、根文件系统、环境和所有挂载。[容器脚本](../tools/linux-validation/validate.py) 新增 `VALIDATION_EXPECTED_MACHINE=x86_64` 核对，要求运行架构与 `rustc -vV` host 相符；最终还读取 debug/release 的 ELF header，要求 64 位小端格式和对应 machine ID。任一不符不能落盘 passed=true。仅有镜像标签、交叉编译成功或 uname 之一均不足以支持本次执行架构结论。

复现时在 [54 的运行示例](54-linux-container-validation.md) 同时给 build/run 增加 `--platform linux/amd64`，并给 run 增加 `--env VALIDATION_EXPECTED_MACHINE=x86_64`。使用新的本项目专属镜像标签、构建目录和临时卷，先 inspect 再跟踪日志；只在容器终止并取回报告后清理本次容器与卷，不使用 prune，不操作其他服务。

## 验证范围

15 阶段覆盖 Rust/Cargo/OpenSSH 版本、最低 Rust 1.85.1 两种特性 all-targets 检查、稳定版 fmt/两种 Clippy/全量 Rust 测试、Python 工具测试、恢复默认测试构建、release 构建及发行工具运行。报告分别记录测试通过、失败、ignored 和 skipped，忽略不计通过。源码输入在容器运行前后及宿主再次逐一哈希核对，阶段日志也逐一核对；文档不参与编译，可以在执行期间补写结果，Rust/工具源码保持冻结。

[架构保护本地回归](../research/linux-architecture-harness-validation.json) 有三项合成测试，覆盖运行/host/期望架构不符、ELF 架构识别、损坏/非 ELF/字节序或未知机器拒绝；本机全工具 151 项中 150 通过、1 项平台跳过。该报告只证明保护逻辑，不是 Linux 测试通过证据。

容器不验证桌面 Secret Service、真实 Google OAuth/ADC/Colab/Drive、长期连接、真实用户终端或 GitHub 下载/发布；这些仍按对应验收项保留。系统钥匙串选择与未确认的云盘写入范围不受本阶段影响。

## 首次失败与修正依据

[首次尝试](../research/linux-x86-first-attempt.json) 的最低版本两种特性、fmt 和默认 Clippy 通过，默认测试在 Jupyter suite 48 通过/28 失败后以 101 终止，未执行剩余阶段。多数失败显示测试服务端 `socket.read().unwrap()` 遇到 EINTR 后退出；生产执行/空闲读取也未区分 Interrupted。该轮源码输入及日志哈希已在修正前复核，不覆盖为通过。

生产 Jupyter 读取现将 Interrupted 交回原轮询，继续检查总期限、空闲期限和取消；测试端在三秒界限内重读。x86_64 定向复验随后 75/76 通过，另一次自动化握手报通用 WebSocketTransport，根因仍未完全定位。底层 transport 因此补上不超过原读取预算的 EINTR 处理，覆盖握手读取；不重发执行请求、写入或 WebSocket 消息，真正断线继续 Unknown。三项单元验证覆盖剩余预算、真实错误和到期停止。

[第二次完整尝试](../research/linux-x86-second-attempt.json) 在更早的 HTTPS 代理滴入测试 7 通过/1 失败后停止，未运行 Jupyter。测试端未在原 150 ms 窗口内读到 ClientHello。原滴入长度仅约 700 ms，而原断言上限 800 ms，不能充分区分总期限与逐读超时；修正后允许 750 ms 协商总期限，滴入数据超过 2 秒，但必须在 1.5 秒内终止，并先确认实际 ClientHello 字节。产品超时配置未放宽。

容器脚本还修正 Rust FAILED suite 的统计：过去只累计 ok 行，可能在非零阶段中记录 tests_failed=0；现在同时统计 ok/FAILED，保留 ignored 并单列 summary 数。首次失败报告另附完整失败统计，不抹去旧解析行为。架构/统计保护当前四项本地测试通过。

上述尝试结束时最终复验尚未完成，不能凭局部通过关闭 x86_64 或 U10；最终结果见下文。历史 macOS 471/469 和发行报告属于修正前二进制；新的读取实现需要自己的完整回归与构建哈希。

[第三次完整尝试](../research/linux-x86-third-attempt.json) 已通过全部 Jupyter 场景，随后 SSH suite 19 通过/2 失败：服务端握手和背压读取仍直接使用可被信号中断的 TCP 流。新增共用测试适配器，在既有读写预算内只处理 EINTR，握手和数据读取均通过该流；Jupyter 也采用同一适配器，移除重复的 WebSocket 层临时读取辅助。实际连接错误、EOF 和超时不重试，不改变字节/背压断言。macOS 对应 Jupyter 76、SSH 21、HTTPS 代理 8 项全部通过。

[第四次尝试](../research/linux-x86-fourth-attempt.json) 在默认 Clippy 检出共享模块的未使用重导出后停止，未运行测试；改成调用方直接导入公开子模块，未放宽 lint。每次重跑均确认上一容器已终止并保存证据后才移除，只复用本次项目拥有的两个缓存卷，没有访问其他服务。

[第五次尝试](../research/linux-x86-fifth-attempt.json) 重复出现同一导入 lint：初次修正的文本替换未匹配 rustfmt 重排后的声明，因此实际仍保留私有模块和未使用重导出。已明确改为公开子模块并移除重导出，macOS 全目标 Clippy 再次通过；错误尝试不计成功。

[第六次完整尝试](../research/linux-x86-sixth-attempt.json) 通过所有 Jupyter 与 SSH 测试，最后代理 suite 15 通过/1 失败，原因为测试端原始 WebSocket TCP 读取 EINTR。HTTPS/HTTP/SOCKS 代理测试现在也采用同一受限适配器。接着一次启动期间 rustfmt 改变两个导入的排列，已主动停止该快照失效的容器并保留 [中断记录](../research/linux-x86-format-interruption.json)，不算通过或产品失败；后续仅从冻结源码重新开始。

[Python 阶段首次失败](../research/linux-x86-python-first-attempt.json) 的 Rust 默认 474 通过/5 ignored、无默认 472 通过/0 ignored，以及两种 Clippy 均通过，但十项探针目录验证失败。隔离复现发现同一脚本在 /tmp tmpfs 正常，在本次构建卷的已持有目录 fd 上首次枚举可返回空列表，随后枚举可见刚创建的两个文件；UTF-8 和按路径文件数据正确。只在相同目录 fd 上显式 `lseek(0, SEEK_SET)` 后，构建卷连续 20 次创建/非递归清理通过。因此探针在条目核对前重置目录读取位置，不重新打开可能变化的路径，不忽略额外内容，新增保留 fd 与额外文件拒绝回归。该变化使先前私有准备 bundle 的模板哈希失效，真实执行前必须重新准备。

## 冻结源码后的最终结果

[最终容器报告](../research/linux-x86-container-validation.json) 的 15 阶段全通过，Rust 1.85.1 两种特性 all-targets、稳定版两种 Clippy/fmt、默认 474 通过/5 ignored、无默认 472 通过/0 ignored、Python 153 全通过/0 跳过。ignored 的五项 Secret Service 专用测试不算本轮凭据库成功。168 份输入在容器前后及宿主复核哈希一致，每阶段日志哈希一致；uname、Rust host 与两个 ELF machine 均为 x86_64。实际 daemon 为 ARM64，模拟执行边界不变。

[Linux x86_64 发行报告](../research/native-release-linux-x86-validation.json) 17 项全通过，优化版 12,024,784 字节，SHA-256 与容器及宿主保留产物一致。本地候选位于忽略目录 `target/linux-x86-validation/colab-x86_64-unknown-linux-gnu`，没有上传或发布。独立 inspect 已确认本次最终容器终止并移除，本次两个专属缓存卷也已移除；专属构建镜像保留，未操作其他容器、镜像或卷。

macOS 当前 [六阶段回归](../research/eintr-verified-regression-validation.json) 474/472 通过，格式及两种 Clippy 通过；[CLI 对照](../research/eintr-cli-comparison.json) 365 场景无失败、92 一致/54 保留差异/219 扩展，[17 项发行](../research/eintr-native-release-validation.json) 与 [两组图片](../research/eintr-release-image-comparison.json) 通过，release 9,173,728 字节。各平台/构建报告分开核对，不把旧报告的真实 Google 结果移植到当前构建。

本轮只完成这组实现修正与本地平台证据。x86 原生硬件、桌面凭据库、真实认证/跨账户/Drive 长期刷新、GitHub 工作流和发布许可等仍未闭环；整体目标未完成。真实云盘测试范围仍待用户确认。

## 当前源码复验

新一轮十五阶段均通过。默认特性 502 项通过、5 项专用测试按配置跳过，无默认特性 500 项通过，Python 187 项通过。日志和源码快照已复核，容器及其两个专属卷已移除，优化产物用于当前四目标候选。运行仍采用 ARM64 上的 x86_64 模拟环境。详见 [当前容器报告](../research/linux-x86-current-validation.json) 与 [原生发行检查](../research/native-release-linux-x86-current.json)。
