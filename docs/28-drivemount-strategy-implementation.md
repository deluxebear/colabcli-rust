# 根 drivemount 策略与回退

更新日期：2026-11-17。本阶段将根 drivemount 接入已有独立 DriveFS 编排；不是长期保活、自动重建或任务检查点恢复。独立凭据与远端生命周期见 [24](24-drivefs-lifecycle-implementation.md)、[25](25-drivefs-inputs-implementation.md)，官方授权协议见 [16](16-colab-automation-implementation.md)。

## 输入与策略

```sh
colab -a compute drivemount -s work --dry-run
colab -a compute drivemount -s work --readonly --no-fallback
colab -a compute drive umount -s work
colab -a compute drivemount -s work --upstream
```

默认路径直接调用原生 drive mount 编排，不启动本地 Python 或子 CLI。账户解析、绑定、凭据来源、shim/rt、私密输入、轮换回收与恢复状态沿用原编排。--cli-token 固定使用计算账户，不接受 --drive-account。两种策略都只操作已有 VM，不申请新 VM。

显式 --upstream 或 COLAB_DRIVE_DEFAULT 的 0/false/no/off（忽略大小写与首尾空白）选择官方策略。主变量为空或未设置时兼容 COLAB_DFS_DEFAULT。官方策略拒绝独立 Drive 账户、跨账户绑定及 DriveFS 专用参数，包括 readonly、force、purge-state、from-env、cli-token、非默认 mode/port、drive-extra、via-cli、dry-run 与 user-id/email 覆盖。后两项从环境预填时也算覆盖。不静默忽略这些要求。

默认缺少凭据时，只有 stdin 是交互终端、没有 DriveFS 专用要求、Drive 仍为计算账户、没有本地恢复责任，才在 stderr 提示后回退官方流程。--no-fallback 禁止自动回退；它不禁止明确选择的官方策略。非交互任务必须先准备凭据或显式 --upstream。已有凭据文件损坏、不完整或有刷新暂存时交给原编排报告错误，不按缺失处理。--from-env 输入缺失也不回退。

默认等待期限为 30 秒。DriveFS 接受 1–3600 的整数秒；显式官方路径保留浮点期限及非正数不限时语义。原生 JSON 使用 drive mount 结果结构；官方路径保留自动化结果结构，不宣称两者同构。

## 状态变化、失败与清理

[策略入口](../src/drivemount_cli.rs) 在远端执行前确定唯一策略；调用原生编排之后不捕获错误转向官方。发送后断线、失败或结果未知都保留原计划及租约，按原 drive status/umount 流程核查与回收，不重复挂载。

官方策略不能绕过相交路径的 drive_plan、drive_mounts 或受管租约。路径比较采用远端 POSIX 规则，处理重复斜线、尾斜线、点路径及父子目录；损坏状态拒绝继续。官方执行前取得与原生编排相同的计算账户/endpoint 操作锁，再核查 VM、Drive 绑定及当前计划/挂载/租约，并持锁直到执行结束，避免早期分派检查后并发创建的新责任被覆盖。错误与退出由 RAII 释放本地锁，不能手删恢复记录。

## 验证与边界

新增五项 Rust 测试覆盖无终端/无回退拒绝、环境策略及参数冲突、缺失与已有恢复责任、路径别名/父子冲突、真实 PTY 下跨 Drive 拒绝、默认跨账户挂载到轮换回收、发送后未知禁止换策略，以及早期检查之后并发写入 DriveFS 责任的晚核查。已有 PTY 测试继续覆盖自动回退、人工输入和干净的 JSON stdout。

CLI 差分新增八个扩展场景。两个既有官方挂载场景在 Rust 端显式加 --upstream，并在报告中记录 mount_strategy=explicit_upstream；官方端仍运行固定官方实现，不把默认 DriveFS 与官方默认流程归一化为相同。

这些测试使用临时配置、合成凭据与 loopback HTTP/WebSocket；没有真实 Google 授权、DriveFS/FUSE 挂载或跨 token 到期读写验收。客户端不承担 supervisor 自动重建与脚本检查点责任。合并版 help、update/发行渠道、其他 ADC 类型与平台验收仍待完成。

最终验证：默认特性 278 项、无默认特性 276 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 270 个场景为 92 一致、53 保留差异、125 Rust 扩展，无断言失败，原有 262 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。
