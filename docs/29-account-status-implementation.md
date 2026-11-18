# account status 联合查询

更新日期：2026-11-17。实现 account status 及隐藏别名 acct status，复用既有 [有界汇总](11-contents-and-inventory-implementation.md) 与账户认证服务。固定扩展的 [命令入口](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/account_commands.py#L547) 和 [账户探针](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/cli_hooks.py#L669) 是行为依据；不是 Google 线上互通验收。

## 输入和结果

```sh
colab account status
colab account status --json
colab acct status --json
```

此命令查询全部已登记账户，-a 和 COLAB_ACCOUNT 先校验有效性，但不限制查询范围，也不改变 active。按注册表文件顺序输出，最多并发四个账户；固定扩展最多八个，Rust 延续现有资源上限。网络请求采用全局控制面地址、请求期限等配置，凭据按显式账户上下文传递，不修改进程环境或切换持久 active。全局 --auth 显式覆盖仍生效，auth_provider 显示有效 provider。

每行包含 account、active、email、auth_provider、drive_account、drive_ready、usage、assignments 和 error。usage 的字段是 balance、rate_hourly、assignments_count。assignment 只输出 name、endpoint、hardware、accelerator、shape、machine_shape、variant，不复制 runtime token、URL、kernel 状态或任意额外字段。孤立 assignment 使用名称 ?；CPU、GPU/TPU 和内存形状沿用既有后端映射。

Drive 绑定沿用显式 Drive 环境、逐账户绑定、本账户凭据、全局默认和本账户的解析规则。drive_ready 仅检查 dfs_credentials.json 是否存在，不加载或刷新 Drive 凭据，不证明其完整、有效、具有 scope 或已挂载；需要进一步检查时使用 drive show/status。绑定解析失败时 drive_account 为 null，并记录当前行错误。

## 状态、失败与清理责任

每账户先查询 CCU，再查询 assignments。某一账户的认证、HTTP、协议或本地同步失败不会中断其他账户；已经取得的余额保留在该行，失败后的后续请求不继续。--json stdout 只输出数组，不夹杂进度或登录提示。部分或全部账户查询失败时命令仍返回 0，消费者必须逐行检查 error。无法读取注册表、恢复事务或无效全局配置等命令级错误返回非零。

不会启动 OAuth 交互流程，但可按现有认证策略刷新/轮换 OAuth 或 authorized_user ADC，可能访问所选凭据后端。ADC 使用独立来源，不改写源文件或 OAuth。每账户刷新和会话同步使用现有锁，不在整个网络汇总期间持有全局注册表锁。

成功的完整 assignments 响应按 SessionService 同步本地会话、刷新 proxy 和裁剪失效项；HTTP/协议失败时不裁剪，也不猜测 VM 已释放。该命令不创建或释放 VM，不清除挂载/执行恢复责任，不需要远端资源清理。并发账户修改可能让各行来自不同时间点，结果不是原子快照。

## 验证与尚未完成项

新增四项 Rust 集成测试，使用临时配置、合成文件凭据和 loopback HTTP：

- 多账户分别携带自己的 Bearer，保留命名/孤立 assignment、Drive 绑定，缺凭据账户仍有独立错误，active/绑定不变。
- CCU 成功后 assignments 失败保留余额；CCU 失败不再请求 assignments；原会话字节不因失败而变化，错误 body/秘密不输出。
- 混合 OAuth/ADC 独立身份，核对 ADC refresh 参数与各自请求数量，ADC 源文件和 OAuth 文件保持不变。
- 文本错误输出、无交互授权，以及无效 Drive 环境绑定在联网前变为行错误。

CLI 差分新增 status help 和六项业务场景，覆盖文本/JSON、凭据文件存在、孤立/空 assignment、HTTP 失败及 acct 别名。固定官方 CLI 没有 account 组，因此均标为 Rust 扩展；扩展源码仅作为契约依据，不宣称完成双端扩展运行时对比。并发上限与工作线程异常隔离另由现有 inventory 测试覆盖。

本阶段未扩大 ADC 类型、未访问真实 Google 或系统凭据库、未验收真实 Drive/FUSE。update/发行渠道、合并版 help 和完整线上/平台验收仍待完成。

完整回归期间还修正了既有 SSH 信号测试的就绪竞态：shell 重定向会先创建空 PID 文件，测试现等待有效 PID 才发信号，提前退出或超时时回收拥有的子进程并保留诊断。修正后两次完整 SSH 并发测试各 21 项通过。差分工具在默认 Rust 回归失败时只列失败测试名，避免丢失定位信息或将任意测试输出写入兼容报告。

最终验证：默认特性 282 项、无默认特性 280 项 Rust 测试通过；两种特性 Clippy、fmt、默认构建和研究文档检查通过。完整 CLI 差分 277 个场景为 92 一致、53 保留差异、132 Rust 扩展，无断言失败，原有 270 场景分类不变；root/account/drive/ext 清单一致，报告二进制哈希与当前 debug 文件相符。

后续会话/用量共享探针与固定合并版实际业务对照见 [44](44-merged-inventory-implementation.md)。
