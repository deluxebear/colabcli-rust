# Drive 受控真实验收执行器

2026-11-18（Asia/Shanghai）。在 [离线准备](62-drive-live-validation-preparation.md) 基础上实现可选执行器和运行时 endpoint 条件保护。本阶段完成本地回归与现有资料的离线预检查，没有调用真实钥匙串、刷新凭据、创建 VM 或访问云盘。V07 尚未真实验收。

## 输入、状态与失败责任

[validate_live_drive.py](../tools/validate_live_drive.py) 默认只检查私有计划、当前二进制、三个探针模板及所选本地 Drive 身份摘要；不创建 CLI 客户端。执行必须显式提供 `--execute` 和与计划完全相同的 `--allow-cloud-test-directory`。先审阅 `plan.json`、`commands.json` 和三个 payload；清单本身不能直接执行。准备快照的 `executed/execution_authorized` 保持 false；真实授权与进度另存于私有 `execution.json`，不修改冻结输入。

```sh
python3 tools/validate_live_drive.py --state-dir /absolute/private/prepared
# 仅在用户确认随机目录写入范围后执行：
python3 tools/validate_live_drive.py --state-dir /absolute/private/prepared \
  --execute --allow-cloud-test-directory colabcli-validation-EXACT_NONCE
# 已有账本的恢复入口；不重跑完整验证：
python3 tools/validate_live_drive.py --state-dir /absolute/private/prepared --cleanup-only
```

执行前拒绝账户/控制面/Drive 待恢复事务、现有租约及同名会话。真实 token 输出仅留内存，核对刷新前后的 user-id、scope 和本地身份摘要；这不是独立 Drive API 身份证明。捕获一个不在基线中的专属 CPU endpoint 后立即落账本，才允许挂载。每次子命令清除子进程环境中的 COLAB_*，显式传递账户和认证来源，不更改父进程身份。

运行顺序为读写挂载、创建并读取随机目录内两份已知文件、卸载回收凭据、只读挂载并验证写入拒绝和内容不变、再次卸载、读写挂载并校验标记/内容后非递归清理、最终卸载回收、条件 stop、独立查询后端确认释放。只读和清理探针不会接管已有或内容异常的目录；具体 FUSE/路径约束沿用 [62](62-drive-live-validation-preparation.md)。

CLI 新增隐藏全局条件 `--colab-runtime-endpoint`，要求显式会话，只适用于 exec、restart-kernel、files 和 Drive mount/umount/status。在读取认证之前进行本地核对，并在账户锁内、刷新 proxy token 前及刷新后再次核对；同名会话被替换后不能误连接新 endpoint。释放仍用既有 `--colab-expected-endpoint`。这是验收工具的条件保护，不改变普通命令默认路由。

每次修改请求发送前持久化 inflight；未知执行不重放，必须经显式 kernel 重启确认后才允许受限清理。未知挂载/卸载先由原 CLI 恢复，执行器只查询核实，不重发未知修改。凭据回收或卸载未确认时保留 VM；释放失败只允许重试条件 stop，不重跑云盘步骤。已知终止的脚本失败可进入受限文件清理。探针发送前重新核对精确字节，通过 stdin 提交，避免 CLI 二次读取已变化文件。

账本、凭据身份摘要和实际账户/endpoint 仅保存在私有目录；公开报告不包含原始 stdout、token 或账户数据。文件哈希绑定、私有权限、普通文件检查和进程锁保护输入及并发。目录丢失、被移动、额外内容、未知修改或凭据回收失败均不能被归类为安全清理成功。

## 本地验证与限制

[执行器回归](../research/live-drive-executor-validation.json) 的 20 项合成测试覆盖正常流程、身份变化、未知结果、显式恢复、endpoint 替换、租约残留、分配回复丢失、释放重试、输入变动与并发。完整 Python 工具 128 项中 127 项通过，1 项 Linux 专属在 macOS 跳过。所有凭据与 CLI 均为临时合成替身。

[运行时条件回归](../research/runtime-endpoint-regression-validation.json) 的默认/无默认特性 471/469 项、两种特性 Clippy 和格式检查通过。[CLI 对照](../research/cli-comparison.json) 365 场景无失败：92 一致、54 保留差异、219 Rust 扩展。本机 [17 项发行检查](../research/native-release-validation.json) 与 [两组图片对照](../research/release-image-comparison.json) 通过；release 9,157,216 字节，其他平台证据仍按历史阶段区分，未发布。

[本地预检查](../research/live-drive-executor-preflight.json) 核对当前构建、探针和既有资料，注册表/凭据字节不变，网络/创建/云盘写入均为零。本次没有重试先前超时的系统钥匙串。用户已选择该认证来源和授权 CPU 测试；新增云盘写入范围仍需确认：仅新随机 MyDrive 目录及两份测试文件，验证后仅清理该目录。

