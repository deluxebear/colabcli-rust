# Drive 真实验收的受限目录准备

2026-11-18（Asia/Shanghai）。本章交付离线准备工具和远端探针，推进 V07 的真实验收；没有挂载、刷新凭据、访问云盘或创建 VM。本章交付时线上生命周期执行器与独立存储身份核对尚未完成，不能将“准备完成”改记为 Drive 通过。

## 输入和生成物

[prepare_live_drive_validation.py](../tools/prepare_live_drive_validation.py) 要求已有计算/Drive 资料、当前二进制和新的私有 state-dir。只直接读取本地注册表及 Drive JSON 的字段存在性、完整 scope 声明；不调用 CLI、不恢复刷新暂存、不访问系统钥匙串或 Google，不复制凭据中的 client/token/user-id/email 字段。资料名本身可能采用邮箱形式，因此私有计划仍不能提交。拒绝非法资料名、缺资料、不完整 scope、符号链接凭据和已有 state-dir；输出目录不能位于配置目录内。

```sh
python3 tools/prepare_live_drive_validation.py \
  --config-dir /absolute/config \
  --compute-account COMPUTE_PROFILE --drive-account DRIVE_PROFILE \
  --token-storage keyring --state-dir /tmp/new-private-drive-plan
```

state-dir 为 0700；plan.json、commands.json、三个 Python payload 和 preparation.json 均为 0600。私有 plan 包含冻结的账户资料名、配置路径、构建哈希、随机会话名、随机挂载点和云盘子目录，不含凭据正文；不要将它原样提交。准备报告只含布尔值、计数和哈希，见 [本机准备](../research/live-drive-validation-preparation.json)。本次候选资料的注册表与凭据原字节保持不变。

commands.json 给出 14 步参数数组：基线查询、租约前的身份/刷新验证、新 CPU 分配、读写挂载/创建校验/卸载、只读挂载/写拒绝/卸载、读写重新挂载/精确清理/最终卸载，以及绑定 endpoint 的 stop 和后端确认。身份命令的 access token 输出明确标注仅内存使用，不能写入日志。stop 的 endpoint 仍是占位符，必须从真实拥有账本解析；这份清单不是可以逐行盲跑的执行器，executable_workflow=false、execution_authorized=false。

未来执行前仍须检查原有事务/租约，核对刷新前后存储身份，冻结计算与 Drive 上下文并清除子进程的 COLAB_* 覆盖，不修改全局环境。真实分配必须返回新 NONE CPU 且不在基线；每个运行时操作前核对账户、名称和 endpoint。任何未知 mutation 都先核查，不自动重放；卸载与凭据回收未确认时保留账本和 VM，不为“清理成功”丢失轮换结果。这些是待执行器落实的条件，不由准备工具代替证明。

## 远端探针边界

[drive_validation_payload.py](../tools/drive_validation_payload.py) 被嵌入三个独立脚本，只用标准库。挂载点固定为 /content/colabcli-drive-随机ID，云盘范围固定为 MyDrive/colabcli-validation-同一随机ID；拒绝其他名称或路径。Linux 探针以已打开目录的设备号核对 /proc/self/mountinfo 中相同路径及 FUSE 类型；不能把普通本地目录冒充挂载。

创建阶段以 mkdir 和 O_EXCL 建立新目录、归属 JSON 和 1,043 字节的二进制/Unicode 样本，然后精确回读。已存在目录即拒绝，即使标记相同也不把未知结果当作可重放创建。归属包含随机 ID、固定目录名及样本 SHA-256，不读取 MyDrive 其他条目或用户文件。

只读阶段先验证标记与样本，再尝试向样本偏移 0 写回原有同一字节；仅 EROFS/EACCES/EPERM 可作为该写入被拒绝的证据，其他 I/O 错误不是只读成功。若写入被允许，则验收失败，样本内容仍保持原样。该局部断言必须结合真实 readonly 挂载结果使用，不独立证明整个 Drive 或用户身份。

归属标记必须与本探针生成的序列化字节完全一致，类型、重复字段或格式被改动也拒绝，不使用宽松 JSON 相等作为删除依据。清理不递归：目录条目只能是两个已知文件，或前次删除样本后剩余的匹配归属文件；内容或标记改变、出现额外文件、符号链接、FIFO 等都拒绝。读取使用目录 fd、O_NOFOLLOW、O_NONBLOCK、普通文件类型和长度上限。仅删除验证过的样本/标记，再 rmdir；新增条目会使 rmdir 失败，不递归删除用户内容。标记已经丢失、目录被移动或清理结果未知时不能猜测归属，应保留计划并检查。准备阶段不提供自动执行/重试入口。

## 验证和未完成项

[17 项回归](../tools/tests/test_live_drive_preparation.py) 覆盖命名范围、精确回读/清理、现有目录拒绝、只读真假与错误分类、归属/内容/额外文件、符号链接和 FIFO、部分清理、FUSE 设备/路径/类型核对，以及准备阶段配置/环境不变、不导出凭据、文件权限与非执行清单。文件行为测试在本地临时目录运行，模拟 FUSE guard；生产 guard 另以合成 mountinfo 验证，不是实际 FUSE 挂载。

完整 Python 工具 108 项中 107 项通过，1 项 Linux 专属在 macOS 跳过，见 [工具验证](../research/live-drive-preparation-validation.json)。Rust 产品未改动，沿用上一阶段 467/465 项回归和发行证据，不重复声称重跑。

本次没有调用云盘写入；V07 仍缺实际身份、挂载读写、只读挂载、超过初始 token 有效期刷新、跨账户与故障清理证据。现有本地 Drive 文件只是候选，完整执行仍须落实上述拥有关系、回收与失败恢复，并明确选用的资料及随机测试目录范围。当前计算钥匙串的交互认证也仍未完成，不重复触发系统窗口。

后续受控执行器、endpoint 锁内保护与当前构建回归见 [63](63-drive-live-validation-executor.md)；本章测试数量保留为准备阶段的历史证据。
