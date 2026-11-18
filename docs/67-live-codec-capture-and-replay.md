# 真实 DEFAULT WebSocket 采集与重放

2026-11-18（Asia/Shanghai）。本阶段补齐 U06 的真实 DEFAULT 文本/二进制样本、上游对照和 Rust 重放证据，使用用户已授权的单台 CPU 与现有系统钥匙串。没有访问 Drive。

## 采集边界与清理

[执行工具](../tools/validate_live_codec.py) 默认只做预检查，`--execute` 才允许分配。账户、认证来源、配置路径显式传入；不切换父进程环境中的身份。控制面调用继承工具的终端环境，并为此次系统钥匙串等待显式设置 120 秒，产品默认仍为 30 秒。分配前独立查询 baseline、拒绝已有控制面事务和同名会话，先写私有意图账本，再请求专属 CPU。只有非 baseline 的精确 endpoint 与请求会话匹配才记录所有权。

原生 CLI 执行固定 `pass`，创建并持久化 kernel 后，[开发探针](../examples/live_codec_probe.rs) 从 stdin 接收该会话状态及预期 endpoint/kernel，核对后只连接已有 kernel。探针使用库中原生代理传输、TLS、DEFAULT codec；显式 `--capture` 才发送固定脚本，`--replay` 只在本地处理输入样本。没有接受任意代码、创建或删除 kernel/VM 的接口。请求禁止 stdin，输出固定文本与显式携带 `[00 ff 80 41]`、空 buffer 的 display_data，不访问文件或 Drive。

采集只选择本次 request parent 下两种完全匹配的输出，同时等待成功 execute_reply 和对应 iopub idle。其他输出不会保存；消息数量、单帧大小、读取和总期限有界。完整 WS 请求 URL、认证 header、proxy token 和输入状态不进入捕获输出。原始两帧及解码结果只写入外部私有目录（0700/0600）。

失败不会重发采集或自动重新分配。finally 只按本次账本的 endpoint 条件 stop，之后独立 inventory 确认释放；无法确认时保留账本。cleanup-only 只清理已记录实例，既不创建 VM，也不依赖开发探针或上游源码仍可用。未知分配且没有可验证 endpoint 时保留 allocation_unknown，不猜测清理其他资源。

## 原始消息对照与脱敏

Rust 在真实 socket 上先直接解码收到的两帧。Python 随后从固定 kernel 基线提取三个已校验 SHA-256 的纯函数，直接解码同一原始响应，比较 envelope 与每个 buffer。空的 JSON buffers 便利字段按协议语义归一化；非空内联 buffers 拒绝。没有导入上游包或调用其认证/配置入口。

公开 [样本](../research/live-codec-samples.json) 保留实际固定 content、协议版本和 buffer 字节；header 身份/时间替换，metadata 与顶层便利字段移除，再用固定上游函数重新编码 JSON 与 offset。公开样本与原始抓包的字节不同；[真实报告](../research/live-codec-validation.json) 分别记录原始直接解码一致和公开样本重放通过。Rust 探针及 [集成测试](../tests/live_codec.rs) 均验证脱敏样本的解码与语义往返，二进制含非 UTF-8 与零长度 buffer。

## 真实结果与局限

一次真实 CPU 完成文本、二进制、非 UTF-8、空 buffer、成功 reply 与 idle 观察，原始 Rust/上游解码一致，两个公开样本重放通过。原始二进制 count=3、首 offset=16，两个 buffer 长度分别为 4 和 0。条件 stop 后独立后端确认 endpoint 消失，其他新增会话为零。

这补齐当前 DEFAULT codec 的真实采集与对照缺口；其他消息类型、断线/重启、长期刷新及平台仍按各自 V/U 条目验收。探针直接发送固定测试请求，不替代普通 exec 的持久化与未知结果保护测试。U06 关闭不代表整个设计目标完成。

[最终专项回归](../research/live-codec-regression-validation.json) 的 fmt、两种特性全目标 Clippy 通过；既有 codec 三项和新增真实样本一项在默认/无默认两种特性均通过。Python 170 项中 169 通过、1 项平台跳过，其中七项新工具回归覆盖私密字段清除、非预期内容/decoder 分歧拒绝、分配前认证失败、endpoint 替换、采集失败不重放且只释放所属 VM、cleanup-only 的独立恢复。

执行器在收尾时补齐失败后非零退出状态，完成受控清理后返回 1。真实报告保留实际采集版本的源码哈希，修正后的当前版本另有 [零创建的真实预检查](../research/live-codec-preflight.json) 和失败路径测试；没有为退出码调整再次分配 VM。最终 CLI 与探针字节均与采集时哈希相同，产品源码也与 [66 的完整回归](../research/checkpoint-merge-regression-validation.json) 一致。本阶段新增开发工具、脱敏样本和专项测试。
