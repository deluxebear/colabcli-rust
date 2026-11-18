# Linux ARM64 隔离验证

本章补充 [U10 平台验收](06-validation-and-open-questions.md) 的 Linux 本地证据，不把 macOS 测试或交叉编译替代 Linux 运行结果。环境为本机 Docker 的原生 Linux ARM64 容器，Debian Bookworm、非 root UID 1000。首次基线镜像 rust:1.85.1-bookworm 的仓库摘要为 sha256:e51d0265072d2d9d5d320f6a44dde6b9ef13653b035098febd68cce8fa7c0bc4；稳定版工具链从 rustup 安装，具体版本由报告记录。

## 隔离与可复现输入

[Dockerfile](../tools/linux-validation/Dockerfile) 的构建上下文只包含 Dockerfile、Cargo.toml 和 Cargo.lock；预先安装 Rust 工具链、Python、OpenSSH 并下载锁定依赖。该阶段可以联网，不执行项目测试，不发送用户环境或凭据。

实际验证由 [容器内脚本](../tools/linux-validation/validate.py) 顺序执行。启动时关闭外网，Cargo 设为 offline；源码仅以只读 bind 挂载，构建目录、缓存、HOME 和报告分别位于本次拥有的临时卷中。根文件系统只读，/tmp 为可执行临时文件系统，capabilities 全部移除并启用 no-new-privileges。容器不挂载 Docker socket、用户 HOME、SSH 配置、系统钥匙串或其他应用的数据卷。现有 Docker 服务和镜像不被停止或清理。

宿主启动后使用 docker inspect 核对 container ID、UID、网络、只读根和全部挂载；脚本本身无法证明宿主究竟挂载了哪些内容，因此报告区分“脚本要求”与“启动方已检查”的隔离证据。CPU 限制为 4，内存限制为 6 GiB，Cargo 并发 2，Rust 测试并发 4；loopback 仍可用于 HTTP/WebSocket 替身。原生 OpenSSH 在容器中运行，私钥和测试配置均由合成测试创建。

脚本保存 Cargo 清单、Rust 代码、测试、资源、Python 工具、容器配置及工作流的逐文件 SHA-256，结束时重新核对未变化。文档不参与编译的文件可以补写结果，README 和资源在验证期间保持不变。原始日志留在宿主临时目录，不提交账户、地址或凭据。

## 验证分层

Rust 1.85.1 分别执行默认与无默认特性的 cargo check --locked --all-targets，验证声明的最低版本编译路径。当前稳定版执行 fmt、两种特性 Clippy/全量测试、Python 工具回归、优化构建与原生发行检查。最低版本编译通过不表示所有旧版 Clippy 的 lint 策略一致。

首轮直接用 Rust 1.85.1 的 Clippy 遇到九项 lint 错误，涉及 format_collect、nonminimal_bool 和枚举布局建议；没有据此改动产品行为或关闭警告。随后将 MSRV 编译与稳定版 lint/运行分层记录。最终 Rust 1.85.1 两种特性编译与 Rust 1.100.0 的两种特性 Clippy、全量测试均已通过，完整阶段记录见下文。

CI 增加独立 [MSRV job](../.github/workflows/ci.yml)，在 Ubuntu 使用 Rust 1.85.1 检查两种特性的 all-targets；远程工作流仍未执行，本机容器证据不冒充 GitHub Actions 结果。

## 复现

先在新临时目录复制两份 Cargo 清单和 Dockerfile，构建本项目专用验证镜像。运行示例（配置路径应为当前仓库的绝对路径）：

```sh
docker run --detach --network none --read-only --user 1000:1000 \
  --cap-drop ALL --security-opt no-new-privileges \
  --cpus 4 --memory 6g --pids-limit 512 \
  --mount type=bind,source=/absolute/path/colabcli-rust,target=/workspace,readonly \
  --mount type=volume,target=/validation \
  --mount type=volume,target=/opt/validation-cargo \
  --tmpfs /tmp:rw,exec,nosuid,nodev,mode=1777 \
  colabcli-rust-linux-validation:rust-1.85.1
```

记录本次返回的 container ID；inspect 核对隔离，再查看 logs --follow 和 wait 的退出码。即使失败也先用 docker cp 取出 /validation/reports，记录失败阶段。确认容器已退出、报告已复制后，仅对本次 container ID 执行 docker rm -v。不要使用 prune、全量 rm 或清理不属于本测试的卷。镜像保留为开发缓存，未推送或发布。

## 验收边界

容器验证不能证明 Linux 桌面 Secret Service 的解锁、用户 D-Bus、Python/Rust keyring 条目互通或真实 Google 行为；默认特性测试使用内存/临时后端，原生库编译通过不等于系统凭据库端到端通过。没有挂载 Drive、申请真实 VM 或访问用户 SSH 身份。

本次仅覆盖 Linux ARM64 与 Debian 用户态，不证明 x86_64、其他 glibc/发行版、桌面终端、真实 Colab SSH 或 Windows。整个 U10 和认证/Drive 矩阵仍按必要真实证据保持未完成。

