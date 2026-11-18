# 执行检查点与恢复记录的合并

2026-11-18（Asia/Shanghai）。本阶段修复账户状态的可复现丢更新路径，继续追查 [65](65-live-drive-validation-and-recovery.md) 中缺失本地恢复字段的问题。两者属于相同风险类别，但现有真实日志不足以证明首次 Drive 故障正是这一时序，历史根因仍保留未定位。

## 输入、状态变化与失败责任

`SessionService::with_execution_session` 取得 endpoint 执行锁后捕获会话快照，执行期间允许其他账户锁持有者更新状态。原实现每次 checkpoint 都逐项复制或删除执行/自动化/Drive 恢复字段：即使旧快照从未包含该字段，也会删除另一个写入者刚补上的记录。只检查 endpoint 和 kernel generation 不能识别同一代 kernel 的独立元数据更新。

新增定向回归先在旧实现稳定失败：执行快照捕获之后，由另一个 `with_session_checkpoints` 写入 `drive_plan`、`drive_mounts` 和 `drive_probe`，旧执行只更新 running，却将三个恢复字段清除。测试使用临时目录、有效的合成 proxy token 和本地状态交错，不读取钥匙串、不访问 Google、不创建 VM。字段内容仅是用于观察合并的合成 JSON，不冒充完整有效 DriveFS 租约；真实租约编排仍由既有生命周期测试覆盖。

修复后，执行检查点以该执行上次成功提交的字段快照为基线：

1. 本次值与基线相同，保持当前磁盘记录，包含其他写入者新增或删除的字段。
2. 本次有修改，只有当前磁盘值仍等于基线时才应用；同字段已变化则报错，不覆盖。
3. 保存成功后才推进该执行的基线，保存或冲突失败保留原基线。

明确的删除也属于修改，因此能够正常提交和重复检查点，同时避免旧快照清空未知记录。冲突使整个 sessions 更新不写入，running、kernel/session 等同次修改也不会部分落盘。endpoint、kernel generation、账户事务、控制面待恢复事务的原有校验继续生效，代理 token 和硬件字段仍由当前磁盘值保留。不删除租约、事务日志或自动重放远端操作；冲突后调用者应重新取得当前会话状态再决定后续操作。

## 验证范围

三项定向回归已通过：新增的独立恢复记录保留、重复检查点与同字段冲突/明确删除，以及已有并发 token 刷新保留和旧 kernel generation 拒绝。修复前失败和修复后成功的日志分别保留，本轮没有新的真实 Google 操作。

[最终完整回归](../research/checkpoint-merge-regression-validation.json) 默认／无默认特性 Rust 478／476 项通过，两种 Clippy、fmt 通过；Python 163 项中 162 通过，1 项 Linux 专属在 macOS 跳过。报告记录修复前失败与修复后三项定向成功的日志哈希，最终源码指纹逐项复核。

[365 场景 CLI 对照](../research/checkpoint-merge-cli-comparison.json) 无测试失败，维持 92 一致、54 保留差异、219 Rust 扩展；[17 项本机发行检查](../research/checkpoint-merge-native-release-validation.json) 通过，最终 debug/release 哈希与对照、打包报告匹配。新构建只取得本机合成协议与发行证据，不继承旧构建的 Linux/Rosetta 或真实 Drive 通过结论。

[远程发行预检查](../research/release-remote-preflight.json) 通过 GitHub API 只读核对：公开仓库无分支、无 Release。没有推送源码、触发工作流或发布；仓库许可元数据为空不代表已经完成源码复用授权判断。

本修复不会将历史真实 Drive 失败改记为一次无中断成功，也不能关闭跨账户、长期刷新、各认证来源和发行验收的缺项。
