# 研究证据与检查记录

`source-inventory.json` 固定三个仓库的提交、受 Git 跟踪文件的 SHA-256、静态命令入口及测试函数位置。它不包含用户凭据，也没有复制源码正文；命令参数取自 AST。

`documentation-checks.json` 是文档检查工具的结果，记录相对链接、固定源码行号及清单哈希的核对情况。零错误仅表示这些静态检查通过，不等于文档中的每项设计已经实现，也不等于真实 Colab 验证通过。

`kernel-codec-probe.json` 保存七项离线协议检查结果及合成帧。这些帧来自固定依赖的纯编解码函数，含中文、非 UTF-8 和空 buffer，可供 Rust 实现重放对照；它们不是真实账户抓包。

三个仓库分别检出文档指定提交后，从项目根目录执行：

```sh
python3 tools/collect_source_inventory.py \
  --cli /absolute/path/google-colab-cli \
  --ext /absolute/path/colab-ext \
  --kernel /absolute/path/jupyter-kernel-client \
  --output research/source-inventory.json

python3 tools/check_research_docs.py \
  --cli /absolute/path/google-colab-cli \
  --ext /absolute/path/colab-ext \
  --kernel /absolute/path/jupyter-kernel-client \
  --output research/documentation-checks.json

python3 tools/probe_kernel_codec.py \
  --kernel /absolute/path/jupyter-kernel-client \
  --output research/kernel-codec-probe.json
```

生成过程使用 Python 标准库和 Git，不安装上游依赖、不导入上游模块、不访问真实账户。codec 探针从已核验哈希的 utils.py 提取五个纯函数执行，不运行模块的其他顶层代码。这些 Python 脚本是研究工具，不属于未来 Rust 客户端的运行时依赖。

`run-wrapper-probe.json` 保存八项本地 Python 合成脚本检查，由 `python3 tools/probe_run_wrapper.py --output research/run-wrapper-probe.json` 生成。它从 Rust 源码提取包装模板，验证退出码、argv、future import 和 finally 结果；使用假的 IPython.display，不请求 Colab，不证明真实 Google 互通。

`cli-comparison.json` 与 [可读报告](cli-comparison.md) 由 `tools/compare_cli.py` 生成，保存固定官方 CLI 与 Rust CLI 的隔离执行结果、请求摘要、产物检查、扩展分类和实际差异。此工具会导入官方运行依赖，使用 `python -S` 跳过扩展注入；与前述纯静态/纯函数探针不同。运行方式及网络、凭据边界见 [CLI 自动对比](../docs/14-cli-comparison.md)，不代表线上 Google 兼容。

[official-cli-fixtures.json](official-cli-fixtures.json) 保存从固定官方执行结果提取的四种历史导出 golden 样本，Notebook 随机 cell ID 仅在回归比较时归一化。`tests/cli_compat.rs` 独立检查 Rust 兼容导出、脱敏、私有权限以及内部审计数据不被修改。

[合并版历史路由](merged-history-routing.json) 保存九个离线账户路由场景的实际输出和构建哈希。通过 [对照脚本](../tools/compare_merged_history.py) 复现。


`live-cpu-validation.json` 保存用户授权的单次真实 CPU 验收脱敏结果，包含 33 项检查的退出码、耗时及最终释放复查。它不包含真实会话名、endpoint、账户凭据或原始服务响应；实际发现的格式问题与验收边界见 [线上记录](../docs/15-live-cpu-validation.md)。

[当前 CPU 控制面复验](live-cpu-control-current.json) 保存本次实际使用的构建哈希、六项检查和独立释放确认。覆盖具名停止的真实成功路径。

[真实 CPU 终端输入](live-cpu-stdin.json) 保存 Unicode 和密码交互的七项检查及独立释放确认。报告只含验证结果和构建哈希。


`install-wrapper-probe.json` 由 `python3 tools/probe_install_wrapper.py --output research/install-wrapper-probe.json` 生成，记录实际 Rust install 模板的七项替身执行检查、源文件与模板哈希。不运行真实 pip/uv、不访问 Google 或软件源；与 Rust 协议测试共同覆盖数据编码和回退行为，仍不能证明真实安装成功，见 [17](../docs/17-install-implementation.md)。

