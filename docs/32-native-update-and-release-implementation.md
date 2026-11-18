# 原生更新与发行候选实现

## 渠道与输入

发行渠道已选择 GitHub Releases。已创建公开仓库 [deluxebear/colabcli-rust](https://github.com/deluxebear/colabcli-rust) 并配置 origin；尚未推送代码、发布 Release 或执行远程构建。当前实现不猜测发行版本，也不把缺少发行源解释为“已是最新”。上游 Python 的 pip/uv 更新流程不适用于本项目的 Rust 可执行文件，此处作为 Rust 扩展单独验收。

```sh
colab update --manifest https://github.com/deluxebear/colabcli-rust/releases/latest/download/colab-update.json
colab update --manifest https://github.com/deluxebear/colabcli-rust/releases/latest/download/colab-update.json --install
colab update --status
colab update --rollback
```

上述链接为规划中的真实仓库地址，首个 Release 发布前不会有可下载清单。源优先级为显式 --manifest、运行时 COLAB_CLI_UPDATE_MANIFEST、编译时 COLAB_CLI_BUILTIN_UPDATE_MANIFEST。编译内嵌地址必须为无认证信息、query、fragment 的公开 HTTPS URL。status/rollback 离线运行，不解析环境发行源，不初始化账户配置或查询钥匙串。--install-path 指向已有当前版本副本，可用于独立安装目录；默认目标为正在运行的文件。--json 输出结构化结果；status 始终输出 JSON。默认每次 HTTP 请求期限 30 秒，可设置 1–3600 秒，不代表安装全过程的总期限。

交互式每日检查与独立缓存已补齐，见 [45](45-daily-native-update-implementation.md)；不自动安装。

## 清单与校验

清单 schema_version=1，package=colabcli-rust，version 为 SemVer；artifacts 以 target triple 为键，包含绝对 url、字节 size、64 位十六进制 sha256。支持 Linux GNU/macOS 的 x86_64、aarch64 四个目标。清单最多 64 KiB，文件最多 256 MiB。比较新旧版本时忽略 build metadata，不执行降级安装；版本自检则要求与清单完整匹配。

初始地址接受无认证信息、query 或 fragment 的 HTTPS。回环测试可用 HTTP。最多跟随五次符合规则的重定向，允许下载资源时使用签名 query。链路一旦使用 HTTPS，后续跳转始终使用 HTTPS。公开清单及发行物的重定向均拒绝回环目的地。清单对发行物 URL 的约束以实际返回清单的地址为准。回环地址按 URL 主机判断，涵盖 `localhost`、IPv4 回环段、IPv6 回环和 IPv4 映射回环。诊断只返回错误类别，避免暴露响应正文和下载 URI。

安装先校验传输长度、SHA-256、原生 ELF/Mach-O 架构，然后执行固定参数 version 自检；5 秒超时或输出超过 4096 字节会终止并回收进程组。SHA-256 证明文件与清单一致，不是独立发布者签名；应仅配置可信发布者。版本自检会执行该发布者的程序，环境清空，不经过 shell 或 Python 包管理器。

## 状态、失败与清理责任

只更新当前用户拥有的普通可执行文件及私有安装目录；拒绝 symlink、setuid/setgid 和组/其他用户可写目标或父目录。更新记录与暂存位于安装目录下私有 .colab-updates，按规范目标路径隔离并持操作锁。下载和准备替换期间再次核对原文件哈希，发现外部修改即拒绝覆盖。

准备完成后持久化 Prepared，原子替换，再持久化 Committed。成功保留唯一一代旧文件；连续更新替换旧备份。显式 rollback 验证当前文件和备份，发布 RollingBack 后恢复旧文件，并消耗备份。Prepared/RollingBack 阻止新安装；用 status 核查，再显式 rollback 恢复。文件被外部修改、日志损坏或备份不匹配时拒绝覆盖。不能手删记录绕过保护。

普通下载/校验失败清理本次临时目录。替换失败且原文件可确认未变时撤销准备记录；替换后同步或记录提交失败保留恢复责任。清理旧目录失败以 cleanup_pending 报告，遇未知文件或 symlink 保留现场。发布 Prepared 之前被强制杀死可能留下无引用暂存目录，status 以 staging_generations 计数，不自动删除。status 是只读非原子快照；不承诺并发变化时所有字段来自同一时刻。整个流程不改变账户凭据或 VM，没有远端资源需要回收。

## 打包与 GitHub Actions

[离线打包工具](../tools/package_release.py) 读取 Cargo 版本与四平台二进制，核对原生头、架构、长度，生成版本化文件、colab-update.json、SHA256SUMS。不执行输入程序，不发布，输出目录必须不存在。--allow-partial 仅用于本地单平台验证。

后续补齐 [上游声明与许可文本](../THIRD_PARTY_NOTICES.md)，打包时必须复制声明及三份许可文本并纳入 SHA256SUMS；缺失、空文件、符号链接、非 UTF-8 或超过 1 MiB 均失败，临时目录自动清理，详见 [73](73-release-notices.md)。这不代表 U12 授权或完整依赖许可审计已完成。

随后加入 [四目标 Rust 依赖材料](74-rust-dependency-licenses.md)，同样随候选校验和分发；锁文件变化而材料未更新会被拒绝。当前本机 release 的十八项检查包含完整候选文件集合及原文字节验证，不能替代四平台远程构建或真实更新源验收。

[候选工作流](../.github/workflows/release-build.yml) 通过手动 `workflow_dispatch` 运行，权限为 `contents:read`。配置任务根据触发仓库和 `Cargo.toml` 版本生成 `releases/download/v<版本>` 与 `releases/latest/download/colab-update.json`，四平台构建、原生验证及最终打包共用这些地址。可显式覆盖为自定义 HTTPS 地址。GitHub 下载目录的固定 tag 必须匹配当前版本，填错时在编译前退出。最终产物上传为 Actions artifact，供人工审阅和发布。

release 使用 strip symbols、thin LTO、单 codegen unit；保留默认 panic 行为。当前只在本机验证真实原生产物，其他 runner、最低 glibc/macOS、签名/notarization、真实 GitHub 下载和安装仍待验收。不能把合成 ELF/Mach-O 头测试解释为跨平台可运行证据。

## 测试证据

15 项 Rust 测试使用临时目录、loopback HTTP 和本地 rustc 编译的合成版本程序，覆盖安装/离线回滚、一代备份、并发串行、外部修改、Prepared/RollingBack 恢复、错误清单/长度/哈希/架构/版本、重定向、版本自检挂起/超量输出、权限和失败清理。替换权限失败分支在 root 运行时跳过。另有 8 项 Python 打包测试，CLI 差分增加一个 help 差异与十个原生更新扩展场景。完整结果见 [验收](06-validation-and-open-questions.md) 与 [差分](14-cli-comparison.md)。

最终验证：默认特性 309 项、无默认特性 307 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 298 个场景为 92 一致、54 保留差异、152 Rust 扩展，无断言失败，原有 287 场景分类不变；root/account/drive/ext 清单一致。离线打包工具 8 项测试通过。

本机 aarch64-apple-darwin 的真实 release 构建、打包文件字节一致性、manifest 大小/哈希、SHA256SUMS 以及打包后 version 运行均通过。release 为 9,157,216 字节，默认特性测试用 debug 为 44,693,592 字节；两者不作为跨平台体积承诺。安全证据见 [原生发行验证](../research/native-release-validation.json)，未上传二进制或发布 Release。

本机打包与运行检查现可通过 [validate_native_release.py](../tools/validate_native_release.py) 复现：在 release 构建后执行 python3 tools/validate_native_release.py。包含原生格式、版本、校验和、账户别名/联合授权帮助、补全和优化二进制的非交互可执行 ADC→loopback STS、执行期间过期拒绝。候选构建工作流每个 runner 已接入该检查并上传独立 validation 报告；远程尚未执行，不作为四平台通过的证据。

本机校验另加入 AWS 环境凭据生成签名后的 loopback STS 登录，后端独立验证实际签名；当前共 17 项原生检查通过（含账户清单、离线未知状态、根级/局部账户校验、合并 JSON 错误行及打包后的 Drive 脱敏/清除、consent 新账户登记和邮箱提示），具体平台、体积和哈希见发行记录。候选工作流未远程执行，未发布 Release。

Linux ARM64 的实际优化二进制也已在非 root、离线容器中通过 16 项原生发行检查，见 [54](54-linux-container-validation.md) 与 [Linux 发行记录](../research/native-release-linux-validation.json)。Linux release 为 10,361,416 字节，默认测试用 debug 为 170,065,648 字节；本机 macOS 报告保持独立，没有合并为四平台已通过。没有上传或发布 Release。

当前实现随后在同样隔离环境中重新完成全量验证和 17 项发行检查，独立记录见 [Linux 发行复验](../research/native-release-linux-regression-validation.json)。release 10,361,416 字节、测试 debug 170,136,208 字节；release 已复制到本地忽略目录并校验哈希，未发布。早期 Linux 报告不被覆盖，两次结果均不能替代其他平台或远程下载验收。

macOS x86_64 后续取得 [Rosetta 翻译执行](59-macos-x86-rosetta-validation.md) 的全量测试和单目标打包/运行证据，release 10,258,384 字节，原生头/字节/manifest 与 SHA256SUMS 一致。候选保留在本地忽略目录，未上传；不等于 Intel 原生硬件、远程下载、签名或四平台全部通过。

纳入 JSON 下载数值修正后的 macOS ARM64 release 通过十八项本机发行检查，包括候选包内的许可材料与校验和。二进制为 9,257,296 字节，源码和校验输入见 [当前 macOS ARM64 记录](../research/native-release-macos-current.json)。

同版 Linux ARM64 release 也通过十八项本机发行检查。两个 ARM64 产物已用 `--allow-partial` 组合打包，清单和校验和覆盖两份二进制及发行声明材料。完整发行仍需同版 x86_64 产物、远程构建和下载验收。详见 [当前 Linux ARM64 记录](../research/native-release-linux-arm64-current.json)。

四目标本地候选现已齐备。GitHub Releases 为每份资产提供按文件名访问的 URL，因此发行目录直接生成四份二进制、`colab-update.json`、`colab-licenses.zip` 和 `SHA256SUMS`。ZIP 内含原始声明和四份许可材料，保留路径与相对链接。下载七份资产到同一目录后，在 Linux 用 `sha256sum -c SHA256SUMS` 或在 macOS 用 `shasum -a 256 -c SHA256SUMS` 校验。更新客户端仍使用清单中的二进制 URL。远程工作流与实际 Release 下载待验收。详见 [GitHub 资产接口](https://docs.github.com/en/rest/releases/assets#upload-a-release-asset) 与 [本地候选记录](../research/current-four-target-release-candidate.json)。

macOS ARM64 已用真实候选二进制演练自更新。旧客户端来自仅将包版本改为 0.0.0 的临时源码副本。演练服务把当前候选清单中所选资产的 URL 指向 loopback。相同长度但校验和错误的下载被拒绝，旧文件保持原样。正确下载安装后与候选逐字节一致，同版检查只请求清单。停止服务后，已安装客户端离线回滚，恢复旧版字节及文件权限。远程 HTTPS 下载仍待验证。详见 [演练工具](../tools/rehearse_native_update.py) 和 [结果](../research/native-update-rehearsal.json)。

集中打包现在从同一次工作流分别下载四个二进制和四份原生验收报告。`--validation-dir` 按目标查找 `validation-<target>/native-validation.json`，核对平台、Cargo 版本、执行所得版本、下载目录、必要运行检查，以及实际打包字节的长度与 SHA-256。报告缺失、损坏或不匹配时，暂存目录被清理，发行目录不会生成。以本地四平台候选和已有验收记录重打包，七份资产的 SHA-256 全部保持一致。将一份二进制改变一个字节后，打包拒绝该产物，未生成部分目录。[候选工作流](../.github/workflows/release-build.yml) · [打包测试](../tools/tests/test_package_release.py)

在本地执行工作流的渠道配置步骤后，用输出地址重打包现有四目标候选，七份资产的 SHA-256 与此前一致。输入旧 tag 时配置步骤退出，未生成供构建读取的输出。独立运行 `validate_native_release.py` 时，默认也从 Cargo 的 `repository` 和 `version` 推导下载目录。测试覆盖版本升级、预发布与构建元数据、仓库名称变化和自定义 HTTPS 地址。GitHub Actions 远程执行仍待验收。

## GitHub Release 发布后演练

准备同平台的低版本 `colab` 和打包输出目录。`dist` 应保存发布前已核对的原始候选文件。用以下命令验收受信任发布者的公开 Release。

```sh
python3 -B tools/rehearse_native_update.py --package dist --older /path/to/older/colab \
  --published-manifest https://github.com/deluxebear/colabcli-rust/releases/latest/download/colab-update.json
```

工具核对 `latest` 与固定版本的公开清单均和本地清单逐字节一致。然后用临时目录中的旧版客户端，从固定版本地址执行真实 `update --install`，验证安装字节、版本和同版本检查，再以无效发行源配置执行离线回滚，核对旧版字节和权限。公开清单不可用或不一致时，在执行客户端前退出。省略 `--published-manifest` 可继续做本机篡改拒绝、安装和回滚演练。

本机 macOS ARM64 候选再次通过 loopback 安装和回滚。实际 GitHub `latest` 与 `v0.1.0` 清单均返回 HTTP 404，线上验收在运行旧版客户端前停止。线上成功需要同一 Release 的公开清单与本平台资产可下载。详见 [验收工具测试](../tools/tests/test_rehearse_native_update.py)。

首轮 GitHub Actions 的 Linux 更新测试在启动原生版本自检时偶发失败。更新器现在在同步下载内容后立即关闭写句柄，Linux 遇到 `ETXTBSY` 时在同一 5 秒自检期限内重试启动。其他启动错误即时返回。回归用例持有可执行文件写句柄，确认 Linux 返回 `ETXTBSY`，关闭句柄后安装继续完成。[更新测试](../tests/update.rs)
