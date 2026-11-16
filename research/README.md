# 研究证据与检查记录

`source-inventory.json` 固定三个仓库的提交、受 Git 跟踪文件的 SHA-256、静态命令入口及测试函数位置。它不包含用户凭据，也没有复制源码正文；命令参数取自 AST。

`documentation-checks.json` 是文档检查工具的结果，记录相对链接、固定源码行号及清单哈希的核对情况。零错误仅表示这些静态检查通过，不等于文档中的每项设计已经实现，也不等于真实 Colab 验证通过。

`kernel-codec-probe.json` 保存七项离线协议检查结果及合成帧。这些帧来自固定依赖的纯编解码函数，含中文、非 UTF-8 和空 buffer，可供 Rust 实现重放对照；它们不是真实账户抓包。

三个仓库分别检出文档指定提交后，从项目根目录执行：

```sh
python3 tools/collect_source_inventory.py \
  --cli /absolute/path/google-colab-cli \
  --ext /absolute/path/colab-ext \
  --kernel /absolute/path/jupyter-kernel-client \
  --output research/source-inventory.json

python3 tools/check_research_docs.py \
  --cli /absolute/path/google-colab-cli \
  --ext /absolute/path/colab-ext \
  --kernel /absolute/path/jupyter-kernel-client \
  --output research/documentation-checks.json

python3 tools/probe_kernel_codec.py \
  --kernel /absolute/path/jupyter-kernel-client \
  --output research/kernel-codec-probe.json
```

生成过程使用 Python 标准库和 Git，不安装上游依赖、不导入上游模块、不访问真实账户。codec 探针从已核验哈希的 utils.py 提取五个纯函数执行，不运行模块的其他顶层代码。这些 Python 脚本是研究工具，不属于未来 Rust 客户端的运行时依赖。

`run-wrapper-probe.json` 保存八项本地 Python 合成脚本检查，由 `python3 tools/probe_run_wrapper.py --output research/run-wrapper-probe.json` 生成。它从 Rust 源码提取包装模板，验证退出码、argv、future import 和 finally 结果；使用假的 IPython.display，不请求 Colab，不证明真实 Google 互通。