SSH 差分仍写入 cli-comparison.json；proxy stdout 单独保存 base64 原始字节，wire 保留两个 agent query 值。SSH、ssh-keygen、反压、信号与资源恢复验证见 [19](../docs/19-ssh-implementation.md) 和 tests/ssh.rs；只有 loopback 与合成凭据，未连接真实 sshd 或 Google。

Drive 独立凭据的 CLI 场景在 cli-comparison.json 中单列 rust_extension；固定官方 CLI 不包含 drive 组。本地 PKCE、轮换、账户隔离和 PTY 检查见 [20](../docs/20-drive-credentials-implementation.md)，没有真实 Drive 授权或挂载。

远端 metadata 组件的离线契约测试为 `python3 -B tools/tests/test_drivefs_shim.py`，直接执行 Rust 内嵌脚本；不依赖扩展源码或真实凭据。测试范围和未接入的挂载控制器见 [21](../docs/21-drivefs-metadata-implementation.md)。

控制器与私密输入包装测试为 `python3 -B tools/tests/test_drivefs_controller.py`，测试实际内嵌源文件，FUSE/DriveFS 使用替身；Linux pidfd 真实子进程用例在 macOS 跳过。Rust 另有实际 loopback WebSocket 的私密输入和断线防重放测试，验收边界见 [22](../docs/22-drivefs-controller-implementation.md)。

Drive status/probe-cleanup 场景以 Rust 扩展计入 CLI 报告，包含未知 mutation 旁的只读核查、仅清理所属 probe、创建未知拒绝及脱敏 pending 展示。凭据冻结/采用/确认回收的 API 和测试见 [23](../docs/23-drivefs-probe-and-recovery.md)，仍未自动编排真实挂载。

- [本机原生发行验证](native-release-validation.json)：真实 macOS arm64 release 构建、离线打包与校验和；无远程发布或跨平台运行结论。

- [合并版运行时帮助与参数](merged-help.json)：固定 CLI/EXT/KERNEL 的 83 个路径、实际长短 help 和 Rust 入口核对；合并版业务等价仍待验证。

[sts-auth-probe.json](sts-auth-probe.json) 为安装的 google-auth 2.62.2 纯内存 STS 请求观察；复跑方法和本地实现范围见 [Workforce 用户 ADC](../docs/37-adc-workforce-user-implementation.md)，不代表真实云服务接受性。

[executable-command-probe.json](executable-command-probe.json) 为 21 项纯标准库 shlex 分词观察，不执行命令；复跑和 Rust 对照见 [可执行 ADC 来源](../docs/39-adc-executable-implementation.md)。

[native-release-validation.json](native-release-validation.json) 由 [validate_native_release.py](../tools/validate_native_release.py) 使用本机 release 二进制、临时配置和 loopback 重建；不发布候选或读取真实凭据。

[executable-expiry-validation.json](executable-expiry-validation.json) 记录相同慢进程令牌过期用例在修复前后原生产物上的差异，仅含合成结果和二进制哈希；复跑方法见 [39](../docs/39-adc-executable-implementation.md)。

[aws-signing-probe.json](aws-signing-probe.json) 是合成 AWS key 的三组标准库 HMAC/google-auth 双参考签名，Rust 单元测试逐项核对；方法与真实平台缺口见 [40](../docs/40-adc-aws-implementation.md)。

[merged-account-comparison.json](merged-account-comparison.json) 是固定合并版账户 list/current 的 16 项实际离线业务差分；file 存储、临时 HOME、合成会话，禁止原版调用网络/钥匙串/浏览器/子进程。复跑方法和保留差异见 [41](../docs/41-account-inventory-implementation.md)。

[merged-account-mutations.json](merged-account-mutations.json) 是固定合并版账户 use/set-drive/rename/remove 的 27 组实际变更对照，检查输出、退出码、保序状态、文件哈希和失败负载不变。Rust 端必须是无原生凭据库构建，哈希与默认 debug 分开记录；方法、恢复及保留边界见 [42](../docs/42-account-mutations-implementation.md)。

