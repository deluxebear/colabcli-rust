# DriveFS 挂载编排与跨账户租约

更新日期：2026-11-17。本阶段把 [22](22-drivefs-controller-implementation.md) 的远端控制器与 [23](23-drivefs-probe-and-recovery.md) 的只读核查、凭据回收接入 CLI。验证使用临时目录、合成凭据和 loopback REST/WebSocket；尚未进行真实 Drive 授权、FUSE 挂载或读写，不能据此宣布完整 P5 验收完成。

## 命令与输入

```sh
colab drive mount -s cpu-demo --path /content/drive --dry-run
colab drive mount -s cpu-demo --drive-account storage --readonly --timeout 30
colab drive status -s cpu-demo --json
colab drive umount -s cpu-demo --purge-state --json
colab drive leases --drive-account storage
colab drive lease-cleanup --endpoint VERIFIED_ABSENT_ENDPOINT
```

支持 dfs 别名。mount/umount 仅使用已有会话，不分配 VM。未指定会话时要求计算账户只有一个可选会话。Drive 账户按显式参数和既有绑定解析；挂载前要求完整 Drive scope、有效 Gaia 身份与 refresh token。默认 shim，显式 --mode rt 仍遵守原 RT client 限制；默认路径 /content/drive、metadata 端口 8012、远端超时 30 秒，--port 0 允许动态端口。--dry-run 展示非秘密参数，不发送控制面或运行时请求；读取凭据时可能完成之前的本地刷新暂存恢复。

健康且身份、模式、只读属性相同的受管挂载可以复用。--force 先完整卸载旧挂载、回收并持久采用轮换凭据、确认远端清理，然后才提交新挂载。不会用 force 接管没有本地所有权记录的远端挂载。--purge-state 只请求清理本操作私有 HOME，沿用控制器的进程和 inode 核查规则。

## 持久状态与失败恢复

[lifecycle.rs](../src/drivefs/lifecycle.rs) 在会话 drive_plan 中保存无秘密的输入、原挂载租约、prepared/dispatching/observed 阶段和可选回收摘要。完成的挂载保存在 drive_mounts，Drive 账户 managed 目录保存对应 [Lease](../src/drivefs/lease.rs)。记录包含计算/Drive 账户、会话、endpoint、操作 ID、配置指纹和身份摘要，不含 token 或 client secret。坏记录与不一致归属均拒绝继续。

每个计算账户/endpoint 使用独立编排文件锁；各步另取原有运行时执行锁及生命周期锁，保存时核对 endpoint，不覆盖正常 kernel、用户执行或并发 proxy 刷新。原自动化 journal 额外记录 DriveFS 操作 ID，只有精确归属的 kernel 才允许恢复清理。

发出 mutation 前持久化计划。连接断开或结果未知后保留计划，不盲目重发挂载或 stdin 凭据。重试先通过独立只读 probe 核查；确认远端已经完成时，清理本计划已知的未知执行 kernel，再次核查后提交本地状态。只有已证明未发送的准备阶段允许恢复发送。创建结果未知且无 kernel ID 时，仍需先核实 ID，走原显式自动化清理流程，不能猜测或删除其他 kernel。

卸载先核对 owner、身份、模式和只读属性。shim 停止后保留 credentials_pending，客户端通过私有 Contents 下载配置，拒绝采用超过 64 KiB 的内容，以原配置指纹进行 CAS 采用。成功持久保存后的原始文件 SHA-256 才能作为 finalize receipt。采用失败保留配置、计划和租约；finalizing 已删除配置但没有本地 receipt 时拒绝猜测。已完成的 finalize 丢失回复时只确认完成，不重复下载或卸载。仅确认 finished 且已卸载后释放租约。

## 账户保护与 VM 清理

活动租约阻止 Drive token/import/clear/consent 更新，避免挂载期间本地刷新与远端轮换互相覆盖；load 和受指纹保护的回收采用仍可用。同一 Drive 账户可以有多个挂载，清理一个不会解锁其余挂载。

计算账户有待恢复计划、已完成挂载或其他 Drive 账户对它的租约引用时，重命名和强制删除均拒绝。即使后端同步已裁掉本地会话，跨账户引用仍有效。新增租约和账户变更通过全局账户锁协调。

stop 在服务端确认释放或不存在后，清理该计算账户及 endpoint 的租约；失败时保留释放 journal，重试先查 VM。清理分别获取各 Drive 账户锁，不在计算账户生命周期锁内嵌套跨账户锁。lease-cleanup 用于本地会话已不存在的情况，必须先查询控制面确认 VM 不存在，仅清理匹配账户/endpoint。VM 销毁后不能回收其未保存的远端轮换凭据，因此需要保留凭据时应先 umount。

## 输出与验证边界

CLI 默认和 --json 均返回严格验证后的结构化结果。DriveFS 原始 stream、未知 MIME 字段及 traceback 不落历史；历史只记录非秘密意图与经过验证的结果。凭据仅通过私密 stdin 输入与私有 Contents 回收，不嵌入执行源码。

集成测试覆盖完整挂载/卸载/采用/finalize、健康复用与 force 替换、三种 mutation 丢失回复、普通 kernel 隔离和历史脱敏。租约测试覆盖重复获取、碰撞拒绝、多挂载精确释放、凭据更新保护、被裁掉会话的跨账户保护，以及释放失败和按账户/endpoint 清理。远端脚本验证显式端口、身份摘要和卸载 owner 防护。

当前核心命令与恢复协议已接入；后续凭据来源与额外参数见 [25](25-drivefs-inputs-implementation.md)。官方 drivemount 的扩展回退策略、真实 DriveFS/Colab 兼容性、RT 内部轮换、多 helper 同时轮换与跨平台发行仍需后续验证或实现。离线测试不代表原生凭据库互通、Google 在线行为或真实 FUSE 已通过。

最终离线回归：默认特性 252 项、无默认特性 250 项 Rust 测试通过；两种特性 Clippy、fmt 和默认构建通过。远端脚本 40 项通过、1 项 Linux 专属跳过。完整 CLI 差分 214 个场景为 88 一致、39 保留差异、87 Rust 扩展，无断言失败；root/account/drive 清单一致，报告二进制哈希与当前 debug 文件相符。

## 后续：缺失本地计划的精确租约恢复

[65](65-live-drive-validation-and-recovery.md) 记录真实挂载后 session 字段缺失但 managed lease 仍在的故障；字段丢失原因尚未定位。仅显式 umount 在没有本地计划/挂载记录时查找已注册账户的独立租约，要求 compute account、endpoint、session 和 mountpoint 全部匹配且唯一。随后仍核对远端 owner、身份、mode 和 readonly，再通过原生计划、凭据采用及 finalize 完成恢复；不会手改事务日志或将该兜底用于自动重放 mount。真实卸载还增加上传变更后退出，见 [控制器后续](22-drivefs-controller-implementation.md#后续真实挂载与上传后卸载)。
