# 真实 kernel 崩溃与显式恢复验收

2026-11-18（Asia/Shanghai）。沿用用户对真实 CPU 与现有系统钥匙串的授权，没有访问 Drive。产品 Rust 源码和 debug 二进制未修改。

## 输入、执行和清理责任

[执行器](../tools/validate_live_disconnect.py) 的 `--fault kernel-death --execute` 分配专属 CPU；默认预检查不分配。新增可选 `--terminal-keyring` 继承控制终端，缺少终端时在创建状态目录和认证前拒绝。账户与配置显式传入，产品默认等待仍为 30 秒。该方式成功读取钥匙串，不证明既有非终端超时或重复提示的根因已经解决。

所有 kernel-death 运行时命令附带同一预期 endpoint，避免同名会话被替换后执行到其他实例。控制面仍使用已有分配意图、归属校验、条件释放及独立 inventory 确认。失败不自动重放崩溃脚本；私有账本和原始输出保存在仓库外受限目录，公开报告没有账户、真实路径、endpoint 或凭据。

## 两次真实尝试

[首次报告](../research/live-kernel-death-first-attempt.json) 保留失败：实际生命周期广播产生 Unknown，但输出列表为空，未满足部分输出断言。脚本打印后立即硬退出，不能由该结果推定客户端丢失输出。该台 VM 已确认释放。

随后增加成功 `pass` 预热，并在 flush 输出后留一秒发送窗口再 `os._exit(7)`。没有放宽结果、错误原因或重放保护断言。[第二次报告](../research/live-kernel-death-validation.json) 四项通过：

1. 实际 kernel 生命周期广播令 JSON 执行返回非零、Unknown，并保留本次输出标记。
2. 持久化 pending 与本次 msg_id 相同，再次执行同脚本被拒绝。
3. 独立 Contents 下载确认副作用文件只有一行 `once`。
4. 显式 restart-kernel 清除 pending，之后执行成功，副作用仍只有一次。

收尾额外直接读取原始 CLI 回执，独立核对 Unknown、准确生命周期错误、部分输出、拒绝重放的同一 msg_id、下载字节、重启和后续成功结果；公开报告仅记录布尔。两台本次 CPU 均已独立后端确认释放，各次其他新增会话均为零。

## 回归与边界

[工具测试](../tools/tests/test_live_disconnect_validation.py) 新增终端模式、无终端提前拒绝和所有运行时调用 endpoint 条件的回归；该模块 15 项通过。完整工具回归结果及源文件哈希见 [回归报告](../research/live-kernel-death-regression-validation.json)。本轮仅改测试工具，不重新构建产品；当前二进制与真实执行时 SHA-256 一致，生产源码与 [66 完整回归](../research/checkpoint-merge-regression-validation.json) 的输入逐项核对。

本次覆盖 macOS ARM64 单账户、一次有输出发送窗口的真实 kernel 硬退出，不证明所有 restarting/dead 时序、零窗口输出、交互输入、run 故障、长连接、token 到期或其他平台通过。首次失败及此前钥匙串预检查超时仍保留；V05.5 的真实生命周期子场景得到证据，完整 V05.5 和总体目标继续保持未完成。