[merged-account-selection.json](merged-account-selection.json) 是固定合并版账户查询/变更的 21 组根级/局部选择对照；复用无原生凭据库安全门、状态和失败负载校验。方法与未覆盖入口见 [43](../docs/43-account-selection-implementation.md)。

[merged-inventory-comparison.json](merged-inventory-comparison.json) 对比实际合并版的 22 组会话/用量探针：真实合成 OAuth 缓存加载、精确 loopback origin、逐账户认证和顺序、缓存刷新/裁剪及失败保持。七组仅错误文案的保留差异单独分类；方法及 JSON 迁移见 [44](../docs/44-merged-inventory-implementation.md)。

执行富文本的固定上游纯函数样本见 [display-text-fixtures.json](display-text-fixtures.json)，采集脚本为 [probe_display_text.py](../tools/probe_display_text.py)。16 组中 13 组按排版空白规范化比较，3 组明确保留终端布局差异并验证内容；不是完整 CLI 或真实 Colab 富文本验收。

图片传输/保存的上游纯函数证据见 [image-output-fixtures.json](image-output-fixtures.json)，由 [probe_image_output.py](../tools/probe_image_output.py) 生成；3 组传输和 4 组保存仅使用合成字节、自有临时目录与 fake stdout，不代表真实图形终端像素验收。

优化二进制的两组显式图片保存差分见 [release-image-comparison.json](release-image-comparison.json)，包含实际 release 哈希；属于补充检查，不与完整 CLI 差分或 PTY 视觉验收混计。

[ADC 配额项目探针](adc-quota-probe.json) 记录已安装 google-auth 的十组纯内存 header/环境/来源观察及四个依赖源码哈希；生成器为 [probe_adc_quota.py](../tools/probe_adc_quota.py)，不执行 ADC 发现、刷新或网络请求。

Linux ARM64 隔离运行结果见 [容器验证](linux-container-validation.json)，包含 MSRV/稳定版、逐阶段状态、源码与日志哈希和已核验的隔离/清理信息；优化产物另见 [Linux 原生发行记录](native-release-linux-validation.json)。不替代真实 Secret Service、Google 或其他 Linux 架构证据。

Drive consent 自动登记后的当前实现另有 [Linux 回归](linux-regression-validation.json) 与 [发行复验](native-release-linux-regression-validation.json)：15 阶段全通过，466/464 项 Rust、74 项 Python、17 项发行检查；默认五项 Secret Service 专用 ignored 单列，不计通过。155 份输入与宿主/容器一致，报告包含本次隔离和资源已清理的确认；保留本地 Linux release，早期报告不覆盖，也不作为四平台或真实 Google 验收。

Linux 私有 D-Bus/GNOME Keyring 的 [原生 Secret Service 验证](secret-service-validation.json) 记录 12 阶段、五项实际原生测试、默认 ignored 保护、源码/日志哈希与资源清理。合成数据只用于独立容器，不将该结果当成完整原版 CLI、桌面或共享配置文件锁互通。

[真实 SSH 预检查](live-ssh-preflight.json) 目前记录缺认证、未执行、创建 0 个会话且无检查项。该报告不是线上 SSH 成功证据；账户、基线 endpoint 和配置路径只存于未提交的私有 ledger。

[SSH 验证脚本流程](live-ssh-harness-validation.json) 是合成 CLI 子进程与本机临时密钥的开发工具回归，不使用 Google 或用户凭据。

[session 构造探针](kernel-session-probe.json) 实际加载固定依赖的构造和 URL 生成路径，三组输入确认传入 session/extra_params 未被消费；所有包源码核对固定哈希，REST/WS/线程使用受控替身，网络禁止。通过带所需依赖的 Python 执行：python3 -S -B tools/probe_kernel_session.py --kernel PATH --output research/kernel-session-probe.json。与 Rust loopback、历史真实 CPU 的边界见 [运行时说明](../docs/03-runtime-and-wire-protocols.md)。

[Drive 凭据合并对照](merged-drive-credentials.json) 使用实际合并 CLI 与无原生凭据库 Rust，覆盖 12 组离线路由、脱敏元数据、清除和缺凭据 token。除临时 HOME、JSON 字段顺序和空白外不规范化业务字段；逐文件验证仅删除选定凭据。其余安全差异与未覆盖刷新范围见 [57](../docs/57-merged-drive-credentials-implementation.md)。

