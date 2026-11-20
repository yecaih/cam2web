# cam2web

一个 Linux V4L2 相机调试工具：在浏览器里看实时预览、读写相机控件。

## 运行

```sh
cargo run --release -- config/orin.toml
```

`config/orin.toml` 是 Jetson Orin 上的 Himax RGB / ToF 示例；其他相机请换成对应的配置。已有可执行文件时，直接运行 `./cam2web config/orin.toml`。

用示例配置启动后，打开 <http://localhost:8766/>（远程访问时把 `localhost` 换成板子的 IP），选中相机，点击「启动预览」。运行账号需要有 `/dev/video*` 的访问权限。服务无鉴权，请只在可信网络使用。

## 交叉编译

以 Linux ARM64 为例，先准备 ARM64 GNU 链接工具链（含目标系统库），再运行：

```sh
rustup target add aarch64-unknown-linux-gnu

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
cargo build --release --target aarch64-unknown-linux-gnu
```

产物在 `target/aarch64-unknown-linux-gnu/release/cam2web`。V4L2 使用仓库内的预生成 Rust 接口，构建时不需要 V4L2 头文件或 C shim。
