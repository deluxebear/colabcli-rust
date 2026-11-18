# 上游来源与许可声明

本项目是 Rust 重实现，以下列固定版本作为接口、行为与协议研究基线。
列出这些来源不表示上游认可本项目，也不表示所有上游文件都被复制。
本文件及附带许可文本不授予本项目自身代码的整体许可证。

| 来源 | 固定版本 | 许可与声明 |
| --- | --- | --- |
| [Google Colab CLI](https://github.com/googlecolab/google-colab-cli) | `02d2321c38dd215b7804eebf78b61d98b856bcd3` | Copyright 2026 Google LLC；[Apache-2.0](licenses/google-colab-cli-Apache-2.0.txt)；本项目对 CLI 行为作 Rust 重实现并有文档所列修改 |
| [Jupyter Kernel Client](https://github.com/googlecolab/jupyter-kernel-client) | `f18e982c3265df5e923aa9def101ab3fd737e139` | Copyright (c) 2025, Datalayer；[BSD-3-Clause](licenses/jupyter-kernel-client-BSD-3-Clause.txt)；用于协议与 codec 对照 |
| [Jupyter Server](https://github.com/jupyter-server/jupyter_server/tree/v2.12.0) | `v2.12.0` | IPython Development Team / Jupyter Development Team；[BSD-3-Clause](licenses/jupyter-server-BSD-3-Clause.txt)；Kernel Client 的 utils.py 声明其部分实现源自该版本 |
| [colab-ext](https://github.com/deluxebear/colab-ext) | `980a39748badc3168a5af8734b2467351bdafcbc` | 固定版本无根 LICENSE；复用授权尚待确认，见源码文档 docs/60 的 U12；不得据此表格推定分发授权 |

Rust 依赖的精确版本与校验和保存在 Cargo.lock，各依赖仍受各自许可约束。
[Rust 依赖许可材料](licenses/rust-dependencies.json) 包含四个原生发行目标默认特性的 normal/build 依赖及归档内许可、版权和 NOTICE 文本；纯 dev 依赖不在此范围。相同原文按 SHA-256 去重，每个包保留文件路径与文本哈希映射。它是可复核的原始材料汇集，不代替许可义务与所选许可分支的人工判断。