[Drive token 合并对照](merged-drive-tokens.json) 后续覆盖七组实际 loopback 刷新、whoami、错误与时区场景；由 [刷新工具](../tools/compare_merged_drive_tokens.py) 生成。服务端验证实际客户端/refresh token 来源，报告只保留合成 access token（该命令明确请求打印）、元数据和布尔持久化状态；不保存 refresh/client secret。七组为契约验证通过，不声称完全等价；scope、轮换、身份注册表、短有效期与时区差异分别登记，失败暂存通过实际 show 恢复且不重发网络请求。

[Drive consent 路由对照](merged-drive-consent.json) 运行九组实际合并版 URL-only 授权起始流程，验证新账户登记、参数位置、环境、邮箱和 pending 归属。原版外部操作禁止，Rust 无原生凭据库；报告只记 state 匹配及 PKCE 验证布尔值，不保存 verifier 或随机 state。原版输出/schema 的差异、完成阶段禁止创建及原子登记边界见 [58](../docs/58-drive-consent-profile-implementation.md)。

[合并版 URL 对照](merged-url-comparison.json) 覆盖 14 组显式会话的离线账户路由、authuser、host/endpoint 与失败诊断，由 [URL 工具](../tools/compare_merged_urls.py) 生成。七组一致，七组已登记差异；业务 URL 不做相等化规范化，所有已有 payload 保持原字节。未执行浏览器、控制面同步或真实前端连接，范围见 [27](../docs/27-utility-commands-implementation.md)。

[macOS x86_64 Rosetta 验证](macos-rosetta-validation.json) 记录六阶段、两种特性 466/464 项测试、Clippy 及实际发行候选运行。120 份输入及日志哈希核对一致，临时 scratch 已删除；报告保留翻译执行模式和单平台本地候选身份，不改称 Intel 原生或已发布证据，范围见 [59](../docs/59-macos-x86-rosetta-validation.md)。

[全范围验收核对](acceptance-audit.json) 保存 53 个 V 条目、17 个 U 问题的证据/局限快照与文件哈希；说明见 [60](../docs/60-acceptance-audit.md)。文件存在、哈希匹配和 ID 无遗漏仅为结构检查，不是测试覆盖或整体完成的证明；overall_complete 明确为 false。

[真实 SSH 验收](live-ssh-validation.json) 与 [钥匙串预检查](live-ssh-keyring-preflight.json) 记录用户明确选择系统钥匙串后的真实单台 CPU/五项 SSH 检查和后端确认释放，独立于文件后端失败记录。报告仅含二进制哈希、计数、测试标签与清理结论，无真实账户/endpoint/token；完整边界见 [56](../docs/56-live-ssh-validation-and-guarded-cleanup.md)。

[真实 run 验收](live-run-validation.json) 记录一次实际 run 的脚本/argv/环境断言、SystemExit(23)、自动释放和后端确认，无真实账户/endpoint/token。硬件只记录默认 CPU 请求，不虚构释放前的测量；未覆盖 keep/信号/故障，详见 [13](../docs/13-run-and-history-implementation.md)。

[真实 run keep](live-run-keep-validation.json) 记录保留会话、后续 exec 读取随机测试文件及最后受保护 stop；[真实 run SIGTERM](live-run-signal-validation.json) 记录收到远端执行标记后仅向 CLI 发信号、退出 143 和自动释放。两台会话顺序创建并经独立后端查询确认清理，无其他新增 assignment。私有归属账本不提交；CPU 为请求类型，信号覆盖仅 SIGTERM，边界见 [13 后续](../docs/13-run-and-history-implementation.md)。

[真实 SSH 扩展验收](live-ssh-extended-validation.json) 记录第三次独立尝试的七项通过，新增远端登录 shell PTY 与命令 SIGTERM/143；[首次](live-ssh-extended-first-attempt.json) binary stdio/255 和 [第二次](live-ssh-extended-second-attempt.json) 交互结果未识别的失败分别保留，原因未定位。三台资源均后端确认释放，原始终端/失败命令字节仅留私有 state-dir。[扩展脚本回归](live-ssh-extended-harness-validation.json) 单独记录本地合成证据，不能替代真实结果；完整边界见 [56 后续](../docs/56-live-ssh-validation-and-guarded-cleanup.md)。

