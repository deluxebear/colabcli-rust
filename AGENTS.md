# Repository Guidelines

## 项目结构与模块职责

本项目用 Rust 实现 Colab CLI 与 colab-ext，当前完成账户、认证与存储基础模块。

- `src/main.rs`：`colab` 命令入口；`src/lib.rs`：库入口。
- `src/accounts.rs`、`context.rs`：账户生命周期、算力与 Drive 账户解析。
- `src/auth.rs`、`credentials.rs`：OAuth、ADC、系统凭据库与文件后端。
- `src/models.rs`、`storage.rs`、`error.rs`：兼容模型、锁、持久化和错误。
- `tests/`：账户、认证、存储和 CLI 集成测试；共享替身位于 `tests/common/`。
- `docs/`：实现规格及阶段状态；`research/`：固定源码证据；`tools/`：Python 研究工具。

## 构建、测试与开发命令

```sh
cargo build --locked                       # 构建 colab
cargo run -- --help                        # 查看命令
cargo fmt --check                          # 检查格式
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked                        # 默认系统凭据库特性
cargo test --locked --no-default-features  # 无原生后端的构建路径
```

GitHub Actions 配置 Linux/macOS 检查。研究文档修改后，使用 `tools/check_research_docs.py` 核对链接和固定源码；参数见 `docs/README.md`。只有更新上游基线时才重新生成源码清单。

## 编码与命名约定

始终用中文沟通。开发前明确输入、状态变化、失败路径和清理责任。Rust 使用 rustfmt 默认格式、四空格缩进；模块与函数为 `snake_case`，类型为 `UpperCamelCase`。通过显式上下文传递账户，禁止通过修改进程环境切换认证身份。文档采用 `NN-topic.md`，区分研究事实、目标设计和已实现行为。

## 测试要求

使用 Rust 内置测试框架，测试命名描述行为，按领域放入 `tests/<feature>.rs`。没有固定覆盖率门槛；认证、迁移和锁变更必须覆盖失败恢复、账户隔离及并发路径。使用临时目录、内存凭据库与 loopback HTTP，不依赖开发者真实凭据。离线通过不代表 Google 线上兼容或原生凭据库互通。

## 提交与 Pull Request

Git 已初始化，尚无提交历史；建议使用 `feat:`、`fix:`、`test:`、`docs:` 前缀。PR 应说明问题、最终行为、关联规格或 issue、验证结果与兼容差异；终端行为变化附输出示例。更新实现时同步 `docs/09-foundations-implementation.md`。

## 凭据与运行边界

禁止提交 token、授权码、client secret 和真实账户数据。不要让 Python 与 Rust 同时修改同一配置目录。重命名或删除中断后通过 `account repair` 恢复，不能手删事务日志绕过保护。真实测试仅清理本次创建的资源；supervisor 尚未实现，不得宣称无限保活或自动恢复已经可用。
