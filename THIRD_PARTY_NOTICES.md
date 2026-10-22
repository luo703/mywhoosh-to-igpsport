# 第三方组件

本项目 Rust 源码采用 MIT 许可。Cargo 依赖各自遵循其上游许可，版本固定于 Cargo.lock。

## iGPSPORT 签名模块

`assets/edge_core.wasm` 来自 iGPSPORT 中国站前端，其来源、大小和 SHA-256 记录在 `assets/provenance.json`。

该模块属于第三方资源，不属于本项目 MIT 授权范围；当前未确认其再分发许可。因此 Git 忽略该文件，源码仓库仅提供来源记录及下载校验脚本。构建前由使用者从官网获取，构建后的单文件程序会内嵌该模块。发布包含该模块的二进制前需确认相应分发条件。

本项目为非官方工具，与 MyWhoosh、iGPSPORT 无隶属关系。