## 首次结果（阶段 54）

[Linux 验证报告](../research/linux-container-validation.json) 记录 15 个阶段全部退出 0，144 份实现/测试/工具/配置文件在容器前后及宿主复核中哈希一致。默认特性 458 项、无默认特性 456 项 Rust 测试通过，两组 ignored 均为 0；Python 59 项全部通过，没有沿用 macOS 上的 Linux 专属跳过。

[Linux 原生发行记录](../research/native-release-linux-validation.json) 通过 16 项打包与运行检查，目标为 aarch64-unknown-linux-gnu。release 为 10,361,416 字节，默认测试用 debug 为 170,065,648 字节；体积包含对应工具链/平台/特性差异，不作为其他平台承诺。优化产物哈希与报告一致，未发布 Release。

复制并核对报告后，本次两个已终止容器和三个临时卷均已移除，并通过 inspect 确认不存在。只保留项目专用构建镜像/cache，未操作已有服务。原始日志保留在宿主临时目录，仓库仅保存脱敏结果。

本阶段没有修改 Rust 产品代码、README 或内嵌资源，macOS 上一阶段的二进制、CLI 对照及发行报告仍保持哈希一致；没有将 Linux 结果混入原有 macOS 报告。

研究文档检查最终覆盖 59 篇 Markdown、660 个相对链接、125 个固定源码引用和 143 个固定仓库源码哈希，无错误；固定研究清单与本阶段 144 份工作区输入快照分别计数，不混同。

## 当前实现复验

在 Drive 凭据输出、token 对照和 consent 自动登记之后，用相同不可变镜像重新创建独立容器，完整执行同一 15 阶段脚本。新的 [Linux 回归报告](../research/linux-regression-validation.json) 与 [Linux 发行复验](../research/native-release-linux-regression-validation.json) 单独保存，以上首次报告保持原样，避免覆盖早期证据。

当前 Rust 1.85.1 的两种特性 all-targets 编译通过；Rust 1.100.0 / Cargo 1.100.0 的两种特性 Clippy、fmt、全量测试通过。默认特性 466 项通过、5 项 Secret Service 专用测试按设计 ignored；无默认特性 464 项通过、0 ignored。Python 工具 74 项全部通过、0 跳过；这里没有沿用 macOS 的 Linux 专属跳过。ignored 的五项不计为本次原生 Secret Service 成功，专用环境的独立证据仍见 [55](55-native-secret-service-validation.md)。

17 项原生发行检查通过，包含优化版 Drive 脱敏/清除、consent 新账户登记、state/S256 和存储邮箱提示。实际使用 Linux ARM64 daemon、arm64 镜像和 aarch64 进程，没有用交叉编译代替执行；OpenSSH 为 Debian 9.2p1。155 份受检输入在容器前后及宿主复核中一致，逐阶段日志哈希也已核对。根只读、UID 1000、网络关闭和全部挂载由启动方 inspect 核实。

release 为 10,361,416 字节，测试 debug 为 170,136,208 字节。release 复制到宿主忽略目录 target/linux-validation/colab-aarch64-unknown-linux-gnu，SHA-256 与容器和原生发行报告一致，作为本地开发产物保留；没有上传或发布。复制报告和产物后，本次一个已退出容器和两个匿名卷已删除，并逐一 inspect 确认不存在，项目镜像缓存保留，未清理其他服务或卷。

本轮不修改产品 Rust、README 或内嵌资源；macOS 默认/无钥匙串/release 的既有报告继续对应各自二进制。当前 Linux 结果只证明该 ARM64/Bookworm 用户态，仍不代表桌面凭据库、x86_64、真实 Colab SSH、OAuth/ADC、Drive 或全部 U10 完成。

## JSON 下载修正后的复验

继续使用同一 ARM64 镜像和独立非 root 容器。验证器把发行声明与许可证材料纳入输入摘要。Rust 1.85.1 的两种特性编译检查、稳定版两种 Clippy 及格式检查通过。默认特性 502 项通过、5 项专用测试按设计跳过，无默认特性 500 项通过。首轮 Python 回归在检查子进程归属时失败，记录见 [首次复验](../research/linux-arm64-current-first-attempt.json)。

该测试会在启动子进程后立即读取 /proc，可能早于新进程环境可见。测试现等待子进程发送就绪标记，再检查真实进程归属和 pidfd 终止。产品路径仍沿用自身的进程身份重查。第二个同配置容器的 Python 全量 187 项通过，其中 Linux 进程归属测试另连续通过 100 次。两次运行之间只有该 Python 测试文件变化。

第二个容器继续完成优化构建和十八项原生发行检查。发行物为 10,492,488 字节，源码及材料共 191 份输入的摘要已在宿主复核。两个测试容器及各自的两个临时卷均已确认移除。Linux 发行物与同一版本的 macOS ARM64 发行物已一起生成本地更新清单，八项文件校验和均通过。结果见 [Linux ARM64 发行复验](../research/native-release-linux-arm64-current.json) 和 [macOS ARM64 发行复验](../research/native-release-macos-current.json)。