[真实执行断线](live-disconnect-validation.json) 保存单次真实 CPU、TLS 透明连接切断、未知状态与不重放、显式重启恢复和最终后端释放证据；历史终止原因/部分输出为同次数据的独立后续核查，两个验证器版本哈希分开记录。[断线工具回归](live-disconnect-harness-validation.json) 当前记录十二项本地安全检查及 91 项工具测试（1 项平台跳过），包含 kernel-death 模式的共用清理责任，不替代线上证据。账本、原始输出和消息身份不提交，边界见 [61](../docs/61-live-execution-disconnect-validation.md)。

[广播状态修复回归](kernel-broadcast-regression-validation.json) 保存 iopub dead/restarting Unknown、部分输出与持久化保护修复后的六阶段本地验证，默认/无默认特性 467/465 项、两种 Clippy 和格式检查通过。完整 CLI 365 场景与本机 17 项发行/两组非 TTY 图片对照已更新到各自新构建。[真实广播预检查](live-kernel-broadcast-preflight.json) 独立记录新构建钥匙串超时、未创建 VM；不与历史构建真实断线通过混同。范围见 [61 后续](../docs/61-live-execution-disconnect-validation.md)。

[Drive 本地预检查](live-drive-offline-preflight.json) 只记录既有资料的字段存在性和 scope 声明，没有导出字段值、刷新 token 或访问云盘。资料存在不等于 Google 接受，也不关闭真实 Drive 授权、挂载或身份验收。

[Drive 验收准备](live-drive-validation-preparation.json) 记录本机候选资料的离线计划生成、三个 payload 哈希、14 步非执行清单及原配置字节不变；不含真实账户、路径或凭据值。[准备工具回归](live-drive-preparation-validation.json) 记录 17 项本地安全回归及全工具 108 项（1 项平台跳过）。FUSE guard 仅为合成 mountinfo 与普通本地目录测试，没有实际挂载或云盘读写，说明见 [62](../docs/62-drive-live-validation-preparation.md)。

Drive 受控执行器、运行时 endpoint 条件、失败恢复与本地验证边界见 [63](../docs/63-drive-live-validation-executor.md)。本阶段没有真实云盘写入或新建 VM。

[Drive API 回归](live-drive-api-validation.json) 保存只读观察器十项 HTTP loopback 和当前执行器 22 项合成验证，完整工具 140 项（1 项平台跳过）。它不含真实存储身份、文件或 Google 请求证据；接入前 20/128 的执行器报告保留为历史输入快照。

[Drive 服务端清理回归](live-drive-api-cleanup-validation.json) 对应 API 17 项、执行器 23 项和全工具 148 项（1 项平台跳过）。只对已验证 ID 发送 GET，将回收站、404、本地 FUSE 清理和 VM 释放分开记录；没有真实云盘证据，也不推定永久删除。

Linux x86_64 隔离模拟执行、EINTR 修正与失败记录见 [64](../docs/64-linux-x86-container-validation.md)；不能代替原生 x86 硬件或真实 Google 验收。

[Linux x86_64 最终报告](linux-x86-container-validation.json) 保存冻结输入的 15 阶段结果、168 份输入及日志哈希、实际镜像/daemon/ELF 架构和清理确认；[发行报告](native-release-linux-x86-validation.json) 对应 17 项本地打包检查。默认 474（另 5 ignored）/无默认 472、Python 153 无跳过通过。之前六次阶段失败及目录探针失败各自保留，未算最终成功。该结果是 ARM64 daemon 的 x86_64 模拟执行，没有真实 Google/钥匙串或发布操作。

[Drive 真实验收](live-drive-validation.json) 保留三次尝试、显式只读恢复、上传后卸载及独立服务端清理的分段结果；[首次](live-drive-first-attempt.json) 与 [第二次](live-drive-second-attempt.json) 失败仍单独保存，不能改记原执行器无中断通过。授权仅一个随机目录及两份测试文件，目录名在已确认清理后复用；没有递归删除、跨真实账户或长期刷新证据。最新状态与本轮回归见 [65](../docs/65-live-drive-validation-and-recovery.md)。

