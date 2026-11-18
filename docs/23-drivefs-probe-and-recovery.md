# DriveFS 只读 CLI 与凭据回收协议

后续状态：核心 mount/umount 编排、跨账户租约与恢复入口已接入，见 [24](24-drivefs-lifecycle-implementation.md)。下文“待接入”描述本阶段当时的边界；真实挂载仍未验收。

更新日期：2026-11-17。新增 `drive status`、`drive probe-cleanup`（同样支持 dfs 别名），以及远端卸载后的凭据回收 API。`drive mount/umount` CLI 与其本地计划/账户租约仍未接入；未进行真实 Colab/Drive 授权、挂载或读写。前置控制器见 [22](22-drivefs-controller-implementation.md)，本章更新其只读与凭据清理边界。

## 只读命令

```sh
colab drive status -s cpu-demo --path /content/drive --json
colab drive probe-cleanup -s cpu-demo
```

status 只使用已有 VM，不分配 VM、不挂载或卸载，也不加载 Drive 账户凭据。计算账户按显式账户、环境、会话归属和 active 路由；未指定会话时只接受该账户唯一会话。错误路径、缺失会话、多会话歧义在读取控制面 OAuth 前失败。只读查询仍需要计算账户认证与有效 runtime proxy；不等于离线状态命令。

默认路径为 `/content/drive`。默认和 --json 均输出结构化 JSON，远端可确认错误返回脱敏错误码并退出 1；不能取得可信结果也退出非零，不输出任意远端 traceback、stream、未知字段或日志。结果描述实际挂载、受管阶段、已核查进程及所有权；本地 sessions.json 中的正常 kernel/session/running 或未知 mutation 不被覆盖。

## Probe kernel 与恢复记录

[probe.rs](../src/drivefs/probe.rs) 使用独立 kernel 和会话内的 drive_probe 记录。创建 POST 前持久化 creating，得到 ID 后 ready，发送只读代码前 executing；断线/期限结果未知保留 unknown。该源代码只调用控制器 status，不请求 stdin，也不修改远端挂载状态。单次查询执行期限为 30 秒，HTTP 使用全局 request-timeout。

已知 probe kernel 的上次查询未完成时，下一次 status 先持久化 deleting，DELETE 该 kernel，确认成功/404 后才创建新的 probe。查询成功或已知失败也清理本次 kernel。删除失败保留目标和阶段供重试。原自动化 mutation 的 automation_pending、用户 running/execution_pending 和正常 kernel 不被清理；这条通道允许在未知 mutation 旁进行只读核查，不借重跑 mutation 恢复结果。

创建结果未知且没有 ID 时，禁止自动再次创建。先通过 `kernels -s cpu-demo` 核查本次遗留的 kernel，再显式清理：

```sh
colab drive probe-cleanup -s cpu-demo --kernel-id VERIFIED_PROBE_KERNEL_ID
```

显式选择需由调用者核实归属，不能把“REST 能访问这个 ID”当作所有权证明。客户端拒绝普通 kernel、当前/最近自动化 kernel，不能用其他 ID 替换已知 probe 目标。没有待恢复 probe 时 cleanup 不获取 OAuth 或访问网络。一般 status/sessions 输出只展示 phase/kernel_id，不展开 journal 的 endpoint、session_id 或未知扩展字段。

会话执行租约串行化同 endpoint 的查询，生命周期锁只用于快照和 checkpoint。checkpoint 核对 endpoint 与 kernel generation，不能在 stop/替换/清理后复活旧状态；并发 proxy 刷新继续保留。存在 drive_probe 时，账户重命名和强制删除均拒绝，防止丢失创建结果未知的清理责任；记录损坏也不绕过保护。

## 结果协议边界

[result.rs](../src/drivefs/result.rs) 仅接受一个 `application/vnd.colab-cli.drivefs-result+json`，核对版本、operation ID、完整路径、阶段、身份和进程字段一致性。缺失、重复、格式错误或跨操作结果返回静态诊断。输出结构仅含允许字段；未知 error 字符串映射为 remote_operation_failed，不打印原始值。probe 历史只记验证后的结果，不存提交代码或原始输出。

