# 每日原生发行检查

## 输入与输出

V01.6 的原生手动更新之外，现已接入交互式每日检查。发行源沿用运行时 COLAB_CLI_UPDATE_MANIFEST、编译时 COLAB_CLI_BUILTIN_UPDATE_MANIFEST 的优先级；没有发行源时不请求网络。目前公开仓库尚未发布 Release，因此开发构建不会自动查询不存在的清单，也不会查询 PyPI。

命令成功完成后，且仅 stdout 和 stderr 都是终端时，账户 list/current/status/use/set-drive/rename/remove 与 sessions/usage/status/new/stop 启用检查。JSON、url-only、重定向、管道和其他命令均跳过；SSH、补全、信息命令、认证 URL 及修复入口不受每日检查影响。有更新时只向 stderr 提示版本、colab update --install 和关闭方法，不下载或执行程序，也不自动安装。

settings.json 的 enable_update_check=false 为总开关。手动 update 保持显式请求语义，不受该开关限制；其默认请求期限仍为 30 秒。

## 与原版的边界

固定基线的 [auto_update.py](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/auto_update.py) 从 PyPI 获取版本，成功检查才更新时间，并可在非终端命令显示提示。Rust 保留每日节流、缓存提示和总开关，使用原生发行源；失败也节流，并把提示限制在上述交互命令。这些是明确的发行和输出策略差异，不宣称与 Python 的失败重试次数完全相同。

## 状态、失败与清理

缓存保存在 settings.json 的 native_update_cache 字段，包含源与当前平台的 SHA-256 标识、checked_at、latest_version。与上游 Python 的 update_url/last_check/latest_version 分离，未知 settings 字段保留。更换发行源后旧版本提示不复用，同源只保留最高观察版本；与当前程序比较时忽略 SemVer build metadata。

使用独立进程锁串行检查，竞争超过 25 毫秒即放弃。成功或失败的请求都节流 24 小时；已缓存的新版本在当天后续交互命令仍可提示。时钟回退导致 checked_at 位于未来时重新检查。HTTP 使用已有清单验证与重定向策略，请求总超时 2 秒；这是同步短检查，不是后台常驻进程。

HTTP、清单、缓存损坏、路径或锁错误都不改变命令退出状态，也不显示响应正文。已有有效缓存遇到短暂请求失败时保留版本。写回采用 settings 的读改写锁，保留并发更新的其他字段，并再次尊重被关闭的总开关。普通关闭与超时由 reqwest 回收连接，没有 VM、下载暂存文件或子进程需要清理；检查锁文件可持久保留，不需要手删。

## 验证与边界

[每日检查测试](../tests/update_daily.rs) 使用临时配置、伪时间、loopback HTTP 与 Unix PTY，覆盖日内缓存、24 小时重查、不降级、更换源、PyPI 字段隔离、失败节流、时钟回退、关闭、锁竞争、损坏配置、无响应超时，以及实际 CLI 的终端提示和 JSON/管道/信息命令抑制，以及无效账户选择失败时不触发检查或写入更新缓存。原有 [原生更新测试](../tests/update.rs) 继续验证手动检查、安装与恢复。

早期 PTY 测试在子进程退出后才读取主端，macOS 上未可靠收到完整输出；已改为启动后持续读取，再回收子进程。此处没有用绕过终端判定的环境变量替代实际 PTY 验证。

本阶段没有发布 GitHub Release，也未完成真实 GitHub 下载或其他平台验收。此前研究 JSON 中的二进制哈希是对应阶段捕获的历史证据，不代表新增检查后的当前产物；本阶段以新增定向测试和全量 Rust 检查为证据。

本轮默认特性全量 403 项、无默认特性全量 401 项通过。最终入口顺序调整后，两种特性各重新执行 15 项手动更新及 8 项每日检查回归，并运行 Clippy；格式和文档检查通过。没有重新采集旧业务差分或 release 报告。
