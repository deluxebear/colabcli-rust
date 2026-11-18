# DriveFS 挂载控制器与私密输入适配

后续状态：核心 mount/umount 编排、跨账户租约与恢复入口已接入，见 [24](24-drivefs-lifecycle-implementation.md)。下文“待接入”描述本阶段当时的边界；真实挂载仍未验收。

更新日期：2026-11-17。本阶段实现远端控制器和 Rust 执行适配 API，`drive mount/umount` CLI 尚未接入，也未在真实 Colab VM 运行。后续已接入只读 status/probe-cleanup，见 [23](23-drivefs-probe-and-recovery.md)。完整目标仍包括挂载 CLI 恢复计划与真实挂载验收，不能据离线通过宣布 P5 完成。前置凭据与 metadata 组件见 [20](20-drive-credentials-implementation.md)、[21](21-drivefs-metadata-implementation.md)。

## 输入和执行通道

[remote.rs](../src/drivefs/remote.rs) 提供 Action、AuthMode、RemoteRequest 和 prepare。请求包含 32 位随机十六进制 operation ID、绝对挂载路径、模式、readonly/force、默认 30 秒启动等待和 purge_state，不含凭据。路径拒绝控制字符、`.`/`..`、重复/末尾斜线以及 DriveFS preferences 无法安全表达的逗号/冒号；允许空格、中文、引号和 shell 元字符，它们始终作为数据或 argv 元素。挂载点不能覆盖控制目录或其祖先。

Mount 预检完整 Drive scope/Gaia ID，rt 另检 DriveFS client 配套来源。凭据转换成 Secret 对象，绑定预先选定的 endpoint，只响应指定 operation 的精确密码提示一次。错误 endpoint、未绑定、普通输入提示、错误 prompt 或重复发送均拒绝；错误请求清除待发送值。普通 runtime 的 parent_header 路由仍生效，无关执行的 stdin 请求不会消耗凭据。

[包装脚本](../src/drivefs/wrapper.py) 用 Jupyter getpass/input_reply 接收 JSON；凭据不进入 Python 源码、shell、包装器 argv 或本地执行历史。静态组件与请求使用 JSON 数据编码，模块临时导入完成后恢复原有 metadata_shim 绑定。包装器捕获异常并返回固定错误，不输出解析失败时可能含秘密的 JSON 片段或 traceback。rt 模式按 DriveFS 原接口把 refresh token 传入远端 DriveFS argv，因此不能承诺同 VM 的进程看不到它。

Document 使用独立 automation kernel，历史 code 只记 operation ID。automation_pending 增加 drivefs 类型，沿用发送前持久意图、已执行结果未知禁止重放、普通 kernel/session 不被覆盖的规则。本阶段最初尚未增加专用只读 probe journal；后续已通过独立只读通道接入 status CLI，不清空 mutation pending 或借普通 exec 绕过保护，见 [23](23-drivefs-probe-and-recovery.md)。

## 远端状态和所有权

[控制器](../src/drivefs/controller.py) 默认使用 `/content/.colab-cli-drivefs`，按挂载路径摘要索引独立操作目录。根、索引、操作及私有 HOME 为 0700；状态、凭据、脚本和日志为 0600。受管目录不允许符号链接，配置/状态/锁要求同 UID、普通文件及单硬链接；拒绝把控制目录放在已有 FUSE 内。每个挂载有非阻塞 flock，冲突明确返回 mount_busy；子进程关闭继承的文件描述符。

阶段依次为 prepared、shim_starting、shim_ready、drive_starting、mounting、mounted；卸载使用 unmounting、finished，启动异常保留 unknown 和失败前阶段。启动前保存期望可执行路径与 argv 摘要，获得 PID 后保存 PID、Linux starttime、可执行路径和 cmdline 摘要。环境带独立 operation 标记，不写凭据。代码不打印 argv、环境、DriveFS 日志或任意异常内容。

`/proc/self/mountinfo` 提供挂载 ID、路径、文件系统和 source，支持内核 octal 路径转义。成功要求实际 FUSE 挂载，不能仅以 My Drive 目录存在判断。再次 mount 仅在挂载记录、身份摘要、readonly/mode、Drive 进程及必要的 helper 全部匹配时复用；其他情形拒绝或要求显式 force。force 先走同一所有权卸载流程，不能接管外来挂载。

status 不创建目录、文件或锁，不发信号，也不读取凭据或日志。它检查实际挂载表与进程；对启动后尚未来得及保存 PID 的窗口，可只读扫描 `/proc`，要求 operation 标记、可执行路径和完整 argv 摘要同时匹配且唯一。只读发现不写回状态；显式卸载可使用核对结果。未找到进程不能证明它没有启动，结果不确定时保留状态并返回 process_result_unknown，而不擅自删除凭据或重新分配。挂载发生但挂载 ID 未持久化的窗口仍可能需要人工核查；控制器不会把该挂载直接认领为自己的。