独立 Drive API 实际存储身份与服务端持久化、真实挂载/只读、长期刷新、跨账户、故障恢复尚未完成。FUSE 的路径不存在不等于独立云端确认删除；标记缺失或被移动需要人工核查。二进制或模板变化后必须重新准备，不能绕过冻结哈希。不能用本阶段合成流程关闭 V07 或整体目标。

## 后续：只读 Drive API 独立读取

[drive_api_validation.py](../tools/drive_api_validation.py) 已接入显式执行路径：在分配 CPU 前用同一个内存 token 查询 `about.user` 和 MyDrive root，严格确认当前 Drive 用户及根目录所有权；创建探针后的第一次卸载完成后，通过 Drive v3 单独列举随机目录、核对父目录/所有权、文件大小和 MD5，再下载两份文件逐字节比较。只使用 GET，没有 API 写入、删除或递归操作；服务端尚未同步会导致验证失败并进入既有受限清理，不会误记通过。

生产地址固定为 HTTPS Google Drive v3，测试只允许显式数字 loopback 地址。拒绝重定向、分页未完整、重复同名目录、额外条目、错误所有者或父目录、Google 文档/shortcut、超限响应与异常字节；错误只输出固定标签，不打印服务端正文或授权头。依据为官方 [当前用户信息](https://developers.google.com/workspace/drive/api/guides/user-info)、[文件列表](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/list) 和 [文件字段](https://developers.google.com/workspace/drive/api/reference/rest/v3/files)。Drive permission ID 与 OAuth subject 不作等同推断。

[API 回归报告](../research/live-drive-api-validation.json) 保存十项 loopback HTTP 测试及两个新增执行器失败流程：API 身份检查失败时不分配，独立字节核对失败时只清理本次内容并释放。执行器当前共 22 项；完整工具 140 项中 139 项通过、1 项平台跳过。上面的 20/128 数字和原报告保留为接入前阶段证据。Rust 产品与二进制未改，沿用已验证的 471/469 和发行报告。

真实执行报告将单独记录 `same_token_drive_ownership_verified` 与 `server_bytes_verified`；`independent_drive_api_identity_verified` 仍为 false，明确未建立 OAuth subject 到 Drive permission ID 的独立映射。当前只有合成证据，实际云端所有权、服务端字节、清理持久化、跨账户与长期刷新均未验收。没有真实 HTTP/钥匙串/VM 或云盘写入，待前述范围确认后才能执行。

## 后续：服务端清理状态核对

观察器在成功验证目录所有权和两文件精确字节后，仅在内存保存三个对象 ID；失败的下一次 inspect 会清除该快照。最终卸载完成后重新核对同一 Drive 用户及 MyDrive root，再逐一 GET 已确认归属的三个 ID。只有对象返回 404，或保持同一所有者且 `trashed=true`，才记 `server_cleanup_verified`；分别记录 missing/trashed 数量，不宣称永久删除。目录同名查询变空、文件被移动、所有权变化、403、认证失效或同步尚未完成都不能替代这些条件。

执行器仅在该核对成功后才记完整验证步骤通过。API 清理核对失败会报告验证失败；如果本地卸载与凭据回收已经确认，仍释放本次 VM，保留报告以便检查云盘残留，不重放清理脚本。`cloud_cleanup_confirmed` 仍表示受限 FUSE 探针完成，不能与独立 `server_cleanup_verified` 混同。cleanup-only 不重新获取/重放整体验收，也不把缺失的服务端证据补记为真。崩溃后的内存快照无法恢复，须独立人工核查。

[服务端清理回归](../research/live-drive-api-cleanup-validation.json) 覆盖已回收与不存在的分开计数、仍存活/被移动对象、身份变化、缺少或失效快照、API 错误、重复 ID 和损坏字段；并增加执行器的“VM 已释放但服务端清理未确认”失败流程。当前 API 共 17 项、执行器 23 项；全工具 148 项中 147 项通过、1 项平台跳过。之前 10/22/140 的报告保留为历史阶段。没有真实 Google/钥匙串调用、VM 创建或云盘写入，真实验收继续等待范围确认。

## 后续：目录枚举位置与当前离线计划

x86_64 容器在专属构建卷发现已持有目录 fd 首次枚举返回空列表；本地 tmpfs 未复现。探针改为在严格条目核对前，于同一已验证 fd 上显式 rewind，不按路径重新打开，也不接受额外内容。构建卷连续 20 次创建/清理复现通过，新增保留 fd/额外文件拒绝回归；[当前工具回归](../research/eintr-tools-validation.json) 153 项中 152 通过、1 项平台跳过。当前私有计划已用新模板和新构建重新生成，注册表/凭据字节不变。历史 payload 哈希仅属于历史计划，不能直接复用；真实执行仍等待原随机目录范围确认。