运行中的受管 shim 可在 `drive status --json` 报告最近一次已记录的 `refresh_status`。`ok` 表示成功，`invalid_grant` 表示 OAuth 服务拒绝了授权，`refresh_failed` 表示其他失败。`invalid_grant` 可用于排查撤销或失效，具体原因仍需向授权来源核实。查询只读本次操作目录中权限为 `0600` 的状态文件，不触发刷新。文件只保存版本和状态枚举，写入失败不影响 token 响应。旧 shim、`rt` 模式、已退出的 helper 或缺少有效记录时省略该字段。正常完成卸载时清理状态文件。

## 冻结、采用与确认删除

原控制器在卸载后立即删除 shim 配置，可能丢失远端轮换的新 refresh token。现在 shim 模式按以下顺序处理：

1. 验证挂载/进程所有权，正常卸载，再通过 pidfd 确认所属 Drive 与 helper 已退出。
2. 保留最终 credentials.json，持久化 credentials_pending。helper 已停止，配置不再由它刷新；挂载点与私有 HOME 也保留。
3. 客户端通过受认证 Contents 下载这些私有字节，调用 [DriveCredentialService::adopt_remote](../src/drive_credentials.rs)，成功持久保存后取得该文件原始字节的 SHA-256 receipt。
4. 显式 finalize 请求携带 owner operation ID 和 receipt。远端核对当前文件摘要，先持久化 finalizing 与 receipt，再删除本操作的凭据、日志、脚本，按需 purge 私有 HOME，并仅用 rmdir 清理本操作创建且 inode 未变的空挂载点，最终标记 finished。

finalize 摘要不匹配、owner 错误、配置缺失或挂载重现时拒绝。force mount 不能跳过 credentials_pending，需先完成回收。清理中断后，finalizing 中保存的确认允许在配置已删时继续清理，不需要重放卸载或从旧本地 token 构造替代凭据。冻结/清理完成的状态不再依据历史 PID 对可能复用 PID 的进程发信号。

adopt_remote 在 Drive 账户生命周期锁下先恢复既有刷新暂存，再核对 client、secret、token URI、Gaia ID 和 email。请求含开始挂载时记录的凭据指纹；若本地已被其他更新改写且不是同一远端结果，拒绝覆盖并保留远端文件。采用使用现有原子刷新暂存协议；重复同一采用幂等，不调用 Google，不修改计算 OAuth，未知本地凭据字段保留。轮换后清除本地旧 access token/expiry，后续 token 命令重新刷新。

远端 metadata 配置新增 scope_confirmed。只有 token 响应明确返回 scope 时才确认其来源；明确空 scope 也按实际结果保存。没有响应确认的范围变化标记 configured_unverified，不虚构服务端授权证明。回收摘要只确认准确文件，不保证 DriveFS 自己的私有缓存不含其他敏感信息；rt 模式内部凭据轮换仍依赖 DriveFS，未实现私有格式读取。

以上冻结/采用/finalize 已提供库与远端 API，**尚未接入挂载/卸载 CLI 的自动编排**。Contents 下载、跨账户受管租约、挂载请求本地计划，以及未知 mutation 的状态调和仍是后续工作；当前 status 不读取或回收秘密。不能仅靠这些 API 宣称真实卸载已验收。

## 离线验证

新增 3 项结果协议单测、2 项 probe WebSocket 测试、2 项 CLI 测试、3 项远端凭据采用测试及 1 项账户生命周期保护测试。覆盖跨账户会话路由、无 Drive 凭据只读、未知 mutation 保留、仅清理 probe、未知创建禁止分配、删除失败/404 恢复、秘密和原始诊断不落历史、结构化结果隔离、并发采用、暂存恢复和强制删除拒绝。

远端脚本增加两步回收和中断恢复用例，并保留既有故障、安全卸载和多挂载隔离检查。29 项控制器/包装器测试在本机 28 通过、1 项 Linux 专属跳过；加上 11 项 metadata 测试，脚本共 39 通过、1 项跳过。不读真实账户、Google、用户 Drive 或系统 FUSE。

CLI 差分工具为两个新入口增加 help、只读结果、未知 mutation、cleanup/拒绝与 pending 展示场景；完整运行另外核对 account/drive 子命令清单。固定官方 CLI 没有 drive 组，这些仅计 Rust 扩展，不能称为与 colab-ext 的双端等价验证。

累计默认 244 项、无默认特性 242 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整差分 204 个场景无断言失败，root/account/drive 命令清单均一致，报告二进制哈希与当前 debug 文件相符。