## 卸载与清理责任

卸载前验证挂载 ID/路径/文件系统/source 与进程身份，先尝试普通 fusermount/fusermount3/umount，不使用 lazy、force 或 shell。每次工具调用前重新核对挂载，发现替换挂载即终止，避免下一种工具误卸载别人的资源。工具 stdout/stderr 不输出到结果。真实 flush 与 DriveFS 各版本行为尚未验证。

卸载失败或挂载表不可读时，不终止所属进程、不移除配置、不删除挂载点。确认不再挂载后才终止精确所属进程：使用 Linux pidfd 固定目标，在获得句柄后再次核对身份，TERM 后等待，必要时 KILL 并确认退出。环境缺少安全句柄支持时在启动前拒绝。没有全局 pkill、进程组终止或仅按数字 PID 发信号。

进程确认退出且挂载未重现后，shim 配置现先冻结为 credentials_pending，收到本地持久采用后的准确文件摘要才进入 finalizing，清除该操作的长期凭据文件、脚本、就绪文件及两份日志；后续协议见 [23](23-drivefs-probe-and-recovery.md)。默认保留私有 HOME 与 token-free 状态以便诊断；DriveFS 自己的缓存可能含敏感数据，不能据 shim 文件删除宣称所有副本已销毁。显式 purge_state 只递归清理该操作的私有 HOME，发现内部挂载则拒绝，不删除全局 DriveFS 缓存或其他操作目录。

挂载点仅在本操作创建、底层 inode/dev 未变化且已卸载时尝试 rmdir；非空则保留，绝不递归删除挂载点。已完成状态不再依据旧 PID 探测/终止后来复用该 PID 的进程。未知结果、清理失败和外来挂载保留错误码及状态供核查，不伪报清理完成。后续已提供本地凭据采用 API，挂载/卸载 CLI 的跨端自动编排仍待完成。

## 验证

[控制器与包装器测试](../tools/tests/test_drivefs_controller.py) 直接执行同一份远端文件。27 个测试在本机运行，其中 26 通过，1 个 Linux `/proc`/pidfd 真实子进程测试因本机 macOS 跳过；已经加入 Linux/macOS CI，但 CI 配置不等于 Linux 实测结果。替身测试覆盖参数/路径、阶段持久化、私有权限、并发锁、多挂载、外来挂载/PID 替换、未记录 PID 的只读恢复、启动/探针/期限/终止失败、未知禁止重放、卸载失败保留、inode 替换、嵌套挂载拒绝 purge、模块恢复与错误脱敏。额外故障用例确认 Popen 成功后的 I/O 失败不能被误判成“子进程未启动”。真实 DriveFS/FUSE 均由替身模拟。

Rust 新增 4 项适配测试及 1 项 WebSocket 集成测试。后者分别执行成功与发出私密回复后断线两种分支，检查 input_reply 的 parent、秘密不进入源码/历史/会话状态、普通 kernel/session 不变，以及 unknown 后再次调用不发送 mutation。未访问真实凭据、Google、Drive 文件或系统 FUSE。

完整默认 233 项、无默认特性 231 项 Rust 测试通过；两种特性的 Clippy、fmt、默认构建和研究文档检查通过。包含前置 metadata 组件，DriveFS 脚本总计 37 项通过、1 项平台跳过。193 个既有官方差分与 Rust 扩展场景重新执行，断言失败为 0；报告二进制哈希与当前 debug 文件一致，未新增 CLI 命令或关闭真实挂载验收缺口。

复现：

```sh
cargo test --locked drivefs
python3 -B tools/tests/test_drivefs_shim.py
python3 -B tools/tests/test_drivefs_controller.py
```

## 后续：真实挂载与上传后卸载

本章以上保留早期离线阶段结果。后续真实故障及最终行为见 [65](65-live-drive-validation-and-recovery.md)：启动使用 `--single_process`，在同一 child 上观察启动身份，zombie 不误报归属变化；仅精确 `fuse.drive`、同 operation 和完整启动身份可恢复特定缺失快照，status 不写回。卸载现先核对所属唯一 IPC listener，执行 Drive 的 push-changes-and-quit；上传成功后才走必要的普通卸载、进程和凭据回收。上传失败保留挂载与恢复状态。真实 API 已核对本次对象进入回收站，其他版本、跨账户和长期刷新仍缺证据。