[Drive 修正后的最终本地回归](live-drive-regression-validation.json) 绑定 476／474 项 Rust、两种 Clippy、fmt 与 163 项工具测试（1 项平台跳过）；[365 场景对照](live-drive-cli-comparison.json) 维持 92 一致、54 差异、219 扩展，未将差异归一成一致；[17 项本机发行检查](live-drive-native-release-validation.json) 与最终 debug/release 哈希一致。真实报告保留不同阶段二进制与显式恢复，整体设计目标尚未完成。

[恢复记录合并回归](checkpoint-merge-regression-validation.json) 保存旧实现丢更新失败及修复后三项定向、478／476 项 Rust、两种 Clippy、fmt、163 工具测试（1 项平台跳过）的独立证据；[365 场景对照](checkpoint-merge-cli-comparison.json) 与 [17 项本机发行检查](checkpoint-merge-native-release-validation.json) 绑定最终构建。没有新 Google/VM 操作，也没有证明首次真实 Drive 丢字段的具体根因。[远程发行预检查](release-remote-preflight.json) 只读记录无分支/无 Release，未执行发布。细节见 [66](../docs/66-recovery-checkpoint-merge.md)。

[真实 DEFAULT 采集](live-codec-validation.json) 记录一台专属 CPU 的原始 Rust/上游解码一致、实际非 UTF-8/空 buffer、reply/idle 和后端确认释放；[公开样本](live-codec-samples.json) 是替换 header、清除 metadata 后重新编码的数据，原始响应仅在外部私有目录。[专项回归](live-codec-regression-validation.json) 记录两种特性各三项原 codec 和一项新样本测试、fmt/两种全目标 Clippy、170 工具测试（1 项平台跳过）。执行器失败退出码修正后的 [真实预检查](live-codec-preflight.json) 创建为零，与实际采集版本分别绑定。说明及 U06 范围见 [67](../docs/67-live-codec-capture-and-replay.md)。

[真实 kernel 崩溃](live-kernel-death-validation.json) 记录第二次尝试的四项通过和独立回执核对；[首次失败](live-kernel-death-first-attempt.json) 保留缺输出结果，两台 CPU 均确认释放。[工具回归](live-kernel-death-regression-validation.json) 绑定最终测试工具输入，产品未重编译。范围与限制见 [68](../docs/68-live-kernel-death-validation.md)。

[macOS 代码身份检查](macos-keychain-identity-validation.json) 保存三份实际代码身份及同路径两次隔离编译的身份变化，签名检查未访问钥匙串或 Google、未运行探针；另列两次 30 秒上限的真实会话查询均成功。它不能证明真实条目 ACL 或此前超时根因，说明见 [69](../docs/69-macos-keychain-code-identity.md)。

[External account 原版探针](external-client-auth-official-probe.json) 记录安装环境 google-auth 的五组纯合成请求捕获，无网络/真实凭据；[Rust 回归](external-client-auth-regression-validation.json) 绑定客户端认证实现、三项新测试与完整本地检查。协议、失败恢复和真实权限边界见 [70](../docs/70-external-account-client-auth.md)。

[X.509 原版探针](x509-adc-official-probe.json) 保存四组合成证书主体和 STS 证书路径捕获，无网络或真实密钥；[完整回归](x509-adc-regression-validation.json) 绑定材料快照、真实本地 mTLS 和七项专项、两种特性及构建检查。范围见 [71](../docs/71-x509-adc-and-mtls.md)，不代表真实 Google 或 metadata mTLS 验收。

[Metadata mTLS 原版探针](metadata-mtls-official-probe.json) 记录十二组文件存在/模式选择和六组 adapter 控制流，无宿主证书或网络；[回归报告](metadata-mtls-regression-validation.json) 绑定本地双向 TLS、轮换、严格模式和默认回退、四个 CLI 场景及完整构建检查。真实平台与 token 绑定未验收，见 [72](../docs/72-metadata-mtls.md)。
