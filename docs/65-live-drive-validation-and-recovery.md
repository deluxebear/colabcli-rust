# Drive 真实验收与恢复修正

2026-11-18（Asia/Shanghai）。用户明确允许在 MyDrive 创建一个随机目录及两份测试文件，并仅清理本次目录。本轮使用现有系统钥匙串和已有 Drive 资料，顺序创建专属 CPU；没有重新登录、递归删除或访问其他用户文件。真实失败、显式恢复与最终清理分别记录，不能当作原执行器一次无中断通过。

## 真实发现与实现变化

首次挂载在启动结果持久化阶段失败：远端已有本次 DriveFS，但缺少 mount 快照；本地 session 的 Drive 字段缺失，而独立 managed lease 仍在。字段丢失原因尚未定位。[首次报告](../research/live-drive-first-attempt.json) 保留原始失败及后续卸载、凭据回收和后端释放证据。控制器只允许从相同 operation、完整启动身份及精确 `fuse.drive` 挂载恢复缺失快照；status 仅内存核查，显式 umount 才修改状态。本地没有计划时，显式 umount 可以从精确匹配 compute account、endpoint、session、mountpoint 的唯一独立租约进入恢复，仍须远端身份核对和凭据采用/确认。

[第二次报告](../research/live-drive-second-attempt.json) 记录创建后的目录枚举短暂缺项；文件写入成功，但严格探针拒绝将空枚举记为成功，随后按标记和内容完成受限清理并释放 VM。探针现在仅在新建阶段、条目为两文件白名单子集时，在同一 fd 上 rewind 并只读观察至多五秒；额外条目立即拒绝，不重建文件。后续沿用已经清理的同一随机目录名，没有扩大云盘目录范围。

后续真实切换读写/只读挂载时发现 Drive launcher 派生子进程，旧记录成为 zombie，遗留 IPC listener 阻止新实例启动。进程捕获现在短暂观察同一个已启动 child，不重复启动；zombie 视为退出，恢复只发现唯一精确启动身份。挂载参数加入运行环境官方实现使用的 `--single_process`，禁止额外参数覆盖；本地和远端同时保留进程模式、退出上传动作及其超时参数，额外 mount flags 不能触发这些受管生命周期操作。旧实例恢复只在 operation、完整命令、启动时间、可执行文件及监听 socket 所有权均核对后，以 pidfd 发送 SIGTERM；没有强杀或 killall，仅清理同 inode 的残留 socket。

真实清理进一步证明：FUSE unlink/rmdir 成功不等于云端已同步。旧流程卸载后马上停止 Drive，API 仍能读取原目录和两文件。控制器现在先调用官方 Drive 的 `--push_changes_and_quit --single_process --timeout_sec=30`，成功后才卸载和回收进程/凭据。调用前要求唯一的 per-uid IPC listener 确由本次精确进程持有；不能确认归属、上传超时或退出非零均保留恢复状态，不继续停止进程或回收凭据。若 flush 已卸载，继续核对无挂载；若仍有挂载，必须与原快照完全相同才尝试卸载。依据是本次 VM 实际安装的 `google.colab.drive.flush_and_unmount`，不将这次运行环境的私有协议概括为所有版本。

显式会话的 exec、文件、重启和 Drive 生命周期命令，在已有有效 runtime proxy token 时延迟读取控制面凭据；需要刷新 proxy 时仍必须取得真实 OAuth/ADC，失败不降级。这样原生恢复不会因一次不必要的钥匙串提示而中断。new、stop、控制面查询仍需认证；新构建可能重新触发 macOS 钥匙串访问提示，30 秒默认等待不变。

## 验收范围与证据

本轮已实际完成 shim metadata 挂载、二进制写入/读取、Drive API 独立下载逐字节比对、只读挂载写入被拒绝且原字节不变、读写恢复清理、flush 后卸载及 managed lease 回收。最后服务端核对三个已确认对象 ID，均为同一所有者且 `trashed=true`，分别计为三个回收站对象、零个 404；不表示永久删除。最终又按同一身份复查三个 ID，并按精确随机目录名查询 MyDrive，存活同名目录数为零。

[真实分段报告](../research/live-drive-validation.json) 保留原执行器失败及只读、清理、释放的显式后续结果；三台本次 CPU 均独立后端确认释放，其他新增会话为零。最后释放在管道环境的钥匙串读取超时后，使用已核对构建哈希的终端环境重试成功，随后独立 inventory 确认 endpoint 消失；钥匙串状态为已解锁，具体超时原因仍未定位，不能断言用户未批准提示。云盘清理、租约回收与后端释放分别核对。

[最终本地回归](../research/live-drive-regression-validation.json) 默认／无默认特性 Rust 476／474 项通过，均无失败或 ignored；Python 163 项中 162 通过、1 项 Linux 专属在 macOS 跳过，两种特性 Clippy 和 fmt 通过。最后追加的受管 flags 限制已纳入这次重新冻结的完整回归；先前冻结输入后发生变化的检查不计最终源码验证。

[最终 CLI 对照](../research/live-drive-cli-comparison.json) 365 场景无测试失败：92 一致、54 保留差异、219 Rust 扩展；扩展和差异不冒充原版一致。[本机发行检查](../research/live-drive-native-release-validation.json) 17 项通过，macOS ARM64 release 为 9,173,792 字节。源码、最终 debug/release、对照与打包哈希逐项核对，最终二进制内 README 也核对为当前字节；没有发布。真实验证中不同阶段构建的哈希各自保留，不把显式恢复拼接为最终构建一次无中断通过。

仍未验证跨真实账户挂载、超过初始 access token 有效期的持续刷新、匹配 client 的 rt 模式、新 OAuth consent、撤销授权与所有平台。permission ID 没有与 OAuth subject 作独立映射。本轮只验证现有资料的 userinfo、同 token 的 Drive 当前用户/root 所有权及本次文件，不能据此关闭整个 V07 或完整设计目标。历史 Linux/Rosetta 报告属于先前构建，本轮产品修正不能继承其通过结论。

后续 [检查点合并修正](66-recovery-checkpoint-merge.md) 稳定复现并修复一种旧快照清除独立恢复字段的路径；没有证据将首次真实故障唯一归因于该时序，因此保留本章的历史根因未定位结论。
