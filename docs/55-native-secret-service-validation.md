# Linux 原生 Secret Service 与 Python 互通

本章补充 V03.6、V03.7 与 U04/U10 的原生凭据证据。环境为独立 Linux ARM64 容器，UID 1000，Rust 1.100.0、keyring crate 3.6.3、GNOME Keyring 42.1、Python keyring 23.9.3。全部密码、token 与账户名称均为合成值，没有访问 Google、用户 HOME 或本机钥匙串。

## 输入与隔离责任

[专用镜像](../tools/secret-service-validation/Dockerfile) 基于前阶段的项目自有 Linux 验证镜像，只在镜像构建阶段联网安装 GNOME Keyring、D-Bus 和 Python keyring。运行时关闭外网、根文件系统和仓库只读，HOME、runtime、Cargo target 与报告使用本次临时卷；/tmp 为私有 tmpfs，不挂载 Docker socket 或桌面会话 socket。

[私有 session bus 配置](../tools/secret-service-validation/session.conf) 不包含 service directories，服务停止后不能自动激活另一守护进程；也不提供 GUI prompter。启动方通过 docker inspect 核对 UID、网络和全部挂载，并确认验证镜像继承已记录的基础镜像层。容器内 [编排器](../tools/secret-service-validation/run.py) 用 dbus-run-session 建立新总线，核对 bus PID 为当前 UID 拥有的 dbus-daemon，所有停止/暂停操作只针对本次拥有的进程。

GNOME Keyring 在受控 foreground 进程首次启动时通过 stdin 接收合成登录密码，显式使用 --unlock；不通过命令参数或环境传递密码，不 eval 守护进程输出。启动方式依据 [GNOME 42.1 初始化分支](https://raw.githubusercontent.com/GNOME/gnome-keyring/42.1/daemon/gkd-main.c)：启动时读取的密码用于解锁或创建登录集合。首轮先启动、再尝试另一进程解锁没有建立默认集合，测试尚未开始即失败；已修正为首次 foreground 启动带入密码，并显式确认集合存在且未锁定，没有改动产品代码或放宽断言。

## 真实客户端边界

[五项 Rust 原生测试](../tests/secret_service.rs) 调用实际 SystemKeyring、TokenStore 和 crypto-rust Secret Service 后端，不注入内存替身。[独立 Python 客户端](../tools/secret-service-validation/client.py) 显式选择 keyring.backends.SecretService.Keyring，既不采用用户配置的 backend，也不导入完整官方 CLI。凭据 JSON 经 stdin 传递，成功不输出凭据。

服务名 google-colab-cli、旧用户名 oauth_token 对齐固定 [官方 auth 常量](https://github.com/googlecolab/google-colab-cli/blob/02d2321c38dd215b7804eebf78b61d98b856bcd3/src/colab_cli/auth.py#L57)；账户槽 oauth_token:PROFILE 对齐 [扩展前缀](https://github.com/deluxebear/colab-ext/blob/980a39748badc3168a5af8734b2467351bdafcbc/colab_ext/accounts.py#L30)。此处证明对应 Python keyring API 的真实存储互通，不把它描述为完整原版登录或 OAuth 验收。

| 场景 | 实际断言 |
| --- | --- |
| 正常互通 | Python 写、Rust 读；Rust 轮换后 Python 读；Unicode/未知 JSON 字段保留；双账户并发写保持隔离；两个方向删除可见 |
| 旧槽迁移 | 复制旧槽至 default；保留源；重复迁移幂等 |
| 集合锁定 | 已存在的合成条目因集合锁定而受控失败，调用小于四秒，不泄露条目正文 |
| D-Bus 无响应 | SIGSTOP 私有 bus，连续两次调用均按一秒 caller timeout 返回；finally 恢复 bus，不承诺后台 OS 调用即时终止 |
| 服务停止 | 严格 keyring 写失败且不产生 token 文件；auto 创建 0600 文件与 authoritative 标记；file 模式继续读写；跨后端删除失败保留文件 |
| 服务恢复 | 原 auto 文件仍优先，auto 继续写文件不覆盖系统旧值；只有显式 keyring 写成功后删除 fallback/标记，随后恢复系统读取与删除 |

这些测试在 Linux + native-keyring 下编译，但默认全部 ignored；运行 --ignored 还必须满足明确的隔离 marker、UID 1000、固定临时 HOME 和 /tmp 下的私有 D-Bus 地址。普通 cargo test 不会访问真实系统 keyring；macOS 上该测试目标实际为 0 项。五项实际验证由编排器按阶段逐项执行，不把默认 ignored 记为通过。

## 结果与复现

[脱敏报告](../research/secret-service-validation.json) 记录 12 个阶段全部退出 0；Rust 1.85.1 编译、稳定版 Clippy 与格式检查通过，默认五项 ignored 的保护生效，随后五项原生测试各通过一次。集合锁定约 0.17 秒、两次无响应 caller timeout 合计约 2.13 秒；这些为本机观察，不是服务延迟承诺。146 份受检实现/测试/工具/资源输入在容器内前后与宿主复核中哈希一致。

复用 [54 的隔离启动参数](54-linux-container-validation.md)，镜像改为 colabcli-rust-secret-service-validation:local；其 CMD 会自行启动 dbus-run-session 和编排器。失败先复制 /validation/reports，确认容器已退出后才清理本次 container ID 和匿名卷。最终一个容器、两个临时卷均已移除并核对不存在；构建镜像保留为开发缓存。

[专用 CI 候选](../.github/workflows/secret-service.yml) 使用相同无外网、非 root、只读挂载方式，不持久化 checkout 凭据；只读仓库权限，日志作为短期测试 artifact 保存。远程尚未执行，不将本机 ARM64 结果解释为 GitHub x86_64 runner 通过。

本阶段没有修改产品 Rust、Cargo 依赖、README 或内嵌资源；上一阶段全量回归与 macOS 发行/CLI 报告保留各自历史证据，本轮没有重新采集完整 CLI 差分。

## 尚未证明

没有验证宿主 Linux 桌面交互解锁、真实用户 D-Bus 政策、其他 Secret Service 实现、macOS/Windows 原生凭据或完整官方 CLI。没有执行 Python/Rust 同时写同一配置目录；共享文件锁互通尚未建立，仍禁止并发混用同一配置目录。没有登录 Google，也不据此关闭完整 U04、U10、OAuth 或 ADC 矩阵。

研究文档检查覆盖 60 篇 Markdown、673 个相对链接、127 个固定源码引用和 143 个固定仓库源码哈希，无错误；本次原生测试的 146 份工作区输入另行记录，不合并计数。macOS 的专用目标实际运行 0 项，确认平台门控；现有产品来源和二进制/报告哈希保持一致。
